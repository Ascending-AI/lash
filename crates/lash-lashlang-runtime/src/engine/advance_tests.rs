//! Laws of the lashlang engine at its boundary (ADR 0132 §8, §10; D-L6a,
//! FIG-5198): `advance` is a pure fold of events into the state and one
//! action, and `vm_run` runs the VM from the committed snapshot to its next
//! quiet point.

use std::sync::Arc;

use lash_core::{
    EngineAction, EngineEvent, EngineState, EngineStepKind, EngineStepRun, ProcessId,
    SettledOutput, StepName, StepRequest,
};
use lashlang::AggregateConsumer;
use lashlang::testing::ast_builders as b;
use tokio_util::sync::CancellationToken;

use super::advance::{advance, decode_for_tests, state_format};
use super::state::{
    BatchShape, Decision, EncodedOutcome, Injection, IssuedLeaf, IssuedOperation, Phase,
    TIMER_STEP, VM_RUN_STEP, VmRunInput, VmRunOutput,
};
use super::vm_run::{VmRunFault, completed, failed};

fn process() -> ProcessId {
    ProcessId::fixture("engine-law")
}

fn payload() -> serde_json::Value {
    serde_json::json!({"process": "payload"})
}

fn snapshot(tag: &str) -> lash_vm_protocol::OpaqueVmState {
    lash_vm_protocol::OpaqueVmState::seal(
        lash_vm_protocol::VmStateKind::Continuation,
        crate::process::segment_continuation_owner(&process()),
        lashlang::vm_contract_versions(),
        tag.as_bytes().to_vec(),
    )
}

fn step(name: &str, outcome: SettledOutput) -> EngineEvent {
    EngineEvent::StepSettled {
        step: StepName(name.to_owned()),
        outcome,
    }
}

fn parked(tag: &str, issued: IssuedOperation) -> SettledOutput {
    let output = VmRunOutput::Parked {
        program_hash: "program".to_owned(),
        vm: snapshot(tag),
        issued,
    };
    completed(
        &process(),
        serde_json::to_string(&output).expect("encode the vm_run output"),
    )
}

fn ended(outcome: lash_core::ProcessOutcome) -> SettledOutput {
    let output = VmRunOutput::Ended {
        outcome: Box::new(outcome),
    };
    completed(
        &process(),
        serde_json::to_string(&output).expect("encode the vm_run output"),
    )
}

fn tool_output(output: &lash_core::ToolCallOutput) -> SettledOutput {
    completed(
        &process(),
        serde_json::to_string(output).expect("encode the tool output"),
    )
}

fn success(value: serde_json::Value) -> SettledOutput {
    tool_output(&lash_core::ToolCallOutput::success(value))
}

fn rejection() -> SettledOutput {
    tool_output(&lash_core::ToolCallOutput::failure(
        lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::Execution,
            "law_rejection",
            "the tool rejected",
        ),
    ))
}

fn fault() -> SettledOutput {
    failed(&process(), &VmRunFault("the worker was lost".to_owned()))
}

fn tool(name: &str) -> IssuedLeaf {
    IssuedLeaf::Tool {
        language_execution: None,
        tool: lash_core::ToolId::from(name),
        input: serde_json::json!({"leaf": name}),
        site: None,
    }
}

fn settled_value(value: f64) -> IssuedLeaf {
    IssuedLeaf::Settled {
        fulfilled: true,
        outcome: EncodedOutcome::encode(&lashlang::ResourceOperationOutcome::Value(
            lashlang::Value::Number(value),
        ))
        .expect("encode a settled leaf"),
    }
}

fn batch(consumer: AggregateConsumer, leaves: Vec<IssuedLeaf>) -> IssuedOperation {
    IssuedOperation::Leaves {
        batch: Some(BatchShape {
            consumer,
            settled_value_after: None,
        }),
        leaves,
    }
}

/// A process driven by `advance` alone, as the process activation drives it.
struct Driven {
    state: EngineState,
}

impl Driven {
    fn start() -> (Self, EngineAction) {
        let empty = EngineState {
            format: state_format(),
            bytes: Vec::new(),
        };
        let (state, action) =
            advance(empty, EngineEvent::Started { payload: payload() }).expect("a process starts");
        (Self { state }, action)
    }

    fn on(&mut self, event: EngineEvent) -> EngineAction {
        let (state, action) = advance(self.state.clone(), event).expect("the event folds");
        self.state = state;
        action
    }

    fn refuses(&self, event: EngineEvent) {
        assert!(
            advance(self.state.clone(), event).is_err(),
            "the event is refused"
        );
    }

    fn phase(&self) -> Phase {
        decode_for_tests(&self.state).phase
    }

    /// Start, and park on `issued` at the first quiet point.
    fn parked_on(issued: IssuedOperation) -> (Self, EngineAction) {
        let (mut driven, _) = Self::start();
        let action = driven.on(step("vm_run.0", parked("first", issued)));
        (driven, action)
    }
}

