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

/// The compiled source identity used to refuse parent/worker build mismatches.
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
