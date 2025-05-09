//! Sequencer-specific API options and initialization.

use std::sync::Arc;

use anyhow::{bail, Context};
use clap::Parser;
use espresso_types::{
    v0::traits::{EventConsumer, NullEventConsumer, PersistenceOptions, SequencerPersistence},
    BlockMerkleTree, PubKey,
};
use futures::{
    channel::oneshot,
    future::{BoxFuture, Future},
};
use hotshot_events_service::events::Error as EventStreamingError;
use hotshot_query_service::{
    data_source::{storage::SqlStorage, ExtensibleDataSource, MetricsDataSource},
    fetching::provider::QueryServiceProvider,
    status::{self, UpdateStatusData},
    ApiState as AppState, Error,
};
use hotshot_types::traits::{
    metrics::{Metrics, NoMetrics},
    network::ConnectedNetwork,
    node_implementation::Versions,
};
use tide_disco::{listener::RateLimitListener, method::ReadState, Api, App, Url};
use vbs::version::StaticVersionType;

use super::{
    data_source::{
        provider, CatchupDataSource, HotShotConfigDataSource, NodeStateDataSource, Provider,
        SequencerDataSource, StateSignatureDataSource, SubmitDataSource,
    },
    endpoints, fs, sql,
    update::ApiEventConsumer,
    ApiState, StorageState,
};
use crate::{
    catchup::CatchupStorage,
    context::{SequencerContext, TaskList},
    persistence,
    state::update_state_storage_loop,
    SequencerApiVersion,
};

#[derive(Clone, Debug)]
pub struct Options {
    pub http: Http,
    pub query: Option<Query>,
    pub submit: Option<Submit>,
    pub status: Option<Status>,
    pub catchup: Option<Catchup>,
    pub config: Option<Config>,
    pub hotshot_events: Option<HotshotEvents>,
    pub explorer: Option<Explorer>,
    pub storage_fs: Option<persistence::fs::Options>,
    pub storage_sql: Option<persistence::sql::Options>,
}

impl From<Http> for Options {
    fn from(http: Http) -> Self {
        Self {
            http,
            query: None,
            submit: None,
            status: None,
            catchup: None,
            config: None,
            hotshot_events: None,
            explorer: None,
            storage_fs: None,
            storage_sql: None,
        }
    }
}

impl Options {
    /// Default options for running a web server on the given port.
    pub fn with_port(port: u16) -> Self {
        Http::with_port(port).into()
    }

    /// Add a query API module backed by a Postgres database.
    pub fn query_sql(mut self, query: Query, storage: persistence::sql::Options) -> Self {
        self.query = Some(query);
        self.storage_sql = Some(storage);
        self
    }

    /// Add a query API module backed by the file system.
    pub fn query_fs(mut self, query: Query, storage: persistence::fs::Options) -> Self {
        self.query = Some(query);
        self.storage_fs = Some(storage);
        self
    }

    /// Add a submit API module.
    pub fn submit(mut self, opt: Submit) -> Self {
        self.submit = Some(opt);
        self
    }

    /// Add a status API module.
    pub fn status(mut self, opt: Status) -> Self {
        self.status = Some(opt);
        self
    }

    /// Add a catchup API module.
    pub fn catchup(mut self, opt: Catchup) -> Self {
        self.catchup = Some(opt);
        self
    }

    /// Add a config API module.
    pub fn config(mut self, opt: Config) -> Self {
        self.config = Some(opt);
        self
    }

    /// Add a Hotshot events streaming API module.
    pub fn hotshot_events(mut self, opt: HotshotEvents) -> Self {
        self.hotshot_events = Some(opt);
        self
    }

    /// Add an explorer API module.
    pub fn explorer(mut self, opt: Explorer) -> Self {
        self.explorer = Some(opt);
        self
    }

    /// Whether these options will run the query API.
    pub fn has_query_module(&self) -> bool {
        self.query.is_some() && (self.storage_fs.is_some() || self.storage_sql.is_some())
    }