/// The one `vm_run` an action asks for, decoded.
fn vm_run(action: &EngineAction) -> (StepName, VmRunInput) {
    let EngineAction::Steps(steps) = action else {
        panic!("expected steps, got {action:?}");
    };
    let runs: Vec<_> = steps
        .iter()
        .filter_map(|request| match request {
            StepRequest::Engine { step, kind, input } if kind.0 == VM_RUN_STEP => Some((
                step.clone(),
                serde_json::from_value(input.clone()).expect("a vm_run input decodes"),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1, "exactly one vm_run: {action:?}");
    runs.into_iter().next().expect("one vm_run")
}

fn step_names(action: &EngineAction) -> Vec<String> {
    let EngineAction::Steps(steps) = action else {
        panic!("expected steps, got {action:?}");
    };
    steps
        .iter()
        .map(|request| request.step().0.clone())
        .collect()
}

fn injected(action: &EngineAction) -> Injection {
    vm_run(action)
        .1
        .inject
        .expect("the vm_run carries an injection")
}

fn failure_code(outcome: &lash_core::ProcessOutcome) -> String {
    match outcome {
        lash_core::ProcessAwaitOutput::Settled { output } => match &output.outcome {
            lash_core::ToolCallOutcome::Failure(failure) => failure.code.clone(),
            other => panic!("a failure terminal, got {other:?}"),
        },
        other => panic!("a settled terminal, got {other:?}"),
    }
}

/// A process starts by running its VM from its entry: there is no snapshot
/// yet, and nothing to inject.
#[test]
fn a_started_process_asks_for_one_vm_run_from_its_entry() {
    let (driven, action) = Driven::start();
    let (name, input) = vm_run(&action);
    assert_eq!(name.0, "vm_run.0");
    assert_eq!(input.payload, payload());
    assert_eq!(input.vm, None);
    assert_eq!(input.program_hash, None);
    assert_eq!(input.inject, None);
    assert!(matches!(driven.phase(), Phase::Running { .. }));
    driven.refuses(EngineEvent::Started { payload: payload() });
}

/// The operation a VM parks on leaves as its own step, admitted with the
/// snapshot that names it; its outcome feeds the next `vm_run` by the
/// operation's number, resuming the snapshot it parked in.
#[test]
fn a_parked_operation_is_one_step_and_its_outcome_feeds_the_next_vm_run() {
    let (mut driven, action) = Driven::parked_on(IssuedOperation::Leaves {
        batch: None,
        leaves: vec![tool("lookup")],
    });
    let EngineAction::Steps(steps) = &action else {
        panic!("expected the tool step, got {action:?}");
    };
    assert!(
        matches!(
            steps.as_slice(),
            [StepRequest::Tool { step, tool, input, .. }]
                if step.0 == "op.0.0"
                    && tool.as_str() == "lookup"
                    && *input == serde_json::json!({"leaf": "lookup"})
        ),
        "{steps:?}"
    );

    let resumed = driven.on(step("op.0.0", success(serde_json::json!(42))));
    let (name, input) = vm_run(&resumed);
    assert_eq!(name.0, "vm_run.1");
    assert_eq!(
        input.vm,
        Some(snapshot("first")),
        "resume the parked snapshot"
    );
    assert_eq!(input.program_hash.as_deref(), Some("program"));
    match input.inject.expect("the outcome is injected") {
        Injection::Leaves {
            operation: 0,
            decision: Decision::Single,
            leaves,
        } => assert_eq!(leaves.len(), 1),
        other => panic!("the lone leaf's outcome, got {other:?}"),
    }

    // The next quiet point is the next operation, under the next number.
    let next = driven.on(step(
        "vm_run.1",
        parked(
            "second",
            IssuedOperation::Leaves {
                batch: None,
                leaves: vec![tool("store")],
            },
        ),
    ));
    assert_eq!(step_names(&next), vec!["op.1.0"]);
}

/// `Once` is honest: a call that started and never settled comes back to
/// the guest as its typed failure, and nothing dispatches it again.
#[test]
fn an_interrupted_call_is_injected_as_its_outcome_and_never_reissued() {
    let (mut driven, _) = Driven::parked_on(IssuedOperation::Leaves {
        batch: None,
        leaves: vec![tool("charge")],
    });
    let resumed = driven.on(step("op.0.0", SettledOutput::Interrupted));
    assert_eq!(step_names(&resumed), vec!["vm_run.1"], "only the VM runs");
    match injected(&resumed) {
        Injection::Leaves { leaves, .. } => assert!(matches!(
            leaves.as_slice(),
            [super::state::Leaf::Step { outcome: Some(outcome), .. }]
                if matches!(**outcome, SettledOutput::Interrupted)
        )),
        other => panic!("the interrupted leaf, got {other:?}"),
    }
}

/// A race is decided by its first settlement; a loser settling later is a
/// step the engine no longer waits on, and changes nothing.
#[test]
fn a_race_is_decided_by_its_first_settlement_and_a_loser_changes_nothing() {
    let (mut driven, action) = Driven::parked_on(batch(
        AggregateConsumer::Race,
        vec![tool("slow"), tool("fast")],
    ));
    assert_eq!(step_names(&action), vec!["op.0.0", "op.0.1"]);

    let decided = driven.on(step("op.0.1", success(serde_json::json!("fast"))));
    assert!(matches!(
        injected(&decided),
        Injection::Leaves {
            decision: Decision::Selected { leaf: 1 },
            ..
        }
    ));
    let state_before = driven.state.clone();
    assert_eq!(
        driven.on(step("op.0.0", success(serde_json::json!("slow")))),
        EngineAction::Idle
    );
    assert_eq!(
        driven.state, state_before,
        "a loser leaves the state as it was"
    );
}

/// `all` waits for every leaf, unless one rejects first; `allSettled` waits
/// for every leaf; `any` resolves on its first fulfilment and is exhausted
/// when every leaf rejects (ADR 0099 §10).
#[test]
fn each_consumer_decides_on_its_own_rule() {
    // all: a fulfilment decides nothing, a rejection decides.
    let (mut all, _) = Driven::parked_on(batch(
        AggregateConsumer::All,
        vec![tool("a"), tool("b"), tool("c")],
    ));
    assert_eq!(
        all.on(step("op.0.0", success(serde_json::json!(1)))),
        EngineAction::Idle
    );
    assert!(matches!(
        injected(&all.on(step("op.0.2", rejection()))),
        Injection::Leaves {
            decision: Decision::Selected { leaf: 2 },
            ..
        }
    ));

    // all, every leaf fulfilled.
    let (mut all_ok, _) =
        Driven::parked_on(batch(AggregateConsumer::All, vec![tool("a"), tool("b")]));
    all_ok.on(step("op.0.1", success(serde_json::json!(2))));
    assert!(matches!(
        injected(&all_ok.on(step("op.0.0", success(serde_json::json!(1))))),
        Injection::Leaves {
            decision: Decision::AllResults,
            ..
        }
    ));

    // allSettled: a rejection decides nothing.
    let (mut settled, _) = Driven::parked_on(batch(
        AggregateConsumer::AllSettled,
        vec![tool("a"), tool("b")],
    ));
    assert_eq!(settled.on(step("op.0.0", rejection())), EngineAction::Idle);
    assert!(matches!(
        injected(&settled.on(step("op.0.1", success(serde_json::json!(1))))),
        Injection::Leaves {
            decision: Decision::AllResults,
            ..
        }
    ));

    // any: rejections until the last are exhausted.
    let (mut any, _) = Driven::parked_on(batch(AggregateConsumer::Any, vec![tool("a"), tool("b")]));
    assert_eq!(any.on(step("op.0.0", rejection())), EngineAction::Idle);
    assert!(matches!(
        injected(&any.on(step("op.0.1", rejection()))),
        Injection::Leaves {
            decision: Decision::ExhaustedRejections,
            ..
        }
    ));
}

/// An aggregate whose plain value decides it at issue answers the VM at
/// once, yet every pending leaf is still admitted with that answer (ADR 0099
/// §11 clause 3); a leaf settled at issue counts first, in leaf order.
#[test]
fn an_aggregate_decided_at_issue_still_admits_its_pending_leaves() {
    let (_, action) = Driven::parked_on(IssuedOperation::Leaves {
        batch: Some(BatchShape {
            consumer: AggregateConsumer::Race,
            settled_value_after: Some(1),
        }),
        leaves: vec![tool("pending")],
    });
    assert_eq!(step_names(&action), vec!["op.0.0", "vm_run.1"]);
    assert!(matches!(
        injected(&action),
        Injection::Leaves {
            decision: Decision::SettledValue,
            ..
        }
    ));

    let (_, action) = Driven::parked_on(batch(
        AggregateConsumer::Race,
        vec![tool("pending"), settled_value(7.0)],
    ));
    assert_eq!(step_names(&action), vec!["op.0.0", "vm_run.1"]);
    assert!(matches!(
        injected(&action),
        Injection::Leaves {
            decision: Decision::Selected { leaf: 1 },
            ..
        }
    ));
}

/// An aggregate's timer leaf is the engine's own `timer` step, settling at
/// its deadline; the timer settles to a fulfilment.
#[test]
fn a_timer_leaf_is_an_engine_timer_step() {
    let (mut driven, action) = Driven::parked_on(batch(
        AggregateConsumer::Race,
        vec![tool("slow"), IssuedLeaf::Timer { until_ms: 900 }],
    ));
    let EngineAction::Steps(steps) = &action else {
        panic!("expected steps, got {action:?}");
    };
    assert!(
        matches!(
            &steps[1],
            StepRequest::Engine { step, kind, input }
                if step.0 == "op.0.1"
                    && *kind == EngineStepKind::new(TIMER_STEP)
                    && *input == serde_json::json!({"until_ms": 900})
        ),
        "{steps:?}"
    );
    assert!(matches!(
        injected(&driven.on(step("op.0.1", completed(&process(), "null".to_owned())))),
        Injection::Leaves {
            decision: Decision::Selected { leaf: 1 },
            ..
        }
    ));
}

/// A sleep is the activation's to keep: the engine answers `Sleep` until
/// `Woke`, and an event it does not wait on re-answers the same deadline.
#[test]
fn a_sleep_stands_until_it_wakes() {
    let (mut driven, action) = Driven::parked_on(IssuedOperation::Sleep { until_ms: 5_000 });
    let sleep = EngineAction::Sleep {
        until: lash_core::durable_port::DurableInstant(5_000),
    };
    assert_eq!(action, sleep);
    assert_eq!(
        driven.on(EngineEvent::ProcessWaitTimedOut { process: process() }),
        sleep
    );
    assert!(matches!(
        injected(&driven.on(EngineEvent::Woke)),
        Injection::Woke { operation: 0 }
    ));
}

/// An await answers `AwaitProcess` until the awaited process ends; another
/// process's end changes nothing.
#[test]
fn an_await_stands_until_its_process_ends() {
    let awaited = ProcessId::fixture("awaited");
    let (mut driven, action) = Driven::parked_on(IssuedOperation::AwaitProcess {
        process: awaited.clone(),
    });
    let standing = EngineAction::AwaitProcess {
        process: awaited.clone(),
        bound: lash_core::ParkBound::UntilScopeEnd,
    };
    assert_eq!(action, standing);
    let outcome = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!("child")),
    );
    assert_eq!(
        driven.on(EngineEvent::ProcessEnded {
            process: ProcessId::fixture("someone-else"),
            outcome: outcome.clone(),
        }),
        standing
    );
    assert!(matches!(
        injected(&driven.on(EngineEvent::ProcessEnded {
            process: awaited,
            outcome,
        })),
        Injection::ProcessEnded { operation: 0, .. }
    ));
}







/// A `vm_run` that reached no quiet point left nothing: the same run is
/// asked again from the same snapshot with the same injection, up to the
/// fault budget, and then the process ends with a typed failure.
#[test]
fn a_faulted_vm_run_is_asked_again_from_the_same_snapshot_within_its_budget() {
    let (mut driven, _) = Driven::parked_on(IssuedOperation::Sleep { until_ms: 10 });
    let first = driven.on(EngineEvent::Woke);
    let (_, first_input) = vm_run(&first);

    let again = driven.on(step("vm_run.1", fault()));
    let (name, input) = vm_run(&again);
    assert_eq!(name.0, "vm_run.2", "a fresh step name");
    assert_eq!(input, first_input, "the same input");
    let again = driven.on(step("vm_run.2", fault()));
    assert_eq!(vm_run(&again).1, first_input);
    let EngineAction::Terminal(outcome) = driven.on(step("vm_run.3", fault())) else {
        panic!("the budget is spent");
    };
    assert_eq!(failure_code(&outcome), "process_segment_resume_failed");
    assert_eq!(driven.phase(), Phase::Ended);
    driven.refuses(EngineEvent::Woke);
}

/// A quiet point resets the fault budget.
#[test]
fn a_quiet_point_resets_the_fault_budget() {
    let (mut driven, _) = Driven::start();
    driven.on(step("vm_run.0", fault()));
    driven.on(step("vm_run.1", fault()));
    driven.on(step(
        "vm_run.2",
        parked("first", IssuedOperation::Sleep { until_ms: 10 }),
    ));
    driven.on(EngineEvent::Woke);
    driven.on(step("vm_run.3", fault()));
    driven.on(step("vm_run.4", fault()));
    assert!(matches!(driven.phase(), Phase::Running { .. }));
}

/// A `vm_run` past its limit ends the process with the bound's failure.
#[test]
fn a_vm_run_past_its_limit_ends_the_process() {
    let (mut driven, _) = Driven::start();
    let EngineAction::Terminal(outcome) = driven.on(step(
        "vm_run.0",
        SettledOutput::TimedOut {
            cause: lash_core::LimitCause::ExecutionTotal,
            evidence: Default::default(),
        },
    )) else {
        panic!("the process ends");
    };
    assert_eq!(failure_code(&outcome), "process_execution_bound_exhausted");
}

/// The VM's end is the process's terminal, as `vm_run` answered it.
#[test]
fn the_vms_end_is_the_processs_terminal() {
    let (mut driven, _) = Driven::start();
    let outcome = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!("done")),
    );
    assert_eq!(
        driven.on(step("vm_run.0", ended(outcome.clone()))),
        EngineAction::Terminal(outcome)
    );
    assert_eq!(driven.phase(), Phase::Ended);
}



