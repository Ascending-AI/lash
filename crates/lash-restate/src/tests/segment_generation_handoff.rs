//! The segment hand-off across a build roll (FIG-3795 §5.2: L1, L2, L6, L8,
//! L11) on the multi-deployment server double.
//!
//! Two builds serve one store set: build N (drain generation `G_N`) and build
//! N+1 (`G_N+1`), each binding every lash service through the deployment
//! binder, so each serves `LashProcessWorkflow` under its stable name and
//! its own `_g<G>` lane. A three-segment process runs segments 0 and 1 on N;
//! N+1 registers at a deployment event around a cut point of segment 1's
//! hand-off suffix, and the laws measure segment 2 and the process terminal:
//!
//! - the cut points are the journaled boundaries of the suffix — the handover
//!   step, the successor send, the cancel-forward step and the retire step;
//! - the deployment events are N+1 registering before the cut (from segment
//!   1's own runner), at the cut (from inside the store call nearest it,
//!   while the step is still open), and at the cut with build N crashing
//!   there, so the double replays segment 1 on N over its journal;
//! - "exactly once" is counted on the runner's own log of segment entries,
//!   the double's invocation table (one `complete_terminal`, one successor
//!   run), and the registry's terminal.

use super::*;
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{
    AttemptDispatch, CrashPoint, CrashRule, DeploymentHooks, DeploymentId, RestateTestServer,
    ServerConfig,
};

use crate::process::{
    RestateProcessAwaitRequest, RestateProcessCancelRequest, RestateProcessWorkflowInput,
    RestateProcessWorkflowPayload,
};
use crate::process_attach::RestateProcessAttachRequest;

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The segment count of the laws' process: 0 and 1 hand over, 2 ends it.
const SEGMENTS: u64 = 3;
/// The segment whose hand-off the cuts are taken in: it runs the whole
/// suffix, the retire step included (segment 0 retires nothing).
const HANDING_OVER: u64 = 1;
/// Its successor, which the laws count.
const SUCCESSOR: u64 = 2;

const PROCESS_WORKFLOW: &str = "LashProcessWorkflow";
const PROGRAM: &str = "blake3:handoff-program";
const NEXT_PROGRAM: &str = "blake3:handoff-program-next";

fn generation(build: &'static str) -> lash_core::engine::BuildGeneration {
    lash_core::engine::BuildGeneration::for_test(build)
}

/// One entry of a build's runner into a segment.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SegmentRun {
    build: &'static str,
    ordinal: u64,
    /// The drain generation the segment's start marker recorded (S1).
    admitted_by: Option<lash_core::engine::BuildGeneration>,
}

type SegmentLog = Arc<Mutex<Vec<SegmentRun>>>;

/// A build's process runner: segments before the last cross a boundary, the
/// last ends the process with the build's name (or, for the cancel law,
/// waits for its cancellation). A one-shot hook may run inside a segment's
/// runner before it returns: the "before the cut" deployment event.
struct BuildRunner {
    build: &'static str,
    executable: lash_core::ExecutableGeneration,
    ends_on_cancel: bool,
    log: SegmentLog,
    hooks: Mutex<HashMap<u64, BoxFuture>>,
}

impl BuildRunner {
    fn new(build: &'static str, program: &str, ends_on_cancel: bool, log: SegmentLog) -> Self {
        Self {
            build,
            executable: lash_core::ExecutableGeneration::new(program),
            ends_on_cancel,
            log,
            hooks: Mutex::default(),
        }
    }

    fn on_segment(&self, ordinal: u64, hook: BoxFuture) {
        self.hooks.lock_recover().insert(ordinal, hook);
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for BuildRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        Some(self.executable.clone())
    }

    async fn run_process_segment(
        &self,
        started: &crate::SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let ordinal = started.segment_ordinal();
        self.log.lock_recover().push(SegmentRun {
            build: self.build,
            ordinal,
            admitted_by: started.build_generation().cloned(),
        });
        let hook = self.hooks.lock_recover().remove(&ordinal);
        if let Some(hook) = hook {
            hook.await;
        }
        if ordinal + 1 < SEGMENTS {
            return Ok(lash_core::ProcessRunOutcome::SegmentBoundary(
                lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: PROGRAM.to_string(),
                    engine_state: vec![u8::try_from(ordinal).unwrap_or(u8::MAX)],
                },
            ));
        }
        if self.ends_on_cancel {
            cancellation.cancelled().await;
            return Ok(process_cancellation("cancelled across the hand-off", None).into());
        }
        Ok(process_success(serde_json::json!({ "build": self.build })).into())
    }
}

/// Where a gate holds build N's continuation store: the store call nearest a
/// cut, before or after it reaches the wrapped store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GatePoint {
    BeforeHandoverPut(u64),
    AfterHandoverPut(u64),
    BeforeRetire(u64),
    AfterRetire(u64),
}

/// Build N's continuation store: the shared store, with a one-shot gate that
/// runs a test future inside the step whose store call reaches it — while
/// the step is open and its result unjournaled.
pub(super) struct GatedContinuations {
    inner: Arc<dyn lash_core::ProcessContinuationStore>,
    gate: Mutex<Option<(GatePoint, BoxFuture)>>,
}

impl GatedContinuations {
    pub(super) fn new(inner: Arc<dyn lash_core::ProcessContinuationStore>) -> Self {
        Self {
            inner,
            gate: Mutex::default(),
        }
    }

