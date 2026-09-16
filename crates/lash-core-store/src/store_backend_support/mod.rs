//! Shared SQL and identity helpers backends implement their store tier with.

use lash_sansio::SessionId;

mod append_identity;
mod process_lifecycle_sql;
pub mod required_constraints;
mod session_meta;

pub use append_identity::decode_append_request_identity;
pub use process_lifecycle_sql::{
    live_process_status_predicate_sql, nonterminal_process_status_predicate_sql,
    retired_process_status_predicate_sql, undelivered_wake_delivery_state_predicate_sql,
    wake_delivery_state_sql_literal,
};
pub use session_meta::{
    CausalColumns, SessionMetaCodec, SessionMetaWrite, StoredObserverIntent, StoredRelation,
    guard_rebind_lineage, guard_session_meta_relation_rewrite,
};

/// Reserved runtime-receipt identity used as the durable completion marker
/// for one settled session-command batch. Backends write one marker for
/// every batch in a coalesced command claim in the same transaction as the
/// head commit and queue deletion.
pub fn session_command_batch_completion_key(
    session_id: &SessionId,
    batch_id: &str,
) -> Result<String, crate::StoreError> {
    crate::OperationId::new(
        crate::ExecutionScope::queue_drain(session_id, batch_id),
        "session-command-settlement",
    )
    .storage_key()
}

/// Durable receipt identity of one turn's final commit.
///
/// A turn's runtime commits are receipted under the turn's own execution
/// scope, and the last of them carries the reserved `final` operation key,
/// so this string is present in the receipt table exactly when the turn
/// committed. Backends implementing
/// [`SessionCommitStore::committed_turn_exists`](crate::store::SessionCommitStore::committed_turn_exists)
/// must test membership with this key rather than deriving one of their
/// own, so the committed-turn fact cannot drift between tiers.
pub fn turn_commit_receipt_storage_key(
    session_id: &SessionId,
    turn_id: &lash_sansio::TurnId,
) -> Result<String, crate::StoreError> {
    crate::OperationId::new(
        crate::ExecutionScope::turn(session_id.clone(), turn_id.clone()),
        "final",
    )
    .storage_key()
}

/// Construct queued-work claim data with the predecessor identity that an
/// abandoning store must restore. Store implementors pass `None` for fresh
/// work and the interrupted `claim_id` for a redrive.
pub fn queued_work_claim_data(
    batches: Vec<crate::runtime::QueuedWorkBatch>,
    abandon_restore_claim_id: Option<String>,
    abandon_restore_claim_token: Option<String>,
) -> Result<crate::runtime::QueuedWorkClaimData, crate::StoreError> {
    if abandon_restore_claim_id.is_some() != abandon_restore_claim_token.is_some() {
        return Err(crate::StoreError::QueuedWorkPredecessorClaimCorrupt {
            claim_id_present: abandon_restore_claim_id.is_some(),
            claim_token_present: abandon_restore_claim_token.is_some(),
        });
    }
    Ok(crate::runtime::QueuedWorkClaimData {
        batches,
        abandon_restore_claim_id,
        abandon_restore_claim_token: abandon_restore_claim_token.map(String::into_boxed_str),
    })
}

/// Return the interrupted predecessor identity an abandoning queued-work
/// store must restore, or `None` when the claim originated as fresh work.
pub fn queued_work_abandon_restore_claim_id(
    claim: &crate::runtime::QueuedWorkClaim,
) -> Option<&str> {
    claim.abandon_restore_claim_id.as_deref()
}

/// Return the interrupted predecessor token paired with its claim identity.
pub fn queued_work_abandon_restore_claim_token(
    claim: &crate::runtime::QueuedWorkClaim,
) -> Option<&str> {
    claim.abandon_restore_claim_token.as_deref()
}

