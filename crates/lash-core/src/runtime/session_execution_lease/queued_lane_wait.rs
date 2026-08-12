//! Aliveness-aware busy handling for the durable queued-work drain.
//!
//! A durable workflow controller's invocation *is* the retry unit, so the drain
//! it hosts cannot answer a busy session lane with "nothing to drain" (the
//! engine would treat the queued row as settled) and must not block on it
//! forever either. This policy decides between the two honest answers, using
//! only facts the store itself published:
//!
//! - a holder whose expiry has not moved since the previous observation looks
//!   crashed, so the drain waits and re-claims - the store's own expiry check
//!   is what eventually grants the lane;
//! - a holder whose expiry moved forward while its identity triple stayed the
//!   same has renewed, so it is alive and no amount of waiting will free the
//!   lane inside this invocation. The drain gives up and reports a typed
//!   retryable error, handing pacing to the engine's retry policy;
//! - waiting is capped regardless, so an unexpected shape cannot become an
//!   unbounded block in a shipped product path.
//!
//! **Clock discipline.** Every expiry comparison uses store-written values only
//! (`expires_at_epoch_ms` against the previously observed
//! `expires_at_epoch_ms`, and `expires_at_epoch_ms - claimed_at_epoch_ms` for
//! the observed TTL). Both are in the store's clock domain, and their difference
//! is a pure duration, so nothing here subtracts a local timestamp from a
//! store-written one. The local clock is used only to *measure* a slice, which
//! is clock-domain free.

use crate::store::SessionExecutionLease;

/// Total in-process waiting for a busy session lane, as a multiple of the
/// holder's own observed TTL.
///
/// One TTL is the whole honest budget for a holder that died without releasing:
/// once it elapses, the store itself lets the next claim displace the row. The
/// second TTL absorbs exactly one hand-off - a *different* crashed executor
/// that took the lane while this drain waited - so a single hand-off does not
/// become a give-up. Beyond that the lane is contended rather than
/// crash-blocked, and pacing belongs to the engine's retry policy, not to a
/// sleep inside one invocation.
const TOTAL_WAIT_TTL_MULTIPLE: u64 = 2;

/// Claim attempts per observed TTL while waiting out a crashed-looking holder.
///
/// The drain cannot compute how much of the holder's TTL is left: that would
/// mean subtracting a host timestamp from a store-written expiry, the exact
/// clock mixing this module refuses. It polls instead, so the probe rate *is*
/// the resolution with which it notices the lane freeing: the delay between the
/// true expiry and the successful claim is bounded by one slice. 64 probes per
/// TTL keeps that inside 1/64 of a TTL - under half a second on the 30s lease
/// TTL the distributed-workers gate uses, which matters because the drain is on
/// the critical path of a failover whose observers have their own deadlines -
/// while costing 64 single-row claim attempts per TTL per waiter, roughly two
/// per second. It also bounds how long a live holder's renewal goes unnoticed.
const PROBES_PER_TTL: u64 = 64;

/// Floor for one wait slice, so a zero or near-zero TTL cannot turn the
/// re-claim loop into a spin against the backend.
const MIN_SLICE_MS: u64 = 25;

/// Why a durable queued drain stopped waiting for the session lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueuedLaneGiveUp {
    /// The holder renewed its lease under an unchanged identity triple.
    HolderIsAlive,
    /// The total in-process wait budget elapsed.
    WaitBudgetExhausted,
}

impl QueuedLaneGiveUp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::HolderIsAlive => "holder_is_alive",
            Self::WaitBudgetExhausted => "wait_budget_exhausted",
        }
    }
}

/// What the drain does with one observed busy holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueuedLaneWaitStep {
    /// Sleep this long, then re-claim.
    Wait { slice_ms: u64 },
    /// Stop waiting and report the typed retryable error.
    GiveUp(QueuedLaneGiveUp),
}

/// The per-drain wait state: one instance spans one drain's Busy rounds.
#[derive(Debug, Default)]
pub(crate) struct QueuedLaneWait {
    budget_ms: u64,
    slice_ms: u64,
    waited_ms: u64,
    previous: Option<PreviousObservation>,
}

#[derive(Debug)]
struct PreviousObservation {
    owner_id: String,
    incarnation_id: String,
    executor_id: String,
    expires_at_epoch_ms: u64,
}

impl PreviousObservation {
    fn is_same_holder(&self, holder: &SessionExecutionLease) -> bool {
        self.owner_id == holder.owner.owner_id
            && self.incarnation_id == holder.owner.incarnation_id
            && self.executor_id == holder.executor_id
    }
}

