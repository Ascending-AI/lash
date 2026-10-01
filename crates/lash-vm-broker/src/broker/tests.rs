//! Unit laws of the broker over the fake worker and an in-memory journal.
//! The tier laws (lash-conformance `vm_broker`) state the same contracts over
//! every engine's real journal.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_store::effect_opener::EffectOpener;
use lash_vm_protocol::{
    DecodeLimits, EffectOutcome, FrameCodec, FrameEpoch, OwnerEpoch, VmLimits, VmOwner,
};

use super::*;
use crate::authority::{
    ArgumentContract, BoundOperation, FrozenBindings, Invocation, ToolRoute, decode_value,
    encode_value,
};
use crate::identity::CodeCallIdentities;
use crate::testing::{
    FAKE_VM_CONTRACT, FakeWorkerPool, Fault, MemoryCheckpoints, ScriptedProgram, Step,
};

/// An in-memory journal: first write wins, and a recorded outcome is served
/// on every later perform of the same command.
#[derive(Default)]
struct Journal {
    outcomes: Mutex<BTreeMap<String, Performed>>,
    retained: Mutex<BTreeMap<String, RequestFingerprint>>,
    observations: Mutex<BTreeMap<u64, bool>>,
    /// Every dispatch, by call id.
    dispatches: Mutex<Vec<String>>,
    cancelled: AtomicBool,
    /// Operations whose dispatch never ends.
    stuck: BTreeSet<String>,
    needs_worker: BTreeSet<String>,
    context: Option<AdmittedContext>,
}

impl Journal {
    fn dispatches(&self) -> Vec<String> {
        self.dispatches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn operation_name(operation: &AdmittedOperation) -> String {
    match &operation.kind {
        AdmittedKind::Invoke(call) => call.call.operation.clone(),
        AdmittedKind::Aggregate(_) => "aggregate".into(),
        AdmittedKind::Await { .. } => "await".into(),
        AdmittedKind::Sleep { .. } => "sleep".into(),
        AdmittedKind::Control { .. } => "control".into(),
    }
}

#[async_trait::async_trait]
impl ParentEffects for Journal {
    async fn retain(
        &self,
        operation: &AdmittedOperation,
    ) -> Result<RequestFingerprint, ParentFault> {
        let key = format!("{}", operation.ordinal);
        Ok(*self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_insert(operation.fingerprint))
    }

    async fn perform(&self, operation: &AdmittedOperation) -> Result<Performed, ParentFault> {
        let key = format!("{}", operation.ordinal);
        if let Some(recorded) = self
            .outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(recorded.clone());
        }
        let name = operation_name(operation);
        for call_id in operation.call_ids() {
            self.dispatches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(call_id.to_string());
        }
        if self.stuck.contains(&name) {
            std::future::pending::<()>().await;
        }
        let _ = &self.context;
        let performed =
            Performed::outcome(EffectOutcome::Value(encode_value(&serde_json::json!({
                "ordinal": operation.ordinal,
                "calls": operation.call_ids().iter().map(ToString::to_string).collect::<Vec<_>>(),
            }))));
        self.outcomes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, performed.clone());
        Ok(performed)
    }

    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
        let live = self.cancelled.load(Ordering::SeqCst);
        Ok(*self
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(checkpoint)
            .or_insert(live))
    }

    fn needs_worker(&self, operation: &AdmittedOperation) -> bool {
        self.needs_worker.contains(&operation_name(operation))
    }
}

fn codec() -> FrameCodec {
    FrameCodec::new(DecodeLimits::standard())
}

fn context() -> AdmittedContext {
    let echo = BoundOperation {
        tool: ToolRoute {
            tool_id: "tool:echo".into(),
            tool_name: "echo".into(),
        },
        arguments: ArgumentContract::Any,
    };
    AdmittedContext {
        owner: VmOwner::new("turn:unit"),
        owner_epoch: OwnerEpoch(0),
        identities: CodeCallIdentities::cell(EffectOpener::turn("session-u", "turn-u"), "cell-u"),
        bindings: Arc::new(
            FrozenBindings::new()
                .bind("tools", "echo", echo.clone())
                .bind("tools", "compile", echo),
        ),
    }
}

fn echo(value: i64) -> Step {
    Step::Invoke(Invocation {
        binding: "tools".into(),
        operation: "echo".into(),
        arguments: serde_json::json!({ "value": value }),
    })
}

fn contract() -> lash_vm_protocol::VmContractReads {
    FAKE_VM_CONTRACT.exact_reads()
}

fn bounds() -> BrokerBounds {
    BrokerBounds {
        settle_deadline: Duration::from_millis(200),
        cancel_grace: Duration::from_millis(50),
        ..BrokerBounds::standard()
    }
}

