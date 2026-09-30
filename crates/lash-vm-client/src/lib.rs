//! Parent-owned worker pool, framed transport and opaque guest-state service.
mod broker;
mod config;
mod context;
mod error;
pub mod ipc;
mod pool;
mod remote_state;
pub mod service;
#[cfg(any(test, feature = "testing"))]
mod testing;
pub use broker::PoolSlots;
pub use config::{Deadlines, PoolConfig, WorkerEntry};
pub use context::{ProjectionDescription, ProjectionRead, RunContext};
pub use error::PoolError;
pub use pool::{Checkout, ExecutionBudget, ParkOutcome, PoolStats, WorkerPool};
pub use remote_state::{RemoteRestoreError, RemoteState, RemoteVm};

/// Exact compiled worker-source identity; parent and helper must agree.
pub fn build_identity() -> lash_vm_protocol::BuildIdentity {
    lash_vm_protocol::BuildIdentity::new(format!(
        "lash-worker/{}/{}/{}/debug-{}/testing-{}",
        env!("LASH_VM_WORKER_BUILD_FINGERPRINT"),
        std::env::consts::ARCH,
        std::env::consts::OS,
        cfg!(debug_assertions),
        cfg!(feature = "testing")
    ))
}

mod projections;
pub use projections::Projections;

mod recovery;
pub use recovery::RecoveryExecution;

mod artifact;
pub use artifact::{InspectedArtifact, ProcessMetadata};
