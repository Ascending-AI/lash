//! Live Restate registration of the shared durable effect-group laws.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_sansio::ProcessId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use lash_core::testing::store_fixtures::durable_admission;
use lash_core::{
    ExecutionScope, GroupExecutors, GroupWakePolicy, LoserPolicy, Resolution, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeErrorCode,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::http_server::HttpServer;
use restate_sdk::serde::Json;

use super::live_turn_probe::ConformanceTurnProbe as _;
use crate::RestateConnection;
use crate::durable_wait::arm_wait_registration_witness;
use crate::effect_group::{
    EffectGroupChildRequest, admit_wait_request, arm_admission_witness, cancel_wait_request,
    decode_wait_resolution, payload_key, rank_wait_request, ready_wait_request,
};
use crate::process::{LashProcessWorkflowImpl, RestateProcessCancelRequest, RestateProcessRunner};
use crate::{
    EffectGroupAdoptRequest, EffectGroupCleanupFacts, EffectGroupDispatchRequest,
    EffectGroupOpenRequest, EffectGroupOpenResponse, EffectGroupPayloadPutRequest,
    EffectGroupPayloadPutResponse, EffectGroupProbeAdoptResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupRecordDispatchRequest,
    EffectGroupRecordDispatchResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupRetireResponse, EffectGroupSettlementTerminal,
    EffectGroupShape, EffectGroupWaitResolution, RestateDurableWaitAddress,
    RestateDurableWaitAwaitRequest, RestateDurableWaitRegistration, RestateEffectHost,
    RestateIngressClient,
};
use lash_http_transport::HttpRequest;

/// The endpoint's process runner for the tool-child laws: a tool child's
/// orchestrating body records its durable starts through the Restate process
/// surface, so the service must exist for the submission to be a legal
/// command. What runs the segment is not under test — the same role
/// `ConformanceExecutors` plays for group children — so the runner settles
/// every submitted process successfully and lets the workflow write the
/// terminal into the law's registry.
struct ToolChildProcessRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for ToolChildProcessRunner {
    fn replay_key_grammar(&self, _registration: &lash_core::ProcessRegistration) -> Option<u32> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &crate::SegmentStarted,
        _registration: lash_core::ProcessRegistration,
        _execution_context: lash_core::ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        Ok(lash_core::ProcessRunOutcome::Terminal {
            output: Box::new(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::json!({
                    "runner": "tool-child-conformance"
                })),
            )),
        })
    }

    async fn request_process_cancel(
        &self,
        _request: RestateProcessCancelRequest,
    ) -> Result<(), lash_core::PluginError> {
        Ok(())
    }
}

/// The endpoint's process runner for the turn-driving laws: a law that runs
/// real process segments (a `spawn_agent` child session) installs its own
/// [`DurableProcessWorker`](lash_core_worker::DurableProcessWorker) here,
/// which is what a deployment's `RestateCoreProcessRunner` serves; until one
/// is installed the endpoint answers as [`ToolChildProcessRunner`] does.
#[derive(Default)]
pub(super) struct LawProcessRunner {
    installed: Mutex<Option<crate::RestateCoreProcessRunner>>,
}

impl LawProcessRunner {
    pub(super) fn install(&self, worker: lash_core_worker::DurableProcessWorker) {
        *self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(crate::RestateCoreProcessRunner::new(worker));
    }

    fn installed(&self) -> Option<crate::RestateCoreProcessRunner> {
        self.installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for LawProcessRunner {
    fn replay_key_grammar(&self, _registration: &lash_core::ProcessRegistration) -> Option<u32> {
        None
    }

    async fn run_process_segment(
        &self,
        started: &crate::SegmentStarted,
        registration: lash_core::ProcessRegistration,
        execution_context: lash_core::ProcessExecutionContext,
        scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        match self.installed() {
            Some(runner) => {
                Box::pin(runner.run_process_segment(
                    started,
                    registration,
                    execution_context,
                    scoped_effect_controller,
                    handover,
                    cancellation,
                ))
                .await
            }
            None => {
                ToolChildProcessRunner
                    .run_process_segment(
                        started,
                        registration,
                        execution_context,
                        scoped_effect_controller,
                        handover,
                        cancellation,
                    )
                    .await
            }
        }
    }

    async fn request_process_cancel(
        &self,
        request: RestateProcessCancelRequest,
    ) -> Result<(), lash_core::PluginError> {
        match self.installed() {
            Some(runner) => runner.request_process_cancel(request).await,
            None => ToolChildProcessRunner.request_process_cancel(request).await,
        }
    }
}

#[derive(Default)]
struct ConformanceExecutors {
    current: Mutex<Option<Arc<dyn GroupExecutors>>>,
    staged: Mutex<HashMap<String, Arc<Mutex<Option<RuntimeEffectLocalExecutor<'static>>>>>>,
    mapping_current: AtomicBool,
}

impl ConformanceExecutors {
    fn install(&self, executors: Arc<dyn GroupExecutors>) {
        self.mapping_current.store(false, Ordering::SeqCst);
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(executors);
    }

    fn install_mapping_current(&self, executors: Arc<dyn GroupExecutors>) {
        self.install(executors);
        self.mapping_current.store(true, Ordering::SeqCst);
    }
}

impl GroupExecutors for ConformanceExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let replay_key = envelope.invocation.replay_key().to_owned();
        if self.mapping_current.load(Ordering::SeqCst) {
            return self
                .current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|executors| executors.executor_for(envelope));
        }
        if let Some(staged) = self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&replay_key)
            .cloned()
        {
            return Some(staged_executor(staged, replay_key));
        }
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()?;
        let executor = current.executor_for(envelope)?;
        let staged = Arc::new(Mutex::new(Some(executor)));
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(replay_key.clone(), Arc::clone(&staged));
        Some(staged_executor(staged, replay_key))
    }
}

fn staged_executor(
    staged: Arc<Mutex<Option<RuntimeEffectLocalExecutor<'static>>>>,
    replay_key: String,
) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |envelope| async move {
        let executor = staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                    format!("conformance executor for {replay_key} was already consumed"),
                )
            })?;
        executor.execute(envelope).await
    })
}

/// Routes every `AwaitEvent` group child to the durable-wait executor, which
/// the Restate controller parks on the wait's own promise.
struct AwaitEventChildren;

impl GroupExecutors for AwaitEventChildren {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        matches!(envelope.command, RuntimeEffectCommand::AwaitEvent { .. })
            .then(|| RuntimeEffectLocalExecutor::await_event(CancellationToken::new(), None))
    }
}

#[derive(Default)]
struct WitnessExecutors {
    staged: Mutex<HashMap<String, WitnessRoute>>,
    /// Every replay key `executor_for` was asked to resolve, in order. The
    /// registry is process-wide for the life of the witness deployment, so a
    /// bare counter is not usable as a per-group assertion: a child from
    /// another group whose runner is absent retries resolution indefinitely
    /// and lands counts in any window. The key list is what lets a guard
    /// assert on *this* group's children alone.
    resolved: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct WitnessRoute {
    executions: Arc<AtomicUsize>,
    label: &'static str,
}

impl WitnessExecutors {
    fn stage(
        &self,
        child: &RuntimeEffectEnvelope,
        executions: Arc<AtomicUsize>,
        label: &'static str,
    ) {
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                child.invocation.replay_key().to_owned(),
                WitnessRoute { executions, label },
            );
    }
}

impl GroupExecutors for WitnessExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        self.resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(envelope.invocation.replay_key().to_owned());
        let route = self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(envelope.invocation.replay_key())
            .cloned()?;
        Some(RuntimeEffectLocalExecutor::testing(move |_| async move {
            route.executions.fetch_add(1, Ordering::SeqCst);
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "witness": route.label }),
            })
        }))
    }
}

type GroupHostFactory =
    Box<dyn Fn(Option<Arc<dyn GroupExecutors>>) -> Arc<dyn lash_core::EffectHost> + Send + Sync>;

