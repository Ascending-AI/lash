//! Ingress delivery (ADR 0109 §3, FIG-3851): every admitted turn input and
//! queued-work batch owes its session a drive.
//!
//! The admission transaction arms the row's ingress obligation. The producer
//! then delivers it at once — claim, ask the engine for the drive, settle —
//! and the reconcile tick's relay pass retries whatever that attempt did not
//! deliver, through the ingress ledger's due index, with capped exponential
//! backoff until the attempt ceiling stalls it. Nothing scans the session
//! catalog for undriven input.
//!
//! The drive an ingress row asks for is [`ingress_drive_request`]: one
//! request per row, so the engine dedupes a redelivery of the same row, while
//! two rows never share an ask a finishing drive could swallow. A waiter on
//! an input attaches to the same request.

use std::sync::Arc;

use super::relay::{DeliveryFailure, ObligationRelay, RelayPolicy, RelayVerdict, deliver_now};
use crate::engine::EngineRefusal;
pub use crate::engine::ingress_drive_request;
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

    /// Deliver the obligation the admission of item `item_id` armed, right
    /// after that admission committed (ADR 0109 §1.8: attempted before the
    /// producer's call returns). A delivery that fails is left to the relay:
    /// the admission stands, and the row stays due.
    pub async fn deliver_admitted(&self, item_id: &str) {
        let id = ingress_obligation_id(item_id);
        match deliver_now(self, &id, self.clock.as_ref()).await {
            Ok(RelayVerdict::Delivered | RelayVerdict::NotDue | RelayVerdict::ClaimLost) => {}
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

    /// Ask the row's session for `ingress:{item_id}`. The engine accepting
    /// the ask delivers the obligation; the drive admits whatever the
    /// session holds, this row included.
    async fn deliver(
        &self,
        _id: &ObligationId,
        key: &ObligationKey,
    ) -> Result<(), DeliveryFailure> {
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
            .request_drive(session_id, ingress_drive_request(item_id))
            .await
            .map_err(delivery_failure)
    }
}