    pub(super) fn is_armed(&self) -> bool {
        self.gate.lock_recover().is_some()
    }

    pub(super) fn arm(&self, point: GatePoint, future: BoxFuture) {
        *self.gate.lock_recover() = Some((point, future));
    }

    async fn pass(&self, point: GatePoint) {
        let future = {
            let mut gate = self.gate.lock_recover();
            match gate.as_ref() {
                Some((armed, _)) if *armed == point => gate.take().map(|(_, future)| future),
                _ => None,
            }
        };
        if let Some(future) = future {
            future.await;
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessContinuationStore for GatedContinuations {
    async fn put_segment_handover(
        &self,
        process_id: &ProcessId,
        handover: lash_core::PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        let ordinal = handover.segment_ordinal;
        self.pass(GatePoint::BeforeHandoverPut(ordinal)).await;
        let put = self.inner.put_segment_handover(process_id, handover).await;
        self.pass(GatePoint::AfterHandoverPut(ordinal)).await;
        put
    }

    async fn get_segment_handover(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<lash_core::PersistedSegmentHandover>, PluginError> {
        self.inner
            .get_segment_handover(process_id, segment_ordinal)
            .await
    }

    async fn latest_segment_handover(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_core::PersistedSegmentHandover>, PluginError> {
        self.inner.latest_segment_handover(process_id).await
    }

    async fn retire_segment_handovers_through(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<(), PluginError> {
        self.pass(GatePoint::BeforeRetire(segment_ordinal)).await;
        let retired = self
            .inner
            .retire_segment_handovers_through(process_id, segment_ordinal)
            .await;
        self.pass(GatePoint::AfterRetire(segment_ordinal)).await;
        retired
    }

    async fn delete_segment_handovers(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        self.inner.delete_segment_handovers(process_id).await
    }

    async fn segment_start(
        &self,
        segment: &lash_core::ProcessSegmentKey,
    ) -> Result<Option<lash_core::SegmentStartMarker>, PluginError> {
        self.inner.segment_start(segment).await
    }

    async fn mark_segment_started(
        &self,
        segment: &lash_core::ProcessSegmentKey,
        marker: lash_core::SegmentStartMarker,
    ) -> Result<lash_core::SegmentStartMarker, PluginError> {
        self.inner.mark_segment_started(segment, marker).await
    }
}

/// A cut point of the hand-off suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    Handover,
    Send,
    CancelForward,
    Retire,
}

impl Cut {
    const ALL: [Self; 4] = [
        Self::Handover,
        Self::Send,
        Self::CancelForward,
        Self::Retire,
    ];

    /// The store call nearest the cut, where a gate holds the open step.
    fn gate(self) -> GatePoint {
        match self {
            Self::Handover => GatePoint::BeforeHandoverPut(SUCCESSOR),
            Self::Send => GatePoint::AfterHandoverPut(SUCCESSOR),
            Self::CancelForward => GatePoint::BeforeRetire(HANDING_OVER),
            Self::Retire => GatePoint::AfterRetire(HANDING_OVER),
        }
    }

    /// Build N crashing at the cut, on segment 1's first attempt.
    fn crash(self, key: &str) -> CrashRule {
        let point = match self {
            Self::Handover => CrashPoint::BeforeRunResult {
                name: Some("lash.segment.handover".to_string()),
            },
            Self::Send => CrashPoint::BeforeFrame {
                ty: MessageType::OneWayCallCommand,
            },
            Self::CancelForward => CrashPoint::BeforeRunResult {
                name: Some("lash.segment.cancel-forward".to_string()),
            },
            Self::Retire => CrashPoint::BeforeRunResult {
                name: Some("lash.segment.retire".to_string()),
            },
        };
        CrashRule::new(point)
            .service(PROCESS_WORKFLOW)
            .handler("run")
            .key(key)
            .within_attempts(1)
    }

    /// Whether the successor is sent after this cut: a build registered at
    /// the cut is then the newest when the send is submitted.
    fn before_send(self) -> bool {
        matches!(self, Self::Handover | Self::Send)
    }
}

/// When build N+1 registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// From segment 1's runner, before any cut of its hand-off.
    Before,
    /// Inside the open step at the cut.
    AtCut,
    /// Inside the open step at the cut, with build N crashing at the cut.
    CrashAtCut,
}

impl Event {
    const ALL: [Self; 3] = [Self::Before, Self::AtCut, Self::CrashAtCut];
}

/// The two builds over one server and one store set.
struct Roll {
    server: RestateTestServer,
    connection: RestateConnection,
    ingress: RestateIngressClient,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    gated: Arc<GatedContinuations>,
    runner_n: Arc<BuildRunner>,
    log: SegmentLog,
    endpoint_next: Arc<Mutex<Option<Endpoint>>>,
    deployment_n: DeploymentId,
    deployment_next: Arc<Mutex<Option<DeploymentId>>>,
    served: Arc<Mutex<Vec<(String, AttemptDispatch)>>>,
}

impl Roll {
    /// Build N registered and serving; build N+1 built, not yet registered.
    /// N+1 runs `next_program`: the process's own program lets it take the
    /// next segment, another one makes it refuse it.
    async fn start(seed: u64, next_program: &str, ends_on_cancel: bool) -> Self {
        let server = RestateTestServer::new(ServerConfig::default().with_seed(seed))
            .expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let ingress = RestateIngressClient::new(connection.clone());
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open the shared SQLite memory store set");
        let registry = stores.process_registry();
        let continuations: Arc<dyn lash_core::ProcessContinuationStore> = registry.clone();
        let registry: Arc<dyn ProcessRegistry> = registry;
        let sessions = stores.session_store_factory() as Arc<dyn lash_core::DeploymentStore>;
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        let log = SegmentLog::default();
        let gated = Arc::new(GatedContinuations::new(Arc::clone(&continuations)));
        let runner_n = Arc::new(BuildRunner::new(
            "N",
            PROGRAM,
            ends_on_cancel,
            Arc::clone(&log),
        ));
        let runner_next = Arc::new(BuildRunner::new(
            "N+1",
            next_program,
            ends_on_cancel,
            Arc::clone(&log),
        ));
        let endpoint = |runner: Arc<BuildRunner>,
                        continuations: Arc<dyn lash_core::ProcessContinuationStore>,
                        build: &'static str| {
            crate::services::bind_lash_services(
                Endpoint::builder(),
                crate::services::LashServiceParts {
                    effect_host: &host,
                    ingress: ingress.clone(),
                    sessions: Arc::clone(&sessions),
                    process_workflow: LashProcessWorkflowImpl::new(
                        runner,
                        Arc::clone(&registry),
                        continuations,
                        ingress.clone(),
                        test_restate_authority_id(),
                        generation(build),
                        &crate::services::DEFAULT_NAMESPACE,
                    ),
                    session_driver: crate::RestateSessionDriverSlot::new(),
                    build_generation: generation(build),
                    namespace: crate::RestateNamespace::default(),
                    fleet: crate::object_state::FleetView::default(),
                },
            )
            .build()
        };
        let endpoint_n = endpoint(
            Arc::clone(&runner_n),
            Arc::clone(&gated) as Arc<dyn lash_core::ProcessContinuationStore>,
            "N",
        );
        let endpoint_next = endpoint(runner_next, Arc::clone(&continuations), "N+1");
        let served: Arc<Mutex<Vec<(String, AttemptDispatch)>>> = Arc::default();
        let deployment_n = server
            .register_with(endpoint_n, "build-N", Self::recording("N", &served))
            .await
            .expect("register build N");
        Self {
            server,
            connection,
            ingress,
            registry,
            continuations,
            gated,
            runner_n,
            log,
            endpoint_next: Arc::new(Mutex::new(Some(endpoint_next))),
            deployment_n,
            deployment_next: Arc::default(),
            served,
        }
    }

    fn recording(
        build: &'static str,
        served: &Arc<Mutex<Vec<(String, AttemptDispatch)>>>,
    ) -> DeploymentHooks {
        let served = Arc::clone(served);
        DeploymentHooks {
            served: Some(Arc::new(move |dispatch: &AttemptDispatch| {
                served
                    .lock_recover()
                    .push((build.to_string(), dispatch.clone()));
            })),
            refuse: None,
        }
    }

    /// The future that registers build N+1, once.
    fn register_next(&self) -> BoxFuture {
        let server = self.server.clone();
        let endpoint = Arc::clone(&self.endpoint_next);
        let deployment = Arc::clone(&self.deployment_next);
        let served = Arc::clone(&self.served);
        Box::pin(async move {
            let Some(endpoint) = endpoint.lock_recover().take() else {
                return;
            };
            let id = server
                .register_with(endpoint, "build-N+1", Self::recording("N+1", &served))
                .await
                .expect("register build N+1");
            *deployment.lock_recover() = Some(id);
        })
    }

    fn deployment_next(&self) -> DeploymentId {
        self.deployment_next
            .lock_recover()
            .clone()
            .expect("build N+1 registered")
    }

    /// Register a fresh process: its awaiters arm before its segment 0 is
    /// sent, so they are pinned to the build newest then.
    async fn register_process(&self) -> ProcessId {
        self.registry
            .register_process(executed_registration())
            .await
            .expect("register the process")
            .id
    }

    /// Send the process's segment 0 to the stable lane, where the newest
    /// build takes it.
    async fn send_segment_zero(&self, process_id: &ProcessId) {
        self.ingress
            .send_lash_workflow(
                PROCESS_WORKFLOW,
                &process_segment_workflow_key(process_id, 0),
                "run",
                &RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration: executed_registration(),
                    execution_context: ProcessExecutionContext::default(),
                    segment_ordinal: 0,
                    sender_generation: Some(generation("N")),
                }),
            )
            .await
            .expect("send segment 0");
    }

    /// Arm an awaiter of the process terminal on the stable root and wait
    /// until the double has admitted it — pinned to the newest build now.
    async fn arm_awaiter(
        &self,
        process_id: &ProcessId,
    ) -> tokio::task::JoinHandle<Result<ProcessAwaitOutput, String>> {
        let ingress = self.ingress.clone();
        let request = RestateProcessAwaitRequest {
            process_id: process_id.clone(),
        };
        let key = process_id.to_string();
        let awaiter = tokio::spawn(async move {
            ingress
                .call_lash_workflow::<_, ProcessAwaitOutput>(
                    PROCESS_WORKFLOW,
                    &key,
                    "await_terminal",
                    &request,
                )
                .await
                .map_err(|error| error.to_string())
        });
        let target = format!("{PROCESS_WORKFLOW}/{process_id}/await_terminal");
        self.wait_for(|roll| roll.invocations_of(&target).len() == 1)
            .await;
        awaiter
    }

    /// Arm a `LashProcessAttach` for the process on the newest build now.
    async fn arm_attach(&self, process_id: &ProcessId) -> String {
        let key = test_restate_await_event_key(
            &ExecutionScope::process(process_id.clone()),
            lash_core::AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
                "handoff-attach",
            )),
        )
        .expect("an attach wait key");
        let workflow_key = crate::process_attach::process_attach_workflow_key(&key);
        self.ingress
            .send_lash_workflow(
                "LashProcessAttach",
                &workflow_key,
                "run",
                &RestateProcessAttachRequest {
                    process_id: process_id.clone(),
                    key,
                },
            )
            .await
            .expect("send the attach");
        let target = format!("LashProcessAttach/{workflow_key}/run");
        self.wait_for(|roll| roll.invocations_of(&target).len() == 1)
            .await;
        target
    }

    fn invocations_of(&self, target: &str) -> Vec<lash_restate_test::InvocationView> {
        self.server
            .invocations()
            .into_iter()
            .filter(|view| view.target == target)
            .collect()
    }

    fn runs_of(&self, ordinal: u64) -> Vec<SegmentRun> {
        self.log
            .lock_recover()
            .iter()
            .filter(|run| run.ordinal == ordinal)
            .cloned()
            .collect()
    }

    /// Wait (bounded) for `done`, letting the double settle between looks.
    async fn wait_for(&self, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the hand-off did not reach its expected state: {:#?}",
                self.server.invocations()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn record(&self, process_id: &ProcessId) -> lash_core::ProcessRecord {
        self.registry
            .get_process(process_id)
            .await
            .expect("read the process")
            .expect("the process exists")
    }

    /// Return once every live attempt on the double is blocked on it.
    async fn settle(&self) {
        self.server.settle().await;
    }

    /// One lost-run pass of the deployment's recovery tick over the shared
    /// stores: it resubmits only a live process whose current segment the
    /// server no longer holds.
    async fn lost_run_pass(&self) -> crate::process::park_reconcile::LostRunPass {
        crate::process::park_reconcile::end_lost_process_runs(
            &crate::RestateAdminClient::new(self.connection.clone()),
            &self.ingress,
            &crate::services::DEFAULT_NAMESPACE,
            &self.registry,
            &self.continuations,
            std::num::NonZeroUsize::new(16).expect("non-zero"),
        )
        .await
        .expect("the lost-run pass")
    }

    /// Every invocation of a generation lane of the process workflow.
    fn generation_lane_invocations(&self) -> Vec<lash_restate_test::InvocationView> {
        let lanes = format!("{PROCESS_WORKFLOW}_g");
        self.server
            .invocations()
            .into_iter()
            .filter(|view| view.target.starts_with(&lanes))
            .collect()
    }
}

/// What one hand-off case expects of its successor.
fn expected_successor(cut: Cut, event: Event) -> &'static str {
    if event == Event::Before || cut.before_send() {
        "N+1"
    } else {
        "N"
    }
}