/// Which Restate server a harness drives its endpoint through.
#[derive(Clone)]
pub(super) enum HarnessServer {
    /// A live `restate-server` (the `just effect-group-conformance-e2e`
    /// gate): the endpoint serves over TCP and registers through the admin
    /// API the environment names.
    Live,
    /// The in-process `lash-restate-test` server double under this seed.
    InProcess { seed: u64, always_replay: bool },
}

impl HarnessServer {
    /// The in-process double under a fresh seed: the law's identities stay
    /// distinct while each run is reproducible from its printed seed.
    ///
    /// `LASH_RESTATE_TEST_SEED` replays one printed seed;
    /// `LASH_RESTATE_TEST_ALWAYS_REPLAY=1` runs every suite in always-replay
    /// mode, suspending at every await.
    pub(super) fn in_process() -> Self {
        let seed = std::env::var("LASH_RESTATE_TEST_SEED")
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or_else(|| u64::try_from(nonce() & u128::from(u64::MAX)).unwrap_or(0));
        let always_replay =
            std::env::var("LASH_RESTATE_TEST_ALWAYS_REPLAY").is_ok_and(|v| v == "1");
        println!("lash-restate-test seed {seed} always_replay {always_replay}");
        Self::InProcess {
            seed,
            always_replay,
        }
    }
}

/// The harness's admin face: where it modifies retained service state.
#[derive(Clone)]
enum HarnessAdmin {
    Live {
        admin_url: String,
    },
    InProcess {
        server: lash_restate_test::RestateTestServer,
    },
}

