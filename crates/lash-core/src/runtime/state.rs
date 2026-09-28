//! Durable session state.
//!
//! The state struct, its checkpoint components and the durable-head adoption
//! rules live in `lash-core-store`; this module re-exports them at their
//! original path and keeps the one commit helper that needs the runtime's
//! drive fence.

pub use lash_core_store::session_state::*;

use std::sync::atomic::{AtomicBool, Ordering};

/// Commit `commit` from a service that runs either inside a running turn's
/// drive (`drive_fence`) or as a lane-less host service (`None`). The
/// explicit context selects the authority, never scheduling or elapsed time.
pub(crate) async fn commit_in_lane_context(
    drive_fence: Option<&crate::store::DriveFence>,
    store: std::sync::Arc<dyn crate::RuntimePersistence>,
    mut commit: crate::RuntimeCommit,
    resident_graph_head_stale: &AtomicBool,
) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
    let Some(fence) = drive_fence else {
        return crate::store::commit_runtime_state_verified(store.as_ref(), commit).await;
    };
    commit.drive_fence = Some(Box::new(fence.clone()));
    let result = crate::store::commit_runtime_state_verified(store.as_ref(), commit).await;
    if result.is_ok() {
        // The drive remains current, but this service committed from a
        // snapshot outside the owning runtime. Force a deliberate head
        // reload before its next physical turn; planner CAS is not the
        // graph-freshness discovery mechanism.
        resident_graph_head_stale.store(true, Ordering::Release);
    }
    result
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