impl QueuedLaneWait {
    /// Total waiting this drain has already committed to, in milliseconds.
    pub(crate) fn waited_ms(&self) -> u64 {
        self.waited_ms
    }

    /// Decide what to do with `holder`, the busy holder this round observed.
    pub(crate) fn observe(&mut self, holder: &SessionExecutionLease) -> QueuedLaneWaitStep {
        let observed_ttl_ms = holder
            .expires_at_epoch_ms
            .saturating_sub(holder.claimed_at_epoch_ms);
        match self.previous.take() {
            None => {
                self.budget_ms = observed_ttl_ms
                    .saturating_mul(TOTAL_WAIT_TTL_MULTIPLE)
                    .max(MIN_SLICE_MS);
                self.slice_ms = (observed_ttl_ms / PROBES_PER_TTL).max(MIN_SLICE_MS);
            }
            Some(previous) => {
                if previous.is_same_holder(holder)
                    && holder.expires_at_epoch_ms > previous.expires_at_epoch_ms
                {
                    return QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::HolderIsAlive);
                }
            }
        }
        let remaining_ms = self.budget_ms.saturating_sub(self.waited_ms);
        if remaining_ms == 0 {
            return QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::WaitBudgetExhausted);
        }
        let slice_ms = self.slice_ms.min(remaining_ms);
        self.waited_ms += slice_ms;
        self.previous = Some(PreviousObservation {
            owner_id: holder.owner.owner_id.clone(),
            incarnation_id: holder.owner.incarnation_id.clone(),
            executor_id: holder.executor_id.clone(),
            expires_at_epoch_ms: holder.expires_at_epoch_ms,
        });
        QueuedLaneWaitStep::Wait { slice_ms }
    }
}

/// The typed retryable error a durable queued drain reports instead of blocking.
///
/// The holder facts are literal in the message so an operator triaging a
/// retrying workflow can name the executor holding the lane. No
/// [`RuntimeErrorCause`](crate::RuntimeErrorCause) is attached deliberately: a
/// cause makes a `RuntimeError` terminal, and this outcome is explicitly safe to
/// retry.
pub(crate) fn lane_busy_error(
    session_id: &str,
    holder: &SessionExecutionLease,
    give_up: QueuedLaneGiveUp,
    waited_ms: u64,
) -> crate::RuntimeError {
    let reason = match give_up {
        QueuedLaneGiveUp::HolderIsAlive => "the holder renewed its lease",
        QueuedLaneGiveUp::WaitBudgetExhausted => "the in-process wait budget elapsed",
    };
    crate::RuntimeError::new(
        crate::RuntimeErrorCode::SessionExecutionLaneBusy,
        format!(
            "session execution lane for session `{session_id}` is held by owner \
             `{owner}` incarnation `{incarnation}` executor `{executor}` \
             (fencing generation {fencing_token}, expires at {expires_at_epoch_ms}); \
             stopped waiting after {waited_ms}ms because {reason}",
            owner = holder.owner.owner_id,
            incarnation = holder.owner.incarnation_id,
            executor = holder.executor_id,
            fencing_token = holder.fencing_token,
            expires_at_epoch_ms = holder.expires_at_epoch_ms,
        ),
    )
}

/// `session_execution_lease.busy_wait`: this drain is waiting out a
/// crashed-looking holder. INFO because a queued workflow that appears stuck is
/// an ordinary production question, and an operator cannot enable debug logging
/// retroactively.
pub(crate) fn trace_busy_wait(
    session_id: &str,
    holder: &SessionExecutionLease,
    slice_ms: u64,
    waited_ms: u64,
) {
    tracing::info!(
        session_id = %session_id,
        holder_owner_id = %holder.owner.owner_id,
        holder_incarnation_id = %holder.owner.incarnation_id,
        holder_executor_id = %holder.executor_id,
        holder_fencing_token = holder.fencing_token,
        holder_expires_at_epoch_ms = holder.expires_at_epoch_ms,
        slice_ms,
        waited_ms,
        event = "session_execution_lease.busy_wait",
        "durable queued drain is waiting out a crashed-looking session lane holder"
    );
}

