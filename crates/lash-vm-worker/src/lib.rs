//! Exec-spawned model-code workers and their bounded pool (FIG-4160).
//!
//! Hosts register [`worker_entry`] with the embedding they build before
//! constructing credentials, or ship the helper executable. Both use an
//! empty environment and one inherited socket. A host names its dialects:
//! each is a build feature (`dialect-typescript`, `dialect-python`), none is
//! a default, and an embedding with no dialect is refused (FIG-5990). The language is the sandbox; the process contains native crashes.
//! Pool failures fence the checkout and never retry guest execution locally.

mod embedding;
#[cfg(all(test, unix))]
mod graph_laws;
#[cfg(feature = "dhat-heap")]
mod heap_profile;
/// The library functions lash ships, as `lash-vm-releases`' build defines
/// each dialect's helpers against them.
#[path = "../../lash-vm-releases/src/library.rs"]
mod library;
#[cfg(all(test, unix))]
mod turn_laws;
#[cfg(feature = "dialect-typescript")]
pub use embedding::typescript;
pub use embedding::{Dialect, EmbedError, Embedder, Embedding};
#[cfg(feature = "dialect-python")]
pub use embedding::{python, python_with_limits};
#[cfg(feature = "dialect-python")]
pub use lash_dialect_python::SourceLimits as PythonSourceLimits;

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
pub use entry::{Embed, worker_entry};
pub use lash_vm_client::PoolError;

/// Assembles what a worker runs from the parent's working policy.
#[cfg(not(unix))]
pub type Embed = dyn Fn(&lash_vm_client::WorkerTuning) -> Result<Embedding, EmbedError>;

#[cfg(not(unix))]
pub fn worker_entry(_embed: &Embed) -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

/// The embedding of the helper executable and of a host that runs every
/// dialect its build names: each dialect this crate was built with
/// (`dialect-typescript`, `dialect-python`). A build with neither feature
/// names none, and the embedding is refused ([`EmbedError::NoDialect`]).
///
/// # Errors
///
/// [`EmbedError`].
pub fn built_dialects(tuning: &lash_vm_client::WorkerTuning) -> Result<Embedding, EmbedError> {
    let embedder = Embedder::kernel()?;
    #[cfg(feature = "dialect-typescript")]
    let embedder = embedder.dialect(typescript(tuning))?;
    #[cfg(feature = "dialect-python")]
    let embedder = embedder.dialect(python_with_limits(
        lash_dialect_python::SourceLimits::new(
            tuning.python_source_bytes,
            tuning.python_source_nesting,
        )
        .map_err(|error| EmbedError::Dialect {
            dialect: lash_vm_releases::PYTHON.to_owned(),
            message: error.to_string(),
        })?,
    ))?;
    #[cfg(not(feature = "dialect-typescript"))]
    let _ = tuning;
    embedder.finish()
}

#[cfg(all(unix, feature = "testing"))]
pub use entry::worker_entry_with_hook;
