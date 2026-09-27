//! The obligation relay (ADR 0109 §1.4): the engine half of store→engine
//! delivery.
//!
//! A producer arms an obligation in the transaction that makes an engine
//! effect owed, then calls [`deliver_now`] for its own commit. The reconcile
//! tick runs [`relay_due`] over every kind's partial due index, retrying what
//! the immediate attempt did not deliver. Both entry points settle a claim by
//! one rule: delivered; refused for good, stalled; undecodable, stalled;
//! retryable, retried after a capped exponential backoff until the kind's
//! attempt ceiling, then stalled. A stalled obligation is never dropped and
//! never retried until an operator re-arms it.
//!
//! The relay is engine-neutral: a kind's [`ObligationRelay::deliver`] is the
//! only engine call, and it is idempotent under a repeated obligation id.

use std::num::{NonZeroU32, NonZeroUsize};

use crate::Clock;
use crate::store::{
    ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, SettleOutcome, StallReason, StoreError,
};

/// Why one delivery attempt did not deliver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryFailure {
    /// Worth another attempt after the backoff.
    Retryable(String),
    /// The engine refused it for good.
    Refused(String),
    /// What the key names cannot be decoded by this build.
    Undecodable(String),
}

/// One kind's retry policy (ADR 0109 §1.4). A host lever (ADR 0014).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayPolicy {
    /// The first retry's delay; each later one doubles it.
    pub base_backoff_ms: u64,
    /// The delay no retry exceeds.
    pub max_backoff_ms: u64,
    /// Attempts after which a retryable failure stalls instead.
    pub attempt_ceiling: NonZeroU32,
    /// How long a claim holds its row before another relay may retake it.
    pub claim_ttl_ms: u64,
}

impl Default for RelayPolicy {
    fn default() -> Self {
        Self {
            base_backoff_ms: 1_000,
            max_backoff_ms: 900_000,
            attempt_ceiling: NonZeroU32::new(16).unwrap_or(NonZeroU32::MIN),
            claim_ttl_ms: 60_000,
        }
    }
}

impl RelayPolicy {
    /// The delay before the attempt that follows attempt `attempts`:
    /// `min(base · 2^(attempts − 1), max)`.
    #[must_use]
    pub fn backoff_ms(&self, attempts: u32) -> u64 {
        let doublings = attempts.saturating_sub(1).min(63);
        self.base_backoff_ms
            .checked_mul(1_u64 << doublings)
            .unwrap_or(u64::MAX)
            .min(self.max_backoff_ms)
    }
}

/// One kind's relay: its ledger, its policy and its engine delivery.
#[async_trait::async_trait]
pub trait ObligationRelay: Send + Sync {
    /// The ledger this relay claims from; its kind is the relay's kind.
    fn ledger(&self) -> &dyn ObligationLedger;

    /// The kind's retry policy.
    fn policy(&self) -> RelayPolicy {
        RelayPolicy::default()
    }

    /// Deliver obligation `id` on the row `key` names. Idempotent under a
    /// repeated `id`: the engine dedupes on a key derived from it.
    async fn deliver(&self, id: &ObligationId, key: &ObligationKey) -> Result<(), DeliveryFailure>;
}

/// How one claimed obligation settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayVerdict {
    Delivered,
    /// Handed back, due again at `due_at_ms`.
    Retried {
        due_at_ms: u64,
    },
    Stalled(StallReason),
    /// Another relay retook the row, or the delivery settled it itself.
    ClaimLost,
    /// Immediate delivery found the obligation not `due`: already claimed,
    /// delivered or stalled.
    NotDue,
}

pub use crate::engine::RelayPass;

fn count(pass: &mut RelayPass, verdict: &RelayVerdict) {
    match verdict {
        RelayVerdict::Delivered => pass.delivered += 1,
        RelayVerdict::Retried { .. } => pass.retried += 1,
        RelayVerdict::Stalled(_) => pass.stalled += 1,
        RelayVerdict::ClaimLost => pass.claim_lost += 1,
        RelayVerdict::NotDue => {}
    }
}

/// The settlement the rule assigns a claim whose attempt ended in `result`.
fn settlement_for(
    policy: &RelayPolicy,
    claimed: &ClaimedObligation,
    result: Result<(), DeliveryFailure>,
    now_ms: u64,
) -> ObligationSettlement {
    match result {
        Ok(()) => ObligationSettlement::Delivered,
        Err(DeliveryFailure::Refused(error)) => ObligationSettlement::Stall {
            reason: StallReason::Refused,
            error,
        },
        Err(DeliveryFailure::Undecodable(error)) => ObligationSettlement::Stall {
            reason: StallReason::Undecodable,
            error,
        },
        Err(DeliveryFailure::Retryable(error))
            if claimed.attempts >= policy.attempt_ceiling.get() =>
        {
            ObligationSettlement::Stall {
                reason: StallReason::AttemptsExhausted,
                error,
            }
        }
        Err(DeliveryFailure::Retryable(error)) => ObligationSettlement::Retry {
            due_at_ms: now_ms.saturating_add(policy.backoff_ms(claimed.attempts)),
            error,
        },
    }
}

