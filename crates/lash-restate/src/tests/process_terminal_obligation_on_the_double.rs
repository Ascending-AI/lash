//! A process's terminal publication is a `ProcessTerminal` obligation (ADR
//! 0109 §3, FIG-3856), on the real `LashProcessWorkflow` handlers served by
//! the in-process server double.
//!
//! The waiters these laws protect are the journal-side ones: a
//! `ProcessCommand::Await` or a `LashProcessAttach` waits only on the
//! process's terminal promise, through the root workflow's `await_terminal`,
//! and never reads SQL. A segment that stored its terminal and stopped before
//! resolving that promise stranded them (prospect S-14): nothing republished
//! a terminal row. Now the terminal transaction arms the row's obligation,
//! the segment that publishes settles it, and the relay publishes what no
//! segment did.

use super::*;

use lash_core::StoreSet as _;
use lash_core::runtime::drive::relay::relay_due;
use lash_core::runtime::process_terminal::ProcessTerminalRelay;
use lash_core::store::{ObligationKind, ObligationState};

/// Completes every run with a fixed success, or fails every attempt live,
/// retryably, while `failing`.
struct TerminalRunner {
    failing: AtomicBool,
    runs: AtomicUsize,
}

#[async_trait::async_trait]
impl RestateProcessRunner for TerminalRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            return Err(PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "the process's store is unreachable",
            )));
        }
        Ok(process_success(serde_json::json!({ "published": "by the segment" })).into())
    }
}

const MAX_ATTEMPTS: u64 = 3;

/// The server double serving the lash services over one SQLite memory store
/// set, the deployment that sweeps it, and the `ProcessTerminal` relay a
/// reconcile tick runs over the same registry and port.
struct World {
    server: lash_restate_test::RestateTestServer,
    ingress: RestateIngressClient,
    registry: Arc<dyn ProcessRegistry>,
    deployment: RestateProcessDeployment,
    relay: ProcessTerminalRelay,
    runner: Arc<TerminalRunner>,
}

