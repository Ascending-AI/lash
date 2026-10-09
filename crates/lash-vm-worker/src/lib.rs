//! Exec-spawned model-code workers and their bounded pool (FIG-4160).
//!
//! Hosts register [`worker_entry`] before constructing credentials, or ship
//! the helper executable. Both use an empty environment and one inherited
//! socket. The language is the sandbox; the process contains native crashes.
//! Pool failures fence the checkout and never retry guest execution locally.

mod frontend;
#[cfg(feature = "dhat-heap")]
mod heap_profile;
pub use frontend::{Frontend, FrontendRefusal};

#[cfg(unix)]
mod entry;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod projection;
#[cfg(unix)]
mod service;
#[cfg(unix)]
mod worker;
#[cfg(unix)]
pub use entry::{worker_entry, worker_entry_with_frontend};
pub use lash_vm_client::PoolError;
#[cfg(not(unix))]
pub fn worker_entry() -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

#[cfg(not(unix))]
pub fn worker_entry_with_frontend(_frontend: &dyn Frontend) -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

#[cfg(all(unix, feature = "testing"))]
pub use entry::worker_entry_with_hook;
