//! Exec-spawned model-code workers and their bounded pool (FIG-4160).
//!
//! Hosts register [`worker_entry`] before constructing credentials, or ship
//! the helper executable. Both use an empty environment and one inherited
//! socket. The language is the sandbox; the process contains native crashes.
//! Pool failures fence the checkout and never retry guest execution locally.

mod config;
#[cfg(unix)]
mod entry;
mod error;
mod identity;
#[cfg(unix)]
mod pool;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod worker;

pub use config::{Deadlines, PoolConfig, WorkerEntry};
#[cfg(unix)]
pub use entry::worker_entry;
pub use error::PoolError;
pub use identity::build_identity;
#[cfg(unix)]
pub use pool::{Checkout, ExecutionBudget, PoolStats, WorkerPool};
#[cfg(unix)]
pub use worker::RunContext;

#[cfg(not(unix))]
pub fn worker_entry(_build: lash_vm_protocol::BuildIdentity) -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

#[cfg(all(unix, feature = "testing"))]
pub use entry::worker_entry_with_hook;
