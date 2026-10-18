//! Parent-owned worker pool, framed transport and the kernel machine a
//! pooled worker hosts.
#![deny(clippy::future_not_send)]

mod cgroup;
mod config;
mod error;
/// A kernel run as checkpoint graph state: the broker's adapter.
pub use lash_vm_broker::kernel::pages::{DiskSpool, SpoolRoot};
pub use lash_vm_broker::kernel::state as graph;
pub mod ipc;
mod machine;
mod measurements;
mod phases;
mod pool;
pub use phases::{EffectPhase, EffectPhases};
pub mod service;
#[cfg(any(test, feature = "testing"))]
mod testing;
pub mod wire;
pub use cgroup::{CpuShare, WorkerCgroup};
pub use config::{
    Deadlines, PageBudgets, PoolConfig, WorkerConfinement, WorkerEntry, WorkerTuning,
};
pub use error::PoolError;
/// The VM-protocol vocabulary a worker pool's configuration and outcomes
/// name.
pub use lash_vm_protocol::{
    BootstrapFault, BoundReached, CgroupController, CgroupRefusal, Charge, CheckpointRefusal,
    CodecRefusal, DecodeLimits, Detail, DetailLimit, Exchange, ExecutionLease, FrameEpoch,
    HeaderRefusal, HostReadId, HostReadKind, InfrastructureOutcome, KernelRestoreRefusal,
    LimitDecision, LimitedBound, MemoryMeter, OwnerEpoch, PayloadKind, PoolFault, ProtocolBounds,
    ProtocolBreach, ProtocolVersionRefusal, RunBounds, RunInput, RunMeters, RunRefusal,
    SequenceFault, SupervisorEvidence, TransportSequence, VmOwner, WorkerDeploymentFault,
    WorkerLimit, WorkerWork,
    page::{HeapFault, HostResource, PageRefusal},
};
pub use machine::{
    RemoteMachine, RemoteMachines, RunChargeLimiter, RunHost, RunMemoryLimiter, RunStart,
    kernel_reads,
};
pub use measurements::{
    ExecutionClass, ExecutionReceipt, FrameTraffic, IpcTraffic, PoolCounters, PoolMeasurements,
};
/// Runtime-only checkout on [`WorkerPool`]; the lash facade does not export it.
pub use pool::runtime_ops::WorkerPoolRuntimeOps;
pub use pool::{Checkout, ExecutionBudget, PoolStats, RunStep, WorkerPool};
