use super::*;

pub(super) fn verify_pre_replay_fence(
    store: &InMemorySessionStore,
    commit: &crate::store::RuntimeCommit,
    transaction_now: u64,
) -> Result<(), crate::StoreError> {
    if commit.turn_cancel_closure_authorization.is_none()
        && let Some(fence) = commit.session_execution_lease_fence.as_ref()
    {
        // This check-then-act read is atomic under the coarse write lock;
        // that serialization is intentional for the development backend.
        store.verify_session_execution_lease(&commit.session_id, fence, transaction_now)?;
    }
    Ok(())
}

pub(super) fn validate_after_receipt_miss(
    store: &InMemorySessionStore,
    commit: &crate::store::RuntimeCommit,
    transaction_now: u64,
) -> Result<(), crate::StoreError> {
    if commit.interrupted_turn_cancel_intent.is_some()
        && commit.turn_cancel_closure_authorization.is_none()
    {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: commit.session_id.clone(),
            turn_id: commit
                .interrupted_turn_input_turn_id
                .clone()
                .unwrap_or_else(|| crate::TurnId::from("missing-turn-id")),
        });
    }
    let Some(closure) = commit.turn_cancel_closure_authorization.as_ref() else {
        return Ok(());
    };
    let current_fence = commit
        .session_execution_lease_fence
        .as_ref()
        .or(commit.release_session_execution_lease.as_ref())
        .ok_or_else(
            || crate::StoreError::TurnCancelClosureAuthorizationMismatch {
                session_id: commit.session_id.clone(),
                turn_id: closure.turn_id().clone(),
            },
        )?;
    store.verify_session_execution_lease(&commit.session_id, current_fence, transaction_now)?;
    if closure.session_id() != &commit.session_id
        || commit.interrupted_turn_input_turn_id.as_ref() != Some(closure.turn_id())
    {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: commit.session_id.clone(),
            turn_id: closure.turn_id().clone(),
        });
    }
    let pending = store.turn_cancel_closure_authorizations.lock_recover();
    if pending.get(closure.turn_id()) != Some(closure) {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: closure.session_id().clone(),
            turn_id: closure.turn_id().clone(),
        });
    }
    Ok(())
}

pub(super) fn consume(store: &InMemorySessionStore, commit: &crate::store::RuntimeCommit) {
    if let Some(closure) = commit.turn_cancel_closure_authorization.as_ref() {
        store
            .turn_cancel_closure_authorizations
            .lock_recover()
            .remove(closure.turn_id());
    }
}
