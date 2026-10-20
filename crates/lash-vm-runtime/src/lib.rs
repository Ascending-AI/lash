//! The lash side of the kernel machine's host boundary.
//!
//! Recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
//!
//! A kernel run reaches the world three ways: it performs effects, it reads
//! through projection handles, and it reads its clock and random source. This
//! crate states each in the kernel's own terms. [`HostBoundary`] is the
//! effects and handle kinds a host offers a document, with every signature
//! in the kernel type grammar; [`ProjectionCatalog`] is the providers that
//! answer reads with kernel data; [`ParentHost`] answers a worker-hosted
//! machine's host reads on the parent. Kernel crates know none of it.

#![deny(clippy::future_not_send)]

mod binding;
mod boundary;
mod formats;
mod host;
mod process;
mod process_plugin;
mod projection;

pub use binding::{
    ResolvedToolBinding, ToolBinding, ToolBindingError, ToolBindingResolutionExt,
    ToolDefinitionBindingExt, ToolManifestBindingExt, required_tool_binding,
    required_tool_executable,
};
pub use boundary::{
    BoundaryError, HostBoundary, HostEffect, MAX_PROJECTED_TYPE_NODES, MAX_RESOLVED_REFERENCES,
    SchemaConflict, SchemaProjectionRefusal, tool_input, type_of_schema,
};
pub use formats::{
    KERNEL_DOCUMENT_SCHEMA_VERSION, KERNEL_HELPER_RELEASE, KERNEL_PARKED_STATE_VERSION,
    KERNEL_SAVED_FUNCTION_VERSION, LASH_KERNEL_VERSION, previous_kernel_version,
    retired_kernel_version,
};
pub use host::{EffectReads, ParentHost};
/// Why a kernel migration does not carry a document or a parked run.
pub use lash_kernel_migrate::{DocumentRefusal, ParkedRefusal};
/// The helper releases lash's shipped worker retains beside its own
/// functions (FIG-5799).
pub use lash_vm_library::{HelperReleaseIndex, standard_helper_releases};
/// lash's own packages, registered once in a process: what a kernel
/// document is linked, admitted and migrated against when the host
/// installs no package of its own. The parent decodes them from the
/// build's images, so it links no dialect (FIG-5812, FIG-6917).
pub use lash_vm_library::{LibraryError, lash_packages};
/// A helper release as checkpoint graph nodes: the exact manifest a run or a
/// saved function pins (FIG-5887 §3).
pub use lash_vm_releases::{HeldManifests, ManifestFunction, ReleaseManifest, ReleaseRefusal};
/// The names of the dialects lash ships: what a session or process records
/// at creation, and what a host passes where it names its dialect
/// (FIG-5990).
pub use lash_vm_releases::{PYTHON as PYTHON_DIALECT, TYPESCRIPT as TYPESCRIPT_DIALECT};
pub use projection::{ProjectionCatalog, ProjectionProvider, ProjectionRefusal};

/// The machine a worker hosts, as the broker drives it, and the run's
/// wire-level host.
pub use lash_vm_client::{RemoteMachine, RemoteMachines, RunHost};

pub use process::RetiredHelpers;
pub use process::{
    AdmittedWorkflow, AdoptedRun, DocumentStoreError, EFFECT_ARGUMENTS, EFFECT_UNKNOWN,
    ExecutionDocument, HelperAdoptionRefusal, HelperDependentProcess, HelperDependentSession,
    KERNEL_RUN_STEP, KernelDocuments, KernelEngineSteps, KernelMigrationRefusal,
    KernelMigrationSurvey, KernelMigrationSurveyError, KernelProcessDefinition,
    KernelProcessEngine, KernelProcessFailureCode, KernelProcessInput, KernelRecordedSettings,
    KernelRunPolicy, KernelSettingsError, KernelStateMigration, LASH_VM_ENGINE_KIND,
    PlannedMigration, ProcessReplayRequest, RefusedKernelCell, RefusedKernelProcess, ReplayFault,
    ReplayPath, ReplaySegments, SegmentAnswer, SegmentReplay, StoredSourceMap, StoredWorkflow,
    TOOL_FAILED, TaskPath, UnmigratedKernelSession, Unreplayable, Unretained,
    WorkflowAdmissionOutcome, WorkflowAdmissionRefusal, WorkflowAdmissionRequest, WorkflowDocument,
    WorkflowDocumentError, WorkflowEnvironment, WorkflowEnvironmentRequest, admit_kernel_process,
    definition_draft, definition_of_entry, entry_signature, kernel_process_engine_registration,
    migrate_function_value, migrate_saved_function, migration_refusal, plan_helper_adoption,
    plan_migration, retired_functions_reached, survey_helper_processes, survey_kernel_processes,
};
pub use process_plugin::KernelProcessPluginFactory;

/// Statement observation requested by recorded kernel settings.
pub use lash_kernel_vm::{Bounds as KernelBounds, ObservationMode};
