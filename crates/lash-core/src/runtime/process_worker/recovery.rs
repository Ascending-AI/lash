use crate::{
    PluginError, ProcessAwaitOutput, ProcessLease, ProcessLeaseCompletion, ProcessRecord,
    ProcessStatus,
};

use super::DurableProcessWorker;

/// Report from a graceful owner drain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessDrainReport {
    /// Process ids this host's own started `OwnerBound` work was terminalized as
    /// `Abandoned{OwnerDrain}` on, in the order they were drained.
    pub abandoned: Vec<String>,
    /// Rows the drain could not terminalize in this pass, in inspection order.
    /// Each entry preserves the typed reason so a host can distinguish ordinary
    /// lease contention or disappearance from a backend failure.
    pub deferred: Vec<ProcessDrainDeferred>,
}

/// One process deferred by a graceful owner drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessDrainDeferred {
    /// Durable process id deferred by this drain pass.
    pub process_id: String,
    /// Typed reason the row did not produce confirmed terminal evidence.
    pub disposition: ProcessRecoveryAttemptDisposition,
}

/// Why a process recovery or drain attempt did not act on a row.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessRecoveryAttemptDisposition {
    /// Another live owner holds the process lease.
    Busy,
    /// The process is no longer a non-terminal candidate after enumeration.
    Absent,
    /// The row was already terminal before this attempt could write it.
    SettledByPeer {
        /// Durable terminal status retained by the registry.
        terminal_status: ProcessStatus,
    },
    /// The registry had already applied the exact proposed terminal outcome.
    AlreadyApplied {
        /// Durable terminal status retained by the registry.
        terminal_status: ProcessStatus,
    },
    /// This attempt's lease fence was superseded by a newer owner.
    LeaseLost {
        /// Operation at which the superseded fence was observed.
        operation: ProcessRecoveryOperation,
    },
    /// A registry operation failed. The row remains deferred rather than being
    /// reported as a legitimate busy or absent outcome.
    BackendError {
        /// Registry operation that failed.
        operation: ProcessRecoveryOperation,
        /// Display form of the registry error for host diagnostics.
        error: String,
    },
}

/// Registry operation that failed during process recovery or owner drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessRecoveryOperation {
    ClaimLease,
    ReadProcess,
    RenewLease,
    WriteTerminal,
    ReleaseLease,
}

impl ProcessRecoveryOperation {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ClaimLease => "claim_lease",
            Self::ReadProcess => "read_process",
            Self::RenewLease => "renew_lease",
            Self::WriteTerminal => "write_terminal",
            Self::ReleaseLease => "release_lease",
        }
    }
}

pub(super) struct RecoveryBackendError {
    pub(super) operation: ProcessRecoveryOperation,
    pub(super) error: PluginError,
}

#[must_use = "a recovery lease claim disposition must be handled"]
pub(super) enum RecoveryClaimDisposition {
    Acquired(ProcessLease),
    Busy,
    BackendError(RecoveryBackendError),
}

#[must_use = "a recovery registry read disposition must be handled"]
pub(super) enum RecoveryReadDisposition {
    Found(Box<ProcessRecord>),
    Absent,
    BackendError(RecoveryBackendError),
}

#[must_use = "a recovery completion disposition must be handled"]
pub(super) enum RecoveryCompletionDisposition {
    Committed,
    Busy,
    Absent,
    AlreadyApplied(ProcessStatus),
    SettledByPeer(ProcessStatus),
    LeaseLost(ProcessRecoveryOperation),
    BackendError(RecoveryBackendError),
}

#[must_use = "a recovery lease release disposition must be handled"]
pub(super) enum RecoveryReleaseDisposition {
    Released,
    BackendError(RecoveryBackendError),
}

impl RecoveryBackendError {
    pub(super) fn into_public(self) -> ProcessRecoveryAttemptDisposition {
        ProcessRecoveryAttemptDisposition::BackendError {
            operation: self.operation,
            error: self.error.to_string(),
        }
    }
}

