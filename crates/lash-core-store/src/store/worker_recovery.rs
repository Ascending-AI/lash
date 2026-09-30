//! Parent-owned execution accounting, outside the positional effect journal.
use super::StoreError;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRecoveryTotals {
    pub attempts: u32,
    pub cpu_nanos: u64,
    pub replacement: bool,
    pub unknown_cpu_attempts: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerRecoveryLimits {
    pub max_attempts: u32,
    pub max_cpu_nanos: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerRecoveryRow {
    pub revision: u64,
    pub totals: WorkerRecoveryTotals,
    pub in_flight: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRecoveryClaim {
    pub scope: String,
    pub revision: u64,
    pub baseline: WorkerRecoveryTotals,
}
#[derive(Debug, thiserror::Error)]
pub enum WorkerRecoveryError {
    #[error("the execution exhausted its worker attempt budget")]
    AttemptsExhausted,
    #[error("the execution exhausted its cumulative worker CPU budget")]
    CpuExhausted,
    #[error("the worker recovery reservation was superseded")]
    Fenced,
    #[error("invalid worker recovery accounting")]
    InvalidTotals,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Reserve before launching a worker. Until settlement replaces it with the
/// measured usage, an interrupted attempt records its CPU as unknown.
/// A normal segment handover preserves its attempt; a failed checkout replaces it.
pub fn reserve_worker_recovery(
    scope: String,
    current: WorkerRecoveryRow,
    limits: WorkerRecoveryLimits,
) -> Result<(WorkerRecoveryClaim, WorkerRecoveryRow), WorkerRecoveryError> {
    if scope.is_empty()
        || limits.max_attempts == 0
        || limits.max_cpu_nanos == 0
        || limits.max_cpu_nanos > i64::MAX as u64
    {
        return Err(WorkerRecoveryError::InvalidTotals);
    }
    if current.totals.cpu_nanos >= limits.max_cpu_nanos {
        return Err(WorkerRecoveryError::CpuExhausted);
    }
    let mut baseline = current.totals;
    if current.in_flight {
        baseline.unknown_cpu_attempts = baseline
            .unknown_cpu_attempts
            .checked_add(1)
            .ok_or(WorkerRecoveryError::InvalidTotals)?;
    }
    if baseline.attempts == 0 || baseline.replacement || current.in_flight {
        if baseline.attempts >= limits.max_attempts {
            return Err(WorkerRecoveryError::AttemptsExhausted);
        }
        baseline.attempts += 1;
    }
    baseline.replacement = false;
    let revision = current
        .revision
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(WorkerRecoveryError::InvalidTotals)?;
    Ok((
        WorkerRecoveryClaim {
            scope,
            revision,
            baseline,
        },
        WorkerRecoveryRow {
            revision,
            totals: baseline,
            in_flight: false,
        },
    ))
}

pub fn validate_worker_recovery_settlement(
    claim: &WorkerRecoveryClaim,
    totals: WorkerRecoveryTotals,
) -> Result<(), WorkerRecoveryError> {
    if totals.attempts < claim.baseline.attempts
        || totals.cpu_nanos < claim.baseline.cpu_nanos
        || totals.unknown_cpu_attempts < claim.baseline.unknown_cpu_attempts
        || totals.cpu_nanos > i64::MAX as u64
    {
        return Err(WorkerRecoveryError::InvalidTotals);
    }
    Ok(())
}

#[async_trait::async_trait]
pub trait WorkerRecoveryStore: Send + Sync {
    /// Atomically preserve this code scope's measured CPU and admit its attempt.
    async fn reserve(
        &self,
        scope: &str,
        limits: WorkerRecoveryLimits,
    ) -> Result<WorkerRecoveryClaim, WorkerRecoveryError>;
    /// Record that this reservation has a worker consuming CPU.
    async fn mark_running(&self, claim: &WorkerRecoveryClaim) -> Result<(), WorkerRecoveryError>;
    /// Replace only this reservation with measured totals. A stale parent cannot refund it.
    async fn settle(
        &self,
        claim: &WorkerRecoveryClaim,
        totals: WorkerRecoveryTotals,
    ) -> Result<(), WorkerRecoveryError>;
}
