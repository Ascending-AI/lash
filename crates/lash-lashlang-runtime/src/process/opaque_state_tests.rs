//! The parent holds a process's VM state as opaque bytes (ADR 0123): its
//! decode of the engine state never reaches the VM's semantic decoder, which
//! validates and compiles regular expressions from guest-controlled bytes.

use super::{segment_continuation_expectation, segment_continuation_owner};
use crate::engine::state::{LashlangEngineState, Phase};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

/// The worker's parked-continuation wire around raw VM bytes.
fn worker_parked_continuation(vm: Vec<u8>) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct ParkedRun {
        vm: lash_vm_protocol::EncodedPayload,
        request: Option<()>,
    }

    rmp_serde::to_vec_named(&ParkedRun {
        vm: lash_vm_protocol::EncodedPayload(vm),
        request: None,
    })
    .expect("encode the worker's parked continuation")
}

/// Answers whether `T` implements `DeserializeOwned`, at compile time: the
/// inherent constant exists only where the bound holds, and the trait
/// constant is what every other type resolves to.
struct DeserializeProbe<T>(std::marker::PhantomData<T>);

trait NoDeserialize {
    const DESERIALIZES: bool = false;
}

impl<T> NoDeserialize for DeserializeProbe<T> {}

impl<T: serde::de::DeserializeOwned> DeserializeProbe<T> {
    const DESERIALIZES: bool = true;
}

// The type-level half: no serde envelope can decode a continuation, and the
// envelope the parent decodes holds its VM state as the protocol's opaque
// bytes, from a crate with no path to the VM.
const _: () = assert!(!DeserializeProbe::<lashlang::VmContinuation>::DESERIALIZES);
const _: () = assert!(DeserializeProbe::<lash_vm_protocol::OpaqueVmState>::DESERIALIZES);
const _: () = assert!(DeserializeProbe::<LashlangEngineState>::DESERIALIZES);
const _: fn(&LashlangEngineState) -> Option<&lash_vm_protocol::OpaqueVmState> =
    |state| state.vm.as_ref();

struct SleepHost;

impl lashlang::ExecutionHost for SleepHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError> {
        match op {
            lashlang::AbilityOp::Sleep(_) => {
                Ok(lashlang::AbilityOutcome::Value(lashlang::Value::Null))
            }
            _ => Err(lashlang::ExecutionHostError::new("the witness only sleeps")),
        }
    }
}

/// A process body parked after its first effect, holding a RegExp.
async fn parked_regexp_continuation() -> Vec<u8> {
    use lashlang::testing::ast_builders as b;

    let program = b::program(vec![
        b::assign(
            "pattern",
            b::builtin(
                "__lashlang_heap_new",
                vec![b::string("RegExp"), b::string("ab+c"), b::string("")],
            ),
        ),
        b::sleep_for(b::num(1.0)),
        b::finish(b::null()),
    ]);
    let compiled =
        lashlang::testing::harness::try_compile_program(&program).expect("compile the witness");
    let mut state = lashlang::State::new();
    let environment = lashlang::ExecutionEnvironment::new(&SleepHost).process();
    let mut vm =
        lashlang::Vm::from_state(&compiled, &mut state, &environment).expect("install the witness");
    assert_eq!(
        vm.run_process_until_effect()
            .await
            .expect("park after the sleep"),
        lashlang::VmRunOutcome::EffectCompleted
    );
    vm.suspend()
        .expect("park the witness")
        .to_bytes()
        .expect("encode the witness")
}

