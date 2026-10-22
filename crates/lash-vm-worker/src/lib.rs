//! Exec-spawned model-code workers and their bounded pool (FIG-4160).
//!
//! Hosts register [`worker_entry`] with the embedding they build before
//! constructing credentials, or ship the helper executable. Both use an
//! empty environment and one inherited socket. A host names its dialects:
//! each is a build feature (`dialect-typescript`, `dialect-python`), none is
//! a default, and an embedding with no dialect is refused (FIG-5990). The language is the sandbox; the process contains native crashes.
//! Pool failures fence the checkout and never retry guest execution locally.

mod embedding;

/// Internal worker loading for runtime laws and measurement tools.
/// Hosts submit runs through the worker service.
#[doc(hidden)]
pub mod core_support {
    pub use crate::embedding::library_for;
}

#[cfg(feature = "dhat-heap")]
mod heap_profile;
#[cfg(all(unix, feature = "testing"))]
pub mod in_process;
/// lash's own packages: the natives of each, by its path, as
/// `lash-vm-releases`' build defines them.
#[path = "../../lash-vm-releases/src/library.rs"]
mod library;
#[cfg(all(test, unix))]
mod page_laws;
#[cfg(all(test, feature = "dialect-typescript", feature = "dialect-python"))]
mod read_laws;
#[cfg(all(test, unix))]
mod turn_laws;
#[cfg(feature = "dialect-typescript")]
pub use embedding::typescript;
pub use embedding::{EmbedError, Embedder, Embedding};
#[cfg(feature = "dialect-python")]
pub use embedding::{python, python_with_limits};
#[cfg(feature = "dialect-python")]
pub use lash_dialect_python::SourceLimits as PythonSourceLimits;
pub use lash_kernel_dialect::Dialect;

#[cfg(unix)]
mod entry;
#[cfg(unix)]
mod host;
mod perf_witness;
#[cfg(unix)]
mod process;
#[cfg(unix)]
mod save;
mod service;
#[cfg(unix)]
mod worker;
#[cfg(unix)]
pub use entry::{Embed, worker_entry};
pub use lash_kernel_vm::PreparedLibrary;
pub use lash_vm_client::PoolError;

/// Configures what a worker runs from the parent's working policy.
#[cfg(not(unix))]
pub type Embed = dyn Fn(&lash_vm_client::WorkerTuning) -> Result<Embedder, EmbedError>;

#[cfg(not(unix))]
pub fn worker_entry(_embed: &Embed) -> Result<bool, PoolError> {
    Err(PoolError::UnsupportedPlatform)
}

/// The configuration of the helper executable and of a host that runs
/// every dialect its build names: each dialect this crate was built with
/// (`dialect-typescript`, `dialect-python`). A build with neither feature
/// names none, and the embedding is refused when it is bound
/// ([`EmbedError::NoDialect`]).
///
/// # Errors
///
/// [`EmbedError`].
pub fn built_dialects(tuning: &lash_vm_client::WorkerTuning) -> Result<Embedder, EmbedError> {
    let embedder = Embedder::new();
    #[cfg(feature = "dialect-typescript")]
    let embedder = embedder.dialect(typescript(tuning)?)?;
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
    Ok(embedder)
}

/// [`built_dialects`] bound to lash's own packages, as this process's
/// parent holds them: the embedding of a host that installs no package of
/// its own.
///
/// # Errors
///
/// [`EmbedError`].
pub fn built_embedding(tuning: &lash_vm_client::WorkerTuning) -> Result<Embedding, EmbedError> {
    let release = lash_vm_releases::helper_release()
        .map_err(|error| EmbedError::Registry(error.to_string()))?;
    let library = lash_vm_client::packages::WorkerLibrary::new(
        &release.functions,
        release.resolver_roots(),
        &release.content.documents,
    )?;
    built_dialects(tuning)?.finish(&library)
}

#[cfg(all(unix, feature = "testing"))]
pub use entry::worker_entry_with_hook;

mod native_tables {
    include!(concat!(env!("OUT_DIR"), "/native_tables.rs"));
}
