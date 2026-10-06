//! Unit laws of the broker over the fake worker and an in-memory snapshot
//! store. The durable laws (`tests/snapshot_matrix.rs`) state the same
//! contracts over the lash store, with a crash at every commit.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_store::effect_opener::EffectOpener;
use lash_vm_protocol::{
    DecodeLimits, EffectOutcome, FrameCodec, FrameEpoch, OwnerEpoch, VmLimits, VmOwner,
};

use super::*;
use crate::authority::{
    self, ArgumentContract, BoundOperation, FrozenBindings, Invocation, OperationRequest,
    OperationRequestCodec, ToolRoute, decode_value, encode_value,
};
use crate::identity::CodeCallIdentities;
use crate::snapshot::{OperationId, SnapshotStore};
use crate::testing::{
    FAKE_VM_CONTRACT, FakeWorkerPool, Fault, MemoryCheckpoints, ScriptedProgram, Step,
    operation_draft,
};

/// The parent: it admits every operation as a `Once` execution but the
/// waits, and records each body it runs with how many quiet points had
/// committed when it ran.
struct Host {
    context: AdmittedContext,
    checkpoints: Arc<MemoryCheckpoints>,
    /// Every body run, by command id, with the quiet points committed then.
    dispatches: Mutex<Vec<(String, usize)>>,
    cancelled: AtomicBool,
    /// Operations whose body leaves them open beyond the activation.
    hand_over: Mutex<BTreeSet<String>>,
    /// Resolves as the runtime adapter does: every decodable request is the
    /// control envelope of its own bytes, whatever its family.
    envelopes: bool,
}

impl Host {
    fn dispatches(&self) -> Vec<String> {
        self.dispatches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(call, _)| call.clone())
            .collect()
    }

    fn commits_at_dispatch(&self) -> Vec<usize> {
        self.dispatches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(_, commits)| *commits)
            .collect()
    }
}

fn operation_name(operation: &AdmittedOperation) -> String {
    match &operation.kind {
        AdmittedKind::Invoke(call) => call.call.operation.clone(),
        AdmittedKind::Aggregate(_) => "aggregate".into(),
        AdmittedKind::Await { .. } => "await".into(),
        AdmittedKind::Sleep { .. } => "sleep".into(),
        // An envelope is named as the family it carries.
        AdmittedKind::Control { kind, payload } => match OperationRequest::decode(payload) {
            Ok(OperationRequest::ResourceOperation(op))
                if *kind == EffectKind::ResourceOperation =>
            {
                op.operation
            }
            Ok(OperationRequest::ResourceOperationBatch(_)) => "aggregate".into(),
            Ok(OperationRequest::Await(_)) => "await".into(),
            Ok(OperationRequest::Sleep(_)) => "sleep".into(),
            _ => "control".into(),
        },
    }
}

#[async_trait::async_trait]
impl ParentEffects for Host {
    fn resolve(
        &self,
        context: &AdmittedContext,
        grants: &BTreeMap<String, crate::HandleGrant>,
        frame: FrameEpoch,
        request: &EffectRequest,
    ) -> Result<authority::ResolvedRequest, AuthorityRefusal> {
        if !self.envelopes {
            return authority::resolve(context, grants, frame, request.kind, &request.payload);
        }
        let decoded = OperationRequest::decode(&request.payload)?;
        if decoded.kind() != request.kind {
            return Err(AuthorityRefusal::KindMismatch {
                kind: request.kind,
                payload: decoded.kind(),
            });
        }
        Ok(authority::ResolvedRequest::Control {
            kind: request.kind,
            payload: request.payload.clone(),
        })
    }

    fn admission(&self, operation: &AdmittedOperation) -> Result<Admission, ParentFault> {
        let name = operation_name(operation);
        let wait = matches!(name.as_str(), "await" | "sleep" | "control");
        Ok(Admission {
            draft: (!wait).then(|| {
                operation_draft(
                    &self.context,
                    operation,
                    &format!("tool:{name}"),
                    lash_sansio::ExecutionPolicy::Once,
                    0,
                )
            }),
            waits: Vec::new(),
        })
    }

