//! Durable session state.
//!
//! The state struct, its checkpoint components and the durable-head adoption
//! rules live in `lash-core-store`; this module re-exports them at their
//! original path and keeps the one commit helper that needs the runtime's
//! session-execution lease.

pub use lash_core_store::session_state::*;

use std::sync::atomic::{AtomicBool, Ordering};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn commit_in_lane_context(
    held_session_execution_lease: Option<&super::session_execution_lease::BorrowedLaneAuthority>,
    store: std::sync::Arc<dyn crate::RuntimePersistence>,
    commit: crate::RuntimeCommit,
    runtime_lease_owner: &crate::LeaseOwnerIdentity,
    runtime_lease_executor_id: &str,
    lease_timings: crate::store::LeaseTimings,
    clock: std::sync::Arc<dyn crate::Clock>,
    resident_graph_head_stale: &AtomicBool,
) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
    // Dual-context sites run either under the parent turn's held lane or as
    // lane-less host services. Select authority from the explicit context,
    // never from scheduling or elapsed time.
    if let Some(lease) = held_session_execution_lease {
        let result = super::session_execution_lease::commit_runtime_state_with_borrowed_lease(
            lease,
            store,
            commit,
            runtime_lease_owner,
        )
        .await;
        if result.is_ok() {
            // The guard remains current, but this service committed from a
            // snapshot outside the owning runtime. Force a deliberate head
            // reload before its next physical turn; planner CAS is not the
            // graph-freshness discovery mechanism.
            resident_graph_head_stale.store(true, Ordering::Release);
        }
        result
    } else {
        super::session_execution_lease::commit_runtime_state_with_fresh_session_execution_lease(
            store,
            commit,
            runtime_lease_owner,
            runtime_lease_executor_id,
            lease_timings,
            clock,
        )
        .await
    }
}
