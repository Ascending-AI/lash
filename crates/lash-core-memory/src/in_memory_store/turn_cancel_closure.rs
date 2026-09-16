use super::*;

pub(super) fn verify_pre_replay_fence(
    store: &InMemorySessionStore,
    commit: &crate::store::RuntimeCommit,
    transaction_now: u64,
) -> Result<(), crate::StoreError> {
    if let Some(fence) = commit.session_execution_lease_fence.as_ref() {
        // This check-then-act read is atomic under the coarse write lock;
        // that serialization is intentional for the development backend.
        store.verify_session_execution_lease(&commit.session_id, fence, transaction_now)?;
    }
    Ok(())
}

pub(super) fn validate_after_receipt_miss(
    store: &InMemorySessionStore,
    commit: &crate::store::RuntimeCommit,
) -> Result<(), crate::StoreError> {
    if commit.interrupted_turn_cancel_intent.is_some()
        && commit.turn_cancel_closure_settlement.is_none()
    {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: commit.session_id.clone(),
            turn_id: commit
                .interrupted_turn_input_turn_id
                .clone()
                .unwrap_or_else(|| crate::TurnId::from("missing-turn-id")),
        });
    }
    let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() else {
        return Ok(());
    };
    let closure = settlement.authorization();
    if commit.interrupted_turn_input_cancellation.as_ref() != settlement.effective_cancellation() {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: commit.session_id.clone(),
            turn_id: closure.turn_id().clone(),
        });
    }
    if closure.session_id() != commit.session_id
        || commit.interrupted_turn_input_turn_id.as_ref() != Some(closure.turn_id())
    {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: commit.session_id.clone(),
            turn_id: closure.turn_id().clone(),
        });
    }
    if closure.admitted_scope().session_id().is_none() {
        let scope_id = closure
            .admitted_scope()
            .journal_identity()
            .map_err(|error| crate::StoreError::Backend(error.to_string()))?
            .key()
            .to_string();
        if store
            .retired_turn_cancel_scopes
            .lock_recover()
            .contains(&scope_id)
        {
            return Err(crate::StoreError::TurnCancelClosureScopeRetired { scope_id });
        }
    }
    let final_key =
        crate::OperationId::turn(closure.session_id(), closure.turn_id(), "final").storage_key()?;
    if store
        .runtime_turn_commits
        .lock_recover()
        .contains_key(&(closure.session_id().clone(), final_key))
    {
        return Err(crate::StoreError::TurnCancelClosureAuthorizationMismatch {
            session_id: closure.session_id().clone(),
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
    if let Some(settlement) = commit.turn_cancel_closure_settlement.as_ref() {
        let closure = settlement.authorization();
        if closure.session_id() != commit.session_id
            || commit.interrupted_turn_input_turn_id.as_ref() != Some(closure.turn_id())
            || commit.interrupted_turn_input_cancellation.as_ref()
                != settlement.effective_cancellation()
        {
            return;
        }

        let mut pending = store.turn_cancel_closure_authorizations.lock_recover();
        // Receipt replay may meet a newly authorized repair. Consume only the
        // exact operation being replayed, never another pending authorization.
        if pending.get(closure.turn_id()) == Some(closure) {
            pending.remove(closure.turn_id());
        }
    }
}

#[cfg(test)]
mod tests;
