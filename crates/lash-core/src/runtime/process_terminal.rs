//! The `ProcessTerminal` relay (ADR 0109 §3): a terminal process owes its
//! engine waiters its terminal.
//!
//! Every transaction that makes a process terminal arms the obligation on the
//! process row. The execution that stored the terminal publishes it itself —
//! on Restate it resolves the process's terminal promise in the journal that
//! stored the terminal — and settles the row delivered. When that execution
//! stops between the terminal commit and the publication (killed, paused, or
//! its journal refused), the waiters parked on the engine would be stranded:
//! this relay takes the due row and publishes the stored terminal through the
//! engine's port instead.
//!
//! The relay is engine-neutral. Its delivery reads the stored terminal and
//! hands it to [`ProcessWorkSubstrate::publish_process_terminal`], keyed by
//! the obligation id, which the engine dedupes on.

use std::sync::Arc;

use crate::runtime::shift::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay, RelayPolicy,
};
use crate::store::{ObligationKey, ObligationKind, ObligationLedger};
use crate::{PluginError, ProcessRegistry, ProcessWorkSubstrate};

/// The `ProcessTerminal` relay over one deployment's process registry and
/// process-work port.
pub struct ProcessTerminalRelay {
    ledger: Arc<dyn ObligationLedger>,
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
    policy: RelayPolicy,
}

impl ProcessTerminalRelay {
    /// The relay over `ledger`, the `ProcessTerminal` ledger of the storage
    /// `registry` keeps its rows in, publishing through `port`.
    pub fn new(
        ledger: Arc<dyn ObligationLedger>,
        registry: Arc<dyn ProcessRegistry>,
        port: Arc<dyn ProcessWorkSubstrate>,
    ) -> Self {
        Self {
            ledger,
            registry,
            port,
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
}

#[async_trait::async_trait]
impl ObligationRelay for ProcessTerminalRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    fn policy(&self) -> RelayPolicy {
        self.policy
    }

    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery { id, key, .. } = delivery;
        let ObligationKey::ProcessTerminal { process_id } = key else {
            return Err(DeliveryFailure::key_mismatch(
                ObligationKind::ProcessTerminal,
                key,
            ));
        };
        let record = match self.registry.get_process(process_id).await {
            Ok(record) => record,
            // Retention reclaimed the process: nothing is left to publish and
            // no waiter is left to strand.
            Err(PluginError::ProcessNoLongerRetained { .. }) => None,
            Err(error) => return Err(DeliveryFailure::of_plugin(error)),
        };
        let Some(record) = record else {
            return Ok(());
        };
        let Some(output) = record.outcome() else {
            return Err(DeliveryFailure::row_invariant(format!(
                "process `{process_id}` owes a terminal publication but stores no terminal"
            )));
        };
        self.port
            .publish_process_terminal(process_id, &output, id.as_str())
            .await
            .map_err(DeliveryFailure::of_plugin)
    }
}
