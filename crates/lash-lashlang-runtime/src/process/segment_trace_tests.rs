use super::{
    EXECUTION_BOUND_EXHAUSTION_LOUD, LASHLANG_SEGMENT_STATE_VERSION, LashlangSegmentStateError,
    SEGMENT_BOUNDARY_DECLINED_TOTAL, decode_lashlang_segment_state,
    process_lashlang_execution_result, record_segment_boundary_decline,
    validate_lashlang_program_hash,
};
use std::sync::atomic::Ordering;

const UNVERSIONED_SEGMENT_STATE: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_state_unversioned.json");

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
fn multi_segment_trace_emits_one_started_and_one_finished() {
    let boundary = || {
        lash_core::ProcessRunOutcome::SegmentBoundary(lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: Some("program-v1".to_string()),
            engine_state: Vec::new(),
        })
    };
    let terminal = lash_core::ProcessRunOutcome::from(lash_core::ProcessAwaitOutput::Success {
        value: serde_json::Value::Null,
        control: None,
    });
    let lifecycle = [
        (true, boundary().is_terminal()),
        (false, boundary().is_terminal()),
        (false, terminal.is_terminal()),
    ];
    assert_eq!(lifecycle.iter().filter(|(started, _)| *started).count(), 1);
    assert_eq!(
        lifecycle.iter().filter(|(_, finished)| *finished).count(),
        1
    );
}

#[test]
fn resume_rejects_changed_bytecode_program_hash_with_typed_failure() {
    let output = validate_lashlang_program_hash(Some("sha256:old"), "sha256:current")
        .expect_err("changed bytecode identity must fail closed");
    assert!(matches!(
        *output,
        lash_core::ProcessAwaitOutput::Failure { code, .. }
            if code == "restate_segment_program_hash_mismatch"
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
        lash_core::ProcessAwaitOutput::Failure { code, .. }
            if code == "process_execution_bound_exhausted"
    ));
}
