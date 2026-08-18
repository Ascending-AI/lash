use crate::{
    PluginError, ProcessAwaitOutput, ProcessLease, ProcessLeaseCompletion, ProcessRecord,
    ProcessStatus,
};

use super::DurableProcessWorker;

/// Report from one admission pass of
/// [`DurableProcessWorker::drive_pending_processes`].
///
/// A drive **admits** rows to this worker's execution scheduler; it does not
/// wait for them. Admission is not completion: an admitted row's claim, read,
/// terminal write, or lease release can still fail after this report is
/// returned, and those faults are reported on the unconditional
/// [`ProcessEventSink`](crate::runtime::ProcessEventSink) fault surface
/// ([`ProcessWorkerFault`]), never as a completed clean drive.
#[must_use = "an admission report names what this call did and did not admit"]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessAdmissionReport {
    /// Whether this call took intake of its own, or coalesced onto a scan
    /// another caller already had in flight.
    ///
    /// An empty report is ambiguous without this: "the worklist was empty" and
    /// "this call never read the worklist" are different facts and a host that
    /// polls until quiet needs to tell them apart.
    pub intake: ProcessAdmissionIntake,
    /// Process ids this call admitted to the worker's execution scheduler, in
    /// intake order.
    pub admitted: Vec<String>,
    /// Rows this call inspected but did not admit, in inspection order. Each
    /// entry preserves the typed reason, so ordinary contention stays distinct
    /// from disappearance and from a backend failure.
    pub deferred: Vec<ProcessAdmissionDeferred>,
}

/// Whether an admission pass read the worklist itself.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessAdmissionIntake {
    /// This call read a worklist page and the report describes what it found.
    /// An empty `Scanned` report means the worklist held nothing to admit.
    #[default]
    Scanned,
    /// A worklist scan was already in flight, so this call requested a rescan
    /// and took no intake of its own. Rows are being admitted by the in-flight
    /// scan; this report says nothing about them.
    Coalesced,
}

impl ProcessAdmissionReport {
    /// Merge a nested admission pass's report into this one.
    ///
    /// Re-entrant drives (a work driver invoked from trigger-delivery
    /// reconcile, say) admit rows that belong to the outer call's report. Their
    /// rows are folded in ahead of the outer pass's own, and a deferred
    /// [`Busy`](ProcessRecoveryAttemptDisposition::Busy) row is dropped when
    /// this same call already admitted that id — a call must never report its
    /// own admission as somebody else's contention.
    pub(super) fn absorb(&mut self, nested: Self) {
        if matches!(nested.intake, ProcessAdmissionIntake::Scanned) {
            self.intake = ProcessAdmissionIntake::Scanned;
        }
        for process_id in nested.admitted {
            if !self.admitted.contains(&process_id) {
                self.admitted.push(process_id);
            }
        }
        for entry in nested.deferred {
            self.push_deferred(entry);
        }
    }

    /// Record a deferral, unless this same call already admitted the row and the
    /// reason is ordinary contention with that admission.
    pub(super) fn push_deferred(&mut self, entry: ProcessAdmissionDeferred) {
        if matches!(entry.disposition, ProcessRecoveryAttemptDisposition::Busy)
            && self.admitted.contains(&entry.process_id)
        {
            return;
        }
        self.deferred.push(entry);
    }
}

/// One process a drive inspected but did not admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessAdmissionDeferred {
    /// Durable process id deferred by this admission pass.
    pub process_id: String,
    /// Typed reason the row was not admitted by this call.
    pub disposition: ProcessRecoveryAttemptDisposition,
}

/// A worker fault that would otherwise be invisible to a host.
///
/// Driving pending processes is fire-and-forget admission: the call that admits
/// a row returns before the row is claimed, run, and terminalized. These are the
/// faults that leave a row non-terminal *after* that return, plus the pass-scoped
/// scan fault, delivered through the unconditional
/// [`ProcessEventSink`](crate::runtime::ProcessEventSink) surface so they are
/// observable in every build.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessWorkerFault {
    /// A registry operation failed while driving an admitted row. The row was
    /// left non-terminal for a later pass instead of being driven terminal.
    RecoveryBackendError {
        /// Durable process id the failing operation targeted.
        process_id: String,
        /// Registry operation that failed.
        operation: ProcessRecoveryOperation,
        /// Display form of the registry error for host diagnostics.
        error: String,
    },
    /// An admitted row could not be rebuilt or executed (a runtime rebuild or
    /// store-facet failure). The lease was released, so the row stays claimable
    /// by a later pass rather than terminal.
    RecoveryRunFailed {
        /// Durable process id whose execution could not be rebuilt.
        process_id: String,
        /// Display form of the execution failure for host diagnostics.
        error: String,
    },
    /// The non-terminal worklist scan stopped short after its retry budget was
    /// exhausted: rows past the last cursor were never admitted by this worker.
    ///
    /// Pass-scoped, not row-scoped, and never attributed to a later call — the
    /// pass whose scan failed reports it.
    WorklistScanIncomplete {
        /// Display form of the worklist read error for host diagnostics.
        error: String,
    },
}

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
    /// The row is externally owned (ADR 0019): Lash never executes it, on any
    /// tier. An admission pass reports it as deferred rather than admitted, so
    /// one registry reads the same whichever tier drove it.
    ExternallyOwned,
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
    /// Handing an admitted row to an external execution engine (the Restate
    /// tier's ingress submit).
    SubmitRun,
}

