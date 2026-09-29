//! Shared SQL and identity helpers backends implement their store tier with.

use lash_sansio::SessionId;

mod append_identity;
mod attachment_owner_sql;
mod head_path;
mod process_lifecycle_sql;
mod run_spec_admission;
mod session_meta;
mod turn_input_batch;
mod turn_input_lifecycle_sql;

pub use append_identity::decode_append_request_identity;
pub use attachment_owner_sql::{
    process_attachment_owner_predicate_sql, turn_attachment_owner_predicate_sql,
};
pub use head_path::{HeadPathProbe, OwnerExit, OwnerExitParent, OwnerLowestNode, PathNode};
pub use process_lifecycle_sql::{
    live_process_status_predicate_sql, nonterminal_process_status_predicate_sql,
    retired_process_status_predicate_sql, undelivered_wake_delivery_state_predicate_sql,
    wake_delivery_state_sql_literal,
};
pub use run_spec_admission::{
    RunSpecAdmission, check_running_root_run_spec, check_steering_run_spec,
    steering_run_spec_target,
};
pub use session_meta::{
    CausalColumns, SessionMetaCodec, SessionMetaWrite, StoredObserverIntent, StoredRelation,
    guard_rebind_lineage, guard_session_meta_relation_rewrite,
};
pub use turn_input_batch::{
    TurnInputDraftAdmission, decide_turn_input_draft_admission, turn_input_submission_digest,
};
pub use turn_input_lifecycle_sql::{
    accepted_turn_input_state_predicate_sql, active_turn_input_state_predicate_sql,
    cancelled_turn_input_state_predicate_sql, deferred_next_turn_turn_input_state_predicate_sql,
    nonterminal_turn_input_state_predicate_sql, pending_active_turn_input_state_predicate_sql,
    terminal_turn_input_state_predicate_sql, undelivered_turn_input_state_predicate_sql,
};

/// Reserved runtime-receipt identity used as the durable completion marker
/// for one settled session-command batch. Backends write one marker for
/// every batch in a coalesced command run in the same transaction as the
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

/// Durable receipt identity of one queued-work drain's end.
///
/// A drain's epilogue commits its end under the drain's own execution scope
/// with the reserved `final` operation key, so this string is present in the
/// receipt table exactly when the drain ended. Backends implementing
/// [`SessionCommitStore::drain_end_exists`](crate::store::SessionCommitStore::drain_end_exists)
/// must test membership with this key rather than deriving one of their own,
/// so the ended-drain fact cannot drift between tiers.
pub fn drain_end_receipt_storage_key(
    session_id: &SessionId,
    drain_id: &str,
) -> Result<String, crate::StoreError> {
    crate::OperationId::new(
        crate::ExecutionScope::queue_drain(session_id.clone(), drain_id),
        "final",
    )
    .storage_key()
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
/// literal and is therefore never admitted here: a node that cannot
/// interpret a boundary leaves the row for a peer that can, where before
/// FIG-1524 the final checkpoint selected such a row and the
/// deserialization failure failed the whole admission.
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
pub fn state_sql_literal(state: crate::TurnInputStateKind) -> String {
    format!("'{}'", state.as_str())
}

/// Quote a turn-input state list for interpolation into backend SQL.
pub fn state_sql_literal_list(states: &[crate::TurnInputStateKind]) -> String {
    states
        .iter()
        .copied()
        .map(state_sql_literal)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Spell the complete terminal turn-input state set for interpolation into backend SQL.
pub fn terminal_turn_input_states_sql() -> String {
    let terminal_states = crate::TurnInputStateKind::ALL
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

/// The fence a backend's [`DriveEpochStore::seal_drive_epoch`] returns for
/// the epoch its compare-and-set raised, or the one a retried seal of the
/// same admission finds. It is the only constructor of a [`DriveFence`]
/// outside `lash-core-store`, and only store backends call it.
///
/// [`DriveEpochStore::seal_drive_epoch`]: crate::store::DriveEpochStore::seal_drive_epoch
/// [`DriveFence`]: crate::store::DriveFence
#[must_use]
pub fn sealed_drive_fence(
    session_id: SessionId,
    epoch: u64,
    admission: crate::store::AdmissionId,
) -> crate::store::DriveFence {
    crate::store::DriveFence::sealed_by_store(session_id, epoch, admission)
}
/// The admission verdicts every backend takes alike; see
/// [`crate::store::admission_plan`].
pub use crate::store::admission_plan::{
    deferred_wake_records, require_admitted_to_root, require_open_command,
};
/// One verdict function per fencing decision; see [`crate::store::fencing`].
pub use crate::store::fencing::{
    FENCED_WRITE_DISAGREEMENT_EVENT, FENCING_TRACE_TARGET, FencedWrite, HeadPublicationVerdict,
    WakeDeliveryClaimFacts, WakeDeliveryClaimVerdict, fenced_write_applied,
    head_publication_verdict, require_fenced_write_applied, require_single_writer_head_publication,
    wake_delivery_claim_verdict,
};
