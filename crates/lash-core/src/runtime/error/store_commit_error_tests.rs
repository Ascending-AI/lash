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
        attachment_referrer_bytes: 1,
        follow_on_bytes: 0,
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
