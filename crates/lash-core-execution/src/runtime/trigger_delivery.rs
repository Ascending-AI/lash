//! Recovery of a reserved trigger delivery through its obligation (ADR 0109,
//! ADR 0021's FIG-4090 amendment).
//!
//! The reserving transaction arms each delivery's `TriggerDelivery`
//! obligation, and the write that binds the delivery to its process delivers
//! it. An emit that starts its deliveries at once binds them before it
//! returns; a crash between the reservation and the bind leaves the row owed,
//! and this relay starts and binds it from the reservation the store holds.
//! Nothing re-emits the occurrence: a replayed emit would only find the
//! reservation already held.

use crate::runtime::obligations::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy, plugin_delivery_error,
};
use crate::store::{ObligationKey, ObligationKind, ObligationLedger};
use crate::triggers::{TriggerDeliveryRecoveryError, TriggerRouter};
use std::sync::Arc;

/// The `TriggerDelivery` kind's relay: a claimed delivery is started from its
/// reservation and bound, and the bind settles the row.
pub struct TriggerDeliveryRelay {
    ledger: Arc<dyn ObligationLedger>,
    router: TriggerRouter,
    policy: RelayPolicy,
    metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
}

impl TriggerDeliveryRelay {
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// A relay over the trigger store's delivery ledger, starting through
    /// `router`: the router must be wired the way the deployment's emits are
    /// (its env store and engines), so a recovered start registers the process
    /// a first attempt would have.
    pub fn new(ledger: Arc<dyn ObligationLedger>, router: TriggerRouter) -> Self {
        Self {
            ledger,
            router,
            policy: RelayPolicy::default(),
            metrics: Default::default(),
        }
    }

    /// The same relay under `policy` rather than the kind's default (a host
    /// lever, ADR 0014).
    #[must_use]
    pub fn with_policy(mut self, policy: RelayPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[async_trait::async_trait]
impl ObligationRelay for TriggerDeliveryRelay {
    fn metrics(&self) -> lash_trace::telemetry::metrics::TelemetryMetrics {
        self.metrics.clone()
    }

    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery { key, .. } = delivery;
        let ObligationKey::TriggerDelivery {
            occurrence_id,
            subscription_id,
        } = key
        else {
            return Err(DeliveryFailure::key_mismatch(
                ObligationKind::TriggerDelivery,
                key,
            ));
        };
        match Box::pin(self.router.recover_delivery(occurrence_id, subscription_id)).await {
            Ok(_) => Ok(()),
            Err(TriggerDeliveryRecoveryError::Refused(error)) => {
                Err(DeliveryFailure::Refused(plugin_delivery_error(error)))
            }
            Err(TriggerDeliveryRecoveryError::Retryable(error)) => {
                Err(DeliveryFailure::Retryable(plugin_delivery_error(error)))
            }
        }
    }
}