/// A lashlang process pins no host key; a pinned or resolved key is a
/// corrupt mailbox, not an event to fold.
#[test]
fn a_host_key_event_is_refused() {
    let (driven, _) = Driven::parked_on(IssuedOperation::Sleep { until_ms: 10 });
    driven.refuses(EngineEvent::ExternalTimedOut {
        name: lash_core::KeyName("key".to_owned()),
    });
}

/// Another engine's state, or another version of this one's, is refused
/// before anything is folded.
#[test]
fn a_state_of_another_format_is_refused() {
    let (driven, _) = Driven::parked_on(IssuedOperation::Sleep { until_ms: 10 });
    let mut foreign = driven.state.clone();
    foreign.format.version += 1;
    assert!(advance(foreign, EngineEvent::Woke).is_err());
}

// ---- vm_run against a real worker ----

const NOW_MS: i64 = 1_000;

struct VmFixture {
    engine: crate::LashlangProcessEngine,
    payload: serde_json::Value,
    settings: serde_json::Value,
}

/// `process worker() -> str { sleep(5); sleep(7); finish "done" }`, published
/// for an engine whose recorded settings the process carries.
async fn vm_fixture() -> VmFixture {
    use lash_core_execution::StoreSet as _;

    let stores = crate::lib_tests::sqlite_memory_store_set().await;
    let artifacts = crate::LashlangArtifacts::new(stores.module_artifacts());
    let bounds = lashlang::ExecutionBounds::new(
        lashlang::ExecutionBound::Unbounded,
        lashlang::ExecutionBound::Unbounded,
    );
    let settings = crate::LashlangRecordedSettings::new(crate::LashlangSurface::default(), bounds);
    let environment = settings
        .clone()
        .into_surface()
        .for_process_registry(true)
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("the host environment");
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process worker() -> str { sleep(5); sleep(7); finish \"done\" }",
        program: b::module(
            vec![b::process_returning(
                "worker",
                Vec::new(),
                lashlang::TypeExpr::Str,
                b::block(vec![
                    b::sleep_for(b::num(5.0)),
                    b::sleep_for(b::num(7.0)),
                    b::finish(b::string("done")),
                ]),
            )],
            Vec::new(),
        ),
        environment: &environment,
    })
    .expect("the process module compiles");
    artifacts
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("the artifact publishes");
    let input = crate::LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: output
            .artifact
            .process_ref("worker")
            .expect("the worker export")
            .clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "worker".to_owned(),
        args: serde_json::Map::new(),
    };
    VmFixture {
        engine: crate::LashlangProcessEngine::new(artifacts, crate::LashlangSurface::default()),
        payload: serde_json::to_value(&input).expect("encode the payload"),
        settings: serde_json::to_value(&settings).expect("encode the settings"),
    }
}

