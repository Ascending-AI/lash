use super::{RuntimeErrorCode, runtime_error_from_store_commit};
use crate::store::StoreError;

#[test]
fn cancellation_intent_change_maps_to_superseded_commit_if_it_escapes() {
    let mapped = runtime_error_from_store_commit(StoreError::TurnCancelIntentChanged {
        session_id: crate::SessionId::from("session-cas"),
        turn_id: crate::TurnId::from("turn-cas"),
    });
    assert_eq!(mapped.code, RuntimeErrorCode::StoreCommitSuperseded);
    assert!(mapped.message.contains("turn-cas"));
}

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
    assert_eq!(unknown.code, RuntimeErrorCode::StoreCommitFailed);
    assert!(!unknown.is_retryable());
    assert!(!unknown.is_terminal());
}

#[test]
fn deleted_commit_is_typed_terminal_and_retains_the_session_id() {
    let deleted = runtime_error_from_store_commit(StoreError::SessionDeleted {
        session_id: crate::SessionId::from("retired-during-commit"),
    });

    assert_eq!(deleted.code, RuntimeErrorCode::SessionDeleted);
    assert_eq!(deleted.deleted_session_id(), Some("retired-during-commit"));
    assert!(!deleted.is_retryable());
    assert!(deleted.is_terminal());
}

#[test]
fn park_and_close_preserve_store_contention_as_typed_store_error() {
    let session_error =
        super::session_commit_error("failed to persist runtime state", StoreError::Contended);

    assert!(matches!(
        session_error,
        crate::SessionError::Store {
            source: StoreError::Contended,
            ..
        }
    ));
}

#[test]
fn a_superseded_drive_fence_is_a_superseded_commit_on_every_commit_path() {
    let stale = || StoreError::StaleDriveFence {
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
        assert!(!crate::runtime::drive::engine_retries(&mapped));
        assert!(mapped.message.contains("fenced"), "{mapped:?}");
    }
}