/// `session_execution_lease.busy_gave_up`: this drain stopped waiting and is
/// handing pacing back to the controller's engine.
pub(crate) fn trace_busy_gave_up(
    session_id: &str,
    holder: &SessionExecutionLease,
    give_up: QueuedLaneGiveUp,
    waited_ms: u64,
) {
    tracing::info!(
        session_id = %session_id,
        holder_owner_id = %holder.owner.owner_id,
        holder_incarnation_id = %holder.owner.incarnation_id,
        holder_executor_id = %holder.executor_id,
        holder_fencing_token = holder.fencing_token,
        holder_expires_at_epoch_ms = holder.expires_at_epoch_ms,
        waited_ms,
        give_up = give_up.as_str(),
        event = "session_execution_lease.busy_gave_up",
        "durable queued drain stopped waiting for the session lane"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(
        executor_id: &str,
        claimed_at_epoch_ms: u64,
        expires_at_epoch_ms: u64,
    ) -> SessionExecutionLease {
        SessionExecutionLease {
            session_id: "queued-lane-wait".to_string(),
            owner: crate::LeaseOwnerIdentity::opaque("host", "host:boot"),
            executor_id: executor_id.to_string(),
            lease_token: "token".to_string(),
            fencing_token: 7,
            claimed_at_epoch_ms,
            expires_at_epoch_ms,
        }
    }

    /// A 6_400ms observed TTL slices into 64 probes of 100ms.
    #[test]
    fn a_static_expiry_is_waited_out_in_one_sixty_fourth_ttl_slices() {
        let mut wait = QueuedLaneWait::default();
        let crashed = holder("executor-a", 1_000, 7_400);

        assert_eq!(
            wait.observe(&crashed),
            QueuedLaneWaitStep::Wait { slice_ms: 100 }
        );
        assert_eq!(
            wait.observe(&crashed),
            QueuedLaneWaitStep::Wait { slice_ms: 100 }
        );
        assert_eq!(wait.waited_ms(), 200);
    }

    #[test]
    fn a_renewed_expiry_under_one_identity_gives_up_immediately() {
        let mut wait = QueuedLaneWait::default();
        assert_eq!(
            wait.observe(&holder("executor-a", 1_000, 7_400)),
            QueuedLaneWaitStep::Wait { slice_ms: 100 }
        );

        assert_eq!(
            wait.observe(&holder("executor-a", 1_000, 7_401)),
            QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::HolderIsAlive)
        );
        assert_eq!(wait.waited_ms(), 100);
    }

    #[test]
    fn a_different_executor_at_a_later_expiry_is_a_hand_off_not_a_renewal() {
        let mut wait = QueuedLaneWait::default();
        assert_eq!(
            wait.observe(&holder("executor-a", 1_000, 7_400)),
            QueuedLaneWaitStep::Wait { slice_ms: 100 }
        );

        assert_eq!(
            wait.observe(&holder("executor-b", 7_400, 13_800)),
            QueuedLaneWaitStep::Wait { slice_ms: 100 }
        );
    }

    #[test]
    fn total_waiting_is_capped_at_twice_the_first_observed_ttl() {
        let mut wait = QueuedLaneWait::default();
        let crashed = holder("executor-a", 1_000, 7_400);
        for _ in 0..128 {
            assert_eq!(
                wait.observe(&crashed),
                QueuedLaneWaitStep::Wait { slice_ms: 100 }
            );
        }
        assert_eq!(wait.waited_ms(), 12_800);

        assert_eq!(
            wait.observe(&crashed),
            QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::WaitBudgetExhausted)
        );
    }

    #[test]
    fn a_zero_ttl_holder_still_gets_a_minimum_backoff_rather_than_a_spin() {
        let mut wait = QueuedLaneWait::default();
        let expired = holder("executor-a", 1_000, 1_000);

        assert_eq!(
            wait.observe(&expired),
            QueuedLaneWaitStep::Wait { slice_ms: 25 }
        );
        assert_eq!(
            wait.observe(&expired),
            QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::WaitBudgetExhausted)
        );
    }

    /// The last slice is trimmed to the remaining budget rather than overshooting
    /// it, so the reported wait is exactly the cap.
    #[test]
    fn the_final_slice_is_trimmed_to_the_remaining_budget() {
        let mut wait = QueuedLaneWait::default();
        // 130ms TTL: budget 260ms, slice floors at 25ms, so ten full slices and
        // a trimmed eleventh.
        let crashed = holder("executor-a", 1_000, 1_130);
        for _ in 0..10 {
            assert_eq!(
                wait.observe(&crashed),
                QueuedLaneWaitStep::Wait { slice_ms: 25 }
            );
        }

        assert_eq!(
            wait.observe(&crashed),
            QueuedLaneWaitStep::Wait { slice_ms: 10 }
        );
        assert_eq!(wait.waited_ms(), 260);
        assert_eq!(
            wait.observe(&crashed),
            QueuedLaneWaitStep::GiveUp(QueuedLaneGiveUp::WaitBudgetExhausted)
        );
    }
}
