//! Session-execution-lease claim observability.
//!
//! These projections describe durable lease records, so they live beside the
//! store that produces them.

use crate::SessionId;
use crate::{SessionExecutionLease, SessionExecutionLeaseAcquisition};

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

pub(crate) fn trace_busy(
    session_id: &SessionId,
    claimant: &crate::LeaseOwnerIdentity,
    claimant_executor_id: &str,
    holder: &SessionExecutionLease,
) {
    tracing::debug!(
        session_id = session_id.as_str(),
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