/// The behavioural half: an engine state whose continuation holds a RegExp the VM's
/// validator refuses decodes and passes every parent-side check — the parent
/// never ran the validator — and only the worker's semantic decode refuses it.
#[tokio::test(flavor = "current_thread")]
async fn parent_state_decode_never_compiles_regexp() {
    let bytes = parked_regexp_continuation().await;
    let text = String::from_utf8(bytes.clone()).expect("the continuation wire is JSON text");
    assert!(text.contains("\"ab+c\""), "the witness parks its RegExp");
    // An unbalanced group: `validate_regexp` refuses it.
    let poisoned = text.replacen("\"ab+c\"", "\"ab+(c\"", 1).into_bytes();

    let process_id = lash_sansio::ProcessId::fixture("regexp-witness");
    let owner = segment_continuation_owner(&process_id);
    let vm_contract = lashlang::vm_contract_versions();
    let valid = lash_vm_protocol::OpaqueVmState::seal(
        lash_vm_protocol::VmStateKind::Continuation,
        owner.clone(),
        vm_contract,
        worker_parked_continuation(bytes),
    );
    assert_eq!(
        worker_continuation_info(&valid)
            .await
            .expect("the worker accepts the unpoisoned RegExp"),
        0
    );
    let envelope = serde_json::to_vec(&LashlangEngineState {
        payload: serde_json::Value::Null,
        program_hash: Some("program".to_string()),
        vm: Some(lash_vm_protocol::OpaqueVmState::seal(
            lash_vm_protocol::VmStateKind::Continuation,
            owner.clone(),
            vm_contract,
            worker_parked_continuation(poisoned),
        )),
        runs: 1,
        operations: 1,
        faults: 0,
        signals: Default::default(),
        phase: Phase::Ended,
    })
    .expect("encode the engine state");

    let decoded: LashlangEngineState = serde_json::from_slice(&envelope)
        .expect("the parent decodes the engine state without touching the VM bytes");
    let decoded = decoded.vm.expect("the state holds its snapshot");
    assert_eq!(
        decoded.check(&segment_continuation_expectation(
            &owner,
            &lashlang::vm_contract_reads()
        )),
        Ok(()),
        "the parent's structural check passes bytes the VM would refuse"
    );

    let refusal = worker_continuation_info(&decoded)
        .await
        .expect_err("the worker's semantic decode validates the RegExp");
    assert!(
        refusal.to_string().contains("RegExp"),
        "the refusal is the RegExp validation: {refusal}"
    );
}

/// FIG-4645: the worker refuses a continuation it cannot decode the same way
/// on every attempt. The refusal crosses the pipe and the broker as its typed
/// cause, is not retryable, and ends the process instead of redriving it
/// into the same refusal.
#[tokio::test(flavor = "current_thread")]
async fn a_continuation_the_worker_refuses_ends_the_process_and_is_never_retried() {
    use lashlang::testing::ast_builders as b;

    let bytes = parked_regexp_continuation().await;
    let text = String::from_utf8(bytes).expect("the continuation wire is JSON text");
    let poisoned = text.replacen("\"ab+c\"", "\"ab+(c\"", 1).into_bytes();
    let process_id = lash_sansio::ProcessId::fixture("refused-continuation");
    let owner = segment_continuation_owner(&process_id);
    let vm = lash_vm_protocol::OpaqueVmState::seal(
        lash_vm_protocol::VmStateKind::Continuation,
        owner.clone(),
        lashlang::vm_contract_versions(),
        worker_parked_continuation(poisoned),
    );
    assert_eq!(
        vm.check(&segment_continuation_expectation(
            &owner,
            &lashlang::vm_contract_reads()
        )),
        Ok(()),
        "the parent's structural check passes bytes the VM would refuse"
    );
    let artifact = lashlang::ModuleArtifact::from_program(b::program(vec![b::finish(b::null())]))
        .expect("module artifact");
    // The worker refuses the start: nothing parks, so nothing commits.
    let cx = lash_core::ActorContext::unavailable();
    let snapshots = lash_vm_broker::DurableSnapshotStore::new(
        &cx,
        lash_vm_broker::ExecKey::Process(process_id.clone()),
    );
    let admissions = crate::RunAdmissions {
        cx: &cx,
        opener: lash_core::EffectOpener::process(process_id.clone()),
        limit: crate::run_operation_limit(&cx),
        policy: &|_, _| None,
        host_state: &|| Ok(None),
    };
    let failure = crate::WorkerRun {
        service: &lash_vm_client::service::Service::default(),
        host: &SleepHost,
        identities: lash_vm_broker::CodeCallIdentities::process_body(process_id),
        owner,
        frame_epoch: lash_vm_protocol::FrameEpoch(0),
        program: lash_vm_protocol::ProgramSource::Artifact {
            module_ref: artifact.module_ref().to_string(),
            entry: lash_vm_protocol::ProgramEntry::Main,
            artifact: artifact.to_store_bytes().expect("artifact bytes"),
        },
        context: lash_vm_client::RunContext::default(),
        projected: lashlang::ProjectedBindings::new(),
        bounds: lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
        ),
        state: lash_vm_protocol::StartState::Continuation(vm),
        from: None,
        snapshots: &snapshots,
        admissions: &admissions,
        boundary: &|| false,
        performing: None,
        providers: lashlang::ProjectionCatalog::new(),
    }
    .run()
    .await
    .expect_err("the worker refuses the continuation");
    assert!(
        !failure.is_retryable(),
        "{failure:?} refuses every attempt the same way"
    );
    assert!(
        matches!(
            &failure,
            lash_vm_broker::BrokerFailure::WorkerLost {
                outcome: lash_vm_protocol::InfrastructureOutcome::RunRefused {
                    refusal: lash_vm_protocol::RunRefusal::Undecodable {
                        input: lash_vm_protocol::RunInput::State {
                            kind: lash_vm_protocol::VmStateKind::Continuation,
                        },
                        ..
                    },
                },
                ..
            }
        ),
        "{failure:?}"
    );
    let terminal = super::execution_result::process_worker_failure(&failure)
        .expect("a refused run is the process's terminal");
    assert!(
        matches!(
            &terminal,
            lash_core::ProcessAwaitOutput::Settled { output }
                if matches!(&output.outcome, lash_core::ToolCallOutcome::Failure(failure)
                    if failure.code == "process_run_refused")
        ),
        "{terminal:?}"
    );
}

