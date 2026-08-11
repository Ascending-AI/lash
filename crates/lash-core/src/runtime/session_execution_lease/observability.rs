use crate::SessionExecutionLease;

pub(super) fn trace_commit_busy_advisory(session_id: &str, holder: &SessionExecutionLease) {
    let holder_owner_id_sha256 = crate::stable_hash::sha256_hex(holder.owner.owner_id.as_bytes());
    let holder_incarnation_id_sha256 =
        crate::stable_hash::sha256_hex(holder.owner.incarnation_id.as_bytes());
    tracing::info!(
        session_id,
        holder_owner_id_sha256,
        holder_incarnation_id_sha256,
        consulted = "session_execution_lease",
        outcome = "proceeding_under_commit_cas",
        event = "session_execution_lease.commit_busy_advisory",
        "live lease holder observed: proceeding under the commit CAS fence"
    );
}
