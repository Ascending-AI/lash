//! The lash side of the kernel machine's host boundary.
//!
//! A kernel run reaches the world three ways: it performs effects, it reads
//! through projection handles, and it reads its clock and random source. This
//! crate states each in the kernel's own terms. [`HostBoundary`] is the
//! effects and handle kinds a host offers a document, with every signature
//! in the kernel type grammar; [`ProjectionCatalog`] is the providers that
//! answer reads with kernel data; [`ParentHost`] answers a worker-hosted
//! machine's host reads on the parent. Kernel crates know none of it.

mod binding;
mod boundary;
mod formats;
mod host;
mod process;
mod projection;

pub use binding::{
    ResolvedToolBinding, TOOL_BINDING_KEY, ToolBinding, ToolBindingError, ToolBindingResolutionExt,
    ToolDefinitionBindingExt, ToolManifestBindingExt, required_tool_binding,
    required_tool_executable,
};
pub use boundary::{BoundaryError, HostBoundary, HostEffect, type_of_schema};
pub use formats::{
    KERNEL_DOCUMENT_SCHEMA_VERSION, KERNEL_PARKED_STATE_VERSION, KERNEL_SAVED_FUNCTION_VERSION,
    LASH_KERNEL_VERSION, previous_kernel_version,
};
pub use host::ParentHost;
/// Why a kernel migration does not carry a document or a parked run.
pub use lash_kernel_migrate::{DocumentRefusal, ParkedRefusal};
/// The library functions lash's shipped worker holds: what a kernel
/// document is linked, admitted and migrated against when the host
/// assembled no worker of its own.
pub use lash_vm_worker::standard_functions;
pub use projection::{ProjectionCatalog, ProjectionProvider, ProjectionRefusal};

/// The machine a worker hosts, as the broker drives it, and the run's
/// wire-level host.
pub use lash_vm_client::{RemoteMachine, RemoteMachines, RunHost};

pub use process::{
    AdmittedWorkflow, DocumentStoreError, EFFECT_ARGUMENTS, EFFECT_UNKNOWN, KERNEL_RUN_STEP,
    KernelDocuments, KernelEngineSteps, KernelMigrationRefusal, KernelMigrationSurvey,
    KernelMigrationSurveyError, KernelProcessDefinition, KernelProcessEngine,
    KernelProcessFailureCode, KernelProcessInput, KernelRecordedSettings, KernelRunPolicy,
    KernelStateMigration, LASH_VM_ENGINE_KIND, PlannedMigration, RefusedKernelProcess,
    SealedKernelRefusal, TOOL_FAILED, WorkflowAdmissionOutcome, WorkflowAdmissionRefusal,
    WorkflowAdmissionRequest, WorkflowDocument, WorkflowDocumentError, WorkflowEnvironment,
    WorkflowEnvironmentRequest, admit_kernel_process, check_sealed_kernel, definition_draft,
    definition_of_entry, entry_signature, kernel_process_engine_registration, migrate_run,
    migrate_saved_function, migration_refusal, plan_migration, survey_kernel_migration,
    with_definitions,
};
