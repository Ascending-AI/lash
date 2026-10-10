//! Hosting the kernel machine: the worker pool, the worker's entry and what
//! it registers, and the host boundary a document runs against.
//!
//! A run executes in a worker process that hosts one kernel machine. The
//! worker registers every library function when it starts; a document names
//! functions by identity and carries none.

/// One shared pool for RLM cells, process bodies, and pure language work.
///
/// SDK releases attach `lash-sdk-worker-VERSION-TARGET.tar.gz` and its SHA256.
/// Pass the extracted `bin/lash-vm-worker` path to [`WorkerService::subprocess`]
/// or [`WorkerEntry::helper`]. Hosts may build the SDK from registry packages.
/// The manifest records protocol and crate diagnostics; crate versions never
/// decide compatibility. Pool admission refuses an unsupported wire version.
/// [`WorkerService::default`] explicitly defaults to the helper beside the host
/// executable and does not search PATH or a repository.
///
/// A single-binary host calls [`worker_entry`] (or [`worker_entry_with`], for
/// its own functions and dialects) as its first action, before runtime
/// creation, credentials, stores or providers, and returns from main when
/// that call returns `true`. It selects [`WorkerEntry::reexec`].
/// `examples/worker_host.rs` proves this bootstrap.
/// The child starts with an empty environment and closes inherited descriptors.
/// The language bounds guest authority; the process contains native crashes.
/// A native escape still has the worker user's OS access.
pub use lash_vm_client::service::Service as WorkerService;
/// The worker pool [`WorkerService::pool`] starts, which a host prewarms at
/// startup, and the counts it reports.
pub use lash_vm_client::{PoolStats as WorkerPoolStats, WorkerPool};

/// Host-selected worker entry, pool bounds, and execution deadlines.
pub use lash_vm_client::{
    Deadlines as WorkerDeadlines, PoolConfig as WorkerPoolConfig, WorkerEntry, WorkerTuning,
};
/// What a worker registers when it starts: the kernel library, extension
/// functions and the dialects it lowers and prints.
pub use lash_vm_worker::{
    Embed as WorkerEmbed, EmbedError as WorkerEmbedError, Embedder as WorkerEmbedder,
    Embedding as WorkerEmbedding, standard as standard_worker_embedding,
    typescript as typescript_worker_dialect, worker_entry, worker_entry_with,
};

/// The host boundary in the kernel type grammar, the providers that answer
/// projection reads with kernel data, and how a tool is named in guest code.
#[cfg(feature = "rlm")]
pub use lash_vm_runtime::{
    BoundaryError, HostBoundary, HostEffect, ProjectionCatalog, ProjectionProvider,
    ProjectionRefusal, ToolBindingError, required_tool_executable, type_of_schema,
};

// The vocabulary this module's signatures name (the facade-completeness rule).
pub use lash_sansio::worker_limit::WorkerFrameKind;
pub use lash_vm_client::{
    BootstrapFault, CodecRefusal, DecodeLimits, Detail, Exchange, ExecutionClass, ExecutionLease,
    ExecutionReceipt, FrameEpoch, HeaderRefusal, HostReadKind, InfrastructureOutcome,
    OpaqueStateRefusal, OpaqueVmState, OwnerEpoch, PayloadKind, PoolCounters, PoolError, PoolFault,
    PoolMeasurements, ProtocolBounds, ProtocolBreach, ProtocolVersionRefusal, RunBounds, RunInput,
    RunMeters, RunRefusal, SequenceFault, SupervisorEvidence, TransportSequence, VmOwner,
    WorkerDeploymentFault, WorkerLimit,
};
#[cfg(feature = "rlm")]
pub use lash_vm_client::{StateDigest, StateExpectation};
/// What a kernel migration would refuse among a deployment's processes
/// (kernel spec §6 "Upgrades"): the survey `lashctl kernel-migration list`
/// prints, over the library a host's workers hold.
#[cfg(feature = "rlm")]
pub use lash_vm_runtime::{
    DocumentRefusal as KernelDocumentRefusal, DocumentStoreError as KernelDocumentStoreError,
    KernelMigrationRefusal, KernelMigrationSurvey, KernelMigrationSurveyError,
    ParkedRefusal as KernelParkedRefusal, RefusedKernelProcess, standard_functions,
    survey_kernel_migration,
};
