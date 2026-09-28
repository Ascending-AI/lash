//! Delivery of a registered process's first run through its obligation.

use std::sync::Arc;

use crate::runtime::drive::relay::{DeliveryFailure, ObligationRelay, deliver_now};
use crate::store::{
    ClaimToken, ObligationId, ObligationKey, ObligationLedger, ObligationSettlement,
    process_start_obligation_id,
};
use crate::{Clock, PluginError, ProcessRegistry, ProcessWorkSubstrate};

pub struct ProcessStartRelay {
    ledger: Arc<dyn ObligationLedger>,
    registry: Arc<dyn ProcessRegistry>,
    port: Arc<dyn ProcessWorkSubstrate>,
    clock: Arc<dyn Clock>,
}

impl ProcessStartRelay {
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
        }
    }

    /// A producer's own-commit attempt on `process_id`'s armed start row
    /// (ADR 0109 §1.5): claim it and run one delivery through this relay at
    /// once. The reconcile tick's `relay_due` retries whatever this could
    /// not deliver; [`super::drive::relay::RelayVerdict::NotDue`] means the
    /// row was already claimed, delivered or stalled.
    ///
    /// # Errors
    ///
    /// Only a store failure; a failed delivery is a settlement on the row,
    /// not an error.
    pub async fn deliver_start(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<super::drive::relay::RelayVerdict, crate::StoreError> {
        deliver_now(
            self,
            &process_start_obligation_id(process_id),
            self.clock.as_ref(),
        )
        .await
    }

    /// Claim `process_id`'s armed start row for a delivery the caller runs
    /// itself and settles with [`Self::settle_start`] — the own-commit rule
    /// of [`Self::deliver_start`] split across an engine that must journal
    /// each step (ADR 0109 §1.5). The token is journaled by the caller and
    /// handed back for the settle; `None` means the row was not due — a
    /// prior attempt claimed, delivered or stalled it.
    ///
    /// # Errors
    ///
    /// Only a store failure.
    pub async fn claim_start(
        &self,
        process_id: &crate::ProcessId,
    ) -> Result<Option<ClaimToken>, crate::StoreError> {
        let claimed = self
            .ledger
            .claim(
                &process_start_obligation_id(process_id),
                self.clock.timestamp_ms(),
                self.policy().claim_ttl_ms,
            )
            .await?;
        Ok(claimed.map(|claimed| claimed.token))
    }

    /// Settle a [`Self::claim_start`] claim with `settlement` — `Delivered`
    /// once the caller's own delivery is durably committed.
    ///
    /// # Errors
    ///
    /// Only a store failure; a settled or re-taken row answers
    /// [`crate::store::SettleOutcome::ClaimLost`] here.
    pub async fn settle_start(
        &self,
        process_id: &crate::ProcessId,
        token: ClaimToken,
        settlement: ObligationSettlement,
    ) -> Result<crate::store::SettleOutcome, crate::StoreError> {
        self.ledger
            .settle(
                &process_start_obligation_id(process_id),
                &token,
                settlement,
                self.clock.timestamp_ms(),
            )
            .await
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
        _id: &ObligationId,
        key: &ObligationKey,
        _attempt: u32,
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
            .deliver_process_start(&record)
            .await
            .map_err(failure)
    }
}