    /// Start the server.
    ///
    /// The function `init_context` is used to create a sequencer context from a metrics object and
    /// optional saved consensus state. The metrics object is created from the API data source, so
    /// that consensus will populuate metrics that can then be read and served by the API.
    pub async fn serve<N, P, F, V: Versions + 'static>(
        mut self,
        init_context: F,
    ) -> anyhow::Result<SequencerContext<N, P, V>>
    where
        N: ConnectedNetwork<PubKey>,
        P: SequencerPersistence,
        F: FnOnce(
            Box<dyn Metrics>,
            Box<dyn EventConsumer>,
            Option<Arc<SqlStorage>>,
        ) -> BoxFuture<'static, anyhow::Result<SequencerContext<N, P, V>>>,
    {
        // Create a channel to send the context to the web server after it is initialized. This
        // allows the web server to start before initialization can complete, since initialization
        // can take a long time (and is dependent on other nodes).
        let (send_ctx, recv_ctx) = oneshot::channel();
        let state = ApiState::new(async move {
            recv_ctx
                .await
                .expect("context initialized and sent over channel")
        });
        let mut tasks = TaskList::default();

        // The server state type depends on whether we are running a query or status API or not, so
        // we handle the two cases differently.
        #[allow(clippy::type_complexity)]
        let (metrics, consumer, storage): (
            Box<dyn Metrics>,
            Box<dyn EventConsumer>,
            Option<Arc<SqlStorage>>,
        ) = if let Some(query_opt) = self.query.take() {
            if let Some(opt) = self.storage_sql.take() {
                self.init_with_query_module_sql(
                    query_opt,
                    opt,
                    state,
                    &mut tasks,
                    SequencerApiVersion::instance(),
                )
                .await?
            } else if let Some(opt) = self.storage_fs.take() {
                self.init_with_query_module_fs(
                    query_opt,
                    opt,
                    state,
                    &mut tasks,
                    SequencerApiVersion::instance(),
                )
                .await?
            } else {
                bail!("query module requested but not storage provided");
            }
        } else if self.status.is_some() {
            // If a status API is requested but no availability API, we use the
            // `MetricsDataSource`, which allows us to run the status API with no persistent
            // storage.
            let ds = MetricsDataSource::default();
            let metrics = ds.populate_metrics();
            let mut app = App::<_, Error>::with_state(AppState::from(ExtensibleDataSource::new(
                ds,
                state.clone(),
            )));

            // Initialize v0 and v1 status API.
            register_api("status", &mut app, move |ver| {
                status::define_api(&Default::default(), SequencerApiVersion::instance(), ver)
                    .context("failed to define status api")
            })?;

            self.init_hotshot_modules(&mut app)?;

            if self.hotshot_events.is_some() {
                self.init_and_spawn_hotshot_event_streaming_module(state, &mut tasks)?;
            }

            tasks.spawn(
                "API server",
                self.listen(self.http.port, app, SequencerApiVersion::instance()),
            );

            (metrics, Box::new(NullEventConsumer), None)
        } else {
            // If no status or availability API is requested, we don't need metrics or a query
            // service data source. The only app state is the HotShot handle, which we use to
            // submit transactions.
            //
            // If we have no availability API, we cannot load a saved leaf from local storage,
            // so we better have been provided the leaf ahead of time if we want it at all.
            let mut app = App::<_, Error>::with_state(AppState::from(state.clone()));

            self.init_hotshot_modules(&mut app)?;

            if self.hotshot_events.is_some() {
                self.init_and_spawn_hotshot_event_streaming_module(state, &mut tasks)?;
            }

            tasks.spawn(
                "API server",
                self.listen(self.http.port, app, SequencerApiVersion::instance()),
            );

            (Box::new(NoMetrics), Box::new(NullEventConsumer), None)
        };

        let ctx = init_context(metrics, consumer, storage).await?;
        send_ctx
            .send(super::ConsensusState::from(&ctx))
            .ok()
            .context("API server exited without receiving context")?;
        Ok(ctx.with_task_list(tasks))
    }

    async fn init_app_modules<N, P, D, V: Versions>(
        &self,
        ds: D,
        state: ApiState<N, P, V>,
        bind_version: SequencerApiVersion,
    ) -> anyhow::Result<(
        Box<dyn Metrics>,
        Arc<StorageState<N, P, D, V>>,
        App<AppState<StorageState<N, P, D, V>>, Error>,
    )>
    where
        N: ConnectedNetwork<PubKey>,
        P: SequencerPersistence,
        D: SequencerDataSource + CatchupStorage + Send + Sync + 'static,
    {
        let metrics = ds.populate_metrics();
        let ds = Arc::new(ExtensibleDataSource::new(ds, state.clone()));
        let api_state: endpoints::AvailState<N, P, D, V> = ds.clone().into();
        let mut app = App::<_, Error>::with_state(api_state);

        // Initialize v0 and v1 status API.
        register_api("status", &mut app, move |ver| {
            status::define_api(&Default::default(), SequencerApiVersion::instance(), ver)
                .context("failed to define status api")
        })?;

        // Initialize availability and node APIs (these both use the same data source).

        // Note: We initialize two versions of the availability module: `availability/v0` and `availability/v1`.
        // - `availability/v0/leaf/0` returns the old `Leaf1` type for backward compatibility.
        // - `availability/v1/leaf/0` returns the new `Leaf2` type

        register_api("availability", &mut app, move |ver| {
            endpoints::availability(ver).context("failed to define availability api")
        })?;

        register_api("node", &mut app, move |ver| {
            endpoints::node(ver).context("failed to define node api")
        })?;

        // Initialize submit API
        if self.submit.is_some() {
            register_api("submit", &mut app, move |ver| {
                endpoints::submit::<_, _, _, SequencerApiVersion>(ver)
                    .context("failed to define submit api")
            })?;
        }

        tracing::info!("initializing catchup API");

        register_api("catchup", &mut app, move |ver| {
            endpoints::catchup(bind_version, ver).context("failed to define catchup api")
        })?;

        register_api("state-signature", &mut app, move |ver| {
            endpoints::state_signature(bind_version, ver)
                .context("failed to define state signature api")
        })?;

        if self.config.is_some() {
            register_api("config", &mut app, move |ver| {
                endpoints::config(bind_version, ver).context("failed to define config api")
            })?;
        }
        Ok((metrics, ds, app))
    }

    async fn init_with_query_module_fs<N, P, V: Versions + 'static>(
        &self,
        query_opt: Query,
        mod_opt: persistence::fs::Options,
        state: ApiState<N, P, V>,
        tasks: &mut TaskList,
        bind_version: SequencerApiVersion,
    ) -> anyhow::Result<(
        Box<dyn Metrics>,
        Box<dyn EventConsumer>,
        Option<Arc<SqlStorage>>,
    )>
    where
        N: ConnectedNetwork<PubKey>,
        P: SequencerPersistence,
    {
        let ds = <fs::DataSource as SequencerDataSource>::create(
            mod_opt,
            provider::<V>(query_opt.peers, bind_version),
            false,
        )
        .await?;

        let (metrics, ds, app) = self
            .init_app_modules(ds, state.clone(), bind_version)
            .await?;

        if self.hotshot_events.is_some() {
            self.init_and_spawn_hotshot_event_streaming_module(state, tasks)?;
        }

        tasks.spawn("API server", self.listen(self.http.port, app, bind_version));
        Ok((metrics, Box::new(ApiEventConsumer::from(ds)), None))
    }

    async fn init_with_query_module_sql<N, P, V: Versions + 'static>(
        self,
        query_opt: Query,
        mod_opt: persistence::sql::Options,
        state: ApiState<N, P, V>,
        tasks: &mut TaskList,
        bind_version: SequencerApiVersion,
    ) -> anyhow::Result<(
        Box<dyn Metrics>,
        Box<dyn EventConsumer>,
        Option<Arc<SqlStorage>>,
    )>
    where
        N: ConnectedNetwork<PubKey>,
        P: SequencerPersistence,
    {
        let mut provider = Provider::default();

        // Use the database itself as a fetching provider: sometimes we can fetch data that is
        // missing from the query service from ephemeral consensus storage.
        provider = provider.with_provider(mod_opt.clone().create().await?);
        // If that fails, fetch missing data from peers.
        for peer in query_opt.peers {
            tracing::info!("will fetch missing data from {peer}");
            provider = provider.with_provider(QueryServiceProvider::new(peer, bind_version));
        }

        let ds = sql::DataSource::create(mod_opt.clone(), provider, false).await?;
        let inner_storage = ds.inner();
        let (metrics, ds, mut app) = self
            .init_app_modules(ds, state.clone(), bind_version)
            .await?;

        if self.explorer.is_some() {
            register_api("explorer", &mut app, move |ver| {
                endpoints::explorer(ver).context("failed to define explorer api")
            })?;
        }

        // Initialize merklized state module for block merkle tree

        register_api("block-state", &mut app, move |ver| {
            endpoints::merklized_state::<N, P, _, BlockMerkleTree, _, 3>(ver)
                .context("failed to define block-state api")
        })?;

        // Initialize merklized state module for fee merkle tree

        register_api("fee-state", &mut app, move |ver| {
            endpoints::fee::<_, SequencerApiVersion>(ver).context("failed to define fee-state api")
        })?;

        register_api("reward-state", &mut app, move |ver| {
            endpoints::reward::<_, SequencerApiVersion>(ver)
                .context("failed to define reward-state api")
        })?;

        let get_node_state = {
            let state = state.clone();
            async move { state.node_state().await.clone() }
        };
        tasks.spawn(
            "merklized state storage update loop",
            update_state_storage_loop(ds.clone(), get_node_state),
        );
        if self.hotshot_events.is_some() {
            self.init_and_spawn_hotshot_event_streaming_module(state, tasks)?;
        }

        tasks.spawn(
            "API server",
            self.listen(self.http.port, app, SequencerApiVersion::instance()),
        );
        Ok((
            metrics,
            Box::new(ApiEventConsumer::from(ds)),
            Some(inner_storage),
        ))
    }

    /// Initialize the modules for interacting with HotShot.
    ///
    /// This function adds the `submit`, `state`, and `state_signature` API modules to the given
    /// app. These modules only require a HotShot handle as state, and thus they work with any data
    /// source, so initialization is the same no matter what mode the service is running in.
    fn init_hotshot_modules<N, P, S>(&self, app: &mut App<S, Error>) -> anyhow::Result<()>
    where
        S: 'static + Send + Sync + ReadState,
        P: SequencerPersistence,
        S::State: Send
            + Sync
            + SubmitDataSource<N, P>
            + StateSignatureDataSource<N>
            + NodeStateDataSource
            + CatchupDataSource
            + HotShotConfigDataSource,
        N: ConnectedNetwork<PubKey>,
    {
        let bind_version = SequencerApiVersion::instance();
        // Initialize submit API
        if self.submit.is_some() {
            register_api("submit", app, move |ver| {
                endpoints::submit::<_, _, _, SequencerApiVersion>(ver)
                    .context("failed to define submit api")
            })?;
        }

        // Initialize state API.
        if self.catchup.is_some() {
            tracing::info!("initializing state API");

            register_api("catchup", app, move |ver| {
                endpoints::catchup(bind_version, ver).context("failed to define catchup api")
            })?;
        }

        register_api("state-signature", app, move |ver| {
            endpoints::state_signature(bind_version, ver)
                .context("failed to define state signature api")
        })?;

        if self.config.is_some() {
            register_api("config", app, move |ver| {
                endpoints::config(bind_version, ver).context("failed to define config api")
            })?;
        }

        Ok(())
    }

    // Enable the events streaming api module
    fn init_and_spawn_hotshot_event_streaming_module<
        N,
        P: SequencerPersistence,
        V: Versions + 'static,
    >(
        &self,
        state: ApiState<N, P, V>,
        tasks: &mut TaskList,
    ) -> anyhow::Result<()>
    where
        N: ConnectedNetwork<PubKey>,
    {
        // Start the event streaming API server if it is enabled.
        // It runs to different port and app because State and Extensible Data source needs to support required
        // EventsSource trait, which is currently intended not to implement to separate hotshot-query-service crate, and
        // hotshot-events-service crate.

        let mut app = App::<_, EventStreamingError>::with_state(AppState::from(state));

        tracing::info!("initializing hotshot events API");

        register_api("hotshot-events", &mut app, move |ver| {
            hotshot_events_service::events::define_api::<_, _, SequencerApiVersion>(
                &hotshot_events_service::events::Options::default(),
                ver,
            )
            .context("failed to define hotshot events api")
        })?;

        tasks.spawn(
            "Hotshot Events Streaming API server",
            self.listen(
                self.hotshot_events.unwrap().events_service_port,
                app,
                SequencerApiVersion::instance(),
            ),
        );

        Ok(())
    }

    fn listen<S, E, ApiVer>(
        &self,
        port: u16,
        app: App<S, E>,
        bind_version: ApiVer,
    ) -> impl Future<Output = anyhow::Result<()>>
    where
        S: Send + Sync + 'static,
        E: Send + Sync + tide_disco::Error,
        ApiVer: StaticVersionType + 'static,
    {
        let max_connections = self.http.max_connections;

        async move {
            if let Some(limit) = max_connections {
                app.serve(RateLimitListener::with_port(port, limit), bind_version)
                    .await?;
            } else {
                app.serve(format!("0.0.0.0:{}", port), bind_version).await?;
            }
            Ok(())
        }
    }
}