/// FIG-4645: the one mapping from a worker failure to a process. Only what
/// meets every attempt the same way ends it; the attempt's own failures are
/// redriven.
#[test]
fn only_a_refusal_or_a_run_limit_ends_the_process() {
    use lash_vm_broker::{BrokerFailure, CheckoutRefusal};
    use lash_vm_protocol::{
        InfrastructureOutcome, OpaqueStateRefusal, ProtocolBreach, RunRefusal, SequenceFault,
        SupervisorEvidence, WorkerLimit,
    };
    let code = |failure: &BrokerFailure| {
        super::execution_result::process_worker_failure(failure).map(|output| match output {
            lash_core::ProcessAwaitOutput::Settled { output } => match output.outcome {
                lash_core::ToolCallOutcome::Failure(failure) => failure.code,
                other => panic!("a failure terminal, got {other:?}"),
            },
            other => panic!("a settled terminal, got {other:?}"),
        })
    };
    let lost = |outcome| BrokerFailure::WorkerLost { outcome };
    let unavailable = |outcome| BrokerFailure::Unavailable {
        refusal: CheckoutRefusal::Infrastructure(outcome),
    };
    let refused = InfrastructureOutcome::from(RunRefusal::ArtifactIdentityMismatch);
    for failure in [
        lost(refused.clone()),
        unavailable(refused),
        BrokerFailure::StateRefused {
            refusal: OpaqueStateRefusal::HashMismatch,
        },
    ] {
        assert!(!failure.is_retryable(), "{failure:?}");
        assert_eq!(
            code(&failure).as_deref(),
            Some("process_run_refused"),
            "{failure:?}"
        );
    }
    for outcome in [
        InfrastructureOutcome::WorkerCrashed {
            evidence: SupervisorEvidence::EndOfStream,
        },
        InfrastructureOutcome::WorkerUnresponsive { silent_ms: 1 },
        InfrastructureOutcome::WorkerLimitExceeded {
            limit: WorkerLimit::Deadline,
        },
        ProtocolBreach::from(SequenceFault::WrongRequestId).into(),
    ] {
        for failure in [lost(outcome.clone()), unavailable(outcome)] {
            assert!(failure.is_retryable(), "{failure:?}");
            assert_eq!(code(&failure), None, "{failure:?} is redriven");
        }
    }
    assert_eq!(
        code(&BrokerFailure::Unavailable {
            refusal: CheckoutRefusal::QueueFull
        }),
        None
    );
}

async fn worker_continuation_info(
    state: &lash_vm_protocol::OpaqueVmState,
) -> Result<usize, String> {
    match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::ContinuationInfo {
            bytes: state.bytes().to_vec(),
        })
        .await
        .map_err(|e| e.to_string())?
    {
        lash_vm_client::service::Response::ContinuationInfo { iterator_count } => {
            Ok(iterator_count)
        }
        lash_vm_client::service::Response::Refused { message, .. } => Err(message),
        other => Err(format!("unexpected continuation response: {other:?}")),
    }
}

#[test]
fn schema_admission_remains_typed_in_a_process_terminal() {
    use lash_vm_broker::{BrokerFailure, CheckoutRefusal};
    use lash_vm_protocol::{InfrastructureOutcome, RunRefusal};
    let source = lash_core::JsonSchema::admit(serde_json::Value::Null)
        .expect_err("null cannot enter as a payload schema");
    let outcome = InfrastructureOutcome::from(RunRefusal::UnusableSchema {
        source: Box::new(source.clone()),
    });
    for failure in [
        BrokerFailure::WorkerLost {
            outcome: outcome.clone(),
        },
        BrokerFailure::Unavailable {
            refusal: CheckoutRefusal::Infrastructure(outcome),
        },
    ] {
        assert!(!failure.is_retryable());
        let terminal = super::execution_result::process_worker_failure(&failure)
            .expect("a schema refusal terminates the process");
        let lash_core::ProcessAwaitOutput::Settled { output } = terminal else {
            panic!("a schema refusal settles with a failure")
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("a schema refusal remains a failure")
        };
        assert!(matches!(failure.cause.as_deref(),
            Some(lash_core::ToolFailureCause::SchemaAdmission { source: retained }) if retained == &source));
    }
}
