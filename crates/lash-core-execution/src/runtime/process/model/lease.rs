//! Durable process leases: the `(owner, lease_token, fencing_token)` triple
//! that keeps exactly one worker executing one non-terminal process.
//!
//! Carved out of `model.rs` unchanged to bring that file back under the
//! production line budget (FIG-2996).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{ProcessId, SessionId};

/// Wire-format version stamped on every persisted [`ProcessLease`].
///
/// Bump when the on-wire shape of `ProcessLease` changes in a way that older
/// code cannot safely deserialize. Version 2 replaced the bare `owner_id`
/// string with a full [`LeaseOwnerIdentity`](crate::LeaseOwnerIdentity)
/// carrying incarnation and liveness metadata for fenced reclaim.
pub const PROCESS_LEASE_SCHEMA_VERSION: u32 = 2;

/// A persisted process lease was written under a schema this reader does not support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessLeaseSchemaVersionError {
    pub actual: u32,
    pub expected: u32,
}

impl fmt::Display for ProcessLeaseSchemaVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported process lease schema version {}; expected {}",
            self.actual, self.expected
        )
    }
}

impl std::error::Error for ProcessLeaseSchemaVersionError {}

/// Refuses a persisted process lease whose exact schema version is unsupported.
pub fn ensure_process_lease_schema_version(
    actual: u32,
) -> Result<(), ProcessLeaseSchemaVersionError> {
    if actual == PROCESS_LEASE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ProcessLeaseSchemaVersionError {
            actual,
            expected: PROCESS_LEASE_SCHEMA_VERSION,
        })
    }
}

/// Durable session stores owned exclusively by one process execution.
pub fn process_runtime_session_ids(process_id: &ProcessId) -> [SessionId; 2] {
    [
        SessionId::from(format!("process-env:{process_id}")),
        SessionId::from(format!("process-session-turn:{process_id}")),
    ]
}

/// Durable lease over a non-terminal background process.
///
/// The lease pair `(owner, lease_token)` plus `fencing_token` are how lash guarantees that
/// one non-terminal process is re-executed by exactly one worker at a time —
/// even after a crash, even across two workers that both sweep the same
/// registry for recoverable work. The durable backend
/// (`lash-sqlite-store`) uses these to serialize concurrent claims on the same
/// `process_id`; future distributed durable backends use the *same* fields to
/// coordinate workers that don't share a file system.
///
/// The owner is a full [`LeaseOwnerIdentity`](crate::LeaseOwnerIdentity), whose
/// owner and incarnation IDs distinguish successive holders. Reclaim remains
/// TTL- and fencing-token-based through
/// [`ProcessRegistry::reclaim_process_lease`](super::ProcessRegistry::reclaim_process_lease).
///
/// **This is not single-process theatre.** The owner / fencing-token /
/// lease-token triple is the public contract that lets any backend detect and
/// reject stale writers. Treat it as load-bearing, not defensive.
#[derive(Clone, Debug, Serialize)]
pub struct ProcessLease {
    pub schema_version: u32,
    pub process_id: ProcessId,
    pub owner: crate::LeaseOwnerIdentity,
    pub lease_token: String,
    pub fencing_token: u64,
    pub claimed_at_epoch_ms: u64,
    pub expires_at_epoch_ms: u64,
}

/// Outcome of claiming (or reclaiming) a [`ProcessLease`].
///
/// Mirrors [`SessionExecutionLeaseClaimOutcome`](crate::SessionExecutionLeaseClaimOutcome):
/// a busy outcome carries the observed holder so the claimant can assess its
/// liveness and perform a fenced reclaim on exactly the lease it observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProcessLeaseClaimOutcome {
    Acquired(ProcessLease),
    Busy { holder: ProcessLease },
}

impl ProcessLeaseClaimOutcome {
    /// Returns the newly acquired lease to process-store implementors and `None` when another
    /// holder remains busy; the busy holder is not discarded before this projection.
    pub fn acquired(self) -> Option<ProcessLease> {
        match self {
            Self::Acquired(lease) => Some(lease),
            Self::Busy { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessLeaseCompletion {
    pub process_id: ProcessId,
    pub lease_token: String,
    /// The fencing generation the presented lease was minted under. The
    /// release verdict compares it against the row's retained
    /// `lease_fencing_token` so a superseded generation cannot release its
    /// successor's lease (FIG-3388).
    pub fencing_token: u64,
}

impl ProcessLeaseCompletion {
    /// Captures the process ID, exact lease token and fencing generation that
    /// process-store implementors must present to complete or release the
    /// claimed execution.
    pub fn from_lease(lease: &ProcessLease) -> Self {
        Self {
            process_id: lease.process_id.clone(),
            lease_token: lease.lease_token.clone(),
            fencing_token: lease.fencing_token,
        }
    }
}