fn outcome_label(verdict: &RelayVerdict) -> Option<&'static str> {
    match verdict {
        RelayVerdict::Delivered => Some("delivered"),
        RelayVerdict::Retried { .. } => Some("retried"),
        RelayVerdict::Stalled(_) => Some("stalled"),
        RelayVerdict::ClaimLost => Some("claim_lost"),
        RelayVerdict::NotDue => None,
    }
}

/// Attempt one claimed obligation and settle it.
async fn attempt(
    relay: &dyn ObligationRelay,
    claimed: ClaimedObligation,
    clock: &dyn Clock,
) -> Result<RelayVerdict, StoreError> {
    let ledger = relay.ledger();
    let kind = ledger.kind();
    let policy = relay.policy();
    let result = match &claimed.key {
        Ok(key) => relay.deliver(&claimed.id, key).await,
        Err(undecodable) => Err(DeliveryFailure::Undecodable(undecodable.detail.clone())),
    };
    let now_ms = clock.timestamp_ms();
    let settlement = settlement_for(&policy, &claimed, result, now_ms);
    let planned = match &settlement {
        ObligationSettlement::Delivered => RelayVerdict::Delivered,
        ObligationSettlement::Retry { due_at_ms, .. } => RelayVerdict::Retried {
            due_at_ms: *due_at_ms,
        },
        ObligationSettlement::Stall { reason, error } => {
            tracing::warn!(
                obligation_kind = kind.label(),
                obligation_id = claimed.id.as_str(),
                reason = reason.as_str(),
                attempts = claimed.attempts,
                error = error.as_str(),
                "obligation stalled; it waits for an operator re-arm"
            );
            RelayVerdict::Stalled(*reason)
        }
    };
    let verdict = match ledger
        .settle(&claimed.id, &claimed.token, settlement, now_ms)
        .await?
    {
        SettleOutcome::Applied => planned,
        SettleOutcome::ClaimLost => RelayVerdict::ClaimLost,
    };
    if let Some(outcome) = outcome_label(&verdict) {
        crate::operational_metrics::record_obligation_attempt(kind.label(), outcome);
    }
    Ok(verdict)
}

/// Immediate delivery of a producer's own commit: claim `id`, deliver it,
/// settle it. [`RelayVerdict::NotDue`] when the obligation is not `due`.
///
/// # Errors
///
/// Only a store failure; a failed delivery is a settlement, not an error.
pub async fn deliver_now(
    relay: &dyn ObligationRelay,
    id: &ObligationId,
    clock: &dyn Clock,
) -> Result<RelayVerdict, StoreError> {
    let policy = relay.policy();
    let Some(claimed) = relay
        .ledger()
        .claim(id, clock.timestamp_ms(), policy.claim_ttl_ms)
        .await?
    else {
        return Ok(RelayVerdict::NotDue);
    };
    attempt(relay, claimed, clock).await
}

/// One bounded due pass: claim at most `limit` due obligations and attempt
/// each. One obligation's failure, undecodable key or lost claim never stops
/// the rows behind it.
///
/// # Errors
///
/// A claim that cannot be read or a settle that cannot be written.
pub async fn relay_due(
    relay: &dyn ObligationRelay,
    clock: &dyn Clock,
    limit: NonZeroUsize,
) -> Result<RelayPass, StoreError> {
    let policy = relay.policy();
    let claimed = relay
        .ledger()
        .claim_due(clock.timestamp_ms(), policy.claim_ttl_ms, limit)
        .await?;
    let mut pass = RelayPass {
        claimed: claimed.len(),
        ..RelayPass::default()
    };
    for obligation in claimed {
        let verdict = attempt(relay, obligation, clock).await?;
        count(&mut pass, &verdict);
    }
    Ok(pass)
}

/// The kind a relay serves.
#[must_use]
pub fn relay_kind(relay: &dyn ObligationRelay) -> ObligationKind {
    relay.ledger().kind()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_base_and_caps_at_fifteen_minutes() {
        let policy = RelayPolicy::default();
        assert_eq!(policy.backoff_ms(1), 1_000);
        assert_eq!(policy.backoff_ms(2), 2_000);
        assert_eq!(policy.backoff_ms(10), 512_000);
        assert_eq!(policy.backoff_ms(11), 900_000);
        assert_eq!(policy.backoff_ms(u32::MAX), 900_000);
    }
}
