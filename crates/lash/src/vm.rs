//! Install [`KernelProcessPluginFactory`] beside the session protocol to edit
//! and execute workflows. The RLM protocol contributes this engine itself.
//!
//! Hosting the kernel machine: the worker pool, the worker's entry and what
//! it registers, and the host boundary a document runs against.
//!
//! A run executes in a worker process that hosts one kernel machine. The
//! worker opens its parent's native library at startup. Each run carries
//! its reached Documents and immutable Link tables.
//!
//! RLM builders take the kernel policies `lash::rlm::{Charge, MemoryMeter}`.
//! A worker's [`RunBounds`] carries their wire mirrors, [`RunCharge`] and
//! [`RunMemoryMeter`].
//!
//! Where a run reaches its memory or charge bound, its host decides: an
//! embedded kernel host through `lash::rlm::{MemoryLimiter, ChargeLimiter}`,
//! and a worker's parent through their mirrors [`RunMemoryLimiter`] and
//! [`RunChargeLimiter`], which the worker asks with one
//! [`HostReadKind::Bound`] read naming a [`BoundReached`] and answered by a
//! [`RunLimitDecision`]. Without one, the run refuses there.

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
/// that dialect's packages for the parent. A single-binary host also
/// enables the `worker` feature and calls `worker_entry` with the worker
/// configuration it builds, naming each dialect and supplying the natives
/// of its own packages by function identity
/// (`WorkerEmbedder::new().native(id, native)?.dialect(typescript_worker_dialect(tuning)?)`),
/// as its first action, before runtime creation, credentials, stores or
/// providers, and returns from main when that call returns `true`. It
/// selects [`WorkerEntry::reexec`]. An embedding with no dialect is refused.
/// A host that runs the shipped worker executable links no worker
/// embedding; that executable installs the dialects it was built with.
/// A dialect feature never links a parser into the host's own process:
/// `typescript` gives the host `lash::typescript`, the front end and
/// printer, and `python` gives it `lash::python`.
/// A session or process records its dialect at creation, and a worker whose
/// embedding lacks it refuses a run that saves functions
/// ([`RunRefusal::DialectNotInstalled`]); running code needs no dialect.
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

pub use lash_protocol_types::engine_fault::WorkerLibraryRefusal;
pub use lash_vm_client::packages::WorkerLibrary;
/// Host-selected worker entry, pool bounds, a cell's lifetime caps,
/// execution deadlines, process confinement and the cgroup placement of
/// workers.
pub use lash_vm_client::{
    CellLifetime, CpuShare as WorkerCpuShare, Deadlines as WorkerDeadlines,
    PageBudgets as WorkerPageBudgets, PoolConfig as WorkerPoolConfig, WorkerCgroup,
    WorkerConfinement, WorkerEntry, WorkerTuning,
};
pub use lash_vm_releases::{HelperRelease, helper_release};
pub use lash_vm_runtime::{
    KERNEL_SERVICES_CONFIG, KERNEL_SERVICES_OWNER, KernelDocuments, KernelProcessPluginFactory,
    KernelServicesComponent, KernelServicesPluginFactory, WorkflowEnvironmentRequest,
    WorkflowPublishRequest, definition_draft,
};
/// The names of the dialects lash ships. A host passes one where it names
/// the dialect its process documents are written in
/// ([`KernelProcessPluginFactory::new`]); a session or process records it
/// at creation.
pub use lash_vm_runtime::{PYTHON_DIALECT, TYPESCRIPT_DIALECT};
/// The TypeScript dialect, for a host to install in its worker embedding.
#[cfg(all(feature = "worker", feature = "dialect-typescript"))]
pub use lash_vm_worker::typescript as typescript_worker_dialect;
/// What a worker is configured with: the natives of the host's packages, by
/// function identity, and the dialects it lowers and prints. Its registry
/// is derived from the packages its parent holds (`lash::package`).
#[cfg(feature = "worker")]
pub use lash_vm_worker::{
    Dialect as WorkerDialect, Embed as WorkerEmbed, EmbedError as WorkerEmbedError,
    Embedder as WorkerEmbedder, Embedding as WorkerEmbedding, PreparedLibrary,
    built_dialects as built_worker_dialects, built_embedding as built_worker_embedding,
    worker_entry,
};
/// The Python dialect, for a host to install in its worker embedding.
#[cfg(all(feature = "worker", feature = "dialect-python"))]
pub use lash_vm_worker::{
    PythonSourceLimits, python as python_worker_dialect,
    python_with_limits as python_worker_dialect_with_limits,
};

/// The host boundary in the kernel type grammar, the providers that answer
/// projection reads with kernel data, and what a tool is offered as.
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    HostBoundary, MAX_PROJECTED_TYPE_NODES, MAX_RESOLVED_REFERENCES, ProjectionCatalog,
    ProjectionProvider, ProjectionRefusal, SchemaConflict, SchemaProjectionRefusal,
    effect_controls, type_of_schema,
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
pub use lash_protocol_types::worker_limit::WorkerFrameKind;
pub use lash_vm_client::{
    BootstrapFault, BoundReached, CgroupController, CgroupRefusal, Charge as RunCharge,
    CheckpointRefusal, CodecRefusal, DecodeLimits, Detail, DetailLimit, DiskSpool, EffectPhase,
    EffectPhases, Exchange, ExecutionClass, ExecutionLease, ExecutionReceipt, FrameEpoch,
    FrameTraffic, HeaderRefusal, HeapFault, HostReadKind, InfrastructureOutcome, IpcTraffic,
    KernelRestoreRefusal, LimitDecision as RunLimitDecision, LimitedBound,
    MemoryMeter as RunMemoryMeter, OwnerEpoch, PageRefusal, PayloadKind, PoolCounters, PoolError,
    PoolFault, PoolMeasurements, ProtocolBounds, ProtocolBreach, ProtocolVersionRefusal, RunBound,
    RunBounds, RunChargeLimiter, RunInput, RunMemoryLimiter, RunMeters, RunRefusal, SequenceFault,
    SpoolRoot, SupervisorEvidence, TransportSequence, VmOwner, WorkerDeploymentFault, WorkerLimit,
    WorkerWork,
};
/// Why a process's run would not carry onto a successor pair's document
/// (FIG-6121).
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::HelperAdoptionRefusal;
#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{
    DocumentRefusal as KernelDocumentRefusal, KernelMigrationRefusal, KernelMigrationSurvey,
    KernelMigrationSurveyError, ParkedRefusal as KernelParkedRefusal, RefusedKernelCell,
    RefusedKernelProcess, UnmigratedKernelSession, migrate_function_value,
};

/// Kernel input for an inline start, including a retained captured callable.
pub use lash_vm_runtime::process::InlineRequest as KernelInlineRequest;

#[cfg(feature = "codemode")]
pub use lash_vm_runtime::{DIALECT_SERVICE_PACKAGES, TIMER_OPERATIONS};

/// Errors and retained source maps named by the runtime host interfaces.
pub use lash_vm_releases::LibraryError;
pub use lash_vm_runtime::{AdoptionRefusal, StoredSourceMap, Unretained};