fn start(program: &ScriptedProgram) -> RunStart {
    RunStart {
        program: program.source(),
        contexts: Vec::new(),
        limits: VmLimits {
            instruction_budget: None,
            memory_limit_bytes: None,
            max_frame_depth: 64,
        },
        from: None,
    }
}

struct Fixture {
    context: AdmittedContext,
    journal: Journal,
    checkpoints: MemoryCheckpoints,
    pool: FakeWorkerPool,
    frames: FrameFence,
}

impl Fixture {
    fn new(slots: usize) -> Self {
        Self {
            context: context(),
            journal: Journal::default(),
            checkpoints: MemoryCheckpoints::default(),
            pool: FakeWorkerPool::new(codec(), slots, Duration::from_millis(500)),
            frames: FrameFence::new(FrameEpoch(0)),
        }
    }

    fn broker(&self) -> Broker<'_> {
        Broker {
            context: &self.context,
            effects: &self.journal,
            checkpoints: &self.checkpoints,
            slots: &self.pool,
            codec: codec(),
            contract: contract(),
            bounds: bounds(),
            frames: self.frames.clone(),
        }
    }

    async fn run(&self, program: &ScriptedProgram) -> Result<BrokeredEnd, BrokerFailure> {
        self.broker()
            .run(start(program), &CancellationToken::new())
            .await
    }
}

fn results(end: &BrokeredEnd) -> Vec<serde_json::Value> {
    let BrokeredEnd::Complete { value, .. } = end else {
        panic!("the run completes: {end:?}");
    };
    decode_value(value)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_run_completes_with_parent_issued_ordinals_and_commits_them_with_its_state() {
    let fixture = Fixture::new(1);
    let program = ScriptedProgram::new(vec![echo(1), Step::Compute, echo(2)]);
    let end = fixture.run(&program).await.expect("the run completes");
    let results = results(&end);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["ordinal"], 0);
    assert_eq!(results[1]["ordinal"], 1);
    assert_eq!(
        results[0]["calls"][0],
        fixture.context.identities.call_id(0).to_string(),
        "the parent derives the call's identity from its ordinal"
    );
    let commits = fixture.checkpoints.commits();
    assert_eq!(commits.len(), 1, "one checkpoint, committed once");
    assert_eq!(
        commits[0].ledger.next_ordinal, 2,
        "the ledger matches the state it commits with"
    );
    assert_eq!(fixture.pool.stats().releases, 1);
}

#[tokio::test]
async fn a_worker_lost_after_its_request_settles_the_operation_before_failing_retryably() {
    let fixture = Fixture::new(1);
    fixture.pool.plan(Some(Fault::DieAfterRequest(0)));
    let program = ScriptedProgram::new(vec![echo(1)]);
    let failure = fixture.run(&program).await.expect_err("the worker is lost");
    assert!(failure.is_retryable());
    let BrokerFailure::WorkerLost {
        outcome,
        settlement,
    } = &failure
    else {
        panic!("a lost worker: {failure:?}");
    };
    assert!(matches!(
        outcome,
        InfrastructureOutcome::WorkerCrashed { .. }
    ));
    assert_eq!(
        settlement.settled.len(),
        1,
        "the admitted operation settled"
    );
    assert_eq!(fixture.journal.dispatches().len(), 1);
    assert_eq!(fixture.pool.stats().discards, 1);
    // The substrate re-drives: the recorded operation is served, not run.
    let end = fixture.run(&program).await.expect("the re-drive completes");
    assert_eq!(results(&end).len(), 1);
    assert_eq!(
        fixture.journal.dispatches().len(),
        1,
        "no recorded effect re-executes"
    );
}

#[tokio::test]
async fn an_operation_unsettled_at_the_bound_is_parked_not_redispatched() {
    let mut fixture = Fixture::new(1);
    fixture.journal.stuck = ["echo".to_string()].into();
    fixture.pool.plan(Some(Fault::DieAfterRequest(0)));
    let failure = fixture
        .run(&ScriptedProgram::new(vec![echo(1)]))
        .await
        .expect_err("the worker is lost");
    let settlement = failure.settlement().expect("a settlement");
    assert!(settlement.settled.is_empty());
    assert_eq!(
        settlement.parked.len(),
        1,
        "the unsettled operation is parked"
    );
    assert_eq!(
        settlement.parked[0].call_ids,
        vec![fixture.context.identities.call_id(0)],
        "addressable by the identity it was admitted under"
    );
}

