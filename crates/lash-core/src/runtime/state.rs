//! Durable session state.
//!
//! The state struct, its checkpoint components and the durable-head adoption
//! rules live in `lash-core-store`; this module re-exports them at their
//! original path and keeps the one commit helper a host service commits
//! through.

pub use lash_core_store::session_state::*;

use std::sync::atomic::{AtomicBool, Ordering};

/// Commit `commit` from a service outside the owning runtime's turn. The
/// service committed from a snapshot the resident runtime does not hold, so
/// a success forces a deliberate head reload before the runtime's next
/// physical turn; planner CAS is not the graph-freshness discovery
/// mechanism.
pub(crate) async fn commit_in_lane_context(
    store: crate::store::SessionStore,
    commit: crate::RuntimeCommit,
    resident_graph_head_stale: &AtomicBool,
    metrics: &lash_trace::telemetry::metrics::TelemetryMetrics,
) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
    let result = store.commit_runtime_state_verified(commit, metrics).await;
    if result.is_ok() {
        resident_graph_head_stale.store(true, Ordering::Release);
    }
    result
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

/// Refuse a switch to a persisted historical frame the resident graph no
/// longer holds (ADR 0112 §7).
///
/// The resident refusal in `open_agent_frame_in_state_with_clock` sees only
/// resident frames. Under a window, a same-session frame below the window base
/// is not resident, so a switch naming it asks the store first: a frame on
/// the active path is historical, and the answer is
/// `HistoricalAgentFrameSwitchUnsupported`. The current frame, and a frame the
/// resident graph already holds, need no read. This is an early answer only;
/// the commit's `NodeIdCollision` stays the authority.
pub(crate) async fn refuse_historical_frame_switch(
    store: Option<&crate::store::SessionStore>,
    session_id: &crate::SessionId,
    current_frame_node_id: Option<&str>,
    session_graph: &crate::SessionGraph,
    frame_key: &crate::FrameKey,
) -> Result<(), crate::RuntimeError> {
    let Some(store) = store else {
        return Ok(());
    };
    let frame_node_id = crate::session_graph::frame_node_id(session_id, frame_key.as_str());
    if current_frame_node_id == Some(frame_node_id.as_str())
        || session_graph.find_node(frame_node_id.as_str()).is_some()
    {
        return Ok(());
    }
    let historical = store
        .contains_active_ancestor(frame_node_id.node_id())
        .await
        .map_err(super::runtime_error_from_store_commit)?;
    if historical {
        return Err(crate::RuntimeError::new(
            crate::RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported,
            "switching to a persisted historical frame requires a commanded config patch, which is not supported",
        ));
    }
    Ok(())
}
