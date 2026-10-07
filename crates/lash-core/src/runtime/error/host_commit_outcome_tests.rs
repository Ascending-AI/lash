use super::{RuntimeErrorCode, runtime_error_from_store_commit};
use crate::store::StoreError;

#[test]
fn head_cas_supersession_requires_reload_before_retry() {
    let superseded = runtime_error_from_store_commit(StoreError::HeadRevisionConflict {
        expected: 7,
        actual: 8,
    });

    assert_eq!(superseded.code, RuntimeErrorCode::StoreCommitSuperseded);
    assert!(!superseded.is_retryable());
    assert!(!superseded.is_terminal());
    assert!(superseded.message.contains("expected 7, actual 8"));
    assert!(superseded.message.contains("reload the durable head"));

    let unknown = runtime_error_from_store_commit(StoreError::Backend(
        "unclassified storage failure".to_string(),
    ));
    assert_eq!(unknown.code, RuntimeErrorCode::RuntimeStore);
    assert!(unknown.is_retryable());
    assert!(!unknown.is_terminal());
}

#[test]
fn deleted_commit_is_typed_terminal_and_retains_the_session_id() {
    let deleted = runtime_error_from_store_commit(StoreError::SessionDeleted {
        session_id: crate::SessionId::from("retired-during-commit"),
    });

    assert_eq!(deleted.code, RuntimeErrorCode::SessionDeleted);
    assert_eq!(
        deleted.deleted_session_id().map(|id| id.as_str()),
        Some("retired-during-commit")
    );
    assert!(!deleted.is_retryable());
    assert!(deleted.is_terminal());
}

#[test]
fn a_superseded_shift_fence_is_a_superseded_commit_on_every_commit_path() {
    let stale = || StoreError::StaleShiftFence {
        session_id: crate::SessionId::from("fenced"),
        fence_epoch: 1,
        current_epoch: 2,
    };
    for mapped in [
        runtime_error_from_store_commit(stale()),
        super::runtime_error_from_turn_input_admission(stale()),
    ] {
        assert_eq!(mapped.code, RuntimeErrorCode::StoreCommitSuperseded);
        assert!(!mapped.is_retryable());
        assert!(!crate::runtime::shift::engine_retries(&mapped));
        assert!(mapped.message.contains("fenced"), "{mapped:?}");
    }
}