/// Set up the deployment event for `cut`/`event` on the roll. `at_cut` runs
/// inside the open step, after N+1 registered.
fn schedule_event(roll: &Roll, process_id: &ProcessId, cut: Cut, event: Event, at_cut: BoxFuture) {
    let register = roll.register_next();
    match event {
        Event::Before => {
            roll.runner_n.on_segment(HANDING_OVER, register);
            roll.gated.arm(cut.gate(), at_cut);
        }
        Event::AtCut | Event::CrashAtCut => {
            roll.gated.arm(
                cut.gate(),
                Box::pin(async move {
                    register.await;
                    at_cut.await;
                }),
            );
            if event == Event::CrashAtCut {
                roll.server
                    .crash_on(cut.crash(&process_segment_workflow_key(process_id, HANDING_OVER)));
            }
        }
    }
}

/// L1 + L8 (stable path) for one cut and deployment event.
async fn successor_runs_once_on_the_newest_build(seed: u64, cut: Cut, event: Event) {
    let case = format!("seed {seed} cut {cut:?} event {event:?}");
    let roll = Roll::start(seed, PROGRAM, false).await;
    // The awaiters arm before segment 0 is sent, so both are pinned to N.
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    let attach = roll.arm_attach(&process_id).await;
    schedule_event(&roll, &process_id, cut, event, Box::pin(async {}));
    roll.send_segment_zero(&process_id).await;

    let output = tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .unwrap_or_else(|_| panic!("{case}: the awaiter armed on N never got the terminal"))
        .expect("the awaiter task")
        .unwrap_or_else(|error| panic!("{case}: the awaiter failed: {error}"));
    roll.settle().await;

    assert!(
        roll.gated.gate.lock_recover().is_none(),
        "{case}: the hand-off reached the cut's gate"
    );
    assert_eq!(
        roll.server.stats().crashes,
        u64::from(event == Event::CrashAtCut),
        "{case}: build N crashed at the cut only when the case says so"
    );
    let successor = expected_successor(cut, event);
    let expected = process_success(serde_json::json!({ "build": successor }));
    assert_eq!(output, expected, "{case}: the awaiter's one terminal");
    assert_eq!(
        roll.record(&process_id).await.outcome,
        Some(expected),
        "{case}: the process terminal"
    );

    // Segments 0 and 1 ran on N only (replays of 1 included); segment 2
    // started exactly once, on the build newest at the send, and its start
    // marker names that build's generation.
    for ordinal in [0, HANDING_OVER] {
        assert!(
            roll.runs_of(ordinal).iter().all(|run| run.build == "N"),
            "{case}: segment {ordinal} ran only on N: {:?}",
            roll.runs_of(ordinal)
        );
    }
    assert_eq!(
        roll.runs_of(SUCCESSOR),
        vec![SegmentRun {
            build: successor,
            ordinal: SUCCESSOR,
            admitted_by: Some(generation(successor)),
        }],
        "{case}: the successor starts exactly once, on the newest build"
    );
    let successor_key = process_segment_workflow_key(&process_id, SUCCESSOR);
    let successor_runs = roll.invocations_of(&format!("{PROCESS_WORKFLOW}/{successor_key}/run"));
    assert_eq!(successor_runs.len(), 1, "{case}: one successor invocation");
    let expected_deployment = if successor == "N" {
        roll.deployment_n.clone()
    } else {
        roll.deployment_next()
    };
    assert_eq!(
        successor_runs[0].pinned_deployment_id,
        expected_deployment.as_str(),
        "{case}: the successor is pinned to the build that ran it"
    );
    let stale_lane = format!("{PROCESS_WORKFLOW}_g");
    assert!(
        roll.server
            .invocations()
            .iter()
            .all(|view| !view.target.starts_with(&stale_lane)),
        "{case}: the stable hand-off addresses no generation lane: {:#?}",
        roll.server.invocations()
    );

    // The terminal is delivered to the stable root once.
    assert_eq!(
        roll.invocations_of(&format!(
            "{PROCESS_WORKFLOW}/{process_id}/complete_terminal"
        ))
        .len(),
        1,
        "{case}: one terminal delivery"
    );
    let handover = roll
        .continuations
        .get_segment_handover(&process_id, SUCCESSOR)
        .await
        .expect("read the successor's handover")
        .expect("the successor's handover is retained until pruning");
    assert_eq!(
        handover.route, PROCESS_WORKFLOW,
        "{case}: the recorded route"
    );
    assert_eq!(
        handover.written_generation,
        Some(generation("N")),
        "{case}: the handover names the build that sent it"
    );

    // L8: the attach armed on N resolved its wait exactly once.
    let attaches = roll.invocations_of(&attach);
    assert_eq!(attaches.len(), 1, "{case}: one attach");
    assert_eq!(attaches[0].status, "completed", "{case}: the attach ended");
    assert!(
        roll.server
            .outcome(&attaches[0].id)
            .is_some_and(|outcome| outcome.is_ok()),
        "{case}: the attach succeeded"
    );
    let resolves = roll
        .server
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with("LashDurableWaitIndex/"))
        .filter(|view| view.target.ends_with("/resolve"))
        .count();
    assert_eq!(resolves, 1, "{case}: the attach resolved its wait once");
}

