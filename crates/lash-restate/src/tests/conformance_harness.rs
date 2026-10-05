//! Endpoint and store support for native Run, process and scope laws.
#![allow(clippy::disallowed_methods)]
use super::live_turn_probe::ConformanceTurnProbe as _;
pub(super) const HARNESS_BUILD: &str = "run-conformance";
use crate::durable_wait::arm_wait_registration_witness;
use crate::process::{LashProcessWorkflowImpl, RestateProcessRunner};
use crate::{
    RestateConnection, RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    RestateDurableWaitRegistration, RestateEffectHost, RestateIngressClient,
};
use lash_core::testing::store_fixtures::durable_admission;
use lash_core::{
    ExecutionScope, Resolution, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
/// The endpoint's process runner for the tool laws: a tool call's
/// declared starts record through the Restate process surface, so the
/// service must exist for the submission to be a legal command. What runs
/// the segment is not under test, so the runner settles
/// every submitted process successfully and lets the workflow write the
/// terminal into the law's registry.
struct ToolProcessRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for ToolProcessRunner {
    fn executable_generation(
        &self,
        _registration: &lash_core::ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &crate::SegmentStarted,
        _process_id: lash_core::ProcessId,
        _registration: lash_core::ProcessRegistration,
        _execution_context: lash_core::ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        Ok(lash_core::ProcessRunOutcome::Terminal {
            output: Box::new(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::json!({
                    "runner": "run-conformance"
                })),
            )),
            prelude: Vec::new(),
        })
    }
}

/// The endpoint's process runner for the turn-executing laws: a law that runs
/// real process segments (a `spawn_agent` child session) installs its own
/// [`DurableProcessWorker`](lash_core_worker::DurableProcessWorker) here,
/// which is what a deployment's `RestateCoreProcessRunner` serves; until one
/// is installed the endpoint answers as [`ToolProcessRunner`] does.
#[derive(Default)]
pub(super) struct LawProcessRunner {
    installed: Mutex<Option<crate::RestateCoreProcessRunner>>,
    /// The processes whose segments a law serves with its own body
    /// ([`ServedSegments`](super::live_turn_probe::ServedSegments)); they win
    /// over the installed worker.
    segments: super::live_turn_probe::ServedSegments,
}

