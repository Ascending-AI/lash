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
    /// Not owed yet: a guard whose authority has not ended it (ADR 0113
    /// §2.5). Settles as `Defer` at `now + policy.max_backoff_ms`. Never
    /// stalls.
    NotYet,
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
            .saturating_mul(1_u64 << doublings)
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

    /// Whether the kind's consumer, not the relay, settles a delivery (ADR
    /// 0109 §3, ingress): the engine accepting [`deliver`](Self::deliver)'s
    /// ask leaves the claim standing, the consumer's own transaction settles
    /// the obligation, and a claim nobody settled lapses and is asked again.
    fn consumer_settles(&self) -> bool {
        false
    }

    /// Deliver obligation `id` on the row `key` names, on its `attempt`th
    /// claim. Idempotent under a repeated `id`: the engine dedupes on a key
    /// derived from it (and from `attempt`, for a kind whose consumer
    /// settles, so a lapsed claim's retry is a new ask).
    async fn deliver(
        &self,
        id: &ObligationId,
        key: &ObligationKey,
        attempt: u32,
    ) -> Result<(), DeliveryFailure>;
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
    /// Not owed yet: back to due at `due_at_ms` with its attempts reset.
    Deferred {
        due_at_ms: u64,
    },
    /// The engine accepted the ask of a kind whose consumer settles it; the
    /// claim stands until the consumer's transaction settles it or it lapses.
    Requested,
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
        RelayVerdict::Requested => pass.requested += 1,
        RelayVerdict::ClaimLost => pass.claim_lost += 1,
        // A deferred guard was claimed and owes nothing yet: `claimed`
        // counts it, and no outcome does.
        RelayVerdict::Deferred { .. } | RelayVerdict::NotDue => {}
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
        Err(DeliveryFailure::NotYet) => ObligationSettlement::Defer {
            due_at_ms: now_ms.saturating_add(policy.max_backoff_ms),
        },
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
        RelayVerdict::Deferred { .. } => Some("deferred"),
        RelayVerdict::Requested => Some("requested"),
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
    let consumer_settles = relay.consumer_settles();
    let result = match &claimed.key {
        // A consumer-settled claim is retaken only when its last ask lapsed
        // unsettled: past the ceiling, it stalls instead of asking again.
        Ok(_) if consumer_settles && claimed.attempts > policy.attempt_ceiling.get() => {
            Err(DeliveryFailure::Retryable(format!(
                "the engine accepted {} asks and nothing admitted the row",
                claimed.attempts.saturating_sub(1)
            )))
        }
        Ok(key) => relay.deliver(&claimed.id, key, claimed.attempts).await,
        Err(undecodable) => Err(DeliveryFailure::Undecodable(undecodable.detail.clone())),
    };
    let now_ms = clock.timestamp_ms();
    if consumer_settles && result.is_ok() {
        let verdict = RelayVerdict::Requested;
        if let Some(outcome) = outcome_label(&verdict) {
            crate::operational_metrics::record_obligation_attempt(kind.label(), outcome);
        }
        return Ok(verdict);
    }
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
        ObligationSettlement::Defer { due_at_ms } => RelayVerdict::Deferred {
            due_at_ms: *due_at_ms,
        },
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

