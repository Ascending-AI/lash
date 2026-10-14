//! Parent-owned worker pool, framed transport and the kernel machine a
//! pooled worker hosts.
mod config;
mod error;
/// A kernel run as checkpoint graph state: the broker's adapter.
pub use lash_vm_broker::kernel::state as graph;
pub mod ipc;
mod machine;
mod measurements;
mod pool;
pub mod service;
#[cfg(any(test, feature = "testing"))]
mod testing;
pub mod wire;
pub use config::{
    Deadlines, PageBudgets, PoolConfig, WorkerConfinement, WorkerEntry, WorkerTuning,
};
pub use error::PoolError;
/// What a [`lash_vm_protocol::SealedGraphState`] is sealed with.
pub use lash_vm_protocol::StateDigest;
/// The VM-protocol vocabulary a worker pool's configuration and outcomes
/// name.
pub use lash_vm_protocol::{
    BootstrapFault, CodecRefusal, DecodeLimits, Detail, DetailLimit, Exchange, ExecutionLease,
    FrameEpoch, GraphStateRefusal, HeaderRefusal, HostReadId, HostReadKind, InfrastructureOutcome,
    KernelRestoreRefusal, OwnerEpoch, PayloadKind, PoolFault, ProtocolBounds, ProtocolBreach,
    ProtocolVersionRefusal, RunBounds, RunInput, RunMeters, RunRefusal, SequenceFault,
    SupervisorEvidence, TransportSequence, VmOwner, WorkerDeploymentFault, WorkerLimit,
    page::PageRefusal,
};
pub use machine::{RemoteMachine, RemoteMachines, RunHost, kernel_reads};
pub use measurements::{ExecutionClass, ExecutionReceipt, PoolCounters, PoolMeasurements};
/// Runtime-only checkout on [`WorkerPool`]; the lash facade does not export it.
pub use pool::runtime_ops::WorkerPoolRuntimeOps;
pub use pool::{Checkout, ExecutionBudget, PoolStats, RunStep, WorkerPool};