/// L2 for one cut: a cancel issued inside the open step at the cut.
async fn cancel_reaches_the_live_segments_recorded_route(seed: u64, cut: Cut) {
    let case = format!("seed {seed} cut {cut:?}");
    let roll = Roll::start(seed, PROGRAM, true).await;
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    let cancel = {
        let ingress = roll.ingress.clone();
        let request = RestateProcessCancelRequest::new(
            process_id.clone(),
            lash_core::CancelRequest::new(
                lash_core::CancelOrigin::OperatorRequested,
                "actor:fixture:handoff-cancel",
                7,
            ),
        );
        let key = process_id.to_string();
        Box::pin(async move {
            ingress
                .call_lash_workflow::<_, ()>(PROCESS_WORKFLOW, &key, "cancel", &request)
                .await
                .expect("the cancel is accepted");
        }) as BoxFuture
    };
    schedule_event(&roll, &process_id, cut, Event::Before, cancel);
    roll.send_segment_zero(&process_id).await;
    let output = tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .unwrap_or_else(|_| panic!("{case}: the cancelled process never ended"))
        .expect("the awaiter task")
        .unwrap_or_else(|error| panic!("{case}: the awaiter failed: {error}"));
    roll.settle().await;

    assert_eq!(
        output.terminal_status(),
        Some(lash_core::ProcessStatus::Cancelled),
        "{case}: the terminal is Cancelled: {output:?}"
    );
    let record = roll.record(&process_id).await;
    assert!(
        record.cancel_request.is_some(),
        "{case}: the cancel request is recorded"
    );
    assert_eq!(
        record.outcome,
        Some(output),
        "{case}: one Cancelled terminal"
    );
    assert_eq!(
        roll.invocations_of(&format!("{PROCESS_WORKFLOW}/{process_id}/cancel"))
            .len(),
        1,
        "{case}: one cancel request"
    );
    assert_eq!(
        roll.invocations_of(&format!(
            "{PROCESS_WORKFLOW}/{process_id}/complete_terminal"
        ))
        .len(),
        1,
        "{case}: one terminal delivery"
    );
    // Every cancel delivery addressed the stable lane, the route every
    // segment of this hand-off was sent under, and the successor got one.
    let deliveries: Vec<_> = roll
        .server
        .invocations()
        .into_iter()
        .filter(|view| view.target.ends_with("/deliver_cancel"))
        .collect();
    assert!(
        deliveries
            .iter()
            .all(|view| view.target.starts_with(&format!("{PROCESS_WORKFLOW}/"))),
        "{case}: no cancel is sent to a generation lane: {deliveries:#?}"
    );
    // The cancel reached the segment live when it landed — segment 1 while
    // its handover is unwritten, segment 2 once it is — and a cancel that
    // landed on segment 1 was forwarded to the successor it sent.
    let delivered: std::collections::BTreeSet<String> =
        deliveries.iter().map(|view| view.target.clone()).collect();
    let delivery = |ordinal| {
        format!(
            "{PROCESS_WORKFLOW}/{}/deliver_cancel",
            process_segment_workflow_key(&process_id, ordinal)
        )
    };
    let expected: std::collections::BTreeSet<String> = match cut {
        Cut::Handover => [delivery(HANDING_OVER), delivery(SUCCESSOR)].into(),
        Cut::Send | Cut::CancelForward | Cut::Retire => [delivery(SUCCESSOR)].into(),
    };
    assert_eq!(delivered, expected, "{case}: the cancel's deliveries");
    assert_eq!(
        roll.runs_of(SUCCESSOR),
        vec![SegmentRun {
            build: "N+1",
            ordinal: SUCCESSOR,
            admitted_by: Some(generation("N+1")),
        }],
        "{case}: the successor started once, on the newest build, and ended on the cancel"
    );
}

