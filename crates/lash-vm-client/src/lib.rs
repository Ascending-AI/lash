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
pub use config::{Deadlines, PoolConfig, WorkerEntry};
pub use context::{ProjectionDescription, ProjectionRead, RunContext};
pub use error::PoolError;
pub use measurements::{ExecutionClass, ExecutionReceipt, PoolCounters, PoolMeasurements};
pub use pool::{Checkout, ExecutionBudget, ParkOutcome, PoolStats, WorkerPool};
pub use remote_state::{RemoteRestoreError, RemoteState, RemoteVm};

mod projections;
pub use projections::Projections;

mod recovery;
pub use recovery::RecoveryExecution;

mod artifact;
pub use artifact::{InspectedArtifact, ProcessMetadata};