/// The one rule deciding whether an active-turn-scoped pending-input row is
/// an orphan this scope may repair.
///
/// Every backend answers with this function or with SQL that mirrors it
/// literally, so "which rows can a dead turn's repair touch" has exactly one
/// definition (FIG-1573). The row must be active-turn scoped and in a state
/// only its own turn could advance; the scope then supplies the proof that
/// the turn is gone. `live_generation` is the fencing token of the lease the
/// backend has just re-validated in this transaction, so a row claimed by
/// the live lane is never an orphan.
pub fn orphaned_active_turn_input_is_repairable(
    scope: crate::OrphanedTurnInputScope<'_>,
    live_generation: u64,
    state: crate::TurnInputState,
    ingress: &crate::TurnInputIngress,
    claim_token_present: bool,
    claim_session_lease_generation: u64,
) -> bool {
    if !matches!(
        state,
        crate::TurnInputState::PendingActive | crate::TurnInputState::Accepted
    ) {
        return false;
    }
    let Some(pinned_turn_id) = ingress.active_turn_id() else {
        return false;
    };
    match scope {
        crate::OrphanedTurnInputScope::Turn(turn_id) => pinned_turn_id == turn_id,
        crate::OrphanedTurnInputScope::LaneGeneration { resumable_turn_id } => {
            // A turn the caller can still resume owns its pinned rows, even
            // though its claim generation is dead: durable recovery replays
            // it under the same turn id, and a swept row changes the
            // request that replay reconstructs (FIG-1573).
            if let Some(resumable) = resumable_turn_id
                && pinned_turn_id
                    .as_str()
                    .strip_prefix(resumable.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(":agent-frame:"))
            {
                return false;
            }
            !claim_token_present || claim_session_lease_generation != live_generation
        }
    }
}

/// Build the SQL predicate admitting exactly the active-turn ingress whose
/// minimum boundary has been reached at `checkpoint`.
///
/// `min_boundary_expr` is the backend expression reading
/// `ingress_json.min_boundary` (JSON extraction differs per dialect). SQL
/// stores splice this in so `min_boundary` is filtered at EVERY checkpoint
/// from the one core rule
/// ([`crate::TurnInputCheckpointBoundary::admits`]) instead of a
/// hand-written per-checkpoint special case (FIG-1524).
///
/// An absent field reads as the serde default (`after_work`), so a
/// hand-written or externally produced row cannot bypass the filter by
/// omitting the key. A value this build does not recognize matches no
/// literal and is therefore never claimed here: a node that cannot
/// interpret a boundary leaves the row for a peer that can, where before
/// FIG-1524 the final checkpoint selected such a row and the
/// deserialization failure failed the whole claim call.
pub fn admitted_min_boundary_sql(
    min_boundary_expr: &str,
    checkpoint: crate::CheckpointKind,
) -> String {
    let admitted = crate::TurnInputCheckpointBoundary::ALL
        .iter()
        .filter(|boundary| boundary.admits(checkpoint))
        .map(|boundary| format!("'{}'", boundary.as_wire_str()))
        .collect::<Vec<_>>();
    if admitted.is_empty() {
        // No boundary reaches this checkpoint: admit nothing. An empty `IN
        // ()` list is a syntax error in both dialects.
        return "FALSE".to_string();
    }
    format!(
        "COALESCE({min_boundary_expr}, '{}') IN ({})",
        crate::TurnInputCheckpointBoundary::default().as_wire_str(),
        admitted.join(", ")
    )
}

/// Quote one turn-input state for interpolation into backend SQL.
pub fn state_sql_literal(state: crate::TurnInputState) -> String {
    format!("'{}'", state.as_str())
}

/// Quote a turn-input state list for interpolation into backend SQL.
pub fn state_sql_literal_list(states: &[crate::TurnInputState]) -> String {
    states
        .iter()
        .copied()
        .map(state_sql_literal)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Spell the complete terminal turn-input state set for interpolation into backend SQL.
pub fn terminal_turn_input_states_sql() -> String {
    let terminal_states = crate::TurnInputState::ALL
        .iter()
        .copied()
        .filter(|state| state.is_terminal())
        .collect::<Vec<_>>();
    if terminal_states.is_empty() {
        // Admit no state rather than interpolating the invalid SQL `IN ()`.
        return "FALSE".to_string();
    }
    state_sql_literal_list(&terminal_states)
}

pub use crate::runtime::turn_input_ingress::derive_pending_turn_input_id;
pub use crate::store::session_execution_lease::{
    SessionExecutionLeaseClaimIdentity, SessionExecutionLeaseFenceFacts,
    SessionExecutionLeaseRefusalFacts, SessionExecutionLeaseRefusalOperation,
    SessionExecutionLeaseRow, lease_owner_from_columns, require_current_session_execution_lease,
    row_to_session_execution_lease, trace_session_execution_lease_refusal,
};
