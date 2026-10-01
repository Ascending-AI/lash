use super::{LASHLANG_SEGMENT_STATE_VERSION, LashlangProcessHost, LashlangSegmentState};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

/// The segment state a boundary hands over: the worker's continuation bytes,
/// sealed as opaque state, beside the parent's own ledgers the next segment
/// resumes with. An error names why the state could not be captured.
pub(super) fn capture_segment(
    vm: lash_vm_protocol::OpaqueVmState,
    host: &LashlangProcessHost<'_>,
    reason: lash_core::BoundaryReason,
    program_hash: &str,
) -> Result<lash_core::SegmentHandover, (String, &'static str)> {
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm,
        ordinals: host.ordinals.snapshot(&host.run),
        started_process_ids: host.ctx.started_process_ids(),
        incorporation_ledger: host.ctx.incorporation_ledger_snapshot(),
        pending_summary: host.effect_summary.pending(),
        effect_omissions: host.effect_summary.omissions(),
        outstanding_groups: host.ctx.outstanding_groups_snapshot(),
        held_tool_calls: host.ctx.held_tool_calls_snapshot(),
        // The worker released at this boundary settled its measured usage,
        // so the budget holds everything the body consumed so far.
        worker_recovery: host.worker_recovery.crossed(
            host.workers
                .execution_budget()
                .map_or(host.worker_recovery.totals, |budget| {
                    budget.recovery_totals()
                }),
        ),
    };
    let engine_state = serde_json::to_vec(&segment_state).map_err(|error| {
        (
            error.to_string(),
            "lashlang segment continuation was not serializable; continuing",
        )
    })?;
    Ok(lash_core::SegmentHandover {
        reason,
        program_hash: program_hash.to_owned(),
        engine_state,
    })
}
