//! The `ParentEnd` obligation's relay (ADR 0109 §1.4, FIG-3853): the engine
//! half that delivers an ended scope's plan to its live children.
//!
//! A parent-end ledger row is its obligation: the claim names the row by its
//! `(parent_kind, parent_id)` key, and the delivery is the same
//! [`apply_parent_end_plan`] the ending execution runs on its live path —
//! every idempotency the live path carries (the per-child delivery key, the
//! first-request-wins cancel, the once-only settle) is the claim's too, so a
//! retaken or repeated claim applies the same plan as a no-op.
//!
//! Failure kinds, by the one settle rule: a malformed row — a key the ledger
//! could not decode, a `parent_payload` this build cannot read — is
//! `undecodable` and stalls that row alone; a refusal the store classifies
//! terminal is `refused` and stalls; every other failure is `retryable` and
//! backs off under the kind's policy. None of them aborts the due page the
//! claim read.

use std::sync::Arc;

use super::relay::{DeliveryFailure, ObligationRelay, RelayPolicy};
use crate::store::{ObligationId, ObligationKey, ObligationLedger};
use crate::{Clock, PluginError, ProcessRegistry, ProcessWorkSubstrate, apply_parent_end_plan};

/// The `ParentEnd` relay: the kind's ledger, plus the registry and process
/// port its delivery applies through.
pub struct ParentEndRelay {
    ledger: Arc<dyn ObligationLedger>,
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
    clock: Arc<dyn Clock>,
    policy: RelayPolicy,
}

