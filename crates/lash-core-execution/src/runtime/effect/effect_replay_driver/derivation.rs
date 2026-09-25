//! Release a claim without sealing its error: an ungrouped transient
//! derivation, or a group child whose refusal parks its opener.

use super::*;

pub(super) async fn release_derivation<P: EffectReplayRowStore, A: AwaitEventBackend>(
    driver: &StoreEffectReplayDriver<P, A>,
    claim: &ClaimedEffect,
    command_kind: crate::RuntimeEffectKind,
    outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
) -> Result<bool, RuntimeEffectControllerError> {
    let Err(error) = outcome else {
        return Ok(false);
    };
    let unsealed = match claim.group_key {
        // A group child that refused where it parks its opener — a drifted
        // tool it would run live (FIG-3725), a replay divergence — records
        // nothing, as on Restate: sealed, the refusal would be served to
        // every later redrive, even one after the tool was restored.
        Some(_) => {
            command_kind == crate::RuntimeEffectKind::ToolInvocation
                && error.turn_failure_cause() == crate::TurnFailureCause::Parked
        }
        None => error
            .journal_disposition(command_kind)
            .is_retryable_derivation(),
    };
    if !unsealed {
        return Ok(false);
    }
    let fence = &claim.fence;
    if driver
        .row_store
        .release_uncommitted_derivation(fence)
        .await?
    {
        Ok(true)
    } else {
        Err(driver.vocabulary().error(
            EffectReplayFailure::LeaseLost,
            format!(
                "runtime effect replay lease was lost before releasing derivation `{}`",
                fence.replay_key
            ),
        ))
    }
}