impl World {
    async fn new(seed: u64, failing: bool) -> Self {
        let server = lash_restate_test::RestateTestServer::new(
            lash_restate_test::ServerConfig::default().with_seed(seed),
        )
        .expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set");
        let sqlite_registry = stores.process_registry();
        let registry: Arc<dyn ProcessRegistry> = sqlite_registry.clone();
        let continuations: Arc<dyn lash_core::ProcessContinuationStore> = sqlite_registry;
        let runner = Arc::new(TerminalRunner {
            failing: AtomicBool::new(failing),
            runs: AtomicUsize::new(0),
        });
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                effect_host: &host,
                ingress: RestateIngressClient::new(connection.clone()),
                sessions: stores.session_store_factory(),
                process_workflow: LashProcessWorkflowImpl::new_for_test(
                    Arc::clone(&runner),
                    Arc::clone(&registry),
                    Arc::clone(&continuations),
                )
                .with_retry_max_attempts(MAX_ATTEMPTS),
                session_driver: crate::RestateSessionDriverSlot::new(),
                build_generation: lash_core::engine::BuildGeneration::for_test(
                    "process-terminal-obligation",
                ),
            },
        )
        .build();
        server
            .register(endpoint)
            .await
            .expect("register the endpoint on the server double");
        let deployment = RestateProcessDeployment::new_for_test(
            connection.clone(),
            Arc::clone(&registry),
            continuations,
        );
        deployment.install_park_reconciler(crate::RestateAdminClient::new(connection.clone()));
        let port: Arc<dyn lash_core::ProcessWorkSubstrate> = deployment.test_process_work();
        let relay = ProcessTerminalRelay::new(
            stores.obligation_ledger(ObligationKind::ProcessTerminal),
            Arc::clone(&registry),
            port,
        );
        Self {
            server,
            ingress: RestateIngressClient::new(connection),
            registry,
            deployment,
            relay,
            runner,
        }
    }

    async fn register(&self) -> ProcessId {
        self.registry
            .register_process(rerunnable_registration())
            .await
            .expect("register the process")
            .id
    }

    async fn sweep(&self) {
        let _ = self
            .deployment
            .test_process_work()
            .admit_pending_processes("process terminal obligation")
            .await
            .expect("the sweep runs");
    }

    async fn publication(&self, process_id: &ProcessId) -> Option<ObligationState> {
        self.registry
            .terminal_publication(process_id)
            .await
            .expect("read the terminal publication")
            .map(|publication| publication.state)
    }

    /// One reconcile tick's due pass over the `ProcessTerminal` ledger.
    async fn relay_pass(&self) -> lash_core::engine::RelayPass {
        relay_due(
            &self.relay,
            &lash_core::runtime::SystemClock,
            std::num::NonZeroUsize::new(64).expect("non-zero"),
        )
        .await
        .expect("the relay's due pass")
    }

    /// A journal-side waiter: the root workflow's `await_terminal`, the
    /// promise wait `ProcessCommand::Await` and `LashProcessAttach` make.
    fn waiter(
        &self,
        process_id: &ProcessId,
    ) -> tokio::task::JoinHandle<Result<ProcessAwaitOutput, crate::RestateHttpError>> {
        let ingress = self.ingress.clone();
        let process_id = process_id.clone();
        tokio::spawn(async move {
            let key = process_id.to_string();
            ingress
                .call_workflow_json::<_, ProcessAwaitOutput>(
                    &crate::services::ServiceRoute::stable(crate::LashService::ProcessWorkflow)
                        .name(),
                    &key,
                    "await_terminal",
                    &RestateProcessAwaitRequest { process_id },
                )
                .await
        })
    }

    async fn wait_for_status(
        &self,
        target: &str,
        status: &str,
    ) -> lash_restate_test::InvocationView {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(view) = self
                .server
                .invocations()
                .into_iter()
                .find(|view| view.target == target && view.status == status)
            {
                return view;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "`{target}` never reached `{status}`: {:?}",
                self.server.invocations()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Store `process_id`'s terminal as a segment's completion step does, and
/// stop there: the execution that stored it never published it.
async fn store_terminal_without_publishing(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> ProcessAwaitOutput {
    let completion = registry
        .complete_process(
            process_id,
            process_success(serde_json::json!({ "published": "by the relay" })),
            crate::process::workflow_key_authority(process_id),
        )
        .await
        .expect("store the terminal");
    completion
        .stored()
        .outcome
        .clone()
        .expect("the completion stores a terminal")
}

/// S-14: a terminal whose execution stopped between the terminal commit and
/// the publication reaches the waiter parked on the engine's promise. The
/// terminal transaction armed the obligation, the relay's due pass publishes
/// the stored terminal through the root's `complete_terminal`, and the row
/// settles delivered; a second pass finds nothing due.
#[tokio::test]
pub(super) async fn a_terminal_no_execution_published_reaches_its_journal_side_waiter() {
    let world = World::new(3856, false).await;
    let process_id = world.register().await;
    assert_eq!(
        world.publication(&process_id).await,
        None,
        "a live process owes no terminal publication"
    );
    let waiter = world.waiter(&process_id);
    let stored = store_terminal_without_publishing(&world.registry, &process_id).await;
    assert_eq!(
        world.publication(&process_id).await,
        Some(ObligationState::Due),
        "the terminal transaction arms the publication"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !waiter.is_finished(),
        "nothing has resolved the promise the waiter is parked on"
    );

    let pass = world.relay_pass().await;
    assert_eq!(
        (pass.claimed, pass.delivered, pass.stalled),
        (1, 1, 0),
        "the due pass publishes the one owed terminal: {pass:?}"
    );
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), waiter)
        .await
        .expect("the waiter is served once the relay publishes")
        .expect("the waiter task")
        .expect("the waiter's call");
    assert_eq!(published, stored, "the waiter reads the stored terminal");
    assert_eq!(
        world.publication(&process_id).await,
        Some(ObligationState::Delivered)
    );
    assert_eq!(
        world.relay_pass().await.claimed,
        0,
        "a delivered publication is never attempted again"
    );
}

/// The immediate delivery: a segment that stores its terminal publishes it in
/// its own journal and settles the obligation itself, so the relay's due pass
/// finds nothing to do.
#[tokio::test]
pub(super) async fn a_segment_that_publishes_its_terminal_settles_the_obligation() {
    let world = World::new(3857, false).await;
    let process_id = world.register().await;
    let waiter = world.waiter(&process_id);
    world.sweep().await;
    world
        .wait_for_status(
            &format!("LashProcessWorkflow/{process_id}/run"),
            "completed",
        )
        .await;
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), waiter)
        .await
        .expect("the segment's own publication serves the waiter")
        .expect("the waiter task")
        .expect("the waiter's call");
    let stored = world
        .registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .and_then(|record| record.outcome)
        .expect("the segment stored its terminal");
    assert_eq!(published, stored);
    assert_eq!(
        world.publication(&process_id).await,
        Some(ObligationState::Delivered),
        "the publishing segment settled its obligation"
    );
    assert_eq!(
        world.relay_pass().await.claimed,
        0,
        "the relay has nothing left to publish"
    );
}

/// A paused segment of a terminal process holds nothing once the terminal is
/// published: the sweep leaves it while the publication is owed, the relay
/// publishes through the root's shared handler with the root's `run` paused,
/// and the next sweep kills the paused invocation.
#[tokio::test]
pub(super) async fn a_paused_terminal_segment_is_killed_once_its_terminal_is_published() {
    let world = World::new(3858, true).await;
    let process_id = world.register().await;
    world.sweep().await;
    let target = format!("LashProcessWorkflow/{process_id}/run");
    let paused = world.wait_for_status(&target, "paused").await;
    assert_eq!(
        world.runner.runs.load(Ordering::SeqCst),
        usize::try_from(MAX_ATTEMPTS).expect("small"),
    );

    // The process ends while its only segment is paused.
    let waiter = world.waiter(&process_id);
    let stored = store_terminal_without_publishing(&world.registry, &process_id).await;
    world.sweep().await;
    assert!(
        world
            .server
            .invocations()
            .iter()
            .any(|view| view.id == paused.id && view.status == "paused"),
        "the sweep leaves a paused terminal segment while its publication is owed"
    );

    assert_eq!(world.relay_pass().await.delivered, 1);
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), waiter)
        .await
        .expect("the relay's publication serves the waiter")
        .expect("the waiter task")
        .expect("the waiter's call");
    assert_eq!(published, stored);

    world.sweep().await;
    let killed = world.wait_for_status(&target, "completed").await;
    assert_eq!(killed.id, paused.id, "the paused segment itself ends");
    assert_eq!(
        world.runner.runs.load(Ordering::SeqCst),
        usize::try_from(MAX_ATTEMPTS).expect("small"),
        "the paused segment is killed, never resumed"
    );
    let record = world
        .registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    assert_eq!(record.outcome, Some(stored), "the stored terminal stands");
}