impl LawProcessRunner {
    pub(super) fn segments(&self) -> &super::live_turn_probe::ServedSegments {
        &self.segments
    }

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
    fn executable_generation(
        &self,
        _registration: &lash_core::ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn admit_plugins(
        &self,
    ) -> Result<Option<lash_core::store::plugin_writers::PluginAdmission>, lash_core::PluginError>
    {
        match self.installed() {
            Some(runner) => runner.admit_plugins().await,
            None => Ok(None),
        }
    }

    async fn run_process_segment(
        &self,
        started: &crate::SegmentStarted,
        process_id: lash_core::ProcessId,
        registration: lash_core::ProcessRegistration,
        execution_context: lash_core::ProcessExecutionContext,
        scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        cancellation: CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        if self.segments.serves(&process_id, &registration) {
            return self
                .segments
                .run(&process_id, scoped_effect_controller)
                .await;
        }
        match self.installed() {
            Some(runner) => {
                Box::pin(runner.run_process_segment(
                    started,
                    process_id,
                    registration,
                    execution_context,
                    scoped_effect_controller,
                    handover,
                    cancellation,
                ))
                .await
            }
            None => {
                ToolProcessRunner
                    .run_process_segment(
                        started,
                        process_id,
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
}

/// Which Restate server a harness executes its endpoint through.
#[derive(Clone)]
pub(super) enum HarnessServer {
    /// A live `restate-server` (the `just run-conformance-e2e`
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

mod admin;
pub(super) use admin::HarnessAdmin;

pub(super) struct LiveConformanceHarness {
    connection: RestateConnection,
    admin: HarnessAdmin,
    session_shifts: crate::RestateSessionShiftsSlot,
    host: Arc<RestateEffectHost>,
    /// The storage the endpoint's process workflow and a law's runtime share.
    stores: Arc<dyn lash_core::StoreSet>,
    /// The same store set as SQLite's own type, for the laws that open a
    /// conformance handle on its catalog: `None` on another store tier.
    sqlite: Option<lash_sqlite_store::SqliteStoreSet>,
    /// What keeps a file or PostgreSQL tier's substrate alive.
    _tier: Option<super::harness_store_tiers::HarnessTierResources>,
    process_runner: Arc<LawProcessRunner>,
    shutdown_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    server: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for LiveConformanceHarness {
    fn drop(&mut self) {
        if matches!(self.admin, HarnessAdmin::Live { .. })
            && self.shutdown_tx.get_mut().is_some()
            && !std::thread::panicking()
        {
            // Registration macros retain this guard through the law. Finish
            // before their runtime exits, while its endpoint can still serve.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.finish());
            });
        }
    }
}

impl LiveConformanceHarness {
    /// The shared-laws endpoint on a live server.
    pub(super) async fn start() -> Self {
        Self::start_on(HarnessServer::Live).await
    }

    pub(super) async fn start_on(target: HarnessServer) -> Self {
        Self::start_for_tools_over(
            target,
            super::harness_store_tiers::HarnessStoreTier::SqliteMemory,
        )
        .await
    }
    pub(super) async fn start_for_tools() -> Self {
        Self::start().await
    }
    pub(super) async fn start_for_tools_on(target: HarnessServer) -> Self {
        Self::start_on(target).await
    }

    pub(super) async fn start_for_tools_over(
        target: HarnessServer,
        tier: super::harness_store_tiers::HarnessStoreTier,
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
                            println!("WATCHDOG {}", admin::open_invocations_report(&watched));
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
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        let invocation_admin = crate::RestateAdminClient::new(admin.connection());
        let (stores, sqlite, tier) = tier.open().await;

        let process_runner = Arc::new(LawProcessRunner::default());
        let session_shifts = crate::RestateSessionShiftsSlot::new();
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                tool_realizer: Arc::new(crate::tests::NoIntentsRealizer),
                effect_host: &host,
                admin: invocation_admin,
                materials: stores.tool_material_store(),
                attachments: stores.attachment_referrers(),
                process_workflow: LashProcessWorkflowImpl::new_for_test(
                    Arc::clone(&process_runner),
                    stores.process_registry(),
                    stores.process_continuations(),
                ),
                // The laws run their turns in the probe's handler; no core
                // installs a session `SessionShifts` on this endpoint.
                session_shifts: session_shifts.clone(),
                build_generation: lash_core::engine::BuildGeneration::for_test(HARNESS_BUILD),
            namespace: crate::RestateNamespace::default(),
            fleet: crate::object_state::FleetView::default(),
            },
        )
        .bind(ScopeLivenessProbeImpl.serve())
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
                    .expect("bind Restate run-conformance endpoint");
                let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                let server = tokio::spawn(async move {
                    crate::serve_endpoint(
                        listener,
                        endpoint,
                        crate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
                        async {
                            let _ = shutdown_rx.await;
                        },
                    )
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
                    .expect("register the run-conformance endpoint on the server double");
                (None, None)
            }
            (None, HarnessAdmin::Live { .. }) => {
                unreachable!("a live harness always has its live endpoint address")
            }
        };

        Self {
            connection,
            admin,
            session_shifts,
            host,
            stores,
            sqlite,
            _tier: tier,
            process_runner,
            shutdown_tx: tokio::sync::Mutex::new(shutdown_tx),
            server: tokio::sync::Mutex::new(server),
        }
    }

    /// The admin API of the server this harness runs on.
    pub(super) fn admin_connection(&self) -> RestateConnection {
        self.admin.connection()
    }

    /// The harness's admin face: an operator's kill and the retention
    /// sweep's purge.
    pub(super) fn harness_admin(&self) -> &HarnessAdmin {
        &self.admin
    }

    /// A client of [`Self::admin_connection`].
    pub(super) fn admin_client(&self) -> crate::RestateAdminClient {
        crate::RestateAdminClient::new(self.admin_connection())
    }

    pub(super) fn session_work(&self) -> crate::RestateSessionWork {
        crate::RestateSessionWork::new(
            crate::RestateIngressClient::new(self.connection.clone()),
            self.session_shifts.clone(),
            lash_core::engine::EngineGeneration::fixed(
                lash_core::engine::BuildGeneration::for_test("run-conformance"),
            ),
            crate::RestateNamespace::default(),
            Arc::new(crate::session_control::RestateSessionControl {
                lost_processes: Default::default(),
                lost_runs: Default::default(),
                admin: self.admin_client(),
                ingress: crate::RestateIngressClient::new(self.connection.clone()),
                namespace: crate::RestateNamespace::default(),
                processes: self.stores.process_registry(),
                continuations: self.stores.process_continuations(),
                generation: lash_core::engine::EngineGeneration::fixed(
                    lash_core::engine::BuildGeneration::for_test("run-conformance"),
                ),
                sessions: self.stores.session_store_factory(),
            }),
        )
    }

    /// The endpoint's host for a law that builds a runtime on it.
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
            self.admin.clone(),
            Arc::clone(&self.process_runner),
        )
    }

    pub(super) fn tool_call_identity_runner(
        &self,
    ) -> Arc<dyn lash_conformance::ConformanceTurnRunner> {
        super::live_turn_probe::LiveTurnRunner::shared(
            self.connection.clone(),
            self.admin.clone(),
            Arc::clone(&self.process_runner),
        )
    }

    /// The endpoint's ingress connection: a law sends to the same Restate
    /// the endpoint's handlers serve.
    pub(super) fn connection(&self) -> RestateConnection {
        self.connection.clone()
    }

    /// A Restate backend over the endpoint's own store set: the process
    /// registry the endpoint's `LashProcessWorkflow` writes into, and a
    /// process port that delivers to the workflows it runs.
    pub(super) fn law_backend(&self) -> lash_core::Backend {
        lash_core::Backend::new(Arc::new(crate::RestateEngine::new(
            Arc::clone(&self.stores),
            crate::RestateConfig::new(
                self.connection.clone(),
                self.admin_connection(),
                // The endpoint's own authority: a law's runtime binds
                // turn-control under it, so it must match what the probe's
                // controller was admitted with.
                crate::RestateAuthorityId::new("lash-restate-tests").expect("valid authority"),
            )
            .stamped(lash_core::engine::BuildGeneration::for_test(
                "parent-end-laws",
            )),
        )))
    }

    /// The in-process server double this harness runs on, if it runs on one.
    pub(super) fn server_double(&self) -> Option<lash_restate_test::RestateTestServer> {
        match &self.admin {
            HarnessAdmin::InProcess { server } => Some(server.clone()),
            HarnessAdmin::Live { .. } => None,
        }
    }

    /// The storage a law's runtime runs over: the store set whose registry
    /// the endpoint's `LashProcessWorkflow` writes terminals into.
    pub(super) fn law_stores(&self) -> Arc<dyn lash_core::StoreSet> {
        Arc::clone(&self.stores)
    }

    pub(super) fn sqlite_database_uri(
        &self,
        database: lash_sqlite_store::SqliteDatabase,
    ) -> String {
        self.sqlite
            .as_ref()
            .expect("SQLite tier")
            .database_uri(database)
    }

    /// A maker of fresh, unbound conformance handles on this endpoint's
    /// session catalog: the store a crash law's runtime commits through and
    /// the law reads and stamps back, over the same database the endpoint's
    /// handlers read.
    pub(super) fn law_persistence(
        &self,
    ) -> impl Fn(&str) -> Arc<lash_sqlite_store::SqliteStore> + Send + Sync + 'static + use<> {
        let stores = self
            .sqlite
            .clone()
            .expect("a conformance handle opens on the SQLite memory tier's catalog");
        move |_scenario| {
            let stores = stores.clone();
            super::conformance_and_poison::sync_await(async move {
                stores
                    .open_store()
                    .await
                    .expect("open a conformance handle on the endpoint's session catalog")
            })
        }
    }

