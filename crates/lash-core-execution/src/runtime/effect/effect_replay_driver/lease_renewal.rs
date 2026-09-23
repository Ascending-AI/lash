//! Effect-lease renewal while a claimed effect executes (FIG-3512).
//!
//! The lease contract is [`LeaseTimings`]'s: the TTL covers at least three
//! renew intervals so a live owner can miss two consecutive renewals. Only the
//! store's fence verdict ends a lease early; a renewal that errors is a miss,
//! and execution is abandoned only once a full TTL has passed without a
//! confirmed renewal. An abandoned execution is never finalized — its row
//! stays reclaimable.

use super::*;

/// How executing a claimed effect ended.
pub(super) enum ClaimedExecution {
    /// The effect produced its own result while the lease was held;
    /// finalize seals it.
    Finished(Result<RuntimeEffectOutcome, RuntimeEffectControllerError>),
    /// Renewal could no longer hold the lease — the store refused it, or the
    /// renewal budget ran out — so the effect future was dropped. The error
    /// is a controller failure, never the effect's terminal: nothing is
    /// finalized, and the row stays reclaimable.
    Relinquished(RuntimeEffectControllerError),
}

impl<P: EffectReplayRowStore, A: AwaitEventBackend> StoreEffectReplayDriver<P, A> {
    /// Keep `claim`'s lease renewed until it can no longer be held, and say
    /// why.
    ///
    /// Only the store's verdict ends the lease early: a renewal the fence
    /// guard refuses means the lease expired or another owner holds the row.
    /// A renewal that errors proves neither — the same rule the session
    /// execution lease renewer applies — so it counts as a missed renewal and
    /// the cadence continues. [`LeaseTimings`] makes the TTL at least three
    /// renew intervals precisely so a live owner can miss two consecutive
    /// renewals; the budget is spent once a full TTL has passed since the last
    /// request the store confirmed, because a peer may then take the row over.
    /// Never returns while the lease is still held.
    async fn hold_effect_lease(&self, claim: &ClaimedEffect) -> RuntimeEffectControllerError {
        let fence = &claim.fence;
        let ttl = self.lease_timings.ttl();
        let renew_every = self.lease_timings.renew_interval();
        let mut confirmed_at = claim.requested_at;
        let mut missed: u32 = 0;
        let mut last_error: Option<RuntimeEffectControllerError> = None;
        loop {
            let held_for = self.clock.now().saturating_duration_since(confirmed_at);
            self.clock
                .sleep(renew_every.min(ttl.saturating_sub(held_for)))
                .await;
            let requested_at = self.clock.now();
            if requested_at.saturating_duration_since(confirmed_at) >= ttl {
                let cause = last_error
                    .as_ref()
                    .map_or_else(String::new, |err| format!("; last renewal error: {err}"));
                return self.vocabulary().error(
                    EffectReplayFailure::LeaseLost,
                    format!(
                        "runtime effect replay lease for scope `{}` replay key `{}` could not be \
                         renewed within its {}ms TTL ({missed} consecutive renewals failed); \
                         execution was abandoned and the row left for reclaim{cause}",
                        fence.scope_id,
                        fence.replay_key,
                        self.lease_timings.ttl_ms(),
                    ),
                );
            }
            match self
                .row_store
                .renew(fence, self.lease_timings.ttl_ms())
                .await
            {
                Ok(true) => {
                    confirmed_at = requested_at;
                    missed = 0;
                    last_error = None;
                }
                Ok(false) => {
                    return self.vocabulary().error(
                        EffectReplayFailure::LeaseLost,
                        format!(
                            "runtime effect replay lease was lost while executing scope `{}` \
                             replay key `{}`",
                            fence.scope_id, fence.replay_key
                        ),
                    );
                }
                Err(err) => {
                    missed = missed.saturating_add(1);
                    tracing::warn!(
                        scope_id = %fence.scope_id,
                        replay_key = %fence.replay_key,
                        missed,
                        error = %err,
                        "runtime effect lease renewal failed; a failed renewal proves no \
                         loss, so the effect keeps running while the lease TTL allows"
                    );
                    last_error = Some(err);
                }
            }
        }
    }

    pub(super) async fn execute_claimed_effect_with_renewal(
        &self,
        claim: &ClaimedEffect,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> ClaimedExecution {
        let effect = self.execute_claimed_effect(claim, envelope, local_executor);
        // Renewal runs beside the effect, never in place of polling it, and
        // returns only once the lease can no longer be held; losing that race
        // drops the effect future.
        tokio::select! {
            biased;
            result = effect => ClaimedExecution::Finished(result),
            err = self.hold_effect_lease(claim) => ClaimedExecution::Relinquished(err),
        }
    }
}