impl ProcessRecoveryOperation {
    /// Stable snake_case label for this operation.
    ///
    /// The one spelling used in structured records, so a fault logged by the
    /// inline worker and one logged by an out-of-crate tier (the Restate
    /// ingress sweep) carry the same `operation` value rather than two
    /// dialects of the same vocabulary.
    pub fn label(self) -> &'static str {
        match self {
            Self::ClaimLease => "claim_lease",
            Self::ReadProcess => "read_process",
            Self::RenewLease => "renew_lease",
            Self::WriteTerminal => "write_terminal",
            Self::ReleaseLease => "release_lease",
            Self::SubmitRun => "submit_run",
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
    Deferred(ProcessRecoveryAttemptDisposition),
    /// The row could not be rebuilt or executed; its lease was released so a
    /// later pass can retry.
    RunFailed(PluginError),
}

impl RecoveryCompletionDisposition {
    pub(super) fn into_outcome(self) -> ProcessRecoveryOutcome {
        match self {
            Self::Committed => ProcessRecoveryOutcome::Committed,
            Self::Busy => ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptDisposition::Busy),
            Self::Absent => {
                ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptDisposition::Absent)
            }
            Self::AlreadyApplied(terminal_status) => ProcessRecoveryOutcome::Deferred(
                ProcessRecoveryAttemptDisposition::AlreadyApplied { terminal_status },
            ),
            Self::SettledByPeer(terminal_status) => {
                ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptDisposition::SettledByPeer {
                    terminal_status,
                })
            }
            Self::LeaseLost(operation) => {
                ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptDisposition::LeaseLost {
                    operation,
                })
            }
            Self::BackendError(error) => ProcessRecoveryOutcome::Deferred(error.into_public()),
        }
    }
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
                let _ = self.release_or_log(lease).await;
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
            match &fault {
                ProcessWorkerFault::RecoveryBackendError {
                    process_id,
                    operation,
                    error,
                } => tracing::error!(
                    target: "lash_core::process_recovery",
                    event = "process_worker.fault",
                    fault = "recovery_backend_error",
                    process_id = %process_id,
                    operation = operation.label(),
                    error = %error,
                    "process worker recovery backend error (no process event sink wired)"
                ),
                ProcessWorkerFault::RecoveryRunFailed { process_id, error } => tracing::error!(
                    target: "lash_core::process_recovery",
                    event = "process_worker.fault",
                    fault = "recovery_run_failed",
                    process_id = %process_id,
                    error = %error,
                    "process worker recovery run failed (no process event sink wired)"
                ),
                ProcessWorkerFault::WorklistScanIncomplete { error } => tracing::error!(
                    target: "lash_core::process_recovery",
                    event = "process_worker.fault",
                    fault = "worklist_scan_incomplete",
                    error = %error,
                    "process worklist scan incomplete (no process event sink wired)"
                ),
            }
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
        process_id: &str,
        outcome: ProcessRecoveryOutcome,
    ) {
        match outcome {
            ProcessRecoveryOutcome::Committed | ProcessRecoveryOutcome::LeftToOwner => {}
            ProcessRecoveryOutcome::Deferred(
                ProcessRecoveryAttemptDisposition::Busy
                | ProcessRecoveryAttemptDisposition::Absent
                | ProcessRecoveryAttemptDisposition::AlreadyApplied { .. }
                | ProcessRecoveryAttemptDisposition::SettledByPeer { .. }
                | ProcessRecoveryAttemptDisposition::LeaseLost { .. }
                | ProcessRecoveryAttemptDisposition::ExternallyOwned,
            ) => {}
            ProcessRecoveryOutcome::Deferred(ProcessRecoveryAttemptDisposition::BackendError {
                operation,
                error,
            }) => {
                self.emit_worker_fault(ProcessWorkerFault::RecoveryBackendError {
                    process_id: process_id.to_string(),
                    operation,
                    error,
                })
                .await;
            }
            ProcessRecoveryOutcome::RunFailed(error) => {
                self.emit_worker_fault(ProcessWorkerFault::RecoveryRunFailed {
                    process_id: process_id.to_string(),
                    error: error.to_string(),
                })
                .await;
            }
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
