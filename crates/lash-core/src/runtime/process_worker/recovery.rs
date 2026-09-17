use crate::ProcessId;
use crate::{
    PluginError, ProcessAwaitOutput, ProcessLease, ProcessLeaseCompletion, ProcessRecord,
    ProcessStatus,
};

use super::DurableProcessWorker;

pub use crate::runtime::process::{
    ProcessAdmissionDeferred, ProcessAdmissionIntake, ProcessAdmissionReport, ProcessDrainDeferred,
    ProcessDrainReport, ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation,
    ProcessWorkerFault,
};

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

/// What one admitted row's recovery attempt actually did.
///
/// `recover_process` returns this instead of `()`: the dispatcher that spawned
/// the attempt reports the fault-worthy outcomes on the worker's fault surface
/// rather than dropping typed dispositions on the floor.
#[must_use = "a process recovery outcome must be observed"]
pub(super) enum ProcessRecoveryOutcome {
    /// The attempt wrote this row's terminal outcome under its lease.
    Committed,
    /// Lash never executes this row (externally owned, or an owner-bound row a
    /// re-run would violate) and it was deliberately left where it is.
    LeftToOwner,
    /// The attempt did not write a terminal, for the typed reason given.
    Deferred(ProcessRecoveryAttemptOutcome),
    /// The row could not be rebuilt or executed; its lease was released so a
    /// later pass can retry.
    RunFailed(PluginError),
}

impl RecoveryCompletionDisposition {
    pub(super) fn into_outcome(self) -> ProcessRecoveryOutcome {
        match self {
            Self::Committed => ProcessRecoveryOutcome::Committed,
            Self::Busy => ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptOutcome::Busy),
            Self::Absent => ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptOutcome::Absent),
            Self::AlreadyApplied(terminal_status) => {
                ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptOutcome::AlreadyApplied {
                    terminal_status,
                })
            }
            Self::SettledByPeer(terminal_status) => {
                ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptOutcome::SettledByPeer {
                    terminal_status,
                })
            }
            Self::LeaseLost(operation) => {
                ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptOutcome::LeaseLost {
                    operation,
                })
            }
            Self::BackendError(error) => ProcessRecoveryOutcome::Deferred(error.into_public()),
        }
    }
}

impl RecoveryBackendError {
    pub(super) fn into_public(self) -> ProcessRecoveryAttemptOutcome {
        ProcessRecoveryAttemptOutcome::BackendError {
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
        process_id: &ProcessId,
        owner: &crate::LeaseOwnerIdentity,
        lease_ttl_ms: u64,
    ) -> RecoveryClaimDisposition {
        match self
            .config
            .process_registry()
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

    pub(super) async fn read_for_recovery(
        &self,
        process_id: &ProcessId,
    ) -> RecoveryReadDisposition {
        match self.config.process_registry().get_process(process_id).await {
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
        process_id: &ProcessId,
        output: ProcessAwaitOutput,
    ) -> RecoveryCompletionDisposition {
        self.complete_and_release_inner(lease, process_id, output)
            .await
    }

    async fn complete_and_release_inner(
        &self,
        lease: &ProcessLease,
        process_id: &ProcessId,
        output: ProcessAwaitOutput,
    ) -> RecoveryCompletionDisposition {
        let fenced = match self
            .config
            .process_registry()
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
                let _ = self.release_or_log(lease).await;
                return RecoveryCompletionDisposition::BackendError(error);
            }
        };
        match self
            .config
            .process_registry()
            .complete_process_with_lease(&fenced, output)
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
                let _ = self.release_or_log(&fenced).await;
                RecoveryCompletionDisposition::BackendError(error)
            }
        }
    }

    /// Release this attempt's lease, returning the typed failure if the release
    /// itself failed. A release fault has no other trace, so callers that would
    /// otherwise report a clean outcome must prefer it — see
    /// [`release_or_outcome`](Self::release_or_outcome).
    pub(super) async fn release_or_log(
        &self,
        lease: &ProcessLease,
    ) -> Option<RecoveryBackendError> {
        match self.release_for_recovery(lease).await {
            RecoveryReleaseDisposition::Released => None,
            RecoveryReleaseDisposition::BackendError(error) => Some(error),
        }
    }

    /// Release the lease and report `otherwise`, unless the release failed — a
    /// failed release is the fault worth reporting.
    pub(super) async fn release_or_outcome(
        &self,
        lease: &ProcessLease,
        otherwise: ProcessRecoveryOutcome,
    ) -> ProcessRecoveryOutcome {
        match self.release_or_log(lease).await {
            Some(error) => ProcessRecoveryOutcome::Deferred(error.into_public()),
            None => otherwise,
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

    /// Push one worker fault to the host-facing sink, when one is wired.
    ///
    /// Unconditional by construction: this is the ordinary
    /// [`ProcessEventSink`](crate::runtime::ProcessEventSink) seam, present in
    /// every build, never a metrics recorder compiled out by a feature flag.
    ///
    /// A host that wired no sink still gets the fault, at `error` level on the
    /// `tracing` seam every host already has. The typed surface is the one to
    /// build on; the log is the floor, so a sinkless host is never blinder than
    /// it was before faults were typed.
    pub(super) async fn emit_worker_fault(&self, fault: ProcessWorkerFault) {
        let Some(sink) = self.config.process_event_sink.as_ref() else {
            fault.trace_without_sink();
            return;
        };
        sink.emit_worker_fault(&fault).await;
    }

    /// Report one admitted row's recovery outcome. Ordinary deferrals (a live
    /// owner, a disappeared row, a peer's terminal, a superseded fence) are not
    /// faults; a backend or execution failure is, because it leaves the row
    /// non-terminal with nothing else to say so.
    pub(super) async fn observe_recovery_outcome(
        &self,
        process_id: &ProcessId,
        outcome: ProcessRecoveryOutcome,
    ) {
        match outcome {
            ProcessRecoveryOutcome::Committed | ProcessRecoveryOutcome::LeftToOwner => {}
            ProcessRecoveryOutcome::Deferred(disposition) => {
                if let Some(fault) = disposition.into_worker_fault(process_id) {
                    self.emit_worker_fault(fault).await;
                }
            }
            ProcessRecoveryOutcome::RunFailed(error) => {
                self.emit_worker_fault(ProcessWorkerFault::RecoveryRunFailed {
                    process_id: ProcessId::from(process_id.to_string()),
                    error: error.to_string(),
                })
                .await;
            }
        }
    }

    pub(super) fn recovery_lease_lost(
        &self,
        process_id: &ProcessId,
        operation: ProcessRecoveryOperation,
        error: &PluginError,
    ) {
        let error = error.to_string();
        tracing::warn!(
            target: "lash_core::process_recovery",
            event = "process_recovery.lease_lost",
            decision_basis = "lease_superseded",
            process_id = process_id.as_str(),
            operation = operation.label(),
            outcome = "deferred_to_new_owner",
            error = error.as_str(),
            "process recovery lease was superseded; deferring to the new owner",
        );
    }

    pub(super) fn recovery_backend_error(
        &self,
        process_id: &ProcessId,
        operation: ProcessRecoveryOperation,
        error: PluginError,
    ) -> RecoveryBackendError {
        let error_message = error.to_string();
        tracing::warn!(
            target: "lash_core::process_recovery",
            event = "process_recovery.backend_error",
            decision_basis = "backend_error",
            process_id = process_id.as_str(),
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
            .process_registry()
            .complete_process_lease(&ProcessLeaseCompletion::from_lease(lease))
            .await
    }
}