#[tokio::test]
async fn a_whole_complete_wins_over_the_end_that_follows_it() {
    let fixture = Fixture::new(1);
    fixture.pool.plan(Some(Fault::DieAfterComplete));
    let end = fixture
        .run(&ScriptedProgram::new(vec![echo(1)]))
        .await
        .expect("the complete wins");
    assert!(matches!(end, BrokeredEnd::Complete { .. }));
    assert_eq!(fixture.checkpoints.commits().len(), 1, "committed once");
}

#[tokio::test]
async fn a_partial_frame_is_refused_and_the_last_checkpoint_stands() {
    let fixture = Fixture::new(1);
    let program = ScriptedProgram::new(vec![echo(1), Step::Boundary, echo(2)]);
    let BrokeredEnd::Suspended { checkpoint } = fixture.run(&program).await.expect("parks") else {
        panic!("the run parks at its boundary");
    };
    fixture.pool.plan(Some(Fault::DieMidSerialization));
    let resumed = RunStart {
        from: Some(checkpoint.clone()),
        ..start(&program)
    };
    let failure = fixture
        .broker()
        .run(resumed.clone(), &CancellationToken::new())
        .await
        .expect_err("the cut-off frame loses the worker");
    assert!(failure.is_retryable());
    assert_eq!(
        fixture.checkpoints.commits(),
        vec![checkpoint],
        "nothing is committed from a partial frame"
    );
    let end = fixture
        .broker()
        .run(resumed, &CancellationToken::new())
        .await
        .expect("the re-drive from the last checkpoint completes");
    assert_eq!(results(&end).len(), 2);
    assert_eq!(
        fixture.journal.dispatches().len(),
        2,
        "each operation ran once"
    );
}

#[tokio::test]
async fn stale_and_repeated_worker_messages_are_never_applied() {
    for fault in [
        Fault::StaleLease(0),
        Fault::StaleFrameEpoch(0),
        Fault::ReplayedFrame(0),
        Fault::RepeatedRequestId(0),
    ] {
        let fixture = Fixture::new(1);
        fixture.pool.plan(Some(fault.clone()));
        let failure = fixture
            .run(&ScriptedProgram::new(vec![echo(1)]))
            .await
            .expect_err("the broken worker is refused");
        assert!(
            matches!(
                failure,
                BrokerFailure::WorkerLost {
                    outcome: InfrastructureOutcome::ProtocolViolation { .. },
                    ..
                }
            ),
            "{fault:?}: {failure:?}"
        );
        assert!(
            fixture.journal.dispatches().len() <= 1,
            "{fault:?}: a refused message dispatches nothing"
        );
    }
}

#[tokio::test]
async fn a_run_awaiting_work_that_needs_a_worker_parks_releases_its_slot_and_resumes() {
    let mut fixture = Fixture::new(1);
    fixture.journal.needs_worker = ["compile".to_string()].into();
    let program = ScriptedProgram::new(vec![
        echo(1),
        Step::Invoke(Invocation {
            binding: "tools".into(),
            operation: "compile".into(),
            arguments: serde_json::json!({ "source": "x" }),
        }),
        echo(3),
    ]);
    let end = fixture.run(&program).await.expect("the run completes");
    assert_eq!(results(&end).len(), 3);
    let stats = fixture.pool.stats();
    assert_eq!(stats.max_active, 1);
    assert_eq!(
        stats.checkouts, 2,
        "the run parked and resumed on a worker again"
    );
    assert_eq!(
        fixture.journal.dispatches().len(),
        3,
        "the parked operation ran once"
    );
}

/// A run that cannot be captured where it stands declines the park: its
/// decline is acknowledged, it issues the request again, and the operation
/// runs in place, once, on the worker it already holds.
#[tokio::test]
async fn a_declined_park_performs_the_reissued_request_in_place_once() {
    let mut fixture = Fixture::new(2);
    fixture.journal.needs_worker = ["compile".to_string()].into();
    fixture.pool.plan(Some(Fault::DeclinePark));
    let program = ScriptedProgram::new(vec![
        echo(1),
        Step::Invoke(Invocation {
            binding: "tools".into(),
            operation: "compile".into(),
            arguments: serde_json::json!({ "source": "x" }),
        }),
        echo(3),
    ]);
    let end = fixture.run(&program).await.expect("the run completes");
    assert_eq!(results(&end).len(), 3);
    let stats = fixture.pool.stats();
    assert_eq!(stats.checkouts, 1, "a declined park keeps its worker");
    assert_eq!(stats.discards, 0);
    assert_eq!(
        fixture.journal.dispatches().len(),
        3,
        "the reissued operation ran once"
    );
}