/// L6 + L8 (generation-lane path): build N+1 cannot run the process's
/// program, so it refuses the successor. The process parks
/// `RetiredGeneration` carrying `G_N` after one attempt with no runner
/// entered; the drain's re-send to `LashProcessWorkflow_g<G_N>` runs the
/// segment once on N and delivers the terminal to the stable root once.
async fn a_refused_successor_parks_for_its_sender_and_reroutes(seed: u64) {
    let case = format!("seed {seed}");
    let roll = Roll::start(seed, NEXT_PROGRAM, false).await;
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    let attach = roll.arm_attach(&process_id).await;
    roll.runner_n.on_segment(HANDING_OVER, roll.register_next());
    roll.send_segment_zero(&process_id).await;

    let successor_key = process_segment_workflow_key(&process_id, SUCCESSOR);
    let stable_successor = format!("{PROCESS_WORKFLOW}/{successor_key}/run");
    roll.wait_for(|roll| {
        roll.invocations_of(&stable_successor)
            .iter()
            .any(|view| view.status == "completed")
    })
    .await;
    roll.settle().await;
    let refused = roll.invocations_of(&stable_successor);
    assert_eq!(refused.len(), 1, "{case}: one stable successor");
    assert_eq!(
        refused[0].pinned_deployment_id,
        roll.deployment_next().as_str(),
        "{case}: the successor went to the newest build"
    );
    assert_eq!(
        refused[0].attempts, 1,
        "{case}: refused on its first attempt"
    );
    assert!(
        roll.server
            .outcome(&refused[0].id)
            .is_some_and(|outcome| outcome.is_err()),
        "{case}: the refusal ends the stable invocation"
    );
    assert!(
        roll.runs_of(SUCCESSOR).is_empty(),
        "{case}: zero dispatch into N+1's runner"
    );
    let park = roll
        .record(&process_id)
        .await
        .park
        .unwrap_or_else(|| panic!("{case}: the refused successor parked its process"));
    assert!(
        matches!(
            park.reason,
            lash_core::store::ParkReason::RetiredGeneration { .. }
        ),
        "{case}: parked RetiredGeneration: {park:?}"
    );
    assert_eq!(
        park.build_generation,
        Some(generation("N")),
        "{case}: the park carries the sender's generation"
    );
    assert_eq!(park.attempts, 1, "{case}: one refusal");
    assert!(
        !awaiter.is_finished(),
        "{case}: the refusal publishes no terminal"
    );

    // The drain's re-send, which FIG-3799 automates: the sender's lane, its
    // own generation stamped.
    let lane = crate::services::DEFAULT_NAMESPACE
        .generation(crate::LashService::ProcessWorkflow, generation("N"));
    roll.ingress
        .send_lash_workflow(
            &lane.name(),
            &successor_key,
            "run",
            &RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                process_id: process_id.clone(),
                registration: executed_registration(),
                execution_context: ProcessExecutionContext::default(),
                segment_ordinal: SUCCESSOR,
                sender_generation: Some(generation("N")),
            }),
        )
        .await
        .expect("re-send the successor to its sender's lane");
    let output = tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .unwrap_or_else(|_| panic!("{case}: the re-routed successor never ended the process"))
        .expect("the awaiter task")
        .unwrap_or_else(|error| panic!("{case}: the awaiter failed: {error}"));
    roll.settle().await;

    let expected = process_success(serde_json::json!({ "build": "N" }));
    assert_eq!(output, expected, "{case}: the awaiter's one terminal");
    let record = roll.record(&process_id).await;
    assert_eq!(
        record.outcome,
        Some(expected),
        "{case}: the process terminal"
    );
    assert_eq!(
        roll.runs_of(SUCCESSOR),
        vec![SegmentRun {
            build: "N",
            ordinal: SUCCESSOR,
            admitted_by: Some(generation("N")),
        }],
        "{case}: the re-routed successor runs once, on N"
    );
    let rerouted = roll.invocations_of(&format!("{}/{successor_key}/run", lane.name()));
    assert_eq!(rerouted.len(), 1, "{case}: one re-routed invocation");
    assert_eq!(
        rerouted[0].pinned_deployment_id,
        roll.deployment_n.as_str(),
        "{case}: only build N serves its lane"
    );
    assert_eq!(
        roll.invocations_of(&format!(
            "{PROCESS_WORKFLOW}/{process_id}/complete_terminal"
        ))
        .len(),
        1,
        "{case}: the terminal is delivered to the stable root once"
    );
    let attaches = roll.invocations_of(&attach);
    assert_eq!(attaches.len(), 1, "{case}: one attach");
    assert!(
        roll.server
            .outcome(&attaches[0].id)
            .is_some_and(|outcome| outcome.is_ok()),
        "{case}: the attach armed on N got the terminal"
    );
}

