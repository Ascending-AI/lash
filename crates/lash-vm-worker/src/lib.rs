//! Exec-spawned model-code workers and their bounded pool (FIG-4160).
//!
//! Hosts register [`worker_entry`] before constructing credentials, or ship
//! the helper executable. Both use an empty environment and one inherited
//! socket. The language is the sandbox; the process contains native crashes.
//! Pool failures fence the checkout and never retry guest execution locally.

mod embedding;
#[cfg(feature = "dhat-heap")]
mod heap_profile;
/// The library functions lash ships, as `lash-vm-releases`' build defines
/// the helpers against them.
#[path = "../../lash-vm-releases/src/library.rs"]
mod library;
pub use embedding::{EmbedError, Embedder, Embedding, standard, typescript};

#[cfg(unix)]
mod entry;
#[cfg(unix)]
mod host;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod service;
#[cfg(unix)]
mod worker;
#[cfg(unix)]
pub use entry::{Embed, worker_entry, worker_entry_with};
pub use lash_vm_client::PoolError;
#[cfg(not(unix))]
pub fn worker_entry() -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

/// Assembles what a worker runs from the parent's working policy.
#[cfg(not(unix))]
pub type Embed = dyn Fn(&lash_vm_client::WorkerTuning) -> Result<Embedding, EmbedError>;

#[cfg(not(unix))]
pub fn worker_entry_with(_embed: &Embed) -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

#[cfg(all(unix, feature = "testing"))]
pub use entry::worker_entry_with_hook;