/// The minimal HTTP API.
///
/// The API automatically includes health and version endpoints. Additional API modules can be
/// added by including the query-api or submit-api modules.
#[derive(Parser, Clone, Copy, Debug)]
pub struct Http {
    /// Port that the HTTP API will use.
    #[arg(long, env = "ESPRESSO_SEQUENCER_API_PORT", default_value = "8080")]
    pub port: u16,

    /// Maximum number of concurrent HTTP connections the server will allow.
    ///
    /// Connections exceeding this will receive and immediate 429 response and be closed.
    ///
    /// Leave unset for no connection limit.
    #[arg(long, env = "ESPRESSO_SEQUENCER_MAX_CONNECTIONS")]
    pub max_connections: Option<usize>,
}

impl Http {
    /// Default options for running a web server on the given port.
    pub fn with_port(port: u16) -> Self {
        Self {
            port,
            max_connections: None,
        }
    }
}

/// Options for the submission API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Submit;

/// Options for the status API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Status;

/// Options for the catchup API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Catchup;

/// Options for the config API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Config;

/// Options for the query API module.
#[derive(Parser, Clone, Debug, Default)]
pub struct Query {
    /// Peers for fetching missing data for the query service.
    #[arg(long, env = "ESPRESSO_SEQUENCER_API_PEERS", value_delimiter = ',')]
    pub peers: Vec<Url>,
}