/// L5 (live stable segment): the recovery tick's lost-run pass after N+1
/// registered finds the run segment 2's recorded key already holds on N and
/// leaves it: one start, no second invocation, no generation lane. Once the
/// process ends, the pass submits nothing.
async fn a_redrive_after_the_roll_addresses_the_recorded_route(seed: u64) {
    let case = format!("seed {seed}");
    let roll = Roll::start(seed, PROGRAM, true).await;
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    // N+1 registers after the send: segment 2 is live on N when it does.
    schedule_event(
        &roll,
        &process_id,
        Cut::Retire,
        Event::AtCut,
        Box::pin(async {}),
    );
    roll.send_segment_zero(&process_id).await;
    let successor_key = process_segment_workflow_key(&process_id, SUCCESSOR);
    let stable_successor = format!("{PROCESS_WORKFLOW}/{successor_key}/run");
    roll.wait_for(|roll| {
        roll.runs_of(SUCCESSOR).len() == 1 && roll.deployment_next.lock_recover().is_some()
    })
    .await;
    let handover = roll
        .continuations
        .latest_segment_handover(&process_id)
        .await
        .expect("read the latest handover")
        .expect("segment 2's handover");
    assert_eq!(
        (handover.segment_ordinal, handover.route.as_str()),
        (SUCCESSOR, PROCESS_WORKFLOW),
        "{case}: the recorded route"
    );

    for _ in 0..2 {
        let pass = roll.lost_run_pass().await;
        assert!(
            pass.resubmitted.is_empty() && pass.ended.is_empty(),
            "{case}: the pass leaves the live segment alone: {pass:?}"
        );
    }
    let live = roll.invocations_of(&stable_successor);
    assert_eq!(
        live.len(),
        1,
        "{case}: the redrive coalesced onto the live run"
    );
    assert_eq!(
        live[0].pinned_deployment_id,
        roll.deployment_n.as_str(),
        "{case}: the live run stays on N"
    );
    assert_eq!(roll.runs_of(SUCCESSOR).len(), 1, "{case}: one start");
    assert!(
        roll.generation_lane_invocations().is_empty(),
        "{case}: the redrive addressed no generation lane"
    );

    // End the process; a later lost-run pass submits nothing.
    roll.ingress
        .call_lash_workflow::<_, ()>(
            PROCESS_WORKFLOW,
            process_id.as_str(),
            "cancel",
            &RestateProcessCancelRequest::new(
                process_id.clone(),
                lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    "actor:fixture:l5",
                    5,
                ),
            ),
        )
        .await
        .expect("cancel the process");
    tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .unwrap_or_else(|_| panic!("{case}: the process never ended"))
        .expect("the awaiter task")
        .unwrap_or_else(|error| panic!("{case}: the awaiter failed: {error}"));
    roll.settle().await;
    let invocations = roll.server.invocations().len();
    roll.lost_run_pass().await;
    roll.settle().await;
    assert_eq!(
        roll.server.invocations().len(),
        invocations,
        "{case}: a lost-run pass after the terminal submits nothing"
    );
    assert_eq!(roll.runs_of(SUCCESSOR).len(), 1, "{case}: still one start");
}

