use super::SessionExecutionLeaseCommitEvidence;
use crate::{SessionExecutionLease, SessionExecutionLeaseAcquisition, StoreError};

/// Report a successful claim, including the atomic displacement evidence when
/// this claim took the lane over from a lapsed holder.
pub(crate) fn trace_acquisition(acquisition: &SessionExecutionLeaseAcquisition) {
    let lease = &acquisition.lease;
    tracing::info!(
        session_id = %lease.session_id,
        owner_id = %lease.owner.owner_id,
        incarnation_id = %lease.owner.incarnation_id,
        executor_id = %lease.executor_id,
        fencing_token = lease.fencing_token,
        expires_at_epoch_ms = lease.expires_at_epoch_ms,
        event = "session_execution_lease.acquired",
        "acquired session execution lease"
    );
    if let Some(displaced) = acquisition.displaced.as_ref() {
        trace_taken_over(lease, displaced);
    }
}

pub(super) fn trace_commit_busy_advisory(session_id: &str, holder: &SessionExecutionLease) {
    let holder_owner_id_sha256 = crate::stable_hash::sha256_hex(holder.owner.owner_id.as_bytes());
    let holder_incarnation_id_sha256 =
        crate::stable_hash::sha256_hex(holder.owner.incarnation_id.as_bytes());
    let holder_executor_id_sha256 = crate::stable_hash::sha256_hex(holder.executor_id.as_bytes());
    tracing::info!(
        session_id,
        holder_owner_id_sha256,
        holder_incarnation_id_sha256,
        holder_executor_id_sha256,
        consulted = "session_execution_lease",
        outcome = "proceeding_under_commit_cas",
        event = "session_execution_lease.commit_busy_advisory",
        "live lease holder observed: proceeding under the commit CAS fence"
    );
}

/// Report a takeover from the winning claim, naming the holder it displaced.
///
/// The fields describe the emitter, as they do on every other lease event:
/// `fencing_token`/`owner_id`/`incarnation_id` are the *new* holder, and the
/// `displaced_*` fields are the lapsed holder this claim took the lane from. Both
/// come from one atomic claim, so a log line here is true regardless of whether
/// the displaced runner is still alive to notice.
fn trace_taken_over(
    lease: &SessionExecutionLease,
    displaced: &crate::store::SessionExecutionLeaseDisplacement,
) {
    tracing::info!(
        session_id = %lease.session_id,
        owner_id = %lease.owner.owner_id,
        incarnation_id = %lease.owner.incarnation_id,
        executor_id = %lease.executor_id,
        fencing_token = lease.fencing_token,
        displaced_owner_id = %displaced.owner.owner_id,
        displaced_incarnation_id = %displaced.owner.incarnation_id,
        displaced_executor_id = %displaced.executor_id,
        displaced_fencing_token = displaced.fencing_token,
        displaced_expired_at_epoch_ms = displaced.expired_at_epoch_ms,
        consulted = "session_execution_lease_claim",
        outcome = "taken_over",
        event = "session_execution_lease.taken_over",
        "took the session execution lane over from a lapsed holder"
    );
}

/// Report a commit whose head compare-and-set lost to a concurrent writer.
///
/// This is the authority speaking, not the advisory lease: a repeated rejection
/// with `lease_lost` false can become livelock when it recurs: `lane_held`
/// distinguishes a holder-side rejection from a distinct Busy claimant that
/// proceeded lane-less. A rejection after `lost` / `taken_over` is an ordinary
/// handoff. Non-CAS store failures are left to their own error paths.
pub(in crate::runtime) fn trace_commit_cas_rejected(
    session_id: &str,
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
        session_id,
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

pub(crate) fn trace_busy(
    session_id: &str,
    claimant: &crate::LeaseOwnerIdentity,
    claimant_executor_id: &str,
    holder: &SessionExecutionLease,
) {
    tracing::debug!(
        session_id,
        claimant_owner_id = %claimant.owner_id,
        claimant_incarnation_id = %claimant.incarnation_id,
        claimant_executor_id,
        holder_owner_id = %holder.owner.owner_id,
        holder_incarnation_id = %holder.owner.incarnation_id,
        holder_executor_id = %holder.executor_id,
        holder_fencing_token = holder.fencing_token,
        holder_expires_at_epoch_ms = holder.expires_at_epoch_ms,
        event = "session_execution_lease.busy",
        "session execution lease is busy"
    );
}