/// Deliver an obligation the producer's own transaction already claimed
/// (FIG-3975): a fused admission takes the claim inside its commit, so the
/// only work left is the attempt and its settlement — identical to the claim
/// [`deliver_now`] takes then hands here.
///
/// # Errors
///
/// Only a store failure; a failed delivery is a settlement, not an error.
pub async fn deliver_claimed(
    relay: &dyn ObligationRelay,
    claimed: ClaimedObligation,
    clock: &dyn Clock,
) -> Result<RelayVerdict, StoreError> {
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
    use std::sync::Mutex;

    use lash_sansio::SessionId;

    use super::*;
    use crate::store::{ClaimToken, StalledObligation, UndecodableObligation};
    use crate::testing::TestClock;

    /// A ledger that hands out one scripted page and records every
    /// settlement, so the relay's own handling of a page is observable.
    struct PageLedger {
        page: Mutex<Vec<ClaimedObligation>>,
        settled: Mutex<Vec<(ObligationId, ObligationSettlement)>>,
    }

    #[async_trait::async_trait]
    impl ObligationLedger for PageLedger {
        fn kind(&self) -> ObligationKind {
            ObligationKind::SessionDelete
        }

        async fn arm(
            &self,
            _key: &ObligationKey,
            _now_ms: u64,
        ) -> Result<Option<ObligationId>, StoreError> {
            Ok(None)
        }

        async fn claim_due(
            &self,
            _now_ms: u64,
            _claim_ttl_ms: u64,
            _limit: NonZeroUsize,
        ) -> Result<Vec<ClaimedObligation>, StoreError> {
            Ok(std::mem::take(
                &mut *self
                    .page
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ))
        }

        async fn claim(
            &self,
            _id: &ObligationId,
            _now_ms: u64,
            _claim_ttl_ms: u64,
        ) -> Result<Option<ClaimedObligation>, StoreError> {
            Ok(None)
        }

        async fn settle(
            &self,
            id: &ObligationId,
            _token: &ClaimToken,
            settlement: ObligationSettlement,
            _now_ms: u64,
        ) -> Result<SettleOutcome, StoreError> {
            self.settled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id.clone(), settlement));
            Ok(SettleOutcome::Applied)
        }

        async fn rearm(&self, _id: &ObligationId, _now_ms: u64) -> Result<bool, StoreError> {
            Ok(false)
        }

        async fn list_stalled(
            &self,
            _after: Option<&ObligationId>,
            _limit: NonZeroUsize,
        ) -> Result<Vec<StalledObligation>, StoreError> {
            Ok(Vec::new())
        }

        async fn count_stalled(&self) -> Result<u64, StoreError> {
            Ok(0)
        }

        async fn standing(
            &self,
            _id: &ObligationId,
        ) -> Result<Option<crate::store::ObligationStanding>, StoreError> {
            Ok(None)
        }
    }

    struct AlwaysDelivers(PageLedger);

    #[async_trait::async_trait]
    impl ObligationRelay for AlwaysDelivers {
        fn ledger(&self) -> &dyn ObligationLedger {
            &self.0
        }

        async fn deliver(
            &self,
            _id: &ObligationId,
            _key: &ObligationKey,
            _attempt: u32,
        ) -> Result<(), DeliveryFailure> {
            Ok(())
        }
    }

    fn claimed(id: &str, key: Result<ObligationKey, UndecodableObligation>) -> ClaimedObligation {
        ClaimedObligation {
            id: ObligationId::new(id),
            token: ClaimToken::new(format!("token-{id}")),
            attempts: 1,
            key,
        }
    }

    fn session_delete(session: &str) -> Result<ObligationKey, UndecodableObligation> {
        Ok(ObligationKey::SessionDelete {
            session_id: SessionId::from(session),
        })
    }

    #[tokio::test]
    async fn a_row_whose_key_does_not_decode_stalls_and_the_page_goes_on() {
        let relay = AlwaysDelivers(PageLedger {
            page: Mutex::new(vec![
                claimed("before", session_delete("s-before")),
                claimed(
                    "poison",
                    Err(UndecodableObligation {
                        detail: "key column `session_id` is Integer(7), not text".to_owned(),
                    }),
                ),
                claimed("after", session_delete("s-after")),
            ]),
            settled: Mutex::new(Vec::new()),
        });
        let clock = TestClock::new(1_000);
        let pass = relay_due(&relay, &clock, NonZeroUsize::MIN)
            .await
            .expect("a pass over a page with an undecodable row");
        assert_eq!(
            pass,
            RelayPass {
                claimed: 3,
                delivered: 2,
                stalled: 1,
                ..RelayPass::default()
            }
        );
        let settled = relay
            .0
            .settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            settled,
            vec![
                (ObligationId::new("before"), ObligationSettlement::Delivered),
                (
                    ObligationId::new("poison"),
                    ObligationSettlement::Stall {
                        reason: StallReason::Undecodable,
                        error: "key column `session_id` is Integer(7), not text".to_owned(),
                    }
                ),
                (ObligationId::new("after"), ObligationSettlement::Delivered),
            ]
        );
    }

    /// A kind whose consumer settles its deliveries (ingress): it records
    /// the attempt each ask named.
    struct ConsumerSettled {
        ledger: PageLedger,
        asked: Mutex<Vec<(ObligationId, u32)>>,
    }

    #[async_trait::async_trait]
    impl ObligationRelay for ConsumerSettled {
        fn ledger(&self) -> &dyn ObligationLedger {
            &self.ledger
        }

        fn consumer_settles(&self) -> bool {
            true
        }

        async fn deliver(
            &self,
            id: &ObligationId,
            _key: &ObligationKey,
            attempt: u32,
        ) -> Result<(), DeliveryFailure> {
            self.asked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id.clone(), attempt));
            Ok(())
        }
    }

    /// ADR 0109 §3 (D19): an ask the engine accepted leaves the claim for the
    /// consumer's own transaction, never settling it here; a retaken claim
    /// asks under its own attempt, and one retaken past the ceiling stalls
    /// typed without asking again.
    #[tokio::test]
    async fn a_consumer_settled_ask_keeps_its_claim_and_stalls_past_the_ceiling() {
        let ceiling = RelayPolicy::default().attempt_ceiling.get();
        let with_attempts = |id: &str, attempts: u32| ClaimedObligation {
            attempts,
            ..claimed(id, session_delete("s"))
        };
        let relay = ConsumerSettled {
            ledger: PageLedger {
                page: Mutex::new(vec![
                    with_attempts("asked", 3),
                    with_attempts("spent", ceiling + 1),
                ]),
                settled: Mutex::new(Vec::new()),
            },
            asked: Mutex::new(Vec::new()),
        };
        let clock = TestClock::new(1_000);
        let pass = relay_due(&relay, &clock, NonZeroUsize::MIN)
            .await
            .expect("a pass over a consumer-settled page");
        assert_eq!(
            pass,
            RelayPass {
                claimed: 2,
                requested: 1,
                stalled: 1,
                ..RelayPass::default()
            }
        );
        assert_eq!(
            *relay
                .asked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(ObligationId::new("asked"), 3)],
            "only the claim within the ceiling asked, under its attempt"
        );
        assert_eq!(
            *relay
                .ledger
                .settled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(
                ObligationId::new("spent"),
                ObligationSettlement::Stall {
                    reason: StallReason::AttemptsExhausted,
                    error: format!(
                        "the engine accepted {ceiling} asks and nothing admitted the row"
                    ),
                }
            )],
            "the accepted ask settled nothing; the spent claim stalled"
        );
    }

    struct NeverYet(PageLedger);

    #[async_trait::async_trait]
    impl ObligationRelay for NeverYet {
        fn ledger(&self) -> &dyn ObligationLedger {
            &self.0
        }

        async fn deliver(
            &self,
            _id: &ObligationId,
            _key: &ObligationKey,
            _attempt: u32,
        ) -> Result<(), DeliveryFailure> {
            Err(DeliveryFailure::NotYet)
        }
    }

    /// ADR 0113 §2.5: a guard that is not owed yet defers at the maximum
    /// backoff with its attempts reset, and never stalls however many claims
    /// it has taken.
    #[tokio::test]
    async fn a_delivery_not_owed_yet_defers_at_the_maximum_backoff_and_never_stalls() {
        let ceiling = RelayPolicy::default().attempt_ceiling.get();
        let relay = NeverYet(PageLedger {
            page: Mutex::new(vec![ClaimedObligation {
                attempts: ceiling + 5,
                ..claimed("guard", session_delete("s"))
            }]),
            settled: Mutex::new(Vec::new()),
        });
        let clock = TestClock::new(1_000);
        let pass = relay_due(&relay, &clock, NonZeroUsize::MIN)
            .await
            .expect("a pass over a guard that is not owed yet");
        assert_eq!(
            pass,
            RelayPass {
                claimed: 1,
                ..RelayPass::default()
            }
        );
        assert_eq!(
            *relay
                .0
                .settled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![(
                ObligationId::new("guard"),
                ObligationSettlement::Defer {
                    due_at_ms: 1_000 + RelayPolicy::default().max_backoff_ms,
                }
            )]
        );
    }

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
