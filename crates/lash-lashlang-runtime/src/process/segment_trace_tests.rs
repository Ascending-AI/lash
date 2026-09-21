use super::{
    EXECUTION_BOUND_EXHAUSTION_LOUD, LASHLANG_SEGMENT_STATE_VERSION, LashlangProcessExecutionTrace,
    LashlangSegmentState, LashlangSegmentStateError, ReplayOrdinalsState,
    SEGMENT_BOUNDARY_DECLINED_TOTAL, decode_lashlang_segment_state,
    process_lashlang_execution_result, process_trace_session_id, record_segment_boundary_decline,
    resolve_child_max_attempts, validate_lashlang_program_hash,
};

/// `finish null`
fn finish_null() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;

    b::program(vec![b::finish(b::null())])
}

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
    let program = lashlang::compile_ast(&finish_null()).expect("compile fixture program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct fixture VM");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture fixture VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 3,
            event_sequence: 5,
            signal_wait_ordinals: [("ready".to_string(), 11)].into(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: std::num::NonZeroU32::new(5).expect("non-zero"),
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

/// The predecessor fixture is refused twice over, and each fence is asserted on
/// its own.
///
/// The fixture is a v11 envelope carrying a v10 VM continuation. Once the
/// envelope generation moved (v12, FIG-3394), decoding it whole stops at the
/// outer version and never reaches the continuation, so a single assertion
/// would silently stop testing the inner fence it was written for. The fixture
/// bytes are not edited to keep it reachable — they are a predecessor capture
/// (`capture_vm_v10_segment_state_from_predecessor_writer`) and hand-editing
/// them would make the evidence describe a shape no writer ever produced.
/// Instead the *outer* refusal is asserted on the file as captured, and the
/// inner one on the same file's `vm` node re-enveloped at the current
/// generation, which is the only construction that can reach the continuation
/// fence at all.
#[test]
fn the_v11_envelope_is_refused_by_the_current_envelope_version() {
    let Err(error) = decode_lashlang_segment_state(VM_V10_SEGMENT_STATE) else {
        panic!("a predecessor envelope must not decode");
    };
    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found: 11,
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn vm_v10_shape_with_projected_slots_is_a_versioned_rejection() {
    assert!(
        VM_V10_SEGMENT_STATE
            .windows(b"projected_slots".len())
            .any(|window| window == b"projected_slots"),
        "the predecessor fixture must preserve the retired key"
    );
    let mut wire: serde_json::Value =
        serde_json::from_slice(VM_V10_SEGMENT_STATE).expect("the predecessor fixture is JSON");
    wire["version"] = serde_json::json!(LASHLANG_SEGMENT_STATE_VERSION);
    let re_enveloped = serde_json::to_vec(&wire).expect("re-envelope the predecessor continuation");

    let Err(LashlangSegmentStateError::FormatMismatch { details }) =
        decode_lashlang_segment_state(&re_enveloped)
    else {
        panic!("the v10 VM continuation must be refused by the current decoder");
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

/// The v11 envelope no longer carries `signal_send_sequence`: its only
/// producer was deleted with the signal special forms (FIG-2999), and a
/// durable field with no producer is removed rather than round-tripped.
#[test]
fn the_current_envelope_carries_no_dead_send_ordinal() {
    let program = lashlang::compile_ast(&finish_null()).expect("compile pinning program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct pinning VM");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture pinning VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 1,
            event_sequence: 2,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: std::num::NonZeroU32::new(5).expect("non-zero"),
    };
    let wire = serde_json::to_value(&segment_state).expect("serialize current segment state");
    assert_eq!(wire["version"], LASHLANG_SEGMENT_STATE_VERSION);
    assert!(
        wire.get("signal_send_sequence").is_none(),
        "the dead send ordinal must not be written: {wire}"
    );
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

#[test]
fn predecessor_v6_segment_state_without_the_attempt_bound_is_a_versioned_rejection() {
    // The shipped v10 VM fixture was re-pinned to the current envelope version,
    // so it no longer exercises the envelope mismatch. Synthesize the immediate
    // predecessor instead: a v6 payload is exactly a v7 payload with the
    // attempt bound absent.
    let program = lashlang::compile_ast(&finish_null()).expect("compile predecessor program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm = lashlang::Vm::from_state(&program, &mut state, &environment)
        .expect("construct predecessor VM");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture predecessor VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 0,
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: std::num::NonZeroU32::new(5).expect("non-zero"),
    };
    let mut wire = serde_json::to_value(segment_state).expect("serialize predecessor writer");
    let object = wire
        .as_object_mut()
        .expect("segment state is a JSON object");
    object.remove("child_max_attempts");
    object.insert(
        "version".to_string(),
        serde_json::json!(LASHLANG_SEGMENT_STATE_VERSION - 1),
    );
    let encoded = serde_json::to_vec(&wire).expect("serialize v6 predecessor");

    let Err(error) = decode_lashlang_segment_state(&encoded) else {
        panic!("a v6 handover must not decode against the v7 envelope");
    };
    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found,
            } if *found == LASHLANG_SEGMENT_STATE_VERSION - 1
        ),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

#[test]
fn a_resumed_segment_keeps_the_recorded_attempt_bound_across_a_host_default_change() {
    let program = lashlang::compile_ast(&finish_null()).expect("compile pinning program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct pinning VM");
    let recorded = std::num::NonZeroU32::new(3).expect("non-zero recorded bound");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture pinning VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 0,
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: recorded,
    };
    let encoded = serde_json::to_vec(&segment_state).expect("encode segment handover");
    let decoded = decode_lashlang_segment_state(&encoded).expect("decode segment handover");

    let changed_host_default = std::num::NonZeroU32::new(11).expect("non-zero host default");
    assert_eq!(
        resolve_child_max_attempts(Some(&decoded), changed_host_default),
        recorded,
        "a resumed segment re-registers children with the bound already in their fingerprint"
    );
    assert_eq!(
        resolve_child_max_attempts(None, changed_host_default),
        changed_host_default,
        "only a first segment reads the live host default"
    );
}