/// L5 (refused successor): while the process is parked for `G_N`, the
/// lost-run pass finds the refused run the recorded key retains and leaves
/// it — no second refusal, no dispatch. After the drain's
/// re-send ran the segment on `_g<G_N>` and ended the process, a forced
/// stable-lane redrive of the segment (its refused run purged) is fenced by
/// admission against the ended process and enters no runner.
async fn a_forced_stable_redrive_after_the_reroute_adds_no_effects(seed: u64) {
    let case = format!("seed {seed}");
    let roll = Roll::start(seed, NEXT_PROGRAM, false).await;
    let process_id = roll.register_process().await;
    let awaiter = roll.arm_awaiter(&process_id).await;
    roll.runner_n.on_segment(HANDING_OVER, roll.register_next());
    roll.send_segment_zero(&process_id).await;
    let successor_key = process_segment_workflow_key(&process_id, SUCCESSOR);
    let stable_successor = format!("{PROCESS_WORKFLOW}/{successor_key}/run");
    roll.wait_for(|roll| {
        roll.invocations_of(&stable_successor)
            .iter()
            .any(|view| view.status == "completed")
    })
    .await;
    roll.settle().await;

    let pass = roll.lost_run_pass().await;
    roll.settle().await;
    assert!(
        pass.resubmitted.is_empty() && pass.ended.is_empty(),
        "{case}: the pass leaves the refused run alone: {pass:?}"
    );
    assert_eq!(
        roll.invocations_of(&stable_successor).len(),
        1,
        "{case}: one refused run"
    );
    assert!(roll.runs_of(SUCCESSOR).is_empty(), "{case}: zero dispatch");
    let park = roll.record(&process_id).await.park.expect("still parked");
    assert_eq!(park.attempts, 1, "{case}: no second refusal");

    let lane = crate::services::DEFAULT_NAMESPACE
        .generation(crate::LashService::ProcessWorkflow, generation("N"));
    let input = |sender: &'static str| {
        RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
            process_id: process_id.clone(),
            registration: executed_registration(),
            execution_context: ProcessExecutionContext::default(),
            segment_ordinal: SUCCESSOR,
            sender_generation: Some(generation(sender)),
        })
    };
    roll.ingress
        .send_lash_workflow(&lane.name(), &successor_key, "run", &input("N"))
        .await
        .expect("the drain's re-send");
    let output = tokio::time::timeout(Duration::from_secs(60), awaiter)
        .await
        .unwrap_or_else(|_| panic!("{case}: the re-routed successor never ended the process"))
        .expect("the awaiter task")
        .unwrap_or_else(|error| panic!("{case}: the awaiter failed: {error}"));
    roll.settle().await;

    let refused = roll.invocations_of(&stable_successor).remove(0);
    assert_eq!(roll.server.purge(&refused.id), Some(true), "{case}: purge");
    roll.ingress
        .send_lash_workflow(PROCESS_WORKFLOW, &successor_key, "run", &input("N"))
        .await
        .expect("the forced stable redrive");
    roll.wait_for(|roll| {
        roll.invocations_of(&stable_successor)
            .iter()
            .any(|view| view.status == "completed")
    })
    .await;
    roll.settle().await;
    assert_eq!(
        roll.runs_of(SUCCESSOR),
        vec![SegmentRun {
            build: "N",
            ordinal: SUCCESSOR,
            admitted_by: Some(generation("N")),
        }],
        "{case}: the forced redrive entered no runner"
    );
    assert_eq!(
        roll.record(&process_id).await.outcome,
        Some(output),
        "{case}: the terminal stands"
    );
}

