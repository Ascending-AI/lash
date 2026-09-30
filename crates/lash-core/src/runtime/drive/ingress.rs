//! Ingress delivery (ADR 0109 §3, FIG-3851): every admitted turn input and
//! queued-work batch owes its session a drive.
//!
//! The admission transaction arms the row's ingress obligation. The producer
//! then asks for the drive at once — claim, ask the engine — and the reconcile
//! tick's relay pass retries whatever that attempt did not reach the engine,
//! through the ingress ledger's due index, with capped exponential backoff
//! until the attempt ceiling stalls it. Nothing scans the session catalog
//! for undriven input.
//!
//! The engine accepting the ask does not deliver the obligation: the drive's
//! admission of the row does, in the admission's own transaction.
//! The relay's claim covers only the ask and the admission after it. A claim
//! that lapses because nothing admitted the row (the engine lost the drive)
//! is retaken and asked again under its next attempt,
//! [`ingress_drive_request`], since the engine dedupes a reused request
//! against the invocation it lost. A waiter on an input attaches to the
//! drive of the current attempt ([`IngressRelay::current_ask`]), and follows
//! the ask to its next attempt when the engine lost the one it waited on.

use std::sync::Arc;

use super::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy, RelayVerdict,
    deliver_claimed, deliver_now,
};
use crate::engine::EngineRefusal;
pub use crate::engine::{FIRST_INGRESS_ATTEMPT, ingress_drive_request};
use crate::store::ingress_obligation::ingress_obligation_id;
use crate::store::{ObligationId, ObligationKey, ObligationKind, ObligationLedger};
use crate::{Clock, SessionWorkEngine, StoreError};

/// The ingress kind's relay: its ledger, the engine it asks for drives, and
/// the clock its settlements are stamped by.
#[derive(Clone)]
pub struct IngressRelay {
    ledger: Arc<dyn ObligationLedger>,
    work: Arc<dyn SessionWorkEngine>,
    clock: Arc<dyn Clock>,
    policy: RelayPolicy,
}