    async fn perform(
        &self,
        operation: &AdmittedOperation,
        _waits: &[(
            lash_core_execution::runtime::actor::waits::WaitRef,
            Option<lash_core_execution::runtime::actor::waits::PinnedKey>,
        )],
    ) -> Result<Performed, ParentFault> {
        let name = operation_name(operation);
        if self
            .hand_over
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&name)
        {
            return Ok(Performed::outcome(EffectOutcome::HandedOver));
        }
        self.dispatches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                operation.command_id(&self.context).to_string(),
                self.checkpoints.commits().len(),
            ));
        // A spawn grants the handle its value names.
        let granted = (name == "spawn").then(|| format!("handle-{}", operation.run));
        Ok(Performed {
            outcome: EffectOutcome::Value(encode_value(&serde_json::json!({
                "run": operation.run,
                "calls": operation.call_ids().iter().map(ToString::to_string).collect::<Vec<_>>(),
                "handle": granted,
            }))),
            granted,
        })
    }

    fn interrupted(&self, operation: &AdmittedOperation) -> EffectOutcome {
        EffectOutcome::Failed(encode_value(
            &serde_json::json!({ "interrupted": operation.run }),
        ))
    }

    async fn observe_cancellation(&self, _checkpoint: u64) -> Result<bool, ParentFault> {
        Ok(self.cancelled.load(Ordering::SeqCst))
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
                .bind("tools", "spawn", echo.clone())
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
        fresh: StartState::Fresh,
    }
}

struct Fixture {
    host: Host,
    checkpoints: Arc<MemoryCheckpoints>,
    pool: FakeWorkerPool,
    frames: FrameFence,
}

impl Fixture {
    fn new(slots: usize) -> Self {
        Self::with_envelopes(slots, false)
    }

    fn with_envelopes(slots: usize, envelopes: bool) -> Self {
        let checkpoints = Arc::new(MemoryCheckpoints::default());
        Self {
            host: Host {
                context: context(),
                checkpoints: Arc::clone(&checkpoints),
                dispatches: Mutex::default(),
                cancelled: AtomicBool::new(false),
                hand_over: Mutex::default(),
                envelopes,
            },
            checkpoints,
            pool: FakeWorkerPool::new(codec(), slots, Duration::from_millis(500)),
            frames: FrameFence::new(FrameEpoch(0)),
        }
    }

    fn broker(&self) -> Broker<'_> {
        Broker {
            context: &self.host.context,
            effects: &self.host,
            checkpoints: &*self.checkpoints,
            slots: &self.pool,
            codec: codec(),
            contract: contract(),
            bounds: bounds(),
            frames: self.frames.clone(),
        }
    }

    /// Runs `program` from the execution's latest snapshot, as an activation
    /// that resumes it does.
    async fn run(&self, program: &ScriptedProgram) -> Result<BrokeredEnd, BrokerFailure> {
        let from = self
            .checkpoints
            .latest()
            .await
            .expect("the store reads")
            .map(|(_, checkpoint)| checkpoint);
        self.broker()
            .run(
                RunStart {
                    from,
                    ..start(program)
                },
                &CancellationToken::new(),
            )
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
async fn every_operation_commits_its_admission_with_a_snapshot_before_its_body_runs() {
    let fixture = Fixture::new(1);
    let program = ScriptedProgram::new(vec![echo(1), Step::Compute, echo(2)]);
    let end = fixture.run(&program).await.expect("the run completes");
    let results = results(&end);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["run"], 0);
    assert_eq!(results[1]["run"], 1);
    assert_eq!(
        results[0]["calls"][0],
        fixture.host.context.identities.call_id(0).to_string(),
        "the parent derives the call's identity from its admission"
    );
    assert_eq!(
        fixture.host.commits_at_dispatch(),
        vec![1, 2],
        "each body ran after its own quiet point committed"
    );
    let commits = fixture.checkpoints.commits();
    assert_eq!(
        commits.len(),
        3,
        "a quiet point per operation, then the end"
    );
    for (run, point) in commits.iter().take(2).enumerate() {
        let pending = point
            .checkpoint
            .ledger
            .pending
            .as_ref()
            .expect("the VM stands on its operation");
        assert_eq!(
            pending.operation,
            Some(OperationId {
                run: run as u64,
                ordinal: 1
            }),
            "the snapshot carries the identity its admission minted"
        );
        assert!(point.admit.is_some(), "the admission commits with it");
    }
    assert!(matches!(
        commits[2].checkpoint.end,
        Some(RecordedEnd::Complete { .. })
    ));
    assert_eq!(fixture.checkpoints.settled().len(), 2);
    let stats = fixture.pool.stats();
    assert_eq!(stats.entries, 1, "the program was entered once");
    assert_eq!(stats.checkouts, 3, "each operation released its slot");
}