/// L11: a generation lane refuses an input its sender stamped with another
/// generation, or none, typed and terminally, and journals nothing after the
/// generation sentinel.
async fn a_generation_lane_refuses_a_misrouted_input(seed: u64) {
    for sender in [Some(generation("N+1")), None] {
        let case = format!("seed {seed} sender {sender:?}");
        let roll = Roll::start(seed, PROGRAM, false).await;
        (roll.register_next()).await;
        let process_id = roll.register_process().await;
        let lane = crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::ProcessWorkflow, generation("N"));
        let key = process_segment_workflow_key(&process_id, 1);
        roll.ingress
            .send_lash_workflow(
                &lane.name(),
                &key,
                "run",
                &RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration: executed_registration(),
                    execution_context: ProcessExecutionContext::default(),
                    segment_ordinal: 1,
                    sender_generation: sender.clone(),
                }),
            )
            .await
            .expect("send the misrouted input");
        let target = format!("{}/{key}/run", lane.name());
        roll.wait_for(|roll| {
            roll.invocations_of(&target)
                .iter()
                .any(|view| view.status == "completed")
        })
        .await;
        let view = roll.invocations_of(&target).remove(0);
        assert_eq!(
            view.pinned_deployment_id,
            roll.deployment_n.as_str(),
            "{case}: only build N serves its own lane"
        );
        assert_eq!(view.attempts, 1, "{case}: refused on the first attempt");
        let (_, message) = roll
            .server
            .outcome(&view.id)
            .expect("the refused invocation completed")
            .expect_err("a misroute is refused");
        assert!(
            message.contains("misrouted"),
            "{case}: the refusal is typed: {message}"
        );
        let journal = roll.server.journal(&view.id).expect("the journal");
        let commands: Vec<_> = journal
            .iter()
            .filter(|entry| {
                !matches!(
                    entry.ty,
                    MessageType::InputCommand
                        | MessageType::RunCompletionNotification
                        | MessageType::OutputCommand
                        | MessageType::End
                )
            })
            .map(|entry| (entry.ty, entry.name.clone()))
            .collect();
        assert_eq!(
            commands,
            vec![(
                MessageType::RunCommand,
                Some(crate::sentinel::GENERATION_SENTINEL.to_string())
            )],
            "{case}: nothing is journaled after the sentinel"
        );
        assert!(
            roll.log.lock_recover().is_empty(),
            "{case}: no runner entered"
        );
        let record = roll.record(&process_id).await;
        assert!(
            record.park.is_none() && record.outcome.is_none(),
            "{case}: a misroute is the sender's error, never the process's: {record:?}"
        );
    }
}

/// The run's seed: fixed, so a failure replays; the laws' repeat runs vary
/// the double's concurrent scheduling on top of it.
fn seed() -> u64 {
    0x3795_d000
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l1_the_successor_starts_once_on_the_newest_build_at_every_cut() {
    let seed = seed();
    for cut in Cut::ALL {
        for event in Event::ALL {
            successor_runs_once_on_the_newest_build(seed, cut, event).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l2_a_cancel_at_every_cut_reaches_the_live_segments_route() {
    let seed = seed();
    for cut in Cut::ALL {
        cancel_reaches_the_live_segments_recorded_route(seed, cut).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l6_a_refused_successor_parks_for_its_sender_and_the_reroute_runs_once() {
    a_refused_successor_parks_for_its_sender_and_reroutes(seed()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l11_a_generation_lane_refuses_a_misrouted_input_after_its_sentinel() {
    a_generation_lane_refuses_a_misrouted_input(seed()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l5_a_redrive_after_the_roll_addresses_the_recorded_route() {
    a_redrive_after_the_roll_addresses_the_recorded_route(seed()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l5_a_forced_stable_redrive_after_the_reroute_adds_no_effects() {
    a_forced_stable_redrive_after_the_reroute_adds_no_effects(seed()).await;
}
