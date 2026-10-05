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

use super::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy, plugin_delivery_error,
};
use crate::store::{DeliveryError, ObligationKey, ObligationKind, ObligationLedger};
use crate::{
    Clock, PluginError, ProcessRegistry, ProcessWorkSubstrate, RuntimeErrorCode,
    apply_parent_end_plan,
};

/// The `ParentEnd` relay: the kind's ledger, plus the registry and process
/// port its delivery applies through.
pub struct ParentEndRelay {
    ledger: Arc<dyn ObligationLedger>,
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
    clock: Arc<dyn Clock>,
    policy: RelayPolicy,
    metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
}

impl ParentEndRelay {
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: lash_trace::telemetry::metrics::TelemetryMetrics,
    ) -> Self {
        self.metrics = metrics;
        self
    }

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
            metrics: Default::default(),
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
        PluginError::StoredDataCorrupt { message, .. } => DeliveryFailure::Undecodable(
            DeliveryError::new(RuntimeErrorCode::RuntimeStoreCorrupt, message.clone()),
        ),
        _ if error.is_terminal() => DeliveryFailure::Refused(plugin_delivery_error(error)),
        _ => DeliveryFailure::Retryable(plugin_delivery_error(error).in_context(context)),
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ParentEndRelay {
    fn metrics(&self) -> lash_trace::telemetry::metrics::TelemetryMetrics {
        self.metrics.clone()
    }

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
    /// The delivery's id and token are the claim's fencing identity, not
    /// the delivery's: it dedupes on the plan's own per-child keys, so a
    /// second claim over one row applies once.
    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery { key, .. } = delivery;
        let ObligationKey::ParentEnd {
            parent_kind,
            parent_id,
        } = key
        else {
            return Err(DeliveryFailure::key_mismatch(
                ObligationKind::ParentEnd,
                key,
            ));
        };
        let plan = self
            .registry
            .get_parent_end_plan_by_key(parent_kind, parent_id)
            .await
            .map_err(classify("parent-end row read"))?
            .ok_or_else(|| {
                DeliveryFailure::row_invariant(format!(
                    "no parent-end row carries {parent_kind} `{parent_id}`"
                ))
            })?;
        // A settled plan is already applied: the settle that applied it ran
        // while this claim was outstanding, so delivering is the no-op the
        // claim's settle marks `delivered`.
        if plan.obligation_state == crate::store::ObligationState::Delivered {
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
