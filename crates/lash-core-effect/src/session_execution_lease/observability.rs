use super::SessionExecutionLeaseCommitEvidence;
use crate::SessionId;
use crate::{SessionExecutionLease, StoreError};

pub use lash_core_store::session_execution_lease::{trace_acquisition, trace_busy};

pub(super) fn trace_commit_busy_advisory(session_id: &SessionId, holder: &SessionExecutionLease) {
    let holder_owner_id_sha256 = crate::stable_hash::sha256_hex(holder.owner.owner_id.as_bytes());
    let holder_incarnation_id_sha256 =
        crate::stable_hash::sha256_hex(holder.owner.incarnation_id.as_bytes());
    let holder_executor_id_sha256 = crate::stable_hash::sha256_hex(holder.executor_id.as_bytes());
    tracing::info!(
        target: "lash_core::runtime::session_execution_lease::observability",
        session_id = session_id.as_str(),
        holder_owner_id_sha256,
        holder_incarnation_id_sha256,
        holder_executor_id_sha256,
        consulted = "session_execution_lease",
        outcome = "proceeding_under_commit_cas",
        event = "session_execution_lease.commit_busy_advisory",
        "live lease holder observed: proceeding under the commit CAS fence"
    );
}

/// Report a commit whose head compare-and-set lost to a concurrent writer.
///
/// This is the authority speaking, not the advisory lease: a repeated rejection
/// with `lease_lost` false can become livelock when it recurs: `lane_held`
/// distinguishes a holder-side rejection from a distinct Busy claimant that
/// proceeded lane-less. A rejection after `lost` / `taken_over` is an ordinary
/// handoff. Non-CAS store failures are left to their own error paths.
pub fn trace_commit_cas_rejected(
    session_id: &SessionId,
    evidence: Option<&SessionExecutionLeaseCommitEvidence>,
    claimant: &crate::LeaseOwnerIdentity,
    claimant_executor_id: &str,
    err: &StoreError,
) {
    let StoreError::HeadRevisionConflict { expected, actual } = err else {
        return;
    };
    // The writer is always nameable: it is the lane holder when one was held, and
    // otherwise the runner that proceeded under the busy advisory. A rejection is
    // never anonymous.
    let owner = evidence.map_or(claimant, |evidence| &evidence.owner);
    let executor_id = evidence.map_or(claimant_executor_id, |evidence| {
        evidence.executor_id.as_str()
    });
    tracing::warn!(
        target: "lash_core::runtime::session_execution_lease::observability",
        session_id = session_id.as_str(),
        fencing_token = evidence.map(|evidence| evidence.fencing_token),
        owner_id = %owner.owner_id,
        incarnation_id = %owner.incarnation_id,
        executor_id,
        lane_held = evidence.is_some(),
        lease_lost = evidence.is_some_and(|evidence| evidence.lease_lost),
        expected_head_revision = expected,
        actual_head_revision = actual,
        consulted = "session_head_revision",
        outcome = "commit_rejected",
        event = "session_execution_lease.commit_cas_rejected",
        "the commit's head compare-and-set was rejected; another writer published first"
    );
}
