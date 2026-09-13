use super::{RuntimeErrorCode, runtime_error_from_store_commit};
use crate::store::StoreError;

#[test]
fn commit_budget_errors_preserve_the_budget_kind_and_limits() {
    let node_error = runtime_error_from_store_commit(StoreError::CommitNodeBudgetExceeded {
        node_count: 513,
        max_nodes: 512,
    });
    assert_eq!(
        node_error.code,
        RuntimeErrorCode::StoreCommitNodeBudgetExceeded
    );
    assert!(
        node_error
            .message
            .contains("records 513 rows for this attempt")
    );
    assert!(
        node_error
            .message
            .contains("configured 512-row node budget")
    );
    assert!(
        node_error
            .message
            .contains("including attachment-intent adoption")
    );

    let byte_error = runtime_error_from_store_commit(StoreError::CommitByteBudgetExceeded {
        session_config_bytes: 0,
        graph_delta_bytes: 900_000,
        checkpoint_bytes: 150_000,
        attachment_manifest_bytes: 1,
        queue_batch_bytes: 0,
        agent_frame_bytes: 0,
        usage_delta_bytes: 0,
        turn_result_bytes: 0,
        total_bytes: 1_050_001,
        max_bytes: 1_048_576,
    });
    assert_eq!(
        byte_error.code,
        RuntimeErrorCode::StoreCommitByteBudgetExceeded
    );
    assert!(
        byte_error
            .message
            .contains("1050001 budgeted payload bytes")
    );
    assert!(
        byte_error
            .message
            .contains("1048576-byte transaction budget")
    );
}

#[test]
fn deterministic_checkpoint_commit_errors_are_typed_and_terminal() {
    let mismatch =
        runtime_error_from_store_commit(StoreError::CheckpointComponentEncodingVersionMismatch {
            key: "execution_state".to_string(),
            actual: 2,
            expected: 1,
        });
    assert_eq!(
        mismatch.code,
        RuntimeErrorCode::CheckpointComponentEncodingVersionMismatch
    );
    assert!(mismatch.code.is_terminal());
    assert!(!mismatch.code.is_retryable());
    assert!(mismatch.message.contains("execution_state"));

    let encoding = runtime_error_from_store_commit(StoreError::RecordEncodingFailed {
        record_kind: "checkpoint root".to_string(),
        message: "deterministic fixture failure".to_string(),
    });
    assert_eq!(encoding.code, RuntimeErrorCode::RecordEncodingFailed);
    assert!(encoding.code.is_terminal());
    assert!(!encoding.code.is_retryable());
    assert!(encoding.message.contains("checkpoint root"));
}

#[test]
fn refused_turn_outcome_materialization_hands_back_the_typed_runtime_error() {
    let refusal = super::RuntimeError::new(
        RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported,
        "frame `frame-a` is a persisted historical frame",
    );
    let mapped = runtime_error_from_store_commit(StoreError::TurnOutcomeMaterializationRefused {
        error: Box::new(refusal.clone()),
    });
    assert_eq!(
        mapped.code,
        RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported
    );
    assert_eq!(mapped.message, refusal.message);
    assert!(mapped.code.is_terminal());
    assert!(!mapped.code.is_retryable());
}

#[test]
fn public_append_and_park_preserve_deterministic_store_errors() {
    for error in [
        StoreError::CheckpointComponentEncodingVersionMismatch {
            key: "execution_state".to_string(),
            actual: 2,
            expected: 1,
        },
        StoreError::RecordEncodingFailed {
            record_kind: "checkpoint root".to_string(),
            message: "deterministic fixture failure".to_string(),
        },
    ] {
        let expected_variant = error.variant_name();
        let session_error =
            super::session_commit_error("public append and park persistence boundary", error);
        assert!(
            matches!(
                session_error,
                crate::SessionError::Store { ref source, .. }
                    if source.variant_name() == expected_variant
            ),
            "{expected_variant} lost its typed store identity: {session_error}"
        );
    }
}