pub(super) struct LiveConformanceHarness {
    connection: RestateConnection,
    admin: HarnessAdmin,
    host: Arc<RestateEffectHost>,
    executors: Arc<ConformanceExecutors>,
    process_registry: Arc<lash_core::TestLocalProcessRegistry>,
    process_runner: Arc<LawProcessRunner>,
    shutdown_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    server: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LiveConformanceHarness {
    /// The shared-laws endpoint on a live server.
    pub(super) async fn start() -> Self {
        Self::start_on(HarnessServer::Live).await
    }

    /// The shared-laws endpoint: the suite's staged resolver is registered on
    /// the host, so `install_tool_child_host` wins nothing here.
    pub(super) async fn start_on(target: HarnessServer) -> Self {
        let executors = Arc::new(ConformanceExecutors::default());
        let registered = Arc::clone(&executors);
        Self::start_with(target, executors, move |host| {
            host.register_group_executors(registered)
                .expect("register the conformance resolver on the endpoint host")
        })
        .await
    }

    /// The tool-child laws' endpoint on a live server.
    pub(super) async fn start_for_tool_children() -> Self {
        Self::start_for_tool_children_on(HarnessServer::Live).await
    }

    /// The tool-child laws' endpoint: nothing is registered, so the runtime's
    /// `install_tool_child_host` installs its `ToolChildHost` on this host and
    /// the endpoint routes `ToolInvocation` children through it — the one
    /// resolver a deployment has. An orchestrating child's durable start
    /// submits `LashProcessWorkflow/run` through the handler's context, which
    /// the endpoint serves with the law's installed process worker.
    pub(super) async fn start_for_tool_children_on(target: HarnessServer) -> Self {
        Self::start_with(target, Arc::new(ConformanceExecutors::default()), |_| {}).await
    }

    /// The endpoint binds every lash service through the one binder a
    /// deployment uses, beside the suite's probes.
    async fn start_with(
        target: HarnessServer,
        executors: Arc<ConformanceExecutors>,
        register: impl FnOnce(&RestateEffectHost),
    ) -> Self {
        let (connection, admin, live) = match &target {
            HarnessServer::Live => {
                let ingress_url = required("RESTATE_INGRESS_URL");
                let admin_url = required("RESTATE_ADMIN_URL");
                let bind_addr = required("EG_RESTATE_ENDPOINT_BIND")
                    .parse::<SocketAddr>()
                    .expect("valid EG_RESTATE_ENDPOINT_BIND");
                let endpoint_url = required("EG_RESTATE_ENDPOINT_URL");
                (
                    RestateConnection::new(ingress_url),
                    HarnessAdmin::Live {
                        admin_url: admin_url.clone(),
                    },
                    Some((admin_url, bind_addr, endpoint_url)),
                )
            }
            HarnessServer::InProcess {
                seed,
                always_replay,
            } => {
                let server = lash_restate_test::RestateTestServer::new(
                    lash_restate_test::ServerConfig::default()
                        .with_seed(*seed)
                        .always_replay(*always_replay),
                )
                .expect("start the in-process Restate server double");
                if std::env::var("LASH_RESTATE_TEST_WATCHDOG").is_ok() {
                    let watched = server.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_secs(20)).await;
                            for view in watched.invocations() {
                                if view.status != "completed" {
                                    let journal: Vec<_> = watched
                                        .journal(&view.id)
                                        .unwrap_or_default()
                                        .into_iter()
                                        .map(|entry| format!("{:?}:{:?}", entry.ty, entry.name))
                                        .collect();
                                    println!("WATCHDOG {view:?} journal={journal:?}");
                                }
                            }
                            println!("WATCHDOG timers {:?}", watched.timers());
                        }
                    });
                }
                (
                    RestateConnection::with_transport(server.ingress_url(), server.transport()),
                    HarnessAdmin::InProcess { server },
                    None,
                )
            }
        };
        let ingress = RestateIngressClient::new(connection.clone());
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        register(&host);
        let process_registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        let process_runner = Arc::new(LawProcessRunner::default());
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                effect_host: &host,
                ingress,
                sessions: Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
                process_workflow: LashProcessWorkflowImpl::new_for_test(
                    Arc::clone(&process_runner),
                    Arc::clone(&process_registry) as Arc<dyn lash_core::ProcessRegistry>,
                    Arc::clone(&process_registry) as Arc<dyn lash_core::ProcessContinuationStore>,
                ),
            },
        )
        .bind(ScopeLivenessProbeImpl.serve())
        .bind(GroupOpenBudgetProbeImpl.serve())
        // A turn handler: a parked attempt fails retryably and the
        // invocation pauses after its last attempt (FIG-3697).
        .bind(crate::turn_service(
            super::live_turn_probe::ConformanceTurnProbeImpl.serve(),
            "run",
        ))
        .build();
        let (shutdown_tx, server) = match (live, &admin) {
            (Some((admin_url, bind_addr, endpoint_url)), _) => {
                let listener = tokio::net::TcpListener::bind(bind_addr)
                    .await
                    .expect("bind Restate effect-group endpoint");
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let server = tokio::spawn(async move {
                    HttpServer::new(endpoint)
                        .serve_with_cancel(listener, async {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                });
                wait_for_endpoint(bind_addr).await;
                register_deployment(&admin_url, &endpoint_url).await;
                (Some(shutdown_tx), Some(server))
            }
            (None, HarnessAdmin::InProcess { server }) => {
                server
                    .register(endpoint)
                    .await
                    .expect("register the effect-group endpoint on the server double");
                (None, None)
            }
            (None, HarnessAdmin::Live { .. }) => {
                unreachable!("a live harness always has its live endpoint address")
            }
        };

        Self {
            connection,
            admin,
            host,
            executors,
            process_registry,
            process_runner,
            shutdown_tx: tokio::sync::Mutex::new(shutdown_tx),
            server: tokio::sync::Mutex::new(server),
        }
    }

    /// The tool-child laws over this endpoint's host.
    ///
    /// Every `make_world` call returns the same host — the process is one
    /// endpoint, as on the in-memory tier — with `drain: None`: Restate
    /// redrives a child invocation itself, so Lash keeps no drain for the
    /// laws to walk and the recovery law takes its open-time shape.
    pub(super) fn tool_child_law_fixture(&self) -> lash_conformance::ToolChildLawFixture {
        let host = Arc::clone(&self.host) as Arc<dyn lash_core::EffectHost>;
        lash_conformance::ToolChildLawFixture {
            make_world: Arc::new(move |_spec| {
                let host = Arc::clone(&host);
                Box::pin(async move { lash_conformance::ToolChildWorld { host, drain: None } })
            }),
            // Deliberately one registry for every scenario, despite
            // `ToolChildLawFixture::make_processes` promising a fresh one: the
            // endpoint's LashProcessWorkflow writes the segment terminal into
            // the registry the orchestrating child's start recorded, so the
            // dispatched context and the workflow must share this one. Rows
            // do not collide because scenario prefixes keep process ids
            // distinct. The process-exec-env store is a fresh SQLite memory
            // backend's: the endpoint reads no environment itself.
            make_processes: Arc::new({
                let registry = Arc::clone(&self.process_registry);
                move || {
                    let registry = Arc::clone(&registry);
                    Box::pin(async move {
                        let backend = lash_sqlite_store::SqliteBackend::memory()
                            .await
                            .expect("tool-child process-exec-env backend");
                        lash_conformance::ToolChildProcesses {
                            registry: registry as Arc<dyn lash_core::ProcessRegistry>,
                            process_env_store: backend.process_env_store()
                                as Arc<dyn lash_core::ProcessExecutionEnvStore>,
                        }
                    })
                }
            }),
            deferrable_routing: lash_conformance::ToolChildDeferrableRouting::Durable,
        }
    }

    /// The endpoint's own host, for a law that builds a runtime on it: the
    /// runtime installs its `ToolChildHost` here, which is the resolver the
    /// endpoint's dispatch invocations route tool children through.
    pub(super) fn endpoint_host(&self) -> Arc<dyn lash_core::EffectHost> {
        Arc::clone(&self.host) as Arc<dyn lash_core::EffectHost>
    }

    /// A per-run discriminator for law identities: the Restate server's
    /// state outlives a test run, so a fixed session or group key would
    /// reopen the previous run's retired state.
    pub(super) fn run_nonce(&self) -> u128 {
        nonce()
    }

    /// Runs a law's turn inside a `ConformanceTurnProbe` handler on this
    /// endpoint.
    pub(super) fn turn_runner(&self) -> Arc<dyn lash_conformance::ConformanceTurnRunner> {
        super::live_turn_probe::LiveTurnRunner::shared(
            self.connection.clone(),
            Arc::clone(&self.process_runner),
        )
    }

    /// The in-process server double this harness runs on, if it runs on one.
    pub(super) fn server_double(&self) -> Option<lash_restate_test::RestateTestServer> {
        match &self.admin {
            HarnessAdmin::InProcess { server } => Some(server.clone()),
            HarnessAdmin::Live { .. } => None,
        }
    }

    /// The registry the endpoint's `LashProcessWorkflow` writes terminals
    /// into: a law whose processes run on the endpoint must register and
    /// observe them here.
    pub(super) fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        Arc::clone(&self.process_registry) as Arc<dyn lash_core::ProcessRegistry>
    }

    pub(super) fn effect_host_factory(
        &self,
    ) -> Box<dyn Fn() -> Arc<dyn lash_core::EffectHost> + Send + Sync> {
        let connection = self.connection.clone();
        Box::new(move || {
            Arc::new(RestateEffectHost::new_for_test(connection.clone()))
                as Arc<dyn lash_core::EffectHost>
        })
    }

    /// A maker of fresh Restate backends on this endpoint's ingress, each over
    /// its own SQLite memory store set: the backend a law that builds a
    /// runtime runs on.
    pub(super) fn backend_factory(
        &self,
    ) -> impl Fn() -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Arc<dyn lash_core::Backend>> + Send>,
    > + Send
    + Sync
    + 'static {
        let connection = self.connection.clone();
        move || {
            let connection = connection.clone();
            Box::pin(async move {
                Arc::new(crate::RestateBackend::new(
                    connection,
                    crate::RestateAuthorityId::new("lash-conformance-backend-laws")
                        .expect("valid authority"),
                    Arc::new(
                        lash_sqlite_store::SqliteStoreSet::memory()
                            .await
                            .expect("open the law's store set"),
                    ),
                    crate::RestateQueuedWork::Disabled,
                )) as Arc<dyn lash_core::Backend>
            })
        }
    }

    pub(super) fn group_host_factory(&self) -> GroupHostFactory {
        let connection = self.connection.clone();
        let executors = Arc::clone(&self.executors);
        Box::new(move |resolver| match resolver {
            Some(resolver) => {
                executors.install(resolver);
                Arc::new(RestateEffectHost::new_for_test(connection.clone()))
                    as Arc<dyn lash_core::EffectHost>
            }
            None => Arc::new(lash_core::facade_support::NativeEffectHost::default())
                as Arc<dyn lash_core::EffectHost>,
        })
    }

    /// Shuts the endpoint task down. Takes `&self` so a harness shared across
    /// a registration fixture's maker, witness and teardown can still be torn
    /// down exactly once; a second call is a no-op.
    pub(super) async fn finish(&self) {
        if let Some(shutdown_tx) = self.shutdown_tx.lock().await.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(server) = self.server.lock().await.take() {
            server.await.expect("Restate effect-group endpoint task");
        }
    }

    /// FIG-3564 on live Restate: an over-budget group open gives up with the
    /// process-command arm's typed failure, and the group index never hears
    /// of the group.
    pub(super) async fn run_group_open_budget_witness(&self) {
        let operation = format!("fig3564-group-open-budget-{}", nonce());
        let ingress = RestateIngressClient::new(self.connection.clone());
        let refused = ingress
            .call_workflow_json::<_, Option<RuntimeEffectControllerError>>(
                "GroupOpenBudgetProbe",
                &operation,
                "run",
                &operation,
            )
            .await
            .expect("the group-open budget probe completes in its handler")
            .expect("an over-budget group open must give up");
        assert_eq!(
            refused.code,
            RuntimeErrorCode::RestateJournaledEffectPoisoned,
            "the group open must give up with the process-command arm's typed failure: {}",
            refused.message
        );
        assert_eq!(
            refused.message,
            format!(
                "journaled effect `lash:{operation}:group` gave up because its payload \
                 exceeded the 16-byte durable journal budget"
            )
        );
        let probe = ingress
            .call_object_empty_json::<crate::EffectGroupProbeResponse>(
                crate::LashService::EffectGroupIndex,
                &operation,
                "probe",
            )
            .await
            .expect("probe the group index");
        assert!(
            matches!(probe, crate::EffectGroupProbeResponse::Absent),
            "the give-up must precede every group-index write: {probe:?}"
        );
    }

    /// ADR 0099 §12 under the cancel race (FIG-3630): the close releases a
    /// cancel-decided wait child's wait itself. The Restate cancel the close
    /// issues can stop a child while it awaits its admission, or before it
    /// runs at all, and such a child reaches no release arm of its own. This
    /// witness takes the extreme of that race: the child's recorded
    /// invocation never runs, so only the close can answer the wait.
    pub(super) async fn run_unstarted_wait_child_release_witness(&self) {
        use lash_core::AwaitEventResolver as _;
        let ingress = RestateIngressClient::new(self.connection.clone());
        let group_key = witness_key("unstarted-wait");
        let scope = ExecutionScope::runtime_operation(group_key.clone());
        let wait_key = self
            .host
            .await_event_key(
                &scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("unstarted-wait-child"),
            )
            .await
            .expect("mint the wait child's key");
        let child = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(scope, format!("{group_key}:child:0"))
                    .expect("valid wait child address"),
                lash_core::RuntimeAttribution::none(),
                "effect",
            ),
            RuntimeEffectCommand::AwaitEvent {
                key: wait_key.clone(),
            },
        );
        let shape = witness_shape(&group_key, std::slice::from_ref(&child));
        let opened: EffectGroupOpenResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape,
                    content_checked: false,
                },
            )
            .await
            .expect("the wait group opens");
        assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
        let adopted: EffectGroupProbeAdoptResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "probe_and_adopt",
                &EffectGroupAdoptRequest {
                    invocation_id: "inv_unstarted_wait_dispatcher".to_owned(),
                },
            )
            .await
            .expect("the wait group adopts its dispatcher");
        assert!(
            matches!(adopted, EffectGroupProbeAdoptResponse::Adopted { .. }),
            "the wait group adopts its dispatcher: {adopted:?}"
        );
        // The recorded child invocation is a real invocation that is not the
        // child: a finished preflight. The close's cancel of it is a no-op,
        // and no line of the child handler ever runs.
        let child_invocation = ingress
            .send_workflow_json(
                "EffectGroupDispatch",
                &format!("{group_key}-stand-in"),
                "preflight",
                &Vec::<RuntimeEffectEnvelope>::new(),
            )
            .await
            .expect("a stand-in invocation is accepted")
            .as_str()
            .to_owned();
        let recorded: EffectGroupRecordDispatchResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "record_dispatch",
                &EffectGroupRecordDispatchRequest {
                    position: 0,
                    invocation_id: child_invocation.clone(),
                },
            )
            .await
            .expect("the dispatch records the child");
        assert_eq!(recorded, EffectGroupRecordDispatchResponse::Recorded);
        let registered: crate::EffectGroupRegisterResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "register_children",
                &crate::EffectGroupRegisterRequest {
                    addresses: [(0, child_invocation)].into_iter().collect(),
                },
            )
            .await
            .expect("the dispatch registers the child");
        assert_eq!(registered, crate::EffectGroupRegisterResponse::Registered);
        assert_eq!(
            self.host
                .peek_await_event(&wait_key)
                .await
                .expect("peek the parked wait"),
            None,
            "nothing has answered the wait before the close"
        );

        let closed: crate::EffectGroupCloseResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "close",
                &crate::EffectGroupCloseRequest {
                    disposition: LoserPolicy::Cancel,
                },
            )
            .await
            .expect("the close decides the wait child");
        assert_eq!(closed, crate::EffectGroupCloseResponse::Closed);
        // The close's own journal holds the release, so it is answered when
        // the close returns: no wait, no poll.
        assert_eq!(
            self.host
                .peek_await_event(&wait_key)
                .await
                .expect("peek the released wait"),
            Some(Resolution::Cancelled),
            "the close released the wait of the child it cancel-decided"
        );
        let late = self
            .host
            .resolve_await_event(&wait_key, Resolution::Ok(serde_json::json!("late-exit")))
            .await
            .expect("the late resolution is answered");
        assert!(
            !matches!(late, lash_core::ResolveOutcome::Accepted),
            "a late resolution after the close is not accepted: {late:?}"
        );
        let _: EffectGroupRetireResponse = ingress
            .call_object_empty_json(crate::LashService::EffectGroupIndex, &group_key, "retire")
            .await
            .expect("the wait group tombstones");
        println!("EFFECT_GROUP_WITNESS unstarted-wait-child-release PASS");

        // The race itself: a real child invocation parked on its admission
        // when the dispatch records it and the close lands at once. Whether
        // the close's cancel reaches the child at its fresh `admit_child`, at
        // its admission wait, or after it parked on the wait, the release is
        // already in the close's journal when the close returns.
        self.executors.install(Arc::new(AwaitEventChildren));
        let group_key = witness_key("admitting-wait");
        let scope = ExecutionScope::runtime_operation(group_key.clone());
        let wait_key = self
            .host
            .await_event_key(
                &scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("admitting-wait-child"),
            )
            .await
            .expect("mint the admitting child's key");
        let child = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(scope, format!("{group_key}:child:0"))
                    .expect("valid wait child address"),
                lash_core::RuntimeAttribution::none(),
                "effect",
            ),
            RuntimeEffectCommand::AwaitEvent {
                key: wait_key.clone(),
            },
        );
        let shape = witness_shape(&group_key, std::slice::from_ref(&child));
        let opened: EffectGroupOpenResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                    content_checked: false,
                },
            )
            .await
            .expect("the admitting group opens");
        assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
        let _: EffectGroupProbeAdoptResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "probe_and_adopt",
                &EffectGroupAdoptRequest {
                    invocation_id: "inv_admitting_wait_dispatcher".to_owned(),
                },
            )
            .await
            .expect("the admitting group adopts its dispatcher");
        let parked_on_admission = arm_admission_witness(&group_key);
        let child_invocation = ingress
            .send_workflow_json(
                "EffectGroupDispatch",
                &group_key,
                "child",
                &EffectGroupChildRequest {
                    group_key: group_key.clone(),
                    shape,
                    position: 0,
                    envelope: child,
                },
            )
            .await
            .expect("the child is accepted before it is recorded");
        tokio::time::timeout(Duration::from_secs(10), parked_on_admission.notified())
            .await
            .expect("the child parks on its admission before the dispatch records it");
        let recorded: EffectGroupRecordDispatchResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "record_dispatch",
                &EffectGroupRecordDispatchRequest {
                    position: 0,
                    invocation_id: child_invocation.as_str().to_owned(),
                },
            )
            .await
            .expect("the dispatch records the admitting child");
        assert_eq!(recorded, EffectGroupRecordDispatchResponse::Recorded);
        let registered: crate::EffectGroupRegisterResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "register_children",
                &crate::EffectGroupRegisterRequest {
                    addresses: [(0, child_invocation.as_str().to_owned())]
                        .into_iter()
                        .collect(),
                },
            )
            .await
            .expect("the dispatch registers the admitting child");
        assert_eq!(registered, crate::EffectGroupRegisterResponse::Registered);
        let closed: crate::EffectGroupCloseResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "close",
                &crate::EffectGroupCloseRequest {
                    disposition: LoserPolicy::Cancel,
                },
            )
            .await
            .expect("the close decides the admitting child");
        assert_eq!(closed, crate::EffectGroupCloseResponse::Closed);
        assert_eq!(
            self.host
                .peek_await_event(&wait_key)
                .await
                .expect("peek the released wait"),
            Some(Resolution::Cancelled),
            "the close released the admitting child's wait whatever the cancel interrupted"
        );
        let late = self
            .host
            .resolve_await_event(&wait_key, Resolution::Ok(serde_json::json!("late-exit")))
            .await
            .expect("the late resolution is answered");
        assert!(
            !matches!(late, lash_core::ResolveOutcome::Accepted),
            "a late resolution after the close is not accepted: {late:?}"
        );
        let _: EffectGroupRetireResponse = ingress
            .call_object_empty_json(crate::LashService::EffectGroupIndex, &group_key, "retire")
            .await
            .expect("the admitting group tombstones");
        println!("EFFECT_GROUP_WITNESS admitting-wait-child-release PASS");
    }

    pub(super) async fn run_design_witnesses(&self) {
        run_design_witnesses(&self.connection, &self.admin, &self.executors).await;
    }

    /// The handler-side half of the quiescence law (FIG-2499 fix round 3,
    /// ruling 4): an effect executing inside a Restate handler under a
    /// runtime-operation scope holds `WhenQuiescent` off until it completes.
    /// The deployment-level host cannot run a local executor, so the shared
    /// law early-returns on it; this witness runs the effect where Restate
    /// runs it.
    pub(super) async fn run_executing_effect_quiescence_witness(&self) {
        let ingress = RestateIngressClient::new(self.connection.clone());
        let host = (self.effect_host_factory())();
        let scope_id = format!("live-effect-{}", nonce());
        let scope = ExecutionScope::runtime_operation(scope_id.clone());
        let gate = executing_effect_gate();
        let workflow_key = scope_id.clone();
        let workflow = tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, bool>(
                    "ScopeLivenessProbe",
                    &workflow_key,
                    "run",
                    &scope_id,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), gate.started.notified())
            .await
            .expect("the handler's effect starts executing");

        let refused = host
            .retire_effect_journal(
                lash_core::EffectJournalRetirement::for_scope(&scope)
                    .expect("runtime operations are retirable")
                    .when_quiescent(),
            )
            .await
            .expect_err("an executing handler effect is not quiescent");
        assert_eq!(refused.code.as_str(), "effect_scope_not_quiescent");
        host.await_event_key(
            &scope,
            lash_core::AwaitEventWaitIdentity::tool_completion("still-open"),
        )
        .await
        .expect("the refused retirement left the scope unfenced");

        gate.release.notify_one();
        let completed = tokio::time::timeout(Duration::from_secs(60), workflow)
            .await
            .expect("the released handler completes")
            .expect("the workflow task joins")
            .expect("the workflow returns");
        assert!(completed, "the handler ran its effect to completion");

        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&scope)
                .expect("runtime operations are retirable")
                .when_quiescent(),
        )
        .await
        .expect("the scope is quiescent once the handler's effect completed");
        let fenced = host
            .await_event_key(
                &scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("after-retirement"),
            )
            .await
            .expect_err("the retired scope mints nothing");
        assert_eq!(fenced.code.as_str(), "await_event_unknown_or_revoked");
    }

    /// FIG-3709: a settled child leaves nothing open on the deployment. Each
    /// dispatched child watches its cancel wait through an ingress call of
    /// its own; the index ends that wait as `Settled` when it seats the
    /// child's settlement, so once every child settled the deployment drains
    /// without the group closing or retiring.
    pub(super) async fn run_settled_children_release_their_cancel_watches_witness(&self) {
        let ingress = RestateIngressClient::new(self.connection.clone());
        let witness_executors = Arc::new(WitnessExecutors::default());
        self.executors
            .install_mapping_current(Arc::clone(&witness_executors) as Arc<dyn GroupExecutors>);
        let group_key = witness_key("settled-cancel-watch");
        let children = [witness_child(&group_key, 0), witness_child(&group_key, 1)];
        let shape = witness_shape(&group_key, &children);
        let executions = Arc::new(AtomicUsize::new(0));
        for child in &children {
            witness_executors.stage(child, Arc::clone(&executions), "settled-cancel-watch");
        }
        let before = open_invocations(&self.admin).await;

        let opened: EffectGroupOpenResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                    content_checked: false,
                },
            )
            .await
            .expect("the witness group opens");
        assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
        ingress
            .send_workflow_json(
                "EffectGroupDispatch",
                &group_key,
                "run",
                &EffectGroupDispatchRequest {
                    group_key: group_key.clone(),
                },
            )
            .await
            .expect("the dispatcher submission is accepted");
        for rank in 1..=2 {
            assert_eq!(
                await_group_wait(
                    &ingress,
                    rank_wait_request(&shape.wait_scope, &group_key, rank).unwrap()
                )
                .await,
                EffectGroupWaitResolution::Rank
            );
        }
        assert_eq!(executions.load(Ordering::SeqCst), 2, "each child runs once");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let still_open = open_invocations(&self.admin)
                .await
                .into_iter()
                .filter(|(id, _)| !before.contains_key(id))
                .collect::<Vec<_>>();
            if still_open.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the deployment did not drain after every child settled; still open: {still_open:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for child in &children {
            let request =
                cancel_wait_request(&shape.wait_scope, &group_key, child.invocation.replay_key())
                    .expect("the child's cancel wait key derives");
            assert_eq!(
                await_group_wait(&ingress, request).await,
                EffectGroupWaitResolution::Settled,
                "a settled child's cancel wait ends as settled, not cancelled"
            );
        }

        ingress
            .call_workflow_json::<_, ()>("EffectGroupDispatch", &group_key, "retire", &group_key)
            .await
            .expect("retirement saga completes");
    }

    /// Prove both serialized orders between an await workflow's durable index
    /// registration and scope retirement.
    ///
    /// The registration-first case uses the host's real await path. Its
    /// test-only marker fires from the index handler after `ctx.set` is issued;
    /// the following retirement is an exclusive call on that same virtual
    /// object, so Restate orders it after registration. Refusal is the state
    /// oracle: omitting the wait-row write would make retirement succeed and
    /// this witness fail.
    pub(super) async fn run_active_wait_registration_witnesses(
        &self,
        host: Arc<dyn lash_core::EffectHost>,
        assert_retirement: lash_conformance::ActiveWaitRetirementAssertion,
    ) {
        let suffix = nonce();
        let scope =
            ExecutionScope::runtime_operation(format!("restate-await-registration-first-{suffix}"));
        let key = host
            .await_event_key(
                &scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("active-wait"),
            )
            .await
            .expect("mint registration-first wait key");
        let registered = arm_wait_registration_witness(&key);
        let waiter_host = Arc::clone(&host);
        let waiter_key = key.clone();
        let waiter = lash_core::task::spawn(async move {
            waiter_host
                .await_await_event(&waiter_key, CancellationToken::new(), None)
                .await
        });
        let registration = tokio::time::timeout(Duration::from_secs(30), registered)
            .await
            .expect("await workflow reached its index registration")
            .expect("registration witness sender remained live");
        assert_eq!(
            registration,
            RestateDurableWaitRegistration::Registered,
            "the workflow registered an unresolved wait before retirement"
        );

        assert_retirement(Arc::clone(&host), scope, key, waiter).await;

        // The opposite legal ordering: retirement fences an empty scope, then
        // the real wait workflow reaches the same index and observes Revoked.
        let retired_scope =
            ExecutionScope::runtime_operation(format!("restate-retirement-first-{suffix}"));
        let retired_key = host
            .await_event_key(
                &retired_scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("late-wait"),
            )
            .await
            .expect("mint retirement-first wait key");
        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&retired_scope)
                .expect("runtime operations are retirable")
                .when_quiescent(),
        )
        .await
        .expect("an empty scope retires before registration");

        let late_registration = arm_wait_registration_witness(&retired_key);
        let ingress = RestateIngressClient::new(self.connection.clone());
        let workflow_key = RestateDurableWaitAddress::for_key(&retired_key).workflow_key;
        let late_workflow = lash_core::task::spawn(async move {
            ingress
                .call_workflow_json::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &RestateDurableWaitAwaitRequest {
                        key: retired_key,
                        deadline: None,
                    },
                )
                .await
        });
        let late_registration = tokio::time::timeout(Duration::from_secs(30), late_registration)
            .await
            .expect("late workflow reached the retired index")
            .expect("late registration witness sender remained live");
        assert_eq!(
            late_registration,
            RestateDurableWaitRegistration::Revoked,
            "retirement legitimately wins before durable registration"
        );
        let late_resolution = tokio::time::timeout(Duration::from_secs(30), late_workflow)
            .await
            .expect("late workflow completed after revoked registration")
            .expect("late workflow task joins")
            .expect("late workflow returns its terminal");
        assert_eq!(late_resolution, Resolution::Cancelled);

        println!(
            "RESTATE_QUIESCENCE await_registration_orders=registered-first,retired-first PASS"
        );
    }

    /// The crash cut between a registry's commit and its post-commit index reinstate (FIG-2499
    /// fix round 3, ruling 2): the index is revoked, the registration is committed with no
    /// host bound, everything is dropped, and a cold registry plus host are opened and bound.
    /// The first effect under the process is admitted with no explicit re-registration: the
    /// host reads through the revoked index to the registry it is bound to.
    /// Runs over a SQLite-backed registry always and over a PostgreSQL-backed one when
    /// `LASH_POSTGRES_DATABASE_URL` names a server.
    pub(super) async fn run_cold_reopen_witnesses(&self) -> usize {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry_path = dir.path().join("registry.db");
        let sessions = dir.path().join("sessions");
        let open_sqlite = || {
            let registry_path = registry_path.clone();
            let sessions = sessions.clone();
            async move {
                Arc::new(
                    lash_sqlite_store::SqliteProcessRegistry::open(&registry_path, sessions)
                        .await
                        .expect("open the SQLite process registry"),
                ) as Arc<dyn lash_core::ProcessRegistry>
            }
        };
        cold_reopen_admits_the_registered_process(
            &self.effect_host_factory(),
            "sqlite",
            open_sqlite,
        )
        .await;
        let mut witnessed = 1;

        if let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") {
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("connect the isolated database");
            let open_postgres = || {
                let storage = storage.clone();
                async move {
                    Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessRegistry>
                }
            };
            cold_reopen_admits_the_registered_process(
                &self.effect_host_factory(),
                "postgres",
                open_postgres,
            )
            .await;
            witnessed += 1;
        } else {
            assert!(
                std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
                "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
            );
        }
        witnessed
    }
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos()
}

