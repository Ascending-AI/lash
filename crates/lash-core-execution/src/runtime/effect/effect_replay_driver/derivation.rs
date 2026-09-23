//! Release an ungrouped transient derivation claim without sealing an error.

use super::*;

pub(super) async fn release_derivation<P: EffectReplayRowStore, A: AwaitEventBackend>(
    driver: &StoreEffectReplayDriver<P, A>,
    claim: &ClaimedEffect,
    command_kind: crate::RuntimeEffectKind,
    outcome: &Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
) -> Result<bool, RuntimeEffectControllerError> {
    if claim.group_key.is_some() {
        return Ok(false);
    }
    let Err(error) = outcome else {
        return Ok(false);
    };
    if !error
        .journal_disposition(command_kind)
        .is_retryable_derivation()
    {
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
