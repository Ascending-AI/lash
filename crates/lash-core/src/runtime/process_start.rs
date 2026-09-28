//! Delivery of a registered process's first run through its obligation.

use std::sync::Arc;

use crate::runtime::drive::relay::{DeliveryFailure, ObligationRelay};
use crate::store::{ObligationId, ObligationKey, ObligationLedger};
use crate::{PluginError, ProcessRegistry, ProcessWorkSubstrate};

pub struct ProcessStartRelay {
    ledger: Arc<dyn ObligationLedger>,
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
}

impl ProcessStartRelay {
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

fn failure(error: PluginError) -> DeliveryFailure {
    if error.is_terminal() {
        DeliveryFailure::Refused(error.to_string())
    } else {
        DeliveryFailure::Retryable(error.to_string())
    }
}

#[async_trait::async_trait]
impl ObligationRelay for ProcessStartRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }

    async fn deliver(
        &self,
        id: &ObligationId,
        key: &ObligationKey,
        attempt: u32,
    ) -> Result<(), DeliveryFailure> {
        let ObligationKey::ProcessStart { process_id } = key else {
            return Err(DeliveryFailure::Undecodable(format!(
                "a {} key on the process_start ledger",
                key.kind()
            )));
        };
        let record = match self.registry.get_process(process_id).await {
            Ok(record) => record,
            Err(PluginError::ProcessNoLongerRetained { .. }) => None,
            Err(error) => return Err(failure(error)),
        };
        let Some(record) = record else {
            return Ok(());
        };
        if record.is_terminal() {
            return Ok(());
        }
        if record.input.is_externally_owned() {
            return Err(DeliveryFailure::Refused(format!(
                "externally owned process `{process_id}` has a start obligation"
            )));
        }
        self.port
            .deliver_process_start(process_id, &format!("process_start:{id}:{attempt}"))
            .await
            .map_err(failure)
    }
}
