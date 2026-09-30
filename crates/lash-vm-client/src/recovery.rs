//! A parent execution's durable reservation and shared checkout accounting.
use crate::{ExecutionBudget, PoolError, service::Service};
use lash_core_execution::store::worker_recovery::*;
use std::sync::Arc;

pub struct RecoveryExecution {
    pub(crate) service: Service,
    pub(crate) store: Arc<dyn WorkerRecoveryStore>,
    pub(crate) claim: WorkerRecoveryClaim,
    pub(crate) budget: ExecutionBudget,
}
impl RecoveryExecution {
    pub fn service(&self) -> &Service {
        &self.service
    }
    pub fn budget(&self) -> ExecutionBudget {
        self.budget.clone()
    }
    pub async fn checkpoint(&self) -> Result<(), PoolError> {
        self.service.checkpoint().await
    }
    pub async fn settle(self) -> Result<(), PoolError> {
        self.store
            .settle(&self.claim, self.budget.recovery_totals())
            .await
            .map_err(recovery_error)
    }
}
pub(crate) fn recovery_error(error: WorkerRecoveryError) -> PoolError {
    match error {
        WorkerRecoveryError::CpuExhausted => {
            lash_vm_protocol::InfrastructureOutcome::WorkerLimitExceeded {
                limit: lash_vm_protocol::WorkerLimit::Deadline,
            }
            .into()
        }
        WorkerRecoveryError::AttemptsExhausted => PoolError::RetryLimitExceeded,
        error => PoolError::Recovery {
            message: error.to_string(),
        },
    }
}

#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub(crate) struct RecoveryDouble(
    std::sync::Mutex<std::collections::BTreeMap<String, WorkerRecoveryRow>>,
);
#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl WorkerRecoveryStore for RecoveryDouble {
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError> {
        let mut rows = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (claim, row) = reserve_worker_recovery(
            scope.to_owned(),
            rows.get(scope).copied().unwrap_or_default(),
            limits,
        )?;
        rows.insert(scope.to_owned(), row);
        Ok(claim)
    }
    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError> {
        let mut rows = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = rows
            .get_mut(&claim.scope)
            .filter(|row| row.revision == claim.revision)
            .ok_or(WorkerRecoveryError::Fenced)?;
        row.in_flight = true;
        Ok(())
    }
    async fn settle(
        &self,
        claim: &WorkerRecoveryClaim,
        totals: WorkerRecoveryTotals,
    ) -> Result<(), WorkerRecoveryError> {
        validate_worker_recovery_settlement(claim, totals)?;
        let mut rows = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = rows
            .get_mut(&claim.scope)
            .filter(|row| row.revision == claim.revision)
            .ok_or(WorkerRecoveryError::Fenced)?;
        validate_worker_recovery_settlement(
            &WorkerRecoveryClaim {
                baseline: row.totals,
                ..claim.clone()
            },
            totals,
        )?;
        row.totals = totals;
        row.in_flight = false;
        Ok(())
    }
}