/// Options for the state API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct State;

/// Options for the Hotshot events streaming API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct HotshotEvents {
    /// Port that the HTTP Hotshot Event streaming API will use.
    #[arg(long, env = "ESPRESSO_SEQUENCER_HOTSHOT_EVENT_STREAMING_API_PORT")]
    pub events_service_port: u16,
}

/// Options for the explorer API module.
#[derive(Parser, Clone, Copy, Debug, Default)]
pub struct Explorer;

/// Registers two versions (v0 and v1) of the same API module under the given path.
fn register_api<E, S, F, ModuleError, ModuleVersion>(
    path: &'static str,
    app: &mut App<S, E>,
    f: F,
) -> anyhow::Result<()>
where
    S: 'static + Send + Sync,
    E: Send + Sync + 'static + tide_disco::Error + From<ModuleError>,
    ModuleError: Send + Sync + 'static,
    ModuleVersion: StaticVersionType + 'static,
    F: Fn(semver::Version) -> anyhow::Result<Api<S, ModuleError, ModuleVersion>>,
{
    let v0 = "0.0.1".parse().unwrap();
    let v1 = "1.0.0".parse().unwrap();
    let result1 = f(v0)?;
    let result2 = f(v1)?;

    app.register_module(path, result1)?;
    app.register_module(path, result2)?;

    Ok(())
}