    /// A maker of session-store factories over this endpoint's store set.
    pub(super) fn session_catalog_factory(
        &self,
    ) -> impl Fn() -> Arc<dyn lash_core::DeploymentStore> + Send + Sync + 'static {
        let stores = Arc::clone(&self.stores);
        move || stores.session_store_factory()
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
    ) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = lash_core::Backend> + Send>>
    + Send
    + Sync
    + 'static {
        let connection = self.connection.clone();
        let admin = self.admin_connection();
        move || {
            let connection = connection.clone();
            let admin = admin.clone();
            Box::pin(async move {
                lash_core::Backend::new(Arc::new(crate::RestateEngine::new(
                    Arc::new(
                        lash_sqlite_store::SqliteStoreSet::memory()
                            .await
                            .expect("open the law's store set"),
                    ),
                    crate::RestateConfig::new(
                        connection,
                        admin,
                        crate::RestateAuthorityId::new("lash-conformance-backend-laws")
                            .expect("valid authority"),
                    )
                    .stamped(lash_core::engine::BuildGeneration::for_test(
                        "run-conformance",
                    )),
                )))
            })
        }
    }

    /// Shuts the endpoint task down. Takes `&self` so a harness shared across
    /// a registration fixture's maker, witness and teardown can still be torn
    /// down exactly once; a second call is a no-op.
    pub(super) async fn finish(&self) {
        if self.shutdown_tx.lock().await.is_none() {
            return;
        }
        let mut open = Vec::new();
        let census = if matches!(self.admin, HarnessAdmin::Live { .. }) {
            let admin = self.admin_client();
            Some(tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    open = admin
                        .query_json::<lash_restate_test::live::LiveInvocation>(
                            "SELECT id, target, status, retry_count, last_failure FROM sys_invocation \
                             WHERE status != 'completed' ORDER BY id",
                        )
                        .await?;
                    if open.is_empty() {
                        return Ok::<_, crate::RestateHttpError>(());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }).await)
        } else {
            None
        };
        if let Some(shutdown_tx) = self.shutdown_tx.lock().await.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(server) = self.server.lock().await.take() {
            server.await.expect("Restate run-conformance endpoint task");
        }
        if let Some(census) = census {
            match census {
                Ok(Ok(())) => (),
                Ok(Err(error)) => panic!("finish census failed: {error}"),
                Err(_) if open.is_empty() => {
                    panic!("finish census did not complete within five seconds")
                }
                Err(_) => panic!("unexpected leftover invocations: {open:#?}"),
            }
        }
    }

    pub(super) async fn kill_open(&self, reason: &str) {
        assert!(
            !reason.trim().is_empty() && !reason.contains(['\n', '\r']),
            "kill_open requires a nonempty, one-line reason",
        );
        let admin = self.admin_client();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let open = admin
                    .query_json::<lash_restate_test::live::LiveInvocation>(
                        "SELECT id, target, status, retry_count, last_failure FROM sys_invocation \
                         WHERE status != 'completed' ORDER BY id",
                    )
                    .await
                    .expect("kill_open census");
                if open.is_empty() {
                    return;
                }
                eprintln!("kill_open: {reason}\n{open:#?}");
                for row in open {
                    admin
                        .kill_invocation(&crate::RestateInvocationId::new(row.id))
                        .await
                        .expect("kill deliberately unfinished invocation");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("kill_open completes within thirty seconds");
    }

    /// An ingress client of the harness's server.
    pub(super) fn ingress(&self) -> RestateIngressClient {
        RestateIngressClient::new(self.connection.clone())
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
            lash_core::AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
                "still-open",
            )),
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
                lash_core::AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
                    "after-retirement",
                )),
            )
            .await
            .expect_err("the retired scope mints nothing");
        assert_eq!(fenced.code.as_str(), "await_event_unknown_or_revoked");
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
                lash_core::AwaitEventWaitIdentity::Custom {
                    key: "active-wait".into(),
                },
            )
            .await
            .expect("mint registration-first wait key");
        let registered = arm_wait_registration_witness(&key);
        let waiter_host = Arc::clone(&host);
        let waiter_key = key.clone();
        let waiter = lash_core::task::spawn(async move {
            waiter_host
                .await_await_event(&waiter_key, CancellationToken::new())
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
                lash_core::AwaitEventWaitIdentity::Custom {
                    key: "late-wait".into(),
                },
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
                .call_lash_workflow::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &RestateDurableWaitAwaitRequest { key: retired_key },
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

pub(super) async fn open_invocations(admin: &HarnessAdmin) -> HashMap<String, String> {
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
        .unwrap_or_else(|_| panic!("{name} must be set by `just run-conformance-e2e`"))
}

async fn wait_for_endpoint(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Restate run-conformance endpoint did not open at {addr}"
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
        .expect("register Restate run-conformance deployment");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "Restate deployment registration failed: {status} {body}"
    );
}