impl VmFixture {
    async fn run(&self, input: VmRunInput) -> VmRunOutput {
        self.run_recorded(input, Some(self.settings.clone())).await
    }

    /// [`Self::run`] on a process whose row recorded `engine_config`.
    async fn run_recorded(
        &self,
        input: VmRunInput,
        engine_config: Option<serde_json::Value>,
    ) -> VmRunOutput {
        let settled = super::vm_run::run_vm_step(
            &self.engine,
            EngineStepRun {
                process: process(),
                engine_config,
                tool_catalog: Arc::new(lash_core::ToolCatalog::default()),
                now: lash_core::durable_port::DurableInstant(NOW_MS),
                clock: Arc::new(lash_core::testing::TestClock::new(0)),
                projection_providers: None,
                kind: EngineStepKind::new(VM_RUN_STEP),
                input: serde_json::to_value(input).expect("encode the input"),
            },
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(settled, SettledOutput::Completed(_)),
            "vm_run completes: {settled:?}"
        );
        serde_json::from_str(settled.payload().expect("a completion carries its output"))
            .expect("the output decodes")
    }

    fn first(&self) -> VmRunInput {
        VmRunInput {
            payload: self.payload.clone(),
            program_hash: None,
            vm: None,
            inject: None,
        }
    }
}

