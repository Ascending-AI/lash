//! Install [`KernelProcessPluginFactory`] beside the session protocol to edit
//! and execute workflows. The RLM protocol contributes this engine itself.
//!
//! Hosting the kernel machine: the worker pool, the worker's entry and what
//! it registers, and the host boundary a document runs against.
//!
//! A run executes in a worker process that hosts one kernel machine. The
//! worker registers every library function when it starts; a document names
//! functions by identity and carries none.

/// One shared pool for Code mode cells, process bodies, and pure language work.
///
/// SDK releases attach `lash-sdk-worker-VERSION-TARGET.tar.gz` and its SHA256.
/// Pass the extracted `bin/lash-vm-worker` path to [`WorkerService::subprocess`]
/// or [`WorkerEntry::helper`]. Hosts may build the SDK from registry packages.
/// The manifest records protocol and crate diagnostics; crate versions never
/// decide compatibility. Pool admission refuses an unsupported wire version.
/// [`WorkerService::default`] explicitly defaults to the helper beside the host
/// executable and does not search PATH or a repository.
///
/// A host names the dialects it runs: `dialect-typescript` and
/// `dialect-python` are build features, neither a default, and each builds
/// that dialect's helper releases for the parent. A single-binary host also
/// enables the `worker` feature and calls `worker_entry` with the embedding
/// it builds, naming each dialect
/// (`WorkerEmbedder::kernel()?.dialect(typescript_worker_dialect(tuning))?.finish()`),
/// as its first action, before runtime creation, credentials, stores or
/// providers, and returns from main when that call returns `true`. It
/// selects [`WorkerEntry::reexec`]. An embedding with no dialect is refused.
/// A host that runs the shipped worker executable links no worker
/// embedding; that executable installs the dialects it was built with.
/// `dialect-typescript` also gives the host `lash::typescript`, the front
/// end and printer in its own process.
/// A session or process records its dialect at creation, and a worker whose
/// embedding lacks it refuses the run
/// ([`RunRefusal::DialectNotInstalled`]).
/// `examples/worker_host.rs` proves this bootstrap.
/// The child starts with an empty environment and closes inherited descriptors.
/// The language bounds guest authority; the process contains native crashes.
/// Before it serves, the worker confines itself ([`WorkerConfinement`]): a
/// fixed system-call allowlist, under which it opens nothing and never maps
/// memory writable and executable at once, and an address-space ceiling. A
/// native escape keeps only what that allowlist grants.
pub use lash_vm_client::service::{DialectRefusal, Service as WorkerService, SourceServiceError};
/// The worker pool [`WorkerService::pool`] starts, which a host prewarms at
/// startup, and the counts it reports.
pub use lash_vm_client::{PoolStats as WorkerPoolStats, WorkerPool};

/// Host-selected worker entry, pool bounds, execution deadlines and process
/// confinement.
pub use lash_vm_client::{
    Deadlines as WorkerDeadlines, PageBudgets as WorkerPageBudgets, PoolConfig as WorkerPoolConfig,
    WorkerConfinement, WorkerEntry, WorkerTuning,
};
/// A helper release a worker retains beside its own functions (FIG-5799).
pub use lash_vm_runtime::{HelperReleaseIndex, KernelProcessPluginFactory};
/// The names of the dialects lash ships. A host passes one where it names
/// the dialect its process documents are written in
/// ([`KernelProcessPluginFactory::new`]); a session or process records it
/// at creation.
pub use lash_vm_runtime::{PYTHON_DIALECT, TYPESCRIPT_DIALECT};
/// The TypeScript dialect, for a host to install in its worker embedding.
#[cfg(all(feature = "worker", feature = "dialect-typescript"))]
pub use lash_vm_worker::typescript as typescript_worker_dialect;
/// What a worker registers when it starts: the kernel library, extension
/// functions and the dialects it lowers and prints.
#[cfg(feature = "worker")]
pub use lash_vm_worker::{
    Dialect as WorkerDialect, Embed as WorkerEmbed, EmbedError as WorkerEmbedError,
    Embedder as WorkerEmbedder, Embedding as WorkerEmbedding,
    built_dialects as built_worker_dialects, worker_entry,
};
/// The Python dialect, for a host to install in its worker embedding.
#[cfg(all(feature = "worker", feature = "dialect-python"))]
pub use lash_vm_worker::{
    PythonSourceLimits, python as python_worker_dialect,
    python_with_limits as python_worker_dialect_with_limits,
};

/// The host boundary in the kernel type grammar, the providers that answer
/// projection reads with kernel data, and how a tool is named in guest code.
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    BoundaryError, HostBoundary, HostEffect, ProjectionCatalog, ProjectionProvider,
    ProjectionRefusal, ToolBindingError, required_tool_executable, type_of_schema,
};

// The vocabulary this module's signatures name (the facade-completeness rule).
/// What a kernel migration would refuse among a deployment's processes and
/// sessions (kernel spec §6 "Upgrades"): the survey `lashctl
/// kernel-migration list` prints, over the library a host's workers hold;
/// and the sweep `lashctl kernel-migration run` makes before the window
/// closes (FIG-5787).
#[cfg(feature = "codemode")]
pub use crate::kernel_migration::{
    KERNEL_MIGRATION_SWEEP, KernelMigrationSweep, survey_kernel_migration, sweep_kernel_migration,
};
pub use lash_sansio::worker_limit::WorkerFrameKind;
pub use lash_vm_client::{
    BootstrapFault, CheckpointRefusal, CodecRefusal, DecodeLimits, Detail, DetailLimit, DiskSpool,
    Exchange, ExecutionClass, ExecutionLease, ExecutionReceipt, FrameEpoch, HeaderRefusal,
    HostReadKind, InfrastructureOutcome, KernelRestoreRefusal, OwnerEpoch, PageRefusal,
    PayloadKind, PoolCounters, PoolError, PoolFault, PoolMeasurements, ProtocolBounds,
    ProtocolBreach, ProtocolVersionRefusal, RunBounds, RunInput, RunMeters, RunRefusal,
    SequenceFault, SpoolRoot, SupervisorEvidence, TransportSequence, VmOwner,
    WorkerDeploymentFault, WorkerLimit, WorkerWork,
};
/// Why lash's shipped library did not assemble: a defect of the build.
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::LibraryError;
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    DocumentRefusal as KernelDocumentRefusal, DocumentStoreError as KernelDocumentStoreError,
    KernelMigrationRefusal, KernelMigrationSurvey, KernelMigrationSurveyError,
    ParkedRefusal as KernelParkedRefusal, RefusedKernelCell, RefusedKernelProcess,
    UnmigratedKernelSession, standard_functions,
};
/// What still pins a helper release before this build's own, and why a
/// process's run would not carry onto the build's helpers (FIG-5799).
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{HelperAdoptionRefusal, HelperDependentProcess, HelperDependentSession};