#[tokio::test]
async fn a_worker_lost_mid_compute_resumes_from_its_last_quiet_point_and_reruns_nothing() {
    let fixture = Fixture::new(1);
    fixture.pool.plan(None);
    fixture.pool.plan(Some(Fault::DieMidCompute));
    let program = ScriptedProgram::new(vec![echo(1), Step::Compute, echo(2)]);
    let failure = fixture.run(&program).await.expect_err("the worker is lost");
    assert!(failure.is_retryable(), "{failure:?}");
    assert!(matches!(
        failure,
        BrokerFailure::WorkerLost {
            outcome: InfrastructureOutcome::WorkerCrashed { .. }
        }
    ));
    assert_eq!(fixture.host.dispatches().len(), 1);
    let end = fixture
        .run(&program)
        .await
        .expect("the resumed run completes");
    assert_eq!(results(&end).len(), 2);
    assert_eq!(
        fixture.host.dispatches().len(),
        2,
        "the completed operation ran once"
    );
    assert_eq!(
        fixture.pool.stats().entries,
        1,
        "the resume entered no program: it continued the snapshot"
    );
}

#[tokio::test]
async fn a_worker_lost_before_its_request_is_admitted_admits_and_runs_nothing() {
    let fixture = Fixture::new(1);
    fixture.pool.plan(Some(Fault::DieAfterRequest(0)));
    let program = ScriptedProgram::new(vec![echo(1)]);
    let failure = fixture.run(&program).await.expect_err("the worker is lost");
    assert!(failure.is_retryable());
    assert!(fixture.checkpoints.commits().is_empty(), "nothing admitted");
    assert!(fixture.host.dispatches().is_empty(), "nothing dispatched");
    let end = fixture.run(&program).await.expect("the rerun completes");
    assert_eq!(results(&end).len(), 1);
    assert_eq!(fixture.host.dispatches().len(), 1);
}

#[tokio::test]
async fn an_outcome_lost_in_delivery_is_injected_again_by_identity_not_redispatched() {
    let fixture = Fixture::new(1);
    fixture.pool.plan(None);
    fixture.pool.plan(Some(Fault::DieBeforeDelivery(0)));
    let program = ScriptedProgram::new(vec![echo(1), echo(2)]);
    fixture
        .run(&program)
        .await
        .expect_err("the worker dies as the outcome arrives");
    assert_eq!(fixture.host.dispatches().len(), 1);
    let end = fixture
        .run(&program)
        .await
        .expect("the resumed run completes");
    let results = results(&end);
    assert_eq!(results[0]["run"], 0, "the saved outcome was fed back");
    assert_eq!(
        fixture.host.dispatches().len(),
        2,
        "the first operation was never dispatched again"
    );
}

#[tokio::test]
async fn a_restored_end_answers_without_starting_the_vm() {
    let fixture = Fixture::new(1);
    let program = ScriptedProgram::new(vec![echo(1)]);
    let first = fixture.run(&program).await.expect("the run completes");
    let starts = fixture.pool.stats().starts;
    let again = fixture.run(&program).await.expect("the end is answered");
    assert_eq!(results(&again), results(&first));
    assert_eq!(fixture.pool.stats().starts, starts, "no worker started");
    assert_eq!(fixture.host.dispatches().len(), 1);
}

#[tokio::test]
async fn an_operation_handed_over_suspends_on_its_quiet_point_and_runs_again_on_restore() {
    let fixture = Fixture::new(1);
    fixture
        .host
        .hand_over
        .lock()
        .expect("hand-over set")
        .insert("sleep".into());
    let program = ScriptedProgram::new(vec![Step::Sleep(5), echo(1)]);
    let end = fixture.run(&program).await.expect("the run suspends");
    let BrokeredEnd::Suspended { checkpoint } = end else {
        panic!("suspended: {end:?}");
    };
    let pending = checkpoint
        .ledger
        .pending
        .expect("the VM stands on the sleep");
    assert_eq!(
        pending.operation, None,
        "a wait is admitted as no execution"
    );
    fixture
        .host
        .hand_over
        .lock()
        .expect("hand-over set")
        .clear();
    let end = fixture
        .run(&program)
        .await
        .expect("the resumed run completes");
    assert_eq!(results(&end).len(), 2);
    assert_eq!(fixture.pool.stats().entries, 1);
}

