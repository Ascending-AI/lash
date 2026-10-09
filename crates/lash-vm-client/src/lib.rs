//! Parent-owned worker pool, framed transport and opaque guest-state service.
mod broker;
mod config;
mod context;
mod error;
pub mod ipc;
mod measurements;
mod pool;
mod remote_state;
pub mod service;
#[cfg(any(test, feature = "testing"))]
mod testing;
pub use broker::PoolSlots;
pub use config::{Deadlines, PoolConfig, WorkerEntry, WorkerTuning};
pub use context::{ProjectionAnswer, ProjectionDescription, ProjectionRead, RunContext};
pub use error::PoolError;
/// The VM-protocol vocabulary a worker pool's configuration and outcomes
/// name.
pub use lash_vm_protocol::{
    BootstrapFault, CodecRefusal, DecodeLimits, Detail, Exchange, ExecutionLease, FrameEpoch,
    HeaderRefusal, InfrastructureOutcome, OpaqueStateRefusal, OwnerEpoch, PayloadKind, PoolFault,
    ProtocolBounds, ProtocolBreach, ProtocolVersionRefusal, RunInput, RunRefusal, SequenceFault,
    SupervisorEvidence, TransportSequence, VmContractComponent, VmLimits, VmOwner, VmStateKind,
    WorkerDeploymentFault, WorkerLimit,
};
pub use measurements::{ExecutionClass, ExecutionReceipt, PoolCounters, PoolMeasurements};
/// Runtime-only checkout on [`WorkerPool`]; the lash facade does not export it.
pub use pool::runtime_ops::WorkerPoolRuntimeOps;
pub use pool::{Checkout, ExecutionBudget, ParkOutcome, PoolStats, WorkerPool};
pub use remote_state::{RemoteRestoreError, RemoteState, RemoteVm};

mod projections;
pub use projections::Projections;

mod artifact;
pub use artifact::{InspectedArtifact, InspectedDocument, ProcessMetadata};