fn parked_sleep(output: VmRunOutput) -> (String, lash_vm_protocol::OpaqueVmState, i64) {
    match output {
        VmRunOutput::Parked {
            program_hash,
            vm,
            issued: IssuedOperation::Sleep { until_ms },
        } => (program_hash, vm, until_ms),
        other => panic!("a VM parked on a sleep, got {other:?}"),
    }
}

/// A first `vm_run` parks at the program's first operation without
/// performing it, and asked again — a crash before its outcome committed —
/// reaches the same quiet point: it is a recomputation of an effect-free
/// stretch of VM.
#[tokio::test(flavor = "current_thread")]
async fn a_first_vm_run_parks_at_the_first_operation_and_repeats_identically() {
    let fixture = vm_fixture().await;
    let (hash, _, until_ms) = parked_sleep(fixture.run(fixture.first()).await);
    assert_eq!(until_ms, NOW_MS + 5);
    let (again_hash, _, again_until) = parked_sleep(fixture.run(fixture.first()).await);
    assert_eq!((again_hash, again_until), (hash, until_ms));
}

/// A resumed VM issues the operation it parked on again, and that reissue
/// is answered from the injection: the run goes on to the next operation
/// rather than parking on the first one a second time, and never runs from
/// the program's entry.
#[tokio::test(flavor = "current_thread")]
async fn a_resumed_vm_run_is_answered_from_its_injection_and_reissues_nothing() {
    let fixture = vm_fixture().await;
    let (hash, vm, _) = parked_sleep(fixture.run(fixture.first()).await);
    let resume = |vm, operation| VmRunInput {
        payload: fixture.payload.clone(),
        program_hash: Some(hash.clone()),
        vm: Some(vm),
        inject: Some(Injection::Woke { operation }),
    };
    let (_, second, until_ms) = parked_sleep(fixture.run(resume(vm, 0)).await);
    assert_eq!(
        until_ms,
        NOW_MS + 7,
        "the second sleep, not the first again"
    );
    match fixture.run(resume(second, 1)).await {
        VmRunOutput::Ended { outcome } => assert!(
            matches!(
                &*outcome,
                lash_core::ProcessAwaitOutput::Settled { output }
                    if **output == lash_core::ToolCallOutput::success(serde_json::json!("done"))
            ),
            "{outcome:?}"
        ),
        other => panic!("the process ends, got {other:?}"),
    }
}



/// A snapshot captured under another program identity is refused before it
/// resumes, naming the identity it recorded.
#[tokio::test(flavor = "current_thread")]
async fn a_snapshot_of_another_program_identity_is_refused_before_it_resumes() {
    let fixture = vm_fixture().await;
    let (_, vm, _) = parked_sleep(fixture.run(fixture.first()).await);
    let output = fixture
        .run(VmRunInput {
            payload: fixture.payload.clone(),
            program_hash: Some("sha256:another-build".to_owned()),
            vm: Some(vm),
            inject: Some(Injection::Woke { operation: 0 }),
        })
        .await;
    let VmRunOutput::Ended { outcome } = output else {
        panic!("the process ends, got {output:?}");
    };
    assert!(
        matches!(
            &*outcome,
            lash_core::ProcessAwaitOutput::Abandoned { evidence, .. }
                if matches!(
                    &evidence.writer,
                    lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::RetiredGeneration { .. }
                    }
                )
        ),
        "{outcome:?}"
    );
}

