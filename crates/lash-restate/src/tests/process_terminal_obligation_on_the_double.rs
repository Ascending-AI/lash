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

use lash_core::runtime::drive::relay::relay_due;
use lash_core::runtime::process_start::ProcessStartRelay;
use lash_core::runtime::process_terminal::ProcessTerminalRelay;
use lash_core::store::{ObligationKind, ObligationState};

/// Completes every run with a fixed success, fails every attempt live,
/// retryably, while `failing`, or never answers while `hanging`.
struct TerminalRunner {
    failing: AtomicBool,
    hanging: AtomicBool,
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
        if self.hanging.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
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
/// set, the deployment whose recovery pass reconciles it, and the
/// `ProcessTerminal` relay a reconcile tick runs over the same registry and
/// port.
struct World {
    server: lash_restate_test::RestateTestServer,
    ingress: RestateIngressClient,
    admin: RestateAdminClient,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    start_relay: ProcessStartRelay,
    relay: ProcessTerminalRelay,
    runner: Arc<TerminalRunner>,
}

impl World {
    async fn new(seed: u64, failing: bool) -> Self {
        Self::build(seed, failing, false).await
    }

    /// A world whose runner never answers: every submitted `run` stays
    /// running until the engine stops it.
    async fn new_hanging(seed: u64) -> Self {
        Self::build(seed, false, true).await
    }

    async fn build(seed: u64, failing: bool, hanging: bool) -> Self {
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
            hanging: AtomicBool::new(hanging),
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
                namespace: crate::RestateNamespace::default(),
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
            Arc::clone(&continuations),
        );
        let port: Arc<dyn lash_core::ProcessWorkSubstrate> = deployment.test_process_work();
        let start_relay = ProcessStartRelay::new(
            stores.obligation_ledger(ObligationKind::ProcessStart),
            Arc::clone(&registry),
            Arc::clone(&port),
            stores.clock(),
        );
        let relay = ProcessTerminalRelay::new(
            stores.obligation_ledger(ObligationKind::ProcessTerminal),
            Arc::clone(&registry),
            port,
        );
        Self {
            server,
            admin: crate::RestateAdminClient::new(connection.clone()),
            ingress: RestateIngressClient::new(connection),
            registry,
            continuations,
            start_relay,
            relay,
            runner,
        }
    }

    async fn register(&self) -> ProcessId {
        self.registry
            .register_process(executed_registration())
            .await
            .expect("register the process")
            .id
    }

