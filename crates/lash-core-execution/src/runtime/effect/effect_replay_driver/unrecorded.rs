//! Release a claim whose failed execution the journal must not record.
//!
//! Two failed executions end without a terminal, and both keep the `pending`
//! row and its canonical envelope so the next claim takes the same replay key
//! over (ADR 0042: the re-run is addressed exactly as the first run was):
//!
//! * an ungrouped derivation the executor marked retryable — an
//!   `EffectErrorJournalDisposition` whose authority is local to the
//!   executor and never decoded from a row;
//! * a group child whose execution ended in a fault that aborts its turn —
//!   store I/O, a pool timeout, a `ControllerAborted` attempt, a parked
//!   replay refusal (FIG-3644). That is a fact about this attempt, never the
//!   child's outcome: sealing it would replay the fault as the child's
//!   `Failed` terminal on every redrive, so the turn could only abort again.
//!   Released, the child is re-driven by whatever claims it next — the
//!   caller's reopen, the loser drain, the opener's end — and settles then.

use super::*;

/// Releases `claim` instead of sealing `outcome` when the journal must not
/// record it, answering whether it did.
///
/// A release that misses its fence is not silently dropped: the cancel
/// disposition winning the child's linearization point is reported as the
/// typed refusal it is, and any other miss as the lease this driver lost.
pub(super) async fn release_unrecorded<P: EffectReplayRowStore, A: AwaitEventBackend>(
    driver: &StoreEffectReplayDriver<P, A>,
    claim: &ClaimedEffect,
    command_kind: crate::RuntimeEffectKind,
    outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
) -> Result<bool, RuntimeEffectControllerError> {
    let Err(error) = outcome else {
        return Ok(false);
    };
    let unrecorded = match &claim.group_key {
        None => error
            .journal_disposition(command_kind)
            .is_retryable_derivation(),
        Some(_) => error.is_unrecorded_abort(),
    };
    if !unrecorded {
        return Ok(false);
    }
    let fence = &claim.fence;
    if driver.row_store.release_uncommitted_claim(fence).await? {
        return Ok(true);
    }
    if claim.group_key.is_some()
        && driver
            .row_store
            .read_group_child_arbitration(&fence.scope_id, &fence.replay_key)
            .await?
            .is_some_and(|arbitration| {
                matches!(arbitration.commit_state, EffectCommitState::CancelDecided)
            })
    {
        return Err(group_child_cancel_decided(&fence.replay_key));
    }
    Err(driver.vocabulary().error(
        EffectReplayFailure::LeaseLost,
        format!(
            "runtime effect replay lease was lost before releasing the unrecorded claim on `{}`",
            fence.replay_key
        ),
    ))
}
