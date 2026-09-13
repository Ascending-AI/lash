use super::{
    EXECUTION_BOUND_EXHAUSTION_LOUD, LASHLANG_SEGMENT_STATE_VERSION, LashlangProcessExecutionTrace,
    LashlangSegmentState, LashlangSegmentStateError, SEGMENT_BOUNDARY_DECLINED_TOTAL,
    decode_lashlang_segment_state, process_lashlang_execution_result, process_trace_session_id,
    record_segment_boundary_decline, validate_lashlang_program_hash,
};

#[test]
fn process_trace_session_attribution_comes_only_from_a_session_originator() {
    let identity = |originator: lash_core::ProcessOriginator| {
        let hash = lashlang::ContentHash::new("trace-provenance");
        LashlangProcessExecutionTrace::new(
            None,
            lash_trace::TraceContext::default().for_session("ambient-capability"),
            process_trace_session_id(&originator),
            lash_core::ProcessId::from("process"),
            lashlang::ModuleRef::new(&hash),
            lashlang::ProcessRef::new(hash, 0),
            "main".to_string(),
        )
        .identity()
    };

    assert_eq!(
        identity(lash_core::ProcessOriginator::host_scoped("operator"))
            .scope
            .session_id,
        None,
        "a host namespace and ambient capability are not runtime session attribution"
    );
    assert_eq!(
        identity(lash_core::ProcessOriginator::session(
            lash_core::SessionScope::new("actual-session")
        ))
        .scope
        .session_id,
        Some(lash_sansio::SessionId::from("actual-session"))
    );
}
use std::sync::atomic::Ordering;

const UNVERSIONED_SEGMENT_STATE: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_state_unversioned.json");
const VM_V10_SEGMENT_STATE: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_state_vm_v10.json");

struct SegmentFixtureHost;

impl lashlang::ExecutionHost for SegmentFixtureHost {
    async fn perform(
        &self,
        _op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
        Err(lashlang::ExecutionHostError::new(
            "the segment fixture does not execute effects",
        ))
    }
}

#[test]
#[ignore = "run only with the predecessor writer at ccab40166"]
fn capture_vm_v10_segment_state_from_predecessor_writer() {
    assert_eq!(
        lashlang::VM_CONTINUATION_FORMAT_VERSION,
        10,
        "capture this fixture only from predecessor writer commit ccab40166"
    );
    let program = lashlang::compile("finish null").expect("compile fixture program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct fixture VM");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture fixture VM continuation"),
        sleep_sequence: 3,
        event_sequence: 5,
        signal_send_sequence: 7,
        signal_wait_ordinals: [("ready".to_string(), 11)].into(),
        parent_end_actions: Vec::new(),
        started_process_ids: Vec::new(),
    };
    let mut wire = serde_json::to_value(segment_state).expect("serialize segment-state writer");
    wire["vm"]["execution_nonce"] = serde_json::json!(16294208416658607535_u64);
    wire["vm"]["active_execution_elapsed"] = serde_json::json!({"nanos": 0, "secs": 0});
    let mut bytes = serde_json::to_vec(&wire).expect("serialize v10 predecessor");
    bytes.push(b'\n');
    std::fs::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/fixtures/lashlang_segment_state_vm_v10.json"),
        bytes,
    )
    .expect("write v10 predecessor fixture");
}

#[test]
fn unversioned_prior_shape_is_typed_rejection_with_cutover_remedy() {
    let Err(error) = decode_lashlang_segment_state(UNVERSIONED_SEGMENT_STATE) else {
        panic!("unversioned handover must not have a compatibility decoder");
    };

    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found: 0,
            }
        ),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

#[test]
fn vm_v10_shape_with_projected_slots_is_a_versioned_rejection() {
    assert!(
        VM_V10_SEGMENT_STATE
            .windows(b"projected_slots".len())
            .any(|window| window == b"projected_slots"),
        "the predecessor fixture must preserve the retired key"
    );
    let Err(LashlangSegmentStateError::FormatMismatch { details }) =
        decode_lashlang_segment_state(VM_V10_SEGMENT_STATE)
    else {
        panic!("the v10 VM continuation must be refused by the v11 decoder");
    };
    assert!(
        details.contains("version 10"),
        "unexpected refusal: {details}"
    );
    assert!(
        details.contains(&format!(
            "version {}",
            lashlang::VM_CONTINUATION_FORMAT_VERSION
        )),
        "the refusal must name the current VM continuation version: {details}"
    );
}

#[test]
fn resume_rejects_changed_bytecode_program_hash_with_typed_failure() {
    let output = validate_lashlang_program_hash("sha256:old", "sha256:current")
        .expect_err("changed bytecode identity must fail closed");
    assert!(matches!(
        *output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Failure(ref failure)
                if failure.code == "restate_segment_program_hash_mismatch")
    ));
}

#[test]
fn declined_boundary_is_warned_and_counted() {
    let before = SEGMENT_BOUNDARY_DECLINED_TOTAL.load(Ordering::Relaxed);
    record_segment_boundary_decline(&"projected state", "test boundary decline");
    assert_eq!(
        SEGMENT_BOUNDARY_DECLINED_TOTAL.load(Ordering::Relaxed),
        before + 1
    );
}

#[test]
fn durable_exhaustion_has_a_typed_process_failure_surface() {
    let previous = EXECUTION_BOUND_EXHAUSTION_LOUD.swap(false, Ordering::SeqCst);
    let output =
        process_lashlang_execution_result(Err(lashlang::RuntimeError::InstructionBudgetExceeded {
            limit: 1,
        }));
    EXECUTION_BOUND_EXHAUSTION_LOUD.store(previous, Ordering::SeqCst);
    assert!(matches!(
        output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Failure(ref failure)
                if failure.code == "process_execution_bound_exhausted")
    ));
}