struct ExecutingEffectGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

fn executing_effect_gate() -> &'static ExecutingEffectGate {
    static GATE: std::sync::OnceLock<ExecutingEffectGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| ExecutingEffectGate {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    })
}

/// A workflow that runs one scoped local effect and holds it until the test
/// releases it: the executing-effect state of a Restate handler, observed
/// from outside through the durable-wait index.
#[restate_sdk::workflow]
pub(super) trait ScopeLivenessProbe {
    async fn run(input: Json<String>) -> HandlerResult<Json<bool>>;
}

pub(super) struct ScopeLivenessProbeImpl;

impl ScopeLivenessProbe for ScopeLivenessProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(scope_id): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let controller = crate::RestateRuntimeEffectController::new_for_test(ctx);
        let scoped = controller
            .scoped_effect_controller(durable_admission(&ExecutionScope::runtime_operation(
                scope_id.clone(),
            )))
            .map_err(TerminalError::from_error)?;
        let envelope = RuntimeEffectEnvelope::new(
            lash_core::RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(
                    ExecutionScope::runtime_operation(scope_id.clone()),
                    format!("{scope_id}:work"),
                )
                .map_err(TerminalError::from_error)?,
                lash_core::RuntimeAttribution::none(),
                "work",
            ),
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "scope-liveness".to_string(),
            },
        );
        scoped
            .controller()
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async {
                    let gate = executing_effect_gate();
                    gate.started.notify_one();
                    gate.release.notified().await;
                    Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: serde_json::json!("completed"),
                    })
                }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        Ok(Json(true))
    }
}

