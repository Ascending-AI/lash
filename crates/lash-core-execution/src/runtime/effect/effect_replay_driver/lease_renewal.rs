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
    /// the cadence continues. The [`RenewalBudget`] decides when a lease that
    /// cannot be confirmed must be given up, and it bounds every renewal
    /// call as well: a renew still in flight at the deadline is abandoned,
    /// because the effect keeps running beside it and a peer may take the row
    /// over once the TTL has passed. Never returns while the lease is held.
    async fn hold_effect_lease(&self, claim: &ClaimedEffect) -> RuntimeEffectControllerError {
        let fence = &claim.fence;
        let mut budget = RenewalBudget::new(self.lease_timings, claim.requested_at);
        let mut missed: u32 = 0;
        let mut last_error: Option<RuntimeEffectControllerError> = None;
        loop {
            self.clock.sleep(budget.next_wait(self.clock.now())).await;
            let requested_at = self.clock.now();
            let renewed = if budget.exhausted(requested_at) {
                None
            } else {
                self.renew_within_budget(fence, &budget).await
            };
            match renewed {
                None => {
                    let cause = last_error
                        .as_ref()
                        .map_or_else(String::new, |err| format!("; last renewal error: {err}"));
                    return self.vocabulary().error(
                        EffectReplayFailure::LeaseLost,
                        format!(
                            "runtime effect replay lease for scope `{}` replay key `{}` could \
                             not be renewed within its {}ms TTL ({missed} consecutive renewals \
                             failed); execution was abandoned and the row left for reclaim{cause}",
                            fence.scope_id,
                            fence.replay_key,
                            self.lease_timings.ttl_ms(),
                        ),
                    );
                }
                Some(Ok(true)) => {
                    budget.confirm(requested_at);
                    missed = 0;
                    last_error = None;
                }
                Some(Ok(false)) => return self.lease_refused(claim).await,
                Some(Err(err)) => {
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

    /// One renewal, raced against the budget's deadline: `None` once the
    /// deadline has passed with the call still in flight. Dropping the call
    /// is safe — a renewal that still lands only extends this owner's own
    /// lease, and the fence guard refuses any later write under it once a
    /// peer takes the row over. A deadline wake the clock does not confirm
    /// (a clock whose sleeps return early) yields and keeps waiting.
    async fn renew_within_budget(
        &self,
        fence: &EffectLeaseFence,
        budget: &RenewalBudget,
    ) -> Option<Result<bool, RuntimeEffectControllerError>> {
        let renew = self.row_store.renew(fence, self.lease_timings.ttl_ms());
        let Some(deadline) = budget.deadline() else {
            return Some(renew.await);
        };
        tokio::pin!(renew);
        loop {
            tokio::select! {
                biased;
                renewed = &mut renew => return Some(renewed),
                () = self.clock.sleep_until(deadline) => {
                    if budget.exhausted(self.clock.now()) {
                        return None;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    /// The error a store verdict against `claim`'s renewal reports.
    ///
    /// A group child whose cancel was decided elsewhere — another host, or a
    /// recovery pass the local token never reached — loses its lease to the
    /// cancelled terminal `decide_cancel` wrote, and that refusal is typed
    /// (ADR 0099 §4), exactly as finalize would report it. Every other
    /// verdict is a lost lease. Nothing is written either way.
    async fn lease_refused(&self, claim: &ClaimedEffect) -> RuntimeEffectControllerError {
        let fence = &claim.fence;
        if let Some(group_key) = &claim.group_key
            && let Ok(Some(arbitration)) = self
                .row_store
                .read_group_child_arbitration(&fence.scope_id, &fence.replay_key)
                .await
            && arbitration.group_key == *group_key
            && matches!(arbitration.commit_state, EffectCommitState::CancelDecided)
        {
            return group_child_cancel_decided(&fence.replay_key);
        }
        self.vocabulary().error(
            EffectReplayFailure::LeaseLost,
            format!(
                "runtime effect replay lease was lost while executing scope `{}` replay key `{}`",
                fence.scope_id, fence.replay_key
            ),
        )
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

/// How long an effect lease this driver cannot confirm is still its own.
///
/// [`LeaseTimings`] keeps the TTL at least three renew intervals, so a live
/// owner can miss two consecutive renewals. The lease is held until a full
/// TTL after the last request the store confirmed, measured from the instant
/// that request was *issued*: the store stamps the expiry no earlier than
/// that, so the budget never overestimates the lease.
#[derive(Clone, Copy, Debug)]
struct RenewalBudget {
    ttl: Duration,
    renew_every: Duration,
    confirmed_at: Instant,
}

impl RenewalBudget {
    fn new(timings: LeaseTimings, requested_at: Instant) -> Self {
        Self {
            ttl: timings.ttl(),
            renew_every: timings.renew_interval(),
            confirmed_at: requested_at,
        }
    }

    /// The instant a peer may treat the lease as expired, if representable.
    fn deadline(&self) -> Option<Instant> {
        self.confirmed_at.checked_add(self.ttl)
    }

    fn remaining(&self, now: Instant) -> Duration {
        self.ttl
            .saturating_sub(now.saturating_duration_since(self.confirmed_at))
    }

    /// Wait for the next renewal, waking at the deadline at the latest.
    fn next_wait(&self, now: Instant) -> Duration {
        self.renew_every.min(self.remaining(now))
    }

    fn exhausted(&self, now: Instant) -> bool {
        self.remaining(now).is_zero()
    }

    /// Record a renewal the store confirmed, issued at `requested_at`.
    fn confirm(&mut self, requested_at: Instant) {
        self.confirmed_at = self.confirmed_at.max(requested_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(claimed_at: Instant) -> RenewalBudget {
        RenewalBudget::new(
            LeaseTimings::new(Duration::from_secs(30), Duration::from_secs(10))
                .expect("valid timings"),
            claimed_at,
        )
    }

    #[test]
    fn the_budget_tolerates_two_missed_renewals_and_is_spent_at_one_ttl() {
        let claimed_at = Instant::now();
        let budget = budget(claimed_at);
        let at = |secs: u64| claimed_at + Duration::from_secs(secs);

        // The cadence renews at +10s and +20s; both may fail.
        assert_eq!(budget.next_wait(claimed_at), Duration::from_secs(10));
        assert!(!budget.exhausted(at(10)));
        assert_eq!(budget.next_wait(at(10)), Duration::from_secs(10));
        assert!(!budget.exhausted(at(20)));
        // The third wait ends exactly at the TTL, where the lease is spent
        // without a third attempt.
        assert_eq!(budget.next_wait(at(20)), Duration::from_secs(10));
        assert!(budget.exhausted(at(30)));
        assert_eq!(budget.deadline(), Some(at(30)));
        // Not a moment sooner.
        assert!(!budget.exhausted(at(30) - Duration::from_millis(1)));
    }

    #[test]
    fn a_wait_never_runs_past_the_deadline() {
        let claimed_at = Instant::now();
        let budget = budget(claimed_at);
        let late = claimed_at + Duration::from_secs(25);
        assert_eq!(budget.next_wait(late), Duration::from_secs(5));
        assert_eq!(
            budget.next_wait(claimed_at + Duration::from_secs(40)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_confirmed_renewal_extends_the_budget_from_its_issue_instant() {
        let claimed_at = Instant::now();
        let mut budget = budget(claimed_at);
        // A slow renewal issued at +10s that the store confirmed at +25s
        // extends the lease from +10s, not from when the answer arrived.
        let issued = claimed_at + Duration::from_secs(10);
        budget.confirm(issued);
        assert_eq!(budget.deadline(), Some(issued + Duration::from_secs(30)));
        assert!(budget.exhausted(issued + Duration::from_secs(30)));
        assert!(!budget.exhausted(claimed_at + Duration::from_secs(39)));
        // An older confirmation never moves the budget backwards.
        budget.confirm(claimed_at);
        assert_eq!(budget.deadline(), Some(issued + Duration::from_secs(30)));
    }
}