#[tokio::test]
async fn an_unobserved_stop_interrupts_and_a_journaled_one_cancels() {
    let fixture = Fixture::new(1);
    let stop = CancellationToken::new();
    stop.cancel();
    let failure = fixture
        .broker()
        .run(start(&ScriptedProgram::new(vec![Step::Hang])), &stop)
        .await
        .expect_err("a stop the journal never observed decides nothing");
    assert!(
        matches!(failure, BrokerFailure::Interrupted { .. }),
        "{failure:?}"
    );
    fixture.journal.cancelled.store(true, Ordering::SeqCst);
    let end = fixture
        .run(&ScriptedProgram::new(vec![Step::Checkpoint(1), echo(1)]))
        .await
        .expect("the journaled observation decides");
    assert_eq!(end, BrokeredEnd::Cancelled);
    assert!(fixture.journal.dispatches().is_empty());
}

#[tokio::test]
async fn a_drifted_request_is_refused_before_dispatch() {
    let fixture = Fixture::new(1);
    fixture
        .run(&ScriptedProgram::new(vec![echo(1)]))
        .await
        .expect("the first run completes");
    // Same ordinal, other content: the redrive's request drifted.
    fixture
        .journal
        .outcomes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let failure = fixture
        .run(&ScriptedProgram::new(vec![echo(2)]))
        .await
        .expect_err("the drifted request is refused");
    assert!(matches!(
        failure,
        BrokerFailure::RetainedRequestDrift { .. }
    ));
    assert!(!failure.is_retryable());
    assert_eq!(
        fixture.journal.dispatches().len(),
        1,
        "the drifted request dispatched nothing"
    );
}

#[tokio::test]
async fn opening_a_frame_retires_the_old_frames_state() {
    let fixture = Fixture::new(1);
    let session = crate::session::VmSession::new(FrameEpoch(0));
    let broker = Broker {
        frames: session.frames().clone(),
        ..fixture.broker()
    };
    let set = ScriptedProgram::new(vec![Step::SetGlobal {
        name: "planted".into(),
        value: serde_json::json!("A"),
    }]);
    broker
        .run(start(&set), &CancellationToken::new())
        .await
        .expect("the first frame's run completes");
    session
        .open_frame(FrameEpoch(1), &fixture.checkpoints, Duration::from_secs(1))
        .await
        .expect("the frame opens");
    assert_eq!(fixture.checkpoints.latest().await.expect("reads"), None);
    fixture.pool.set_epochs(OwnerEpoch(0), FrameEpoch(1));
    let read = ScriptedProgram::new(vec![Step::ReadGlobal {
        name: "planted".into(),
    }]);
    let end = broker
        .run(
            RunStart {
                from: fixture.checkpoints.latest().await.expect("reads"),
                ..start(&read)
            },
            &CancellationToken::new(),
        )
        .await
        .expect("the new frame's run completes");
    assert_eq!(results(&end), vec![serde_json::json!("undefined")]);
}

#[tokio::test]
async fn an_oversized_journaled_effect_result_is_a_typed_run_limit() {
    for needs_worker in [false, true] {
        for failed in [false, true] {
            let mut fixture = Fixture::new(1);
            if needs_worker {
                fixture.journal.needs_worker.insert("echo".into());
            }
            let payload = encode_value(&serde_json::json!("x".repeat(1024)));
            let size = payload.0.len() as u64;
            let outcome = if failed {
                EffectOutcome::Failed(payload)
            } else {
                EffectOutcome::Value(payload)
            };
            fixture
                .journal
                .outcomes
                .lock()
                .expect("journal")
                .insert("0".into(), Performed::outcome(outcome));
            let mut broker = fixture.broker();
            broker.bounds.protocol.max_effect_value_bytes = 512;
            let failure = broker
                .run(
                    start(&ScriptedProgram::new(vec![echo(1)])),
                    &CancellationToken::new(),
                )
                .await
                .expect_err("an oversized result ends the run");
            assert!(!failure.is_retryable());
            let BrokerFailure::WorkerLost {
                outcome,
                settlement,
            } = failure
            else {
                panic!("typed run limit")
            };
            assert_eq!(
                serde_json::to_value(&outcome).expect("cause"),
                serde_json::json!({
                    "worker_limit_exceeded": { "limit": { "effect_value": { "size": size, "bound": 512 } } }
                })
            );
            assert_eq!(settlement.settled.len(), 1, "the effect stays journaled");
            assert_eq!(fixture.journal.outcomes.lock().expect("journal").len(), 1);
            assert!(
                fixture.journal.dispatches().is_empty(),
                "the recorded effect was not dispatched again"
            );
        }
    }
}