// ---- vm_run's admission and artifact refusals ----

/// Run one `vm_run` step of `engine` on `input`, under the recorded
/// `settings` and the deployment's `catalog`, and answer how it settled.
async fn vm_step(
    engine: &crate::LashlangProcessEngine,
    settings: &serde_json::Value,
    catalog: lash_core::ToolCatalog,
    input: VmRunInput,
) -> SettledOutput {
    super::vm_run::run_vm_step(
        engine,
        EngineStepRun {
            process: process(),
            engine_config: Some(settings.clone()),
            tool_catalog: Arc::new(catalog),
            now: lash_core::durable_port::DurableInstant(NOW_MS),
            clock: Arc::new(lash_core::testing::TestClock::new(0)),
            projection_providers: None,
            kind: EngineStepKind::new(VM_RUN_STEP),
            input: serde_json::to_value(input).expect("encode the input"),
        },
        CancellationToken::new(),
    )
    .await
}

/// The process outcome a completed `vm_run` ended with.
fn ended_with(settled: &SettledOutput) -> lash_core::ProcessOutcome {
    assert!(
        matches!(settled, SettledOutput::Completed(_)),
        "vm_run completes: {settled:?}"
    );
    let output: VmRunOutput =
        serde_json::from_str(settled.payload().expect("a completion carries its output"))
            .expect("the output decodes");
    match output {
        VmRunOutput::Ended { outcome } => *outcome,
        other => panic!("the process ends before its VM runs, got {other:?}"),
    }
}

/// A first `vm_run` of `input`.
fn first_run(input: &crate::LashlangProcessInput) -> VmRunInput {
    VmRunInput {
        payload: serde_json::to_value(input).expect("encode the payload"),
        program_hash: None,
        vm: None,
        inject: None,
    }
}

/// The fixture echo tool under the `tools` module binding.
fn bound_echo() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    lash_core::testing::fixture_echo_definition()
        .with_tool_binding(lash_core::ToolBinding::new(["tools"], "echo"))
}

/// A start the artifact or the live host does not admit ends at its first
/// `vm_run` with its typed refusal, before the VM runs: a start naming
/// other host requirements than the artifact's, a start naming a process
/// the module does not export, a host whose catalog does not convert, and a
/// host that lacks the surface the artifact requires. The start preparation
/// refuses the two immutable mismatches with the same codes and leaves the
/// live host to the run.
#[tokio::test(flavor = "current_thread")]
async fn a_start_its_artifact_or_host_does_not_admit_ends_with_its_typed_refusal() {
    use lash_core_execution::StoreSet as _;

    let stores = crate::lib_tests::sqlite_memory_store_set().await;
    let artifacts = crate::LashlangArtifacts::new(stores.module_artifacts());
    let bounds = lashlang::ExecutionBounds::new(
        lashlang::ExecutionBound::Unbounded,
        lashlang::ExecutionBound::Unbounded,
    );
    let settings = serde_json::to_value(crate::LashlangRecordedSettings::new(
        crate::LashlangSurface::default(),
        bounds,
    ))
    .expect("encode the settings");
    let echo_catalog = lash_core::ToolCatalog::from_tool_definitions(vec![bound_echo()]);
    let environment = crate::LashlangSurface::default()
        .for_process_registry(true)
        .host_environment(&echo_catalog)
        .expect("the host environment");
    let echo = b::unwrap(b::receiver_call(
        b::resource(&["tools"]),
        "echo",
        vec![b::record(vec![("value", b::string("hi"))])],
    ));
    let output = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source: "process scan() -> any { finish tools.echo({value: 'hi'})? }",
        program: b::module(
            vec![b::process(
                "scan",
                Vec::new(),
                b::block(vec![b::finish(echo)]),
            )],
            Vec::new(),
        ),
        environment: &environment,
    })
    .expect("the module compiles");
    artifacts
        .publish_module_artifact(&crate::lib_tests::host_claim(), &output.artifact)
        .await
        .expect("the artifact publishes");
    let process_ref = output
        .artifact
        .process_ref("scan")
        .expect("the export")
        .clone();
    let input = crate::LashlangProcessInput {
        module_ref: output.module_ref.clone(),
        process_ref: process_ref.clone(),
        host_requirements_ref: output.host_requirements_ref.clone(),
        process_name: "scan".to_owned(),
        args: serde_json::Map::new(),
    };
    let mut requirements_mismatch = input.clone();
    requirements_mismatch.host_requirements_ref =
        lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("other-requirements"));
    let mut process_mismatch = input.clone();
    process_mismatch.process_ref =
        lashlang::ProcessRef::new(lashlang::ContentHash::new("other-process"), 0);
    let mut malformed = bound_echo();
    malformed.manifest.bindings.insert(
        lash_core::TOOL_BINDING_KEY.to_owned(),
        serde_json::json!({ "not": "a tool binding" }),
    );
    let engine =
        crate::LashlangProcessEngine::new(artifacts.clone(), crate::LashlangSurface::default());
    for (start, catalog, code) in [
        (
            &requirements_mismatch,
            echo_catalog.clone(),
            crate::LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
        ),
        (
            &process_mismatch,
            echo_catalog.clone(),
            crate::LashlangProcessFailureCode::ProcessRefMismatch,
        ),
        (
            &input,
            lash_core::ToolCatalog::from_tool_definitions(vec![malformed]),
            crate::LashlangProcessFailureCode::ProcessHostEnvironmentInvalid,
        ),
        (
            &input,
            lash_core::ToolCatalog::default(),
            crate::LashlangProcessFailureCode::ProcessHostEnvironmentIncompatible,
        ),
    ] {
        let settled = vm_step(&engine, &settings, catalog, first_run(start)).await;
        assert_eq!(failure_code(&ended_with(&settled)), code.as_str());
    }
    // The admitted start runs: the VM parks at its tool call.
    let settled = vm_step(&engine, &settings, echo_catalog, first_run(&input)).await;
    assert!(
        matches!(
            serde_json::from_str::<VmRunOutput>(settled.payload().expect("an output")),
            Ok(VmRunOutput::Parked { .. })
        ),
        "the admitted start runs its VM: {settled:?}"
    );

    let prepared = |host_requirements_ref: &lashlang::HostRequirementsRef,
                    process_ref: &lashlang::ProcessRef| lashlang::ProcessStart {
        module_ref: output.module_ref.clone(),
        process_ref: process_ref.clone(),
        host_requirements_ref: host_requirements_ref.clone(),
        start_site: crate::lib_tests::test_start_site("child_process:scan", 1),
        process_name: "scan".to_owned(),
        args: lashlang::Record::new(),
    };
    for (start, code) in [
        (
            prepared(&requirements_mismatch.host_requirements_ref, &process_ref),
            crate::LashlangProcessFailureCode::ProcessHostRequirementsMismatch,
        ),
        (
            prepared(&output.host_requirements_ref, &process_mismatch.process_ref),
            crate::LashlangProcessFailureCode::ProcessRefMismatch,
        ),
    ] {
        let error = crate::prepare_lashlang_process_start(
            &lash_vm_client::service::Service::default(),
            artifacts.clone(),
            Some("parent:admission"),
            start,
            lash_core::ProcessOriginator::host(),
            lash_core::LifetimeDecision::Detached,
        )
        .await
        .expect_err("the preparation refuses an immutable mismatch");
        let crate::LashlangRuntimeError::ProcessAdmission(refusal) = error else {
            panic!("the preparation keeps the typed refusal: {error:?}");
        };
        assert_eq!(refusal.failure_code(), code);
    }
    crate::prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifacts,
        Some("parent:admission"),
        prepared(&output.host_requirements_ref, &process_ref),
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .await
    .expect("the preparation leaves the live host to the run");
}