impl ParentEndRelay {
    /// The relay over `ledger`, delivering plans from `registry` through
    /// `port`. `policy` defaults to the kind's shared policy.
    #[must_use]
    pub fn new(
        ledger: Arc<dyn ObligationLedger>,
        registry: Arc<dyn ProcessRegistry>,
        port: Arc<dyn ProcessWorkSubstrate>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            ledger,
            registry,
            port,
            clock,
            policy: RelayPolicy::default(),
        }
    }

    /// The same relay under a non-default policy (a host lever, ADR 0014).
    #[must_use]
    pub fn with_policy(mut self, policy: RelayPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// How `PluginError` maps to the settle rule: a row the store cannot decode
/// is `undecodable` (stall that row alone); a refusal retrying cannot repair
/// is `refused` (stall); anything else is `retryable`.
fn classify(context: &'static str) -> impl Fn(PluginError) -> DeliveryFailure {
    move |error| match &error {
        PluginError::StoredDataCorrupt { message, .. } => {
            DeliveryFailure::Undecodable(message.clone())
        }
        _ if error.is_terminal() => DeliveryFailure::Refused(error.to_string()),
        _ => DeliveryFailure::Retryable(format!("{context}: {error}")),
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ParentEndRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    /// Apply the plan `key`'s row carries: read it back by the claim's
    /// columns — this read is where the typed payload decodes, so a row this
    /// build cannot read is the undecodable case — then run the same body
    /// the producer's own apply runs.
    ///
    /// `id` is the claim's fencing identity, not the delivery's: the
    /// delivery dedupes on the plan's own per-child keys, so a second claim
    /// over one row applies once.
    async fn deliver(
        &self,
        _id: &ObligationId,
        key: &ObligationKey,
        _attempt: u32,
    ) -> Result<(), DeliveryFailure> {
        let ObligationKey::ParentEnd {
            parent_kind,
            parent_id,
        } = key
        else {
            return Err(DeliveryFailure::Undecodable(format!(
                "a {} key cannot name a ParentEnd row",
                key.kind().label()
            )));
        };
        let plan = self
            .registry
            .get_parent_end_plan_by_key(parent_kind, parent_id)
            .await
            .map_err(classify("parent-end row read"))?
            .ok_or_else(|| {
                DeliveryFailure::Refused(format!(
                    "no parent-end row carries {parent_kind} `{parent_id}`"
                ))
            })?;
        // A settled plan is already applied: the settle that applied it ran
        // while this claim was outstanding, so delivering is the no-op the
        // claim's settle marks `delivered`.
        if plan.settled_at_ms.is_some() {
            return Ok(());
        }
        apply_parent_end_plan(
            self.registry.as_ref(),
            self.port.as_ref(),
            &plan.parent,
            self.clock.timestamp_ms(),
        )
        .await
        .map(drop)
        .map_err(classify("parent-end apply"))
    }
}

#[cfg(test)]
mod tests {
    //! The relay mechanics the `ParentEnd` obligations rely on (ADR 0109
    //! §1.4, FIG-3853): one failed claim never stops its page, a retryable
    //! failure backs off to its ceiling and stalls, and a settle another
    //! claim won is counted, never an error.
    //!
    //! The ledger is scripted so each law states the claims and settle
    //! outcomes it runs the generic loop over; the stores' own legs prove
    //! the SQL side of the same contract.

    use std::collections::VecDeque;
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::sync::Mutex;

    use super::*;
    use crate::engine::RelayPass;
    use crate::runtime::drive::relay::{RelayVerdict, deliver_now, relay_due};
    use crate::store::{
        ClaimToken, ClaimedObligation, ObligationKind, ObligationSettlement, SettleOutcome,
        StallReason, StalledObligation, StoreError, UndecodableObligation,
    };
    use crate::testing::TestClock;

    fn claim(id: &str, attempts: u32, key: &str) -> ClaimedObligation {
        ClaimedObligation {
            id: ObligationId::new(id),
            token: ClaimToken::new(format!("token-{id}")),
            attempts,
            key: Ok(ObligationKey::ParentEnd {
                parent_kind: "process".to_string(),
                parent_id: key.to_string(),
            }),
        }
    }

    fn undecodable_claim(id: &str) -> ClaimedObligation {
        ClaimedObligation {
            id: ObligationId::new(id),
            token: ClaimToken::new(format!("token-{id}")),
            attempts: 1,
            key: Err(UndecodableObligation {
                detail: format!("{id} cannot be read by this build"),
            }),
        }
    }

    /// A ledger whose claims and settle outcomes are scripted for one pass.
    struct ScriptedLedger {
        due: Mutex<VecDeque<ClaimedObligation>>,
        settle_outcome: Mutex<SettleOutcome>,
        settles: Mutex<Vec<(ObligationId, ObligationSettlement)>>,
    }

    impl ScriptedLedger {
        fn due(&self, claims: Vec<ClaimedObligation>) {
            self.due.lock().expect("due queue").extend(claims);
        }

        fn settle_outcome(&self, outcome: SettleOutcome) {
            *self.settle_outcome.lock().expect("settle outcome") = outcome;
        }

        fn settles(&self) -> Vec<(ObligationId, ObligationSettlement)> {
            self.settles.lock().expect("settles").clone()
        }
    }

    #[async_trait::async_trait]
    impl ObligationLedger for ScriptedLedger {
        fn kind(&self) -> ObligationKind {
            ObligationKind::ParentEnd
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
            Ok(self.due.lock().expect("due queue").drain(..).collect())
        }

        async fn claim(
            &self,
            id: &ObligationId,
            _now_ms: u64,
            _claim_ttl_ms: u64,
        ) -> Result<Option<ClaimedObligation>, StoreError> {
            let index = self
                .due
                .lock()
                .expect("due queue")
                .iter()
                .position(|claimed| &claimed.id == id);
            Ok(index.and_then(|index| self.due.lock().expect("due queue").remove(index)))
        }

        async fn settle(
            &self,
            id: &ObligationId,
            _token: &ClaimToken,
            settlement: ObligationSettlement,
            _now_ms: u64,
        ) -> Result<SettleOutcome, StoreError> {
            self.settles
                .lock()
                .expect("settles")
                .push((id.clone(), settlement));
            Ok(*self.settle_outcome.lock().expect("settle outcome"))
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

    /// A relay over the scripted ledger whose `deliver` answers by the id it
    /// is handed, recording every call.
    struct ScriptedRelay {
        ledger: ScriptedLedger,
        deliveries: Mutex<Vec<ObligationId>>,
        outcome: Mutex<Result<(), DeliveryFailure>>,
    }

    impl ScriptedRelay {
        fn new() -> Self {
            Self {
                ledger: ScriptedLedger {
                    due: Mutex::new(VecDeque::new()),
                    settle_outcome: Mutex::new(SettleOutcome::Applied),
                    settles: Mutex::new(Vec::new()),
                },
                deliveries: Mutex::new(Vec::new()),
                outcome: Mutex::new(Ok(())),
            }
        }

        fn fails_with(&self, outcome: DeliveryFailure) {
            *self.outcome.lock().expect("outcome") = Err(outcome);
        }

        fn delivered(&self) -> Vec<ObligationId> {
            self.deliveries.lock().expect("deliveries").clone()
        }
    }

    #[async_trait::async_trait]
    impl ObligationRelay for ScriptedRelay {
        fn ledger(&self) -> &dyn ObligationLedger {
            &self.ledger
        }

        fn policy(&self) -> RelayPolicy {
            RelayPolicy {
                base_backoff_ms: 10_000,
                max_backoff_ms: 900_000,
                attempt_ceiling: NonZeroU32::new(3).unwrap_or(NonZeroU32::MIN),
                claim_ttl_ms: 60_000,
            }
        }

        async fn deliver(
            &self,
            id: &ObligationId,
            _key: &ObligationKey,
            _attempt: u32,
        ) -> Result<(), DeliveryFailure> {
            self.deliveries.lock().expect("deliveries").push(id.clone());
            self.outcome.lock().expect("outcome").clone()
        }
    }

    #[tokio::test]
    async fn a_page_attempts_every_claim_even_after_failures() {
        let relay = ScriptedRelay::new();
        let clock = TestClock::new(1_000);
        relay.ledger.due(vec![
            claim("obligation:one", 1, "one"),
            claim("obligation:two", 1, "two"),
            undecodable_claim("obligation:three"),
            claim("obligation:four", 1, "four"),
        ]);
        // A refusal for every deliverable row: one, two and four stall
        // refused, three stalls undecodable on its claim, and the page
        // completes — a failed claim never stops the rows behind it.
        relay.fails_with(DeliveryFailure::Refused(
            "the engine refused it for good".to_string(),
        ));
        let pass = relay_due(
            &relay,
            &clock,
            NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN),
        )
        .await
        .expect("the scripted pass never errors");
        assert_eq!(
            pass,
            RelayPass {
                claimed: 4,
                delivered: 0,
                requested: 0,
                retried: 0,
                stalled: 4,
                claim_lost: 0,
            }
        );
        assert_eq!(
            relay.delivered(),
            vec![
                ObligationId::new("obligation:one"),
                ObligationId::new("obligation:two"),
                ObligationId::new("obligation:four")
            ],
            "the undecodable claim is settled without a deliver, and every other row is attempted"
        );
        let reasons: Vec<StallReason> = relay
            .ledger
            .settles()
            .iter()
            .map(|(_, settlement)| match settlement {
                ObligationSettlement::Stall { reason, .. } => *reason,
                _ => panic!("a failure settles a stall, not {settlement:?}"),
            })
            .collect();
        assert_eq!(
            reasons,
            vec![
                StallReason::Refused,
                StallReason::Refused,
                StallReason::Undecodable,
                StallReason::Refused
            ]
        );
    }

    #[tokio::test]
    async fn a_retryable_failure_at_the_attempt_ceiling_stalls_instead_of_retrying() {
        let relay = ScriptedRelay::new();
        let clock = TestClock::new(1_000);
        relay.fails_with(DeliveryFailure::Retryable("the engine is down".to_string()));
        // Attempt 1 hands the row back under the policy's backoff; attempt 3
        // meets the ceiling and stalls instead.
        relay.ledger.due(vec![claim("obligation:a", 1, "a")]);
        let pass = relay_due(&relay, &clock, NonZeroUsize::MIN)
            .await
            .expect("the scripted pass never errors");
        assert_eq!(pass.retried, 1);
        assert_eq!(
            relay.ledger.settles()[0].1,
            ObligationSettlement::Retry {
                due_at_ms: 11_000,
                error: "the engine is down".to_string(),
            }
        );

        relay.ledger.due(vec![claim("obligation:a", 3, "a")]);
        let pass = relay_due(&relay, &clock, NonZeroUsize::MIN)
            .await
            .expect("the scripted pass never errors");
        assert_eq!(pass.stalled, 1);
        assert_eq!(
            relay.ledger.settles()[1].1,
            ObligationSettlement::Stall {
                reason: StallReason::AttemptsExhausted,
                error: "the engine is down".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn immediate_delivery_claims_only_a_due_obligation() {
        let relay = ScriptedRelay::new();
        let clock = TestClock::new(1_000);
        let verdict = deliver_now(&relay, &ObligationId::new("obligation:absent"), &clock)
            .await
            .expect("a missing claim is a verdict, not an error");
        assert_eq!(verdict, RelayVerdict::NotDue);
        assert!(relay.delivered().is_empty(), "nothing was claimed");

        relay.ledger.due(vec![claim("obligation:now", 1, "now")]);
        let verdict = deliver_now(&relay, &ObligationId::new("obligation:now"), &clock)
            .await
            .expect("the due claim delivers");
        assert_eq!(verdict, RelayVerdict::Delivered);
        assert_eq!(relay.delivered(), vec![ObligationId::new("obligation:now")]);
    }

    #[tokio::test]
    async fn a_settle_another_claim_won_is_claim_lost_not_an_error() {
        let relay = ScriptedRelay::new();
        let clock = TestClock::new(1_000);
        relay
            .ledger
            .due(vec![claim("obligation:raced", 1, "raced")]);
        relay.ledger.settle_outcome(SettleOutcome::ClaimLost);
        let pass = relay_due(&relay, &clock, NonZeroUsize::MIN)
            .await
            .expect("the scripted pass never errors");
        assert_eq!(
            pass,
            RelayPass {
                claimed: 1,
                claim_lost: 1,
                ..RelayPass::default()
            }
        );
    }
}
