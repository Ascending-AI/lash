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

use crate::runtime::drive::relay::{DeliveryFailure, ObligationDelivery, ObligationRelay};
use crate::store::{ObligationKey, ObligationLedger};
use crate::{PluginError, ProcessRegistry, ProcessWorkSubstrate};

/// The `ProcessTerminal` relay over one deployment's process registry and
/// process-work port.
pub struct ProcessTerminalRelay {
    ledger: Arc<dyn ObligationLedger>,
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
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
        }
    }
}

/// A failure no retry can fix without changing durable state or wiring is a
/// refusal; anything else — the engine or the store unreachable — is worth
/// another attempt, since nothing about the terminal changed.
fn failure(error: PluginError) -> DeliveryFailure {
    if error.is_terminal() {
        DeliveryFailure::Refused(error.to_string())
    } else {
        DeliveryFailure::Retryable(error.to_string())
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ProcessTerminalRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    async fn deliver(&self, delivery: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        let ObligationDelivery { id, key, .. } = delivery;
        let ObligationKey::ProcessTerminal { process_id } = key else {
            return Err(DeliveryFailure::Undecodable(format!(
                "a {} key on the process_terminal ledger",
                key.kind()
            )));
        };
        let record = match self.registry.get_process(process_id).await {
            Ok(record) => record,
            // Retention reclaimed the process: nothing is left to publish and
            // no waiter is left to strand.
            Err(PluginError::ProcessNoLongerRetained { .. }) => None,
            Err(error) => return Err(failure(error)),
        };
        let Some(record) = record else {
            return Ok(());
        };
        let Some(output) = record.outcome.as_ref() else {
            return Err(DeliveryFailure::Refused(format!(
                "process `{process_id}` owes a terminal publication but stores no terminal"
            )));
        };
        self.port
            .publish_process_terminal(process_id, output, id.as_str())
            .await
            .map_err(failure)
    }
}