/// A workflow that opens one effect group on a controller configured with a
/// 16-byte journal budget, and returns the open's error (`None` when the open
/// succeeded): the FIG-3564 group-open pre-flight, observed on a real journal.
#[restate_sdk::workflow]
pub(super) trait GroupOpenBudgetProbe {
    async fn run(
        operation: Json<String>,
    ) -> HandlerResult<Json<Option<RuntimeEffectControllerError>>>;
}

pub(super) struct GroupOpenBudgetProbeImpl;

impl GroupOpenBudgetProbe for GroupOpenBudgetProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(operation): Json<String>,
    ) -> HandlerResult<Json<Option<RuntimeEffectControllerError>>> {
        let controller = crate::RestateRuntimeEffectController::with_options_for_test(
            ctx,
            crate::RestateEffectControllerOptions::default().journaled_effect_byte_budget(16),
        );
        let opened = lash_core::RuntimeEffectController::open_effect_group(
            &controller,
            super::conformance_and_poison::fig3564_budget_group(&operation),
        )
        .await;
        Ok(Json(opened.err()))
    }
}

async fn cold_reopen_admits_the_registered_process<F, Fut>(
    host_factory: &dyn Fn() -> Arc<dyn lash_core::EffectHost>,
    label: &str,
    open_registry: F,
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Arc<dyn lash_core::ProcessRegistry>>,
{
    let nonce = nonce();
    let process_id = ProcessId::from(format!("cold-reopen-{label}-{nonce}"));
    let scope = ExecutionScope::process(process_id.clone());
    let registration = || {
        lash_core::ProcessRegistration::new(
            process_id.clone(),
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
        .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
            lash_core::ProcessIdentity::new("test"),
        ))
    };

    // The index is revoked, and the registration commits with no host bound:
    // the post-commit reinstate never reaches the engine.
    let host = host_factory();
    host.retire_effect_journal(lash_core::EffectJournalRetirement::process(
        process_id.clone(),
    ))
    .await
    .expect("retire the process scope");
    let registry = open_registry().await;
    registry
        .register_process(registration())
        .await
        .expect("register the process");
    drop(registry);
    drop(host);

    // Cold reopen: a fresh registry and a fresh host, bound the way
    // `LashCore::build` binds them, and nothing else.
    let registry = open_registry().await;
    let cold = host_factory();
    registry.bind_effect_host(&cold);
    let other = ExecutionScope::runtime_operation(format!("cold-reopen-ready-{label}-{nonce}"));
    let key = cold
        .await_event_key(
            &other,
            lash_core::AwaitEventWaitIdentity::tool_completion("ready"),
        )
        .await
        .expect("mint the effect's promise");
    cold.resolve_await_event(&key, Resolution::Ok(serde_json::json!("ready")))
        .await
        .expect("resolve the effect's promise");
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                scope.clone(),
                format!("cold-reopen-first-{label}-{nonce}"),
            )
            .expect("valid cold-reopen effect address"),
            lash_core::RuntimeAttribution::none(),
            "first",
        ),
        RuntimeEffectCommand::AwaitEvent { key },
    );
    cold.scoped(durable_admission(&scope))
        .expect("the process scope binds")
        .controller()
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::await_event(CancellationToken::new(), None),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("the first effect under the registered process is admitted after a cold reopen over a {label} registry, with no explicit re-registration: {error:?}")
        });
    cold.await_event_key(
        &scope,
        lash_core::AwaitEventWaitIdentity::tool_completion("after-reopen"),
    )
    .await
    .expect("the registered process mints after the cold reopen");
}