    /// One recovery pass of the deployment: the due `ProcessStart` relay,
    /// then the park reconcile and the lost-run scan.
    async fn recovery_pass(&self) {
        relay_due(
            &self.start_relay,
            &lash_core::facade_support::SystemClock,
            std::num::NonZeroUsize::new(128).expect("nonzero"),
        )
        .await
        .expect("deliver due process starts");
        reconcile_parked_processes(&self.admin, &self.registry, &self.continuations).await;
        crate::process::park_reconcile::end_lost_process_runs(
            &self.admin,
            &self.ingress,
            &crate::services::DEFAULT_NAMESPACE,
            &self.registry,
            &self.continuations,
            std::num::NonZeroUsize::new(128).expect("nonzero"),
        )
        .await
        .expect("end lost process runs");
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
                    &crate::services::DEFAULT_NAMESPACE
                        .stable(crate::LashService::ProcessWorkflow)
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
    world.recovery_pass().await;
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
/// published: the pass leaves it while the publication is owed, the relay
/// publishes through the root's shared handler with the root's `run` paused,
/// and the next pass kills the paused invocation.
#[tokio::test]
pub(super) async fn a_paused_terminal_segment_is_killed_once_its_terminal_is_published() {
    let world = World::new(3858, true).await;
    let process_id = world.register().await;
    world.recovery_pass().await;
    let target = format!("LashProcessWorkflow/{process_id}/run");
    let paused = world.wait_for_status(&target, "paused").await;
    assert_eq!(
        world.runner.runs.load(Ordering::SeqCst),
        usize::try_from(MAX_ATTEMPTS).expect("small"),
    );

    // The process ends while its only segment is paused.
    let waiter = world.waiter(&process_id);
    let stored = store_terminal_without_publishing(&world.registry, &process_id).await;
    world.recovery_pass().await;
    assert!(
        world
            .server
            .invocations()
            .iter()
            .any(|view| view.id == paused.id && view.status == "paused"),
        "the pass leaves a paused terminal segment while its publication is owed"
    );

    assert_eq!(world.relay_pass().await.delivered, 1);
    let published = tokio::time::timeout(std::time::Duration::from_secs(10), waiter)
        .await
        .expect("the relay's publication serves the waiter")
        .expect("the waiter task")
        .expect("the waiter's call");
    assert_eq!(published, stored);

    world.recovery_pass().await;
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

/// FIG-3900: the lost-run scan reads the runs lash still waits on — the
/// current segment of every live process — never a newest-first page of the
/// engine's retained history. A kill whose failed run 65 newer retained
/// failures crowd past the end of any such page still ends the process
/// `SubstrateLost` and serves its waiter.
#[tokio::test]
pub(super) async fn a_killed_run_ends_substrate_lost_however_many_newer_failed_runs_are_kept() {
    let world = World::new_hanging(3900).await;
    let process_id = world.register().await;
    let mut crowd = Vec::new();
    for _ in 0..65 {
        crowd.push(world.register().await);
    }
    world.recovery_pass().await;
    let target = |process: &ProcessId| format!("LashProcessWorkflow/{process}/run");
    for process in [&process_id].into_iter().chain(crowd.iter()) {
        world.wait_for_status(&target(process), "running").await;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while world
        .registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .and_then(|record| record.first_started.map(|started| started.owner))
        .is_none()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the process's run never recorded its start"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let waiter = world.waiter(&process_id);

    // The kill lands first, so the crowd's 65 failed runs are all newer in
    // the engine's retained history and push the victim off a newest-64 page.
    let victim_run = world.wait_for_status(&target(&process_id), "running").await;
    assert_eq!(
        world.server.kill_and_await(&victim_run.id).await,
        Some(true),
        "the victim's run is killed"
    );
    for process in &crowd {
        let run = world.wait_for_status(&target(process), "running").await;
        assert_eq!(
            world.server.kill_and_await(&run.id).await,
            Some(true),
            "the crowd run is killed"
        );
    }

    let pass = crate::process::park_reconcile::end_lost_process_runs(
        &world.admin,
        &world.ingress,
        &crate::services::DEFAULT_NAMESPACE,
        &world.registry,
        &world.continuations,
        std::num::NonZeroUsize::new(64).expect("non-zero"),
    )
    .await
    .expect("the lost-run pass");
    assert!(
        pass.ended.contains(&process_id),
        "the killed process ends however many newer failed runs are kept: {pass:?}"
    );

    // Every ended process armed its `ProcessTerminal` obligation; the
    // relay's due passes page through them until the victim's publication
    // serves its waiter.
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !waiter.is_finished() {
            world.relay_pass().await;
        }
    })
    .await
    .expect("the waiter is served once the relay publishes");
    let outcome = waiter
        .await
        .expect("the waiter task")
        .expect("the waiter's call");
    assert!(
        matches!(
            outcome,
            ProcessAwaitOutput::Abandoned { ref evidence, .. }
                if evidence.writer
                    == lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                    }
        ),
        "the killed process ends substrate-lost: {outcome:?}"
    );
}

/// FIG-3962: a started process whose run Restate no longer holds — killed
/// and then purged, so no failed run is left to read either — is reached by
/// the recovery tick's lost-run pass. Its `ProcessStart` was delivered once
/// and nothing re-arms it; the pass resubmits the current segment, whose
/// admission ends the process `SubstrateLost`, and its waiter is served.
#[tokio::test]
pub(super) async fn a_started_process_whose_run_restate_purged_ends_substrate_lost() {
    let world = World::new_hanging(3962).await;
    let process_id = world.register().await;
    let start = || {
        relay_due(
            &world.start_relay,
            &lash_core::facade_support::SystemClock,
            std::num::NonZeroUsize::new(16).expect("non-zero"),
        )
    };
    let delivered = start().await.expect("deliver the process start");
    assert_eq!(
        delivered.delivered, 1,
        "the start is delivered: {delivered:?}"
    );
    let target = format!("LashProcessWorkflow/{process_id}/run");
    let run = world.wait_for_status(&target, "running").await;
    // The kill trails the body's entry, not only the journaled start: the
    // marker commits in admission and the runner's first poll is a
    // scheduling gap of awaits later, so a kill ordered on the marker alone
    // can abort the attempt before it counted its body — leaving `runs` at
    // zero, with no started work for the resubmission to be measured
    // against.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while world.runner.runs.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the process's run never entered its body"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(world.server.kill_and_await(&run.id).await, Some(true));
    assert_eq!(world.server.purge(&run.id), Some(true), "the run is purged");
    let again = start().await.expect("the start relay's next pass");
    assert_eq!(again.claimed, 0, "nothing re-arms the start: {again:?}");

    let pass = crate::process::park_reconcile::end_lost_process_runs(
        &world.admin,
        &world.ingress,
        &crate::services::DEFAULT_NAMESPACE,
        &world.registry,
        &world.continuations,
        std::num::NonZeroUsize::new(16).expect("non-zero"),
    )
    .await
    .expect("the lost-run pass");
    assert_eq!(pass.resubmitted, vec![process_id.clone()], "{pass:?}");

    let waiter = world.waiter(&process_id);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !waiter.is_finished() {
            world.relay_pass().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the waiter is served");
    let outcome = waiter
        .await
        .expect("the waiter task")
        .expect("the waiter's call");
    assert!(
        matches!(
            outcome,
            ProcessAwaitOutput::Abandoned { ref evidence, .. }
                if evidence.writer
                    == lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                    }
        ),
        "the purged process ends substrate-lost: {outcome:?}"
    );
    assert_eq!(
        world.runner.runs.load(Ordering::SeqCst),
        1,
        "no started work re-ran from scratch"
    );
    let pass = crate::process::park_reconcile::end_lost_process_runs(
        &world.admin,
        &world.ingress,
        &crate::services::DEFAULT_NAMESPACE,
        &world.registry,
        &world.continuations,
        std::num::NonZeroUsize::new(16).expect("non-zero"),
    )
    .await
    .expect("the next lost-run pass");
    assert!(
        pass.resubmitted.is_empty(),
        "the ended process is left: {pass:?}"
    );
}