/// How the fault store answers a read of its one artifact.
#[derive(Clone, Copy, Debug)]
enum ArtifactFault {
    Missing,
    MissingStore,
    CorruptBytes,
    CorruptStore,
    /// Unavailable on the first read, then the bytes.
    Unavailable,
}

/// A module store holding one artifact's bytes behind `fault`, counting its
/// reads. A `vm_run` only reads.
struct FaultArtifacts {
    fault: ArtifactFault,
    bytes: Vec<u8>,
    reads: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::ModuleArtifactStore for FaultArtifacts {
    async fn publish_module_artifact(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &str,
        _: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        panic!("a vm_run publishes no artifact")
    }

    async fn acquire_module_artifact(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &str,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        panic!("a vm_run acquires no artifact")
    }

    async fn end_module_referrer(
        &self,
        _: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        panic!("a vm_run ends no referrer")
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.fault {
            ArtifactFault::Missing => Ok(None),
            ArtifactFault::MissingStore => Err(lash_core::ArtifactStoreError::ArtifactMissing {
                artifact_ref: module_ref.into(),
            }),
            ArtifactFault::CorruptBytes => Ok(Some(b"not an artifact".to_vec())),
            ArtifactFault::CorruptStore => Err(lash_core::ArtifactStoreError::StoredDataCorrupt {
                source: lashlang::ModuleArtifactCorruption::Storage {
                    record_kind: "module artifact".into(),
                    message: "corrupt store row".into(),
                },
            }),
            ArtifactFault::Unavailable if read == 0 => Err(lash_core::ArtifactStoreError::Backend(
                "storage unavailable".into(),
            )),
            ArtifactFault::Unavailable => Ok(Some(self.bytes.clone())),
        }
    }
}

/// A process whose module artifact is gone or corrupt ends at its `vm_run`
/// for good, typed: a missing artifact settles the process failed, and a
/// corrupt one abandons it as a refused resume naming the artifact, never
/// as a retired generation. A store outage is no outcome: the step faults,
/// and asked again once the store answers, it reads the artifact and goes
/// on to the start's admission, here refused, before its VM runs.
#[tokio::test(flavor = "current_thread")]
async fn an_unreadable_artifact_ends_the_process_but_a_store_outage_faults_the_step() {
    let fixture = vm_fixture().await;
    let mut input: crate::LashlangProcessInput =
        serde_json::from_value(fixture.payload.clone()).expect("the fixture's payload");
    let bytes = lash_core::ModuleArtifactStore::get_module_artifact(
        fixture.engine.artifact_store.store().as_ref(),
        input.module_ref.as_str(),
    )
    .await
    .expect("read the fixture's artifact")
    .expect("the fixture's artifact is published");
    input.host_requirements_ref = lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new(
        "admission-runs-after-the-read",
    ));
    for fault in [
        ArtifactFault::Missing,
        ArtifactFault::MissingStore,
        ArtifactFault::CorruptBytes,
        ArtifactFault::CorruptStore,
        ArtifactFault::Unavailable,
    ] {
        let store = Arc::new(FaultArtifacts {
            fault,
            bytes: bytes.clone(),
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let engine = crate::LashlangProcessEngine::new(
            crate::LashlangArtifacts::new(store.clone()),
            crate::LashlangSurface::default(),
        );
        let step = || {
            vm_step(
                &engine,
                &fixture.settings,
                lash_core::ToolCatalog::default(),
                first_run(&input),
            )
        };
        let settled = step().await;
        match fault {
            ArtifactFault::Missing | ArtifactFault::MissingStore => assert_eq!(
                failure_code(&ended_with(&settled)),
                crate::LashlangProcessFailureCode::ProcessModuleArtifactMissing.as_str(),
                "{fault:?}"
            ),
            ArtifactFault::CorruptBytes | ArtifactFault::CorruptStore => {
                let outcome = ended_with(&settled);
                assert!(
                    matches!(
                        &outcome,
                        lash_core::ProcessAwaitOutput::Abandoned { evidence, .. }
                            if matches!(
                                &evidence.writer,
                                lash_core::AbandonWriter::ResumeRefused {
                                    reason: lash_core::ProcessResumeRefusal::StoredArtifactCorrupt {
                                        artifact_ref,
                                        ..
                                    }
                                } if artifact_ref == input.module_ref.as_str()
                            )
                    ),
                    "{fault:?}: {outcome:?}"
                );
            }
            ArtifactFault::Unavailable => {
                assert!(
                    matches!(settled, SettledOutput::Failed(_)),
                    "an outage faults the step: {settled:?}"
                );
                let retried = step().await;
                assert_eq!(
                    failure_code(&ended_with(&retried)),
                    crate::LashlangProcessFailureCode::ProcessHostRequirementsMismatch.as_str(),
                    "the step asked again reads the artifact and reaches the admission"
                );
                assert_eq!(store.reads.load(std::sync::atomic::Ordering::SeqCst), 2);
            }
        }
    }
}

// ---- recorded settings (FIG-4398; ported by FIG-5310 from the deleted
// `recorded_inheritance_tests.rs`) ----

/// The fixture's settings with `abilities`, every other fact as recorded.
fn settings_with(abilities: lashlang::LashlangAbilities) -> serde_json::Value {
    let bounds = lashlang::ExecutionBounds::new(
        lashlang::ExecutionBound::Unbounded,
        lashlang::ExecutionBound::Unbounded,
    );
    serde_json::to_value(crate::LashlangRecordedSettings::new(
        crate::LashlangSurface::new(
            abilities,
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::new(),
        ),
        bounds,
    ))
    .expect("encode the settings")
}

/// A process whose row recorded no settings, or settings that do not
/// decode, ends with a typed failure: it never runs under the running
/// engine's own surface, and as a completed step it is never retried.
#[tokio::test(flavor = "current_thread")]
async fn unreadable_recorded_settings_end_the_process_typed() {
    let fixture = vm_fixture().await;
    for recorded in [
        None,
        Some(serde_json::json!("corrupt")),
        Some(serde_json::json!({ "abilities": 7 })),
    ] {
        let output = fixture
            .run_recorded(fixture.first(), recorded.clone())
            .await;
        let VmRunOutput::Ended { outcome } = output else {
            panic!("{recorded:?}: the process ends, got {output:?}");
        };
        assert_eq!(
            failure_code(&outcome),
            "process_host_environment_invalid",
            "{recorded:?}"
        );
    }
}

/// The abilities a process recorded at creation decide what it may do, not
/// the running engine's wiring: an engine that allows `sleep` does not
/// enable it for a process that recorded none, and an engine that withholds
/// it does not take it from one that recorded it.
#[tokio::test(flavor = "current_thread")]
async fn recorded_sleep_is_neither_enabled_nor_withheld_by_run_wiring() {
    let fixture = vm_fixture().await;
    let output = fixture
        .run_recorded(
            fixture.first(),
            Some(settings_with(lashlang::LashlangAbilities::default())),
        )
        .await;
    let VmRunOutput::Ended { outcome } = output else {
        panic!("a process that recorded no sleep ends, got {output:?}");
    };
    assert!(
        matches!(&*outcome, lash_core::ProcessAwaitOutput::Settled { output } if !output.is_success()),
        "{outcome:?}"
    );
    assert!(
        serde_json::to_string(&*outcome)
            .expect("encode the outcome")
            .contains("sleep"),
        "the refusal names the unrecorded ability: {outcome:?}"
    );

    let withholding = VmFixture {
        engine: crate::LashlangProcessEngine::new(
            fixture.engine.artifact_store.clone(),
            crate::LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
        ),
        payload: fixture.payload.clone(),
        settings: settings_with(lashlang::LashlangAbilities::default().with_sleep()),
    };
    let (_, _, until_ms) = parked_sleep(withholding.run(withholding.first()).await);
    assert_eq!(
        until_ms,
        NOW_MS + 5,
        "the recorded sleep runs on an engine that withholds it"
    );
}