#[tokio::test]
async fn a_whole_complete_wins_over_the_end_that_follows_it() {
    let fixture = Fixture::new(1);
    fixture.pool.plan(None);
    fixture.pool.plan(Some(Fault::DieAfterComplete));
    let end = fixture
        .run(&ScriptedProgram::new(vec![echo(1)]))
        .await
        .expect("the complete wins");
    assert!(matches!(end, BrokeredEnd::Complete { .. }));
    assert_eq!(fixture.checkpoints.commits().len(), 2, "committed once");
}

#[tokio::test]
async fn an_effect_free_run_commits_nothing_and_runs_again_from_its_start() {
    let fixture = Fixture::new(1);
    let program = ScriptedProgram::new(vec![Step::Compute]);
    let end = fixture.run(&program).await.expect("the run completes");
    assert!(matches!(end, BrokeredEnd::Complete { .. }));
    assert!(
        fixture.checkpoints.commits().is_empty(),
        "no snapshot, so no end over it"
    );
    fixture
        .run(&program)
        .await
        .expect("the run completes again");
    assert_eq!(
        fixture.pool.stats().entries,
        2,
        "the effect-free stretch runs again from its start"
    );
}

#[tokio::test]
async fn a_partial_frame_is_refused_and_the_last_checkpoint_stands() {
    let fixture = Fixture::new(1);
    let program = ScriptedProgram::new(vec![echo(1), Step::Boundary, echo(2)]);
    let BrokeredEnd::Suspended { checkpoint } = fixture.run(&program).await.expect("parks") else {
        panic!("the run parks at its boundary");
    };
    let committed = fixture.checkpoints.commits().len();
    fixture.pool.plan(Some(Fault::DieMidSerialization));
    let failure = fixture
        .run(&program)
        .await
        .expect_err("the cut-off frame loses the worker");
    assert!(failure.is_retryable());
    let commits = fixture.checkpoints.commits();
    assert_eq!(
        commits.len(),
        committed,
        "nothing committed from a partial frame"
    );
    assert_eq!(
        fixture
            .checkpoints
            .latest()
            .await
            .expect("reads")
            .map(|(_, c)| c),
        Some(checkpoint)
    );
    let end = fixture
        .run(&program)
        .await
        .expect("the resume from the last checkpoint completes");
    assert_eq!(results(&end).len(), 2);
    assert_eq!(
        fixture.host.dispatches().len(),
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
                }
            ),
            "{fault:?}: {failure:?}"
        );
        assert!(
            fixture.host.dispatches().is_empty(),
            "{fault:?}: a refused message dispatches nothing"
        );
    }
}

/// A run that cannot be captured where it issues an operation has no
/// snapshot to admit it with: the operation is refused to the guest, typed,
/// whatever its family, and nothing is admitted or dispatched for it.
#[tokio::test]
async fn a_declined_park_refuses_the_operation_without_admitting_it() {
    let compile = || Invocation {
        binding: "tools".into(),
        operation: "compile".into(),
        arguments: serde_json::json!({ "source": "x" }),
    };
    let wait_signal = Step::Raw {
        kind: EffectKind::WaitSignal,
        payload: OperationRequest::WaitSignal {
            name: "go".into(),
            call_site: None,
        }
        .encode()
        .0,
    };
    let families = [
        ("resource operation", Step::Invoke(compile())),
        (
            "resource operation batch",
            Step::Aggregate(vec![compile(), compile()]),
        ),
        ("sleep", Step::Sleep(5)),
        ("signal wait", wait_signal),
    ];
    for envelopes in [false, true] {
        for (family, step) in &families {
            let case = format!("{family}, envelopes: {envelopes}");
            let fixture = Fixture::with_envelopes(1, envelopes);
            fixture.pool.plan(Some(Fault::DeclinePark));
            let end = fixture
                .run(&ScriptedProgram::new(vec![step.clone()]))
                .await
                .unwrap_or_else(|failure| panic!("{case}: {failure}"));
            let results = results(&end);
            assert_eq!(
                results[0]["failed"]["refusal"]["refusal"], "not_capturable",
                "{case}: {results:?}"
            );
            assert!(fixture.host.dispatches().is_empty(), "{case}");
            assert!(
                fixture.checkpoints.commits().is_empty(),
                "{case}: nothing admitted, so no snapshot and no end over one"
            );
            assert_eq!(fixture.pool.stats().checkouts, 1, "{case}");
        }
    }
}