impl IngressRelay {
    /// The relay over `ledger` (the store set's ingress ledger) that asks
    /// `work` for drives.
    #[must_use]
    pub fn new(
        ledger: Arc<dyn ObligationLedger>,
        work: Arc<dyn SessionWorkEngine>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            ledger,
            work,
            clock,
            policy: RelayPolicy::default(),
        }
    }

    /// The same relay under `policy` rather than the kind's default (a host
    /// lever, ADR 0014).
    #[must_use]
    pub fn with_policy(mut self, policy: RelayPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The relay of `backend`'s ingress ledger that asks `work` for drives.
    #[must_use]
    pub fn over_backend(
        backend: &crate::Backend,
        work: Arc<dyn SessionWorkEngine>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(
            backend.obligation_ledger(ObligationKind::Ingress),
            work,
            clock,
        )
    }

    /// The ingress obligation of item `item_id`, when its delivery stalled:
    /// the relay's attempts ran out, or the engine refused it for good.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn stalled(
        &self,
        item_id: &str,
    ) -> Result<Option<crate::store::StalledObligation>, StoreError> {
        crate::store::ingress_obligation::stalled_obligation(
            self.ledger.as_ref(),
            &ingress_obligation_id(item_id),
        )
        .await
    }

    /// The drive the relay last asked for item `item_id`:
    /// `ingress:{item_id}:{attempt}`, the attempt being its latest claim's,
    /// while that claim stands or once an admission delivered the row.
    /// `None` while no ask is outstanding — the row is due for a later
    /// attempt, or stalled — or before any claim. A waiter follows it: after
    /// the engine loses an ask (an operator kill), the relay asks again under
    /// the next attempt.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn current_ask(
        &self,
        item_id: &str,
    ) -> Result<Option<crate::engine::DriveRequestId>, StoreError> {
        use crate::store::ObligationState;
        Ok(self
            .ledger
            .standing(&ingress_obligation_id(item_id))
            .await?
            .filter(|standing| {
                standing.attempts > 0
                    && matches!(
                        standing.state,
                        ObligationState::Claimed | ObligationState::Delivered
                    )
            })
            .map(|standing| ingress_drive_request(item_id, standing.attempts)))
    }

    /// The claim TTL a fused admission stamps on the ingress claims it takes
    /// inside its commit (FIG-3975): the relay's own TTL, so the claim
    /// outlives the ask it precedes exactly as [`deliver_now`]'s would.
    pub fn claim_ttl_ms(&self) -> u64 {
        self.policy.claim_ttl_ms
    }

    /// Ask for the drive `claimed` — taken by the admission's own
    /// transaction (FIG-3975) — owes, settling it as [`deliver_admitted`]
    /// would after its own claim.
    pub async fn deliver_claimed(&self, claimed: crate::store::ClaimedObligation) {
        let id = claimed.id.clone();
        match deliver_claimed(self, claimed, self.clock.as_ref()).await {
            Ok(
                RelayVerdict::Requested
                | RelayVerdict::Delivered
                | RelayVerdict::NotDue
                | RelayVerdict::ClaimLost,
            ) => {}
            Ok(verdict) => tracing::debug!(
                obligation_id = id.as_str(),
                ?verdict,
                "admitted ingress was not delivered at once; the relay retries it"
            ),
            Err(error) => report_store_failure(&id, &error),
        }
    }

    /// Ask for the drive the admission of item `item_id` owes, right after
    /// that admission committed (ADR 0109 §1.8: attempted before the
    /// producer's call returns). An ask that fails is left to the relay: the
    /// admission stands, and the row stays due.
    pub async fn deliver_admitted(&self, item_id: &str) {
        let id = ingress_obligation_id(item_id);
        match deliver_now(self, &id, self.clock.as_ref()).await {
            Ok(
                RelayVerdict::Requested
                | RelayVerdict::Delivered
                | RelayVerdict::NotDue
                | RelayVerdict::ClaimLost,
            ) => {}
            Ok(verdict) => tracing::debug!(
                obligation_id = id.as_str(),
                ?verdict,
                "admitted ingress was not delivered at once; the relay retries it"
            ),
            Err(error) => report_store_failure(&id, &error),
        }
    }
}

fn report_store_failure(id: &ObligationId, error: &StoreError) {
    tracing::warn!(
        obligation_id = id.as_str(),
        error = %error,
        "admitted ingress could not be claimed for immediate delivery; the relay retries it"
    );
}

/// The engine's refusal as a delivery failure: a retryable refusal is worth
/// another attempt after the backoff, a permanent one stalls the row.
fn delivery_failure(refusal: EngineRefusal) -> DeliveryFailure {
    match refusal {
        EngineRefusal::Retryable(message) => DeliveryFailure::Retryable(message),
        EngineRefusal::Permanent { code, message } => {
            DeliveryFailure::Refused(format!("{code}: {message}"))
        }
        other => DeliveryFailure::Retryable(other.to_string()),
    }
}

#[async_trait::async_trait]
impl ObligationRelay for IngressRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    /// The drive's claim of the row settles its obligation (ADR 0109 §3).
    fn consumer_settles(&self) -> bool {
        true
    }

    /// Ask the row's session for `ingress:{item_id}:{attempt}`. The drive
    /// admits whatever the session holds, this row included, and its claim
    /// of the row settles the obligation.
    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery { key, attempt, .. } = delivery;
        let ObligationKey::Ingress {
            session_id,
            item_id,
        } = key
        else {
            return Err(DeliveryFailure::Undecodable(format!(
                "the ingress relay was handed a {} obligation",
                key.kind()
            )));
        };
        self.work
            .request_drive(session_id, ingress_drive_request(item_id, attempt))
            .await
            .map_err(delivery_failure)
    }
}
