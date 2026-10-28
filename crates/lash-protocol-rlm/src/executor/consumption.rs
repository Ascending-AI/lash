//! Read the meters already used by the kernel and worker supervisor.

use super::{CellServices, host::CellHost};
use lash_core::{CellConsumption, CellFailure, CellResource};
use lash_kernel_vm::{Bounds, Charge, MemoryMeter};
use lash_vm_broker::kernel::Growth;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

pub(super) fn snapshot(
    host: Option<&CellHost<'_>>,
    services: &CellServices,
    bounds: Bounds,
    growth: Option<Growth>,
    failure: Option<&CellFailure>,
) -> Vec<CellConsumption> {
    use CellResource as R;
    let pool = services.workers.config();
    #[expect(
        clippy::expect_used,
        reason = "run_cell begins an execution budget before running or answering setup"
    )]
    let budget = services
        .workers
        .execution_budget()
        .expect("an execution budget");
    let meters = budget.meters();
    let mut values = Vec::new();
    let mut add = |resource, observed: Option<u64>, limit| {
        values.push(CellConsumption {
            resource,
            consumed: observed.map(|used| {
                if resource == R::Charges {
                    used.min(limit)
                } else {
                    used
                }
            }),
            observed,
            limit,
        });
    };
    if bounds.charge == Charge::Exact {
        add(
            R::Charges,
            meters.and_then(|m| m.charged),
            bounds.charge_limit,
        );
    }
    if bounds.memory_meter == MemoryMeter::On {
        add(R::MemoryBytes, meters.map(|m| m.memory), bounds.memory);
    }
    add(
        R::LiveTasks,
        meters.map(|m| u64::from(m.live_tasks)),
        u64::from(bounds.live_tasks),
    );
    add(R::CallDepth, None, u64::from(bounds.call_depth));
    add(
        R::RequestsPerPark,
        None,
        u64::from(bounds.requests_per_park),
    );
    add(R::JoinMembers, None, u64::from(bounds.join_members));
    let nanos =
        |duration: std::time::Duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
    add(
        R::WorkerCpuNanos,
        Some(nanos(budget.totals().1)),
        nanos(pool.deadlines.cumulative_cpu),
    );
    add(
        R::HeldWallNanos,
        Some(nanos(budget.held())),
        nanos(pool.deadlines.execution_wall),
    );
    add(R::ComputeNanos, None, nanos(pool.deadlines.compute));
    add(
        R::SerializationNanos,
        None,
        nanos(pool.deadlines.serialization),
    );
    add(
        R::EffectValueBytes,
        None,
        pool.protocol.max_effect_value_bytes,
    );
    add(R::VmStateBytes, None, pool.protocol.max_vm_state_bytes);
    add(
        R::FrameBytes,
        None,
        u64::from(pool.protocol.decode.max_frame_bytes),
    );
    add(R::HostReadNanos, None, nanos(pool.deadlines.host_read));
    if let Some(cgroup) = &pool.cgroup {
        add(R::WorkerMemoryBytes, None, cgroup.memory_max_bytes);
    }
    add(
        R::Effects,
        growth.map(|g| g.effects),
        pool.cell_lifetime.effects,
    );
    add(
        R::CheckpointBytes,
        growth.map(|g| g.checkpoint_bytes),
        pool.cell_lifetime.checkpoint_bytes,
    );
    if let Some(host) = host {
        add(
            R::ToolCalls,
            Some(host.ledgers().counted() as u64),
            host.ctx.max_tool_calls().get() as u64,
        );
    }
    if let Some(limit) = failure.and_then(|failure| failure.worker_limit) {
        use lash_vm_client::WorkerLimit as W;
        let size = match limit {
            W::EffectValue { size, bound } => Some((R::EffectValueBytes, size, bound)),
            W::VmState { size, bound } => Some((R::VmStateBytes, size, bound)),
            W::Frame { size, bound, .. } => Some((R::FrameBytes, size, bound)),
            _ => None,
        };
        if let Some((resource, used, limit)) = size
            && let Some(reading) = values
                .iter_mut()
                .find(|reading| reading.resource == resource)
        {
            reading.consumed = Some(used);
            reading.observed = Some(used);
            reading.limit = limit;
        }
    }
    values
}