#[tokio::test]
async fn an_unobserved_stop_interrupts_and_an_observed_one_cancels() {
    let fixture = Fixture::new(1);
    let stop = CancellationToken::new();
    stop.cancel();
    let failure = fixture
        .broker()
        .run(start(&ScriptedProgram::new(vec![Step::Hang])), &stop)
        .await
        .expect_err("a stop the parent never observed decides nothing");
    assert!(matches!(failure, BrokerFailure::Interrupted), "{failure:?}");
    fixture.host.cancelled.store(true, Ordering::SeqCst);
    let end = fixture
        .broker()
        .run(
            start(&ScriptedProgram::new(vec![Step::Checkpoint(1), echo(1)])),
            &CancellationToken::new(),
        )
        .await
        .expect("the observation decides");
    assert_eq!(end, BrokeredEnd::Cancelled);
    assert!(fixture.host.dispatches().is_empty());
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
        .open_frame(FrameEpoch(1), &*fixture.checkpoints, Duration::from_secs(1))
        .await
        .expect("the frame opens");
    assert_eq!(fixture.checkpoints.latest().await.expect("reads"), None);
    fixture.pool.set_epochs(OwnerEpoch(0), FrameEpoch(1));
    let read = ScriptedProgram::new(vec![Step::ReadGlobal {
        name: "planted".into(),
    }]);
    let end = broker
        .run(start(&read), &CancellationToken::new())
        .await
        .expect("the new frame's run completes");
    assert_eq!(results(&end), vec![serde_json::json!("undefined")]);
}

#[tokio::test]
async fn an_oversized_effect_result_stays_recorded_and_is_a_typed_run_limit() {
    for failed in [false, true] {
        let fixture = Fixture::new(1);
        let payload = encode_value(&serde_json::json!("x".repeat(1024)));
        let size = payload.0.len() as u64;
        let outcome = if failed {
            EffectOutcome::Failed(payload)
        } else {
            EffectOutcome::Value(payload)
        };
        struct Oversized<'a> {
            host: &'a Host,
            outcome: EffectOutcome,
        }
        #[async_trait::async_trait]
        impl ParentEffects for Oversized<'_> {
            fn admission(&self, operation: &AdmittedOperation) -> Result<Admission, ParentFault> {
                self.host.admission(operation)
            }
            async fn perform(
                &self,
                _operation: &AdmittedOperation,
                _waits: &[(
                    lash_core_execution::runtime::actor::waits::WaitRef,
                    Option<lash_core_execution::runtime::actor::waits::PinnedKey>,
                )],
            ) -> Result<Performed, ParentFault> {
                Ok(Performed::outcome(self.outcome.clone()))
            }
            fn interrupted(&self, operation: &AdmittedOperation) -> EffectOutcome {
                self.host.interrupted(operation)
            }
            async fn observe_cancellation(&self, _checkpoint: u64) -> Result<bool, ParentFault> {
                Ok(false)
            }
        }
        let effects = Oversized {
            host: &fixture.host,
            outcome,
        };
        let mut broker = fixture.broker();
        broker.effects = &effects;
        broker.bounds.protocol.max_effect_value_bytes = 512;
        let failure = broker
            .run(
                start(&ScriptedProgram::new(vec![echo(1)])),
                &CancellationToken::new(),
            )
            .await
            .expect_err("an oversized result ends the run");
        assert!(!failure.is_retryable());
        let BrokerFailure::WorkerLost { outcome } = failure else {
            panic!("typed run limit")
        };
        assert_eq!(
            serde_json::to_value(&outcome).expect("cause"),
            serde_json::json!({
                "worker_limit_exceeded": { "limit": { "effect_value": { "size": size, "bound": 512 } } }
            })
        );
        assert_eq!(
            fixture.checkpoints.settled().len(),
            1,
            "the outcome stays recorded"
        );
    }
}