impl DurableProcessWorker {
    /// Claim the recovery lease without collapsing backend errors into ordinary
    /// live-owner contention.
    pub(super) async fn claim_for_recovery(
        &self,
        process_id: &str,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> RecoveryClaimDisposition {
        match self
            .config
            .process_registry
            .claim_process_lease(process_id, owner, lease_ttl_ms)
            .await
        {
            Ok(crate::ProcessLeaseClaimOutcome::Acquired(lease)) => {
                RecoveryClaimDisposition::Acquired(lease)
            }
            Ok(crate::ProcessLeaseClaimOutcome::Busy { .. }) => RecoveryClaimDisposition::Busy,
            Err(error) => RecoveryClaimDisposition::BackendError(self.recovery_backend_error(
                process_id,
                ProcessRecoveryOperation::ClaimLease,
                error,
            )),
        }
    }

    pub(super) async fn read_for_recovery(&self, process_id: &str) -> RecoveryReadDisposition {
        match self.config.process_registry.get_process(process_id).await {
            Ok(Some(record)) => RecoveryReadDisposition::Found(Box::new(record)),
            Ok(None) => RecoveryReadDisposition::Absent,
            Err(error) => RecoveryReadDisposition::BackendError(self.recovery_backend_error(
                process_id,
                ProcessRecoveryOperation::ReadProcess,
                error,
            )),
        }
    }

    /// Write a recovered process's terminal outcome and release its lease in one
    /// atomic fenced registry operation.
    pub(super) async fn complete_and_release(
        &self,
        lease: &ProcessLease,
        process_id: &str,
        output: ProcessAwaitOutput,
    ) -> RecoveryCompletionDisposition {
        self.complete_and_release_with_parent_end(lease, process_id, output, Vec::new())
            .await
    }

    pub(super) async fn complete_and_release_with_parent_end(
        &self,
        lease: &ProcessLease,
        process_id: &str,
        output: ProcessAwaitOutput,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> RecoveryCompletionDisposition {
        let fenced = match self
            .config
            .process_registry
            .renew_process_lease(lease, self.lease_timings().ttl_ms())
            .await
        {
            Ok(renewed) => renewed,
            Err(err) => {
                if matches!(&err, PluginError::ProcessLeaseSuperseded { .. }) {
                    self.recovery_lease_lost(
                        process_id,
                        ProcessRecoveryOperation::RenewLease,
                        &err,
                    );
                    return RecoveryCompletionDisposition::LeaseLost(
                        ProcessRecoveryOperation::RenewLease,
                    );
                }
                let error = self.recovery_backend_error(
                    process_id,
                    ProcessRecoveryOperation::RenewLease,
                    err,
                );
                // Release is token-fenced, so it cannot clear a successor's lease.
                // If the transient failure left our lease live, releasing it makes
                // Rerunnable work immediately retryable instead of retaining the
                // lease TTL as an implicit backoff.
                self.release_or_log(lease).await;
                return RecoveryCompletionDisposition::BackendError(error);
            }
        };
        match self
            .config
            .process_registry
            .complete_process_with_lease_and_parent_end(&fenced, output, actions)
            .await
        {
            Ok(crate::ProcessCompletionOutcome::Committed(_)) => {
                RecoveryCompletionDisposition::Committed
            }
            Ok(crate::ProcessCompletionOutcome::AlreadyApplied { stored }) => {
                match self.release_for_recovery(&fenced).await {
                    RecoveryReleaseDisposition::Released => {
                        RecoveryCompletionDisposition::AlreadyApplied(stored.status)
                    }
                    RecoveryReleaseDisposition::BackendError(error) => {
                        RecoveryCompletionDisposition::BackendError(error)
                    }
                }
            }
            Ok(crate::ProcessCompletionOutcome::Superseded { stored }) => {
                match self.release_for_recovery(&fenced).await {
                    RecoveryReleaseDisposition::Released => {
                        RecoveryCompletionDisposition::SettledByPeer(stored.status)
                    }
                    RecoveryReleaseDisposition::BackendError(error) => {
                        RecoveryCompletionDisposition::BackendError(error)
                    }
                }
            }
            Err(err) if matches!(&err, PluginError::ProcessLeaseSuperseded { .. }) => {
                self.recovery_lease_lost(process_id, ProcessRecoveryOperation::WriteTerminal, &err);
                RecoveryCompletionDisposition::LeaseLost(ProcessRecoveryOperation::WriteTerminal)
            }
            Err(err) => {
                let error = self.recovery_backend_error(
                    process_id,
                    ProcessRecoveryOperation::WriteTerminal,
                    err,
                );
                self.release_or_log(&fenced).await;
                RecoveryCompletionDisposition::BackendError(error)
            }
        }
    }

    pub(super) async fn release_or_log(&self, lease: &ProcessLease) {
        match self.release_for_recovery(lease).await {
            RecoveryReleaseDisposition::Released | RecoveryReleaseDisposition::BackendError(_) => {}
        }
    }

    pub(super) async fn release_for_recovery(
        &self,
        lease: &ProcessLease,
    ) -> RecoveryReleaseDisposition {
        match self.release_process_lease(lease).await {
            Ok(()) => RecoveryReleaseDisposition::Released,
            Err(error) => RecoveryReleaseDisposition::BackendError(self.recovery_backend_error(
                &lease.process_id,
                ProcessRecoveryOperation::ReleaseLease,
                error,
            )),
        }
    }

    pub(super) fn observe_recovery_completion(disposition: RecoveryCompletionDisposition) {
        match disposition {
            RecoveryCompletionDisposition::Committed
            | RecoveryCompletionDisposition::Busy
            | RecoveryCompletionDisposition::Absent
            | RecoveryCompletionDisposition::AlreadyApplied(_)
            | RecoveryCompletionDisposition::SettledByPeer(_)
            | RecoveryCompletionDisposition::LeaseLost(_)
            | RecoveryCompletionDisposition::BackendError(_) => {}
        }
    }

    pub(super) fn recovery_lease_lost(
        &self,
        process_id: &str,
        operation: ProcessRecoveryOperation,
        error: &PluginError,
    ) {
        let error = error.to_string();
        tracing::warn!(
            target: "lash_core::process_recovery",
            event = "process_recovery.lease_lost",
            decision_basis = "lease_superseded",
            process_id,
            operation = operation.label(),
            outcome = "deferred_to_new_owner",
            error = error.as_str(),
            "process recovery lease was superseded; deferring to the new owner",
        );
    }

    pub(super) fn recovery_backend_error(
        &self,
        process_id: &str,
        operation: ProcessRecoveryOperation,
        error: PluginError,
    ) -> RecoveryBackendError {
        let error_message = error.to_string();
        tracing::warn!(
            target: "lash_core::process_recovery",
            event = "process_recovery.backend_error",
            decision_basis = "backend_error",
            process_id,
            operation = operation.label(),
            outcome = "deferred",
            error = error_message.as_str(),
            "process recovery backend operation failed; row deferred",
        );
        RecoveryBackendError { operation, error }
    }

    pub(super) fn lease_timings(&self) -> crate::LeaseTimings {
        self.config.runtime_host.control.lease_timings
    }

    async fn release_process_lease(&self, lease: &ProcessLease) -> Result<(), PluginError> {
        self.config
            .process_registry
            .complete_process_lease(&ProcessLeaseCompletion::from_lease(lease))
            .await
    }
}