async fn run_design_witnesses(
    connection: &RestateConnection,
    admin: &HarnessAdmin,
    executors: &Arc<ConformanceExecutors>,
) {
    let ingress = RestateIngressClient::new(connection.clone());
    let witness_executors = Arc::new(WitnessExecutors::default());
    executors.install_mapping_current(Arc::clone(&witness_executors) as Arc<dyn GroupExecutors>);

    let group_key = witness_key("dispatcher");
    let child = witness_child(&group_key, 0);
    let shape = witness_shape(&group_key, std::slice::from_ref(&child));
    let executions = Arc::new(AtomicUsize::new(0));
    witness_executors.stage(&child, Arc::clone(&executions), "dispatcher-convergence");
    let opened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "open",
            &EffectGroupOpenRequest {
                shape: shape.clone(),
                content_checked: false,
            },
        )
        .await
        .expect("witness group opens");
    assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
    let request = EffectGroupDispatchRequest {
        group_key: group_key.clone(),
    };
    let (first, second) = tokio::join!(
        ingress.send_workflow_json("EffectGroupDispatch", &group_key, "run", &request),
        ingress.send_workflow_json("EffectGroupDispatch", &group_key, "run", &request)
    );
    let first = first.expect("first dispatcher submission is accepted");
    let second = second.expect("concurrent dispatcher submission attaches");
    assert_eq!(first, second, "one workflow key has one invocation id");
    assert_eq!(
        await_group_wait(
            &ingress,
            ready_wait_request(&shape.wait_scope, &group_key).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Ready
    );
    assert_eq!(
        await_group_wait(
            &ingress,
            rank_wait_request(&shape.wait_scope, &group_key, 1).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Rank
    );
    let rank: EffectGroupReadRankResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "read_rank",
            &EffectGroupReadRankRequest {
                rank: 1,
                for_caller: false,
            },
        )
        .await
        .expect("dispatcher witness rank reads");
    assert!(matches!(rank, EffectGroupReadRankResponse::Settled { .. }));
    assert_eq!(executions.load(Ordering::SeqCst), 1, "child runs once");
    let reopened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "open",
            &EffectGroupOpenRequest {
                shape: shape.clone(),
                content_checked: false,
            },
        )
        .await
        .expect("converged group reopens");
    assert_eq!(reopened, EffectGroupOpenResponse::ReopenedReady);
    println!("EFFECT_GROUP_WITNESS h dispatcher-convergence PASS");
    println!("EFFECT_GROUP_WITNESS l workflow-exactly-once-key PASS");

    let resolved_before_guard = witness_executors
        .resolved
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    ingress
        .call_workflow_json::<_, ()>(
            "EffectGroupDispatch",
            &format!("{group_key}:stale-dispatch-diagnostic"),
            "run",
            &request,
        )
        .await
        .expect("stale dispatcher reaches its index guard");
    let resolved_during_guard = witness_executors
        .resolved
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .split_off(resolved_before_guard);
    assert!(
        resolved_during_guard
            .iter()
            .all(|key| !key.starts_with(&format!("{group_key}:"))),
        "Ready probe guard exits before preflight or sends; this group's \
         children resolved during the window: {resolved_during_guard:?}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    println!("EFFECT_GROUP_WITNESS k dispatcher-probe-guard PASS");

    ingress
        .call_workflow_json::<_, ()>("EffectGroupDispatch", &group_key, "retire", &group_key)
        .await
        .expect("retirement saga completes");
    let payload_put: EffectGroupPayloadPutResponse = ingress
        .call_object_json(
            "EffectGroupPayload",
            &payload_key(&group_key, 0),
            "put",
            &EffectGroupPayloadPutRequest {
                bytes: b"late-write".to_vec(),
            },
        )
        .await
        .expect("retired payload fence answers");
    assert_eq!(payload_put, EffectGroupPayloadPutResponse::Retired);
    let late_record: EffectGroupRecordSettlementResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "record_settlement",
            &EffectGroupRecordSettlementRequest {
                position: 0,
                terminal: EffectGroupSettlementTerminal::Cancelled,
            },
        )
        .await
        .expect("retired index fence answers");
    assert_eq!(late_record, EffectGroupRecordSettlementResponse::Retired);
    println!("EFFECT_GROUP_WITNESS i object-local-retired-fence PASS");

    for request in [
        ready_wait_request(&shape.wait_scope, &group_key).unwrap(),
        rank_wait_request(&shape.wait_scope, &group_key, 1).unwrap(),
    ] {
        assert_eq!(
            await_group_wait(&ingress, request).await,
            EffectGroupWaitResolution::Retired,
            "late registration observes the retained retirement fence"
        );
    }
    println!("EFFECT_GROUP_WITNESS j late-registration-reresolve PASS");

    let admission_group = witness_key("admit");
    let admission_child = witness_child(&admission_group, 0);
    let admission_shape = witness_shape(&admission_group, std::slice::from_ref(&admission_child));
    let opened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &admission_group,
            "open",
            &EffectGroupOpenRequest {
                shape: admission_shape.clone(),
                content_checked: false,
            },
        )
        .await
        .expect("admission witness opens");
    assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
    let adopted: EffectGroupProbeAdoptResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &admission_group,
            "probe_and_adopt",
            &EffectGroupAdoptRequest {
                invocation_id: "inv_admission_dispatcher".to_owned(),
            },
        )
        .await
        .expect("admission witness adopts dispatcher");
    assert!(
        matches!(adopted, EffectGroupProbeAdoptResponse::Adopted { .. }),
        "admission witness adopts the dispatcher: {adopted:?}"
    );
    let admission_executions = Arc::new(AtomicUsize::new(0));
    witness_executors.stage(
        &admission_child,
        Arc::clone(&admission_executions),
        "fresh-admission",
    );
    let first_admit = arm_admission_witness(&admission_group);
    let child_invocation = ingress
        .send_workflow_json(
            "EffectGroupDispatch",
            &admission_group,
            "child",
            &EffectGroupChildRequest {
                group_key: admission_group.clone(),
                shape: admission_shape.clone(),
                position: 0,
                envelope: admission_child,
            },
        )
        .await
        .expect("send-before-record child is accepted");
    tokio::time::timeout(Duration::from_secs(10), first_admit.notified())
        .await
        .expect("child reaches NotYetRecorded before dispatcher redrive");
    let recorded: EffectGroupRecordDispatchResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &admission_group,
            "record_dispatch",
            &EffectGroupRecordDispatchRequest {
                position: 0,
                invocation_id: child_invocation.as_str().to_owned(),
            },
        )
        .await
        .expect("dispatcher redrive records mapping");
    assert_eq!(recorded, EffectGroupRecordDispatchResponse::Recorded);
    assert_eq!(
        await_group_wait(
            &ingress,
            admit_wait_request(&admission_shape.wait_scope, &admission_group, 0).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Admit,
        "record-before-register retains the ADMIT notification"
    );
    assert_eq!(
        await_group_wait(
            &ingress,
            rank_wait_request(&admission_shape.wait_scope, &admission_group, 1).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Rank,
        "fresh admission executes and records a settlement"
    );
    assert_eq!(
        admission_executions.load(Ordering::SeqCst),
        1,
        "the crash-before-record child executes exactly once"
    );
    let retired: EffectGroupRetireResponse = ingress
        .call_object_empty_json(
            crate::LashService::EffectGroupIndex,
            &admission_group,
            "retire",
        )
        .await
        .expect("admission witness tombstones");
    let cleanup = match retired {
        EffectGroupRetireResponse::Retired { cleanup }
        | EffectGroupRetireResponse::AlreadyRetired { cleanup } => cleanup,
        other => panic!("admission witness expected cleanup facts, got {other:?}"),
    };
    assert_admission_enumerated(&cleanup, child_invocation.as_str());

    let gap_group = witness_key("gap");
    let gap_child = witness_child(&gap_group, 0);
    let gap_shape = witness_shape(&gap_group, std::slice::from_ref(&gap_child));
    let gap_executions = Arc::new(AtomicUsize::new(0));
    witness_executors.stage(
        &gap_child,
        Arc::clone(&gap_executions),
        "never-recorded-child",
    );
    let _: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &gap_group,
            "open",
            &EffectGroupOpenRequest {
                shape: gap_shape.clone(),
                content_checked: false,
            },
        )
        .await
        .expect("send-record-gap witness opens");
    let _: EffectGroupRetireResponse = ingress
        .call_object_empty_json(crate::LashService::EffectGroupIndex, &gap_group, "retire")
        .await
        .expect("send-record-gap witness tombstones");
    let executions_before_child = gap_executions.load(Ordering::SeqCst);
    ingress
        .call_workflow_json::<_, ()>(
            "EffectGroupDispatch",
            &gap_group,
            "child",
            &EffectGroupChildRequest {
                group_key: gap_group.clone(),
                shape: gap_shape,
                position: 0,
                envelope: gap_child,
            },
        )
        .await
        .expect("post-tombstone never-recorded child is refused");
    assert_eq!(
        gap_executions.load(Ordering::SeqCst),
        executions_before_child,
        "post-tombstone child whose mapping was never recorded must not execute"
    );
    println!("EFFECT_GROUP_WITNESS m admission-enumeration PASS");

    run_drain_barrier_witnesses(&ingress, admin).await;
}

