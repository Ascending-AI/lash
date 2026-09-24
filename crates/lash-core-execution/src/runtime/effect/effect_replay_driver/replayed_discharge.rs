//! Seating the rank a replayed group child's saved terminal still owes
//! (ADR 0099 W19).

use super::*;

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// Seats the rank a replayed group child's saved terminal still owes.
    ///
    /// A child's terminal and its rank are two transactions (§5 seats the
    /// rank at discharge), so a process can die between them and leave a
    /// saved result with no rank. The drain finishes such a child, and so
    /// does this: a reopen that replays the saved terminal discharges it
    /// first, or a caller parked on the next rank — and every sibling queued
    /// behind it at the commit-order barrier — would wait on a rank nothing
    /// is left to seat. Only a `committed` row owes a discharge: a drained
    /// one already holds its rank, and a cancel-decided one took its rank
    /// with the decision.
    pub(super) async fn discharge_replayed_child(
        &self,
        scope: &ExecutionScope,
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Some(membership) = envelope.group.as_deref() else {
            return Ok(());
        };
        let scope_id = scope
            .journal_identity()
            .map_err(RuntimeEffectControllerError::from)?
            .key()
            .to_string();
        let replay_key = envelope.invocation.replay_key();
        let owes_discharge = self
            .row_store
            .read_group_child_arbitration(&scope_id, replay_key)
            .await?
            .is_some_and(|arbitration| {
                arbitration.group_key == membership.group_key
                    && matches!(arbitration.commit_state, EffectCommitState::Committed)
            });
        if !owes_discharge {
            return Ok(());
        }
        self.discharge_committed_claim(&scope_id, replay_key, &membership.group_key, None)
            .await
    }
}
