use crate::PluginError;

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessRecoveryAttemptDisposition {
    /// Another live owner holds the process lease.
    Busy,
    /// The process is no longer a non-terminal candidate after enumeration.
    Absent,
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

pub(super) enum RecoveryStoreDisposition<T> {
    Ready(T),
    Busy,
    Absent,
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