/// FIG-3598. The §5 barrier's drained wake is released by retirement: a
/// committed child that retirement cancels before it seats never resolves its
/// own wake, so a sibling parked behind it — here a waiter outside the retired
/// invocation — must be released as `Retired`. And an index whose state
/// another protocol version wrote refuses at handler entry with the typed
/// terminal error.
async fn run_drain_barrier_witnesses(ingress: &RestateIngressClient, admin: &HarnessAdmin) {
    use crate::effect_group::{
        EffectGroupCommitChildRequest, EffectGroupCommitChildResponse,
        EffectGroupDrainBlockersRequest, EffectGroupDrainBlockersResponse, drained_wait_request,
    };

    let group_key = witness_key("drained-retire");
    let children = [witness_child(&group_key, 0), witness_child(&group_key, 1)];
    let shape = witness_shape(&group_key, &children);
    let opened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "open",
            &EffectGroupOpenRequest {
                shape: shape.clone(),
                content_checked: false,
            },
        )
        .await
        .expect("drained-wake witness opens");
    assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
    let mut commit_seqs = Vec::new();
    for child in &children {
        let committed: EffectGroupCommitChildResponse = ingress
            .call_object_json(
                "EffectGroupIndex",
                &group_key,
                "commit_child",
                &EffectGroupCommitChildRequest {
                    replay_key: child.invocation.replay_key().to_owned(),
                },
            )
            .await
            .expect("drained-wake witness child commits");
        let EffectGroupCommitChildResponse::Committed { commit_seq, .. } = committed else {
            panic!("drained-wake witness child commits fresh, got {committed:?}");
        };
        commit_seqs.push(commit_seq);
    }
    let blockers: EffectGroupDrainBlockersResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "drain_blockers",
            &EffectGroupDrainBlockersRequest {
                commit_seq: commit_seqs[1],
            },
        )
        .await
        .expect("drained-wake witness reads the barrier");
    assert_eq!(
        blockers,
        EffectGroupDrainBlockersResponse::Blocked {
            wait_scope: shape.wait_scope.clone(),
            positions: vec![0],
        },
        "child 1 is held behind child 0's owed seat, under the retained wait scope"
    );
    let waiter = tokio::spawn({
        let ingress = ingress.clone();
        let request =
            drained_wait_request(&shape.wait_scope, &group_key, 0).expect("drained wake request");
        async move { await_group_wait(&ingress, request).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !waiter.is_finished(),
        "child 0 never seated, so its drained wake is unresolved"
    );
    ingress
        .call_workflow_json::<_, ()>("EffectGroupDispatch", &group_key, "retire", &group_key)
        .await
        .expect("retirement saga completes");
    let released = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("retirement releases the drained wake")
        .expect("drained-wake waiter task");
    assert_eq!(released, EffectGroupWaitResolution::Retired);
    println!("EFFECT_GROUP_WITNESS n drained-wake-retired PASS");

    let stale_group = witness_key("stale-protocol");
    let stale_child = witness_child(&stale_group, 0);
    let stale_shape = witness_shape(&stale_group, std::slice::from_ref(&stale_child));
    let _: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &stale_group,
            "open",
            &EffectGroupOpenRequest {
                shape: stale_shape.clone(),
                content_checked: false,
            },
        )
        .await
        .expect("stale-protocol witness opens");
    // Rewrite the group's state as a deployment that predates the stamp
    // left it: the same record with no protocol version.
    let stale_state = serde_json::json!({
        "shape_digest": stale_shape.digest().expect("witness shape digest"),
        "lifecycle": {"type": "retired", "cleanup": {"type": "complete"}},
    });
    overwrite_index_state(admin, &stale_group, &stale_state).await;
    let refused = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let probed = ingress
                .call_object_json::<_, EffectGroupDrainBlockersResponse>(
                    "EffectGroupIndex",
                    &stale_group,
                    "drain_blockers",
                    &EffectGroupDrainBlockersRequest { commit_seq: 1 },
                )
                .await;
            match probed {
                Err(error) => break error,
                // The admin state write lands asynchronously.
                Ok(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .expect("the rewritten state reaches the index");
    let typed = crate::effect_host::ingress_protocol_refusal(&refused)
        .unwrap_or_else(|| panic!("the refusal is typed: {refused}"));
    assert_eq!(
        typed.code,
        RuntimeErrorCode::RestateEffectGroupProtocolRetired
    );
    println!("EFFECT_GROUP_WITNESS o stale-protocol-refused-typed PASS");
}

/// Replace an effect-group index's retained state through the Restate admin
/// API.
async fn overwrite_index_state(admin: &HarnessAdmin, group_key: &str, state: &serde_json::Value) {
    let bytes = serde_json::to_vec(state).expect("encode the index state");
    let body = serde_json::json!({
        "object_key": group_key,
        "new_state": { "effect-group/v1/state": bytes },
    });
    let (status, body) = match admin {
        HarnessAdmin::Live { admin_url } => {
            let client = reqwest::Client::builder()
                .http2_prior_knowledge()
                .build()
                .expect("build Restate admin client");
            let response = client
                .post(format!(
                    "{}/services/EffectGroupIndex/state",
                    admin_url.trim_end_matches('/')
                ))
                .json(&body)
                .send()
                .await
                .expect("modify the effect-group index state");
            let status = response.status().as_u16();
            (status, response.text().await.unwrap_or_default())
        }
        HarnessAdmin::InProcess { server } => {
            let request = HttpRequest::post(
                format!("{}/services/EffectGroupIndex/state", server.ingress_url()),
                serde_json::to_vec(&body).expect("encode the state modification"),
            )
            .with_header("content-type", "application/json");
            let response = server
                .transport()
                .send(request, None)
                .await
                .expect("modify the effect-group index state");
            let status = response.status;
            let bytes = lash_http_transport::read_http_body_bytes(response.body, None, "state")
                .await
                .unwrap_or_default();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    assert!(
        (200..300).contains(&status),
        "Restate state modification failed: {status} {body}"
    );
}

fn witness_child(group_key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                ExecutionScope::runtime_operation(group_key),
                format!("{group_key}:child:{position}"),
            )
            .expect("valid witness child address"),
            lash_core::RuntimeAttribution::none(),
            "effect",
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: format!("witness-child-{position}"),
        },
    )
}

fn witness_key(label: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "effect-group-witness-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn witness_shape(group_key: &str, children: &[RuntimeEffectEnvelope]) -> EffectGroupShape {
    EffectGroupShape {
        wake: GroupWakePolicy::All,
        loser_disposition: LoserPolicy::RunToCompletion,
        replay_keys: children
            .iter()
            .map(|child| child.invocation.replay_key().to_owned())
            .collect(),
        wait_scope: ExecutionScope::runtime_operation(group_key),
        membership: children
            .iter()
            .map(|child| serde_json::to_string(child).expect("witness child serializes"))
            .collect(),
    }
}

async fn await_group_wait(
    ingress: &RestateIngressClient,
    request: RestateDurableWaitAwaitRequest,
) -> EffectGroupWaitResolution {
    let address = RestateDurableWaitAddress::for_key(&request.key);
    let resolution = ingress
        .call_workflow_json::<_, Resolution>(
            "LashDurableWaitWorkflow",
            &address.workflow_key,
            "await_resolution",
            &request,
        )
        .await
        .expect("effect-group witness wait resolves");
    decode_wait_resolution(resolution).expect("effect-group witness resolution is tagged")
}

fn assert_admission_enumerated(cleanup: &EffectGroupCleanupFacts, invocation_id: &str) {
    assert_eq!(
        cleanup.dispatched.get(&0).map(String::as_str),
        Some(invocation_id)
    );
}

/// Every invocation the server holds open, by id, with its target.
async fn open_invocations(admin: &HarnessAdmin) -> HashMap<String, String> {
    match admin {
        HarnessAdmin::Live { admin_url } => {
            #[derive(serde::Deserialize)]
            struct Row {
                id: String,
                target: String,
            }
            let admin = crate::RestateAdminClient::new(RestateConnection::new(admin_url.clone()));
            // A server that just started answers its SQL surface only once its
            // partition is up, so an early query is retried.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                match admin
                    .query_json::<Row>(
                        "SELECT id, target FROM sys_invocation WHERE status != 'completed'",
                    )
                    .await
                {
                    Ok(rows) => break rows.into_iter().map(|row| (row.id, row.target)).collect(),
                    Err(error) => assert!(
                        tokio::time::Instant::now() < deadline,
                        "query the server's open invocations: {error}"
                    ),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        HarnessAdmin::InProcess { server } => server
            .invocations()
            .into_iter()
            .filter(|view| view.status != "completed")
            .map(|view| (view.id, view.target))
            .collect(),
    }
}

fn required(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set by `just effect-group-conformance-e2e`"))
}

async fn wait_for_endpoint(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Restate effect-group endpoint did not open at {addr}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn register_deployment(admin_url: &str, endpoint_url: &str) {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build Restate admin client");
    let response = client
        .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .json(&serde_json::json!({
            "uri": endpoint_url,
            "force": true,
            "breaking": true,
        }))
        .send()
        .await
        .expect("register Restate effect-group deployment");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "Restate deployment registration failed: {status} {body}"
    );
}
