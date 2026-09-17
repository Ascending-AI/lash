use crate::ProcessId;
use crate::ProcessStatus;

/// Report from one admission pass of
/// [`DurableProcessWorker::drive_pending_processes`](crate::runtime::DurableProcessWorker::drive_pending_processes).
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
    pub admitted: Vec<ProcessId>,
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
    /// [`Busy`](ProcessRecoveryAttemptOutcome::Busy) row is dropped when
    /// this same call already admitted that id — a call must never report its
    /// own admission as somebody else's contention.
    pub(crate) fn absorb(&mut self, nested: Self) {
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
    pub(crate) fn push_deferred(&mut self, entry: ProcessAdmissionDeferred) {
        if matches!(entry.disposition, ProcessRecoveryAttemptOutcome::Busy)
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
    pub process_id: ProcessId,
    /// Typed reason the row was not admitted by this call.
    pub disposition: ProcessRecoveryAttemptOutcome,
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
        process_id: ProcessId,
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
        process_id: ProcessId,
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
    pub process_id: ProcessId,
    /// Typed reason the row did not produce confirmed terminal evidence.
    pub disposition: ProcessRecoveryAttemptOutcome,
}

/// Why a process recovery or drain attempt did not act on a row.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessRecoveryAttemptOutcome {
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
    /// native worker and one logged by an out-of-crate tier (the Restate
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
