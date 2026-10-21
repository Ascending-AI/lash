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

mod authority;
mod boundary;
mod formats;
mod host;
mod process;
mod process_plugin;
mod projection;

pub use authority::{
    AuthorityUnread, InternalAuthority, NoInternalOperations, StoredAuthority, admit_internal,
};
pub use boundary::BoundaryError;
pub use boundary::HostBoundary;
pub use boundary::effect_controls;
pub use boundary::offered_tool;
pub use boundary::owned_operation;
pub use boundary::tool_input;
pub use formats::{
    KERNEL_DOCUMENT_SCHEMA_VERSION, KERNEL_PARKED_STATE_VERSION, KERNEL_SAVED_FUNCTION_VERSION,
    LASH_KERNEL_VERSION, previous_kernel_version, retired_kernel_version,
};
pub use host::{EffectReads, ParentHost};
/// Why a kernel migration does not carry a document or a parked run.
pub use lash_kernel_migrate::{DocumentRefusal, ParkedRefusal};
pub use lash_vm_broker::kernel::loader::LoadRefusal;
/// lash's own packages, registered once in a process: what a kernel
/// document is linked, admitted and migrated against when the host
/// installs no package of its own. The parent decodes them from the
/// build's images, so it links no dialect (FIG-5812, FIG-6917).
pub use lash_vm_releases::{HelperRelease, LibraryError, helper_release};
/// The names of the dialects lash ships: what a session or process records
/// at creation, and what a host passes where it names its dialect
/// (FIG-5990).
pub use lash_vm_releases::{PYTHON as PYTHON_DIALECT, TYPESCRIPT as TYPESCRIPT_DIALECT};
pub use lash_vocabulary::schema::MAX_PROJECTED_TYPE_NODES;
pub use lash_vocabulary::schema::MAX_RESOLVED_REFERENCES;
pub use lash_vocabulary::schema::SchemaConflict;
pub use lash_vocabulary::schema::SchemaProjectionRefusal;
pub use lash_vocabulary::schema::type_of_schema;
pub use projection::{ProjectionCatalog, ProjectionProvider, ProjectionRefusal};

/// The machine a worker hosts, as the broker drives it, and the run's
/// wire-level host.
pub use lash_vm_client::{RemoteMachine, RemoteMachines, RunHost};

pub use lash_vocabulary::observation::ReplayPath;
pub use lash_vocabulary::observation::TaskPath;
pub use process::AdmittedWorkflow;
pub use process::AdoptedRun;
pub use process::DocumentStoreError;
pub use process::EFFECT_ARGUMENTS;
pub use process::EFFECT_UNKNOWN;
pub use process::ExecutionDocument;
pub use process::HelperAdoptionRefusal;
pub use process::KERNEL_RUN_STEP;
pub use process::KernelDocuments;
pub use process::KernelEngineSteps;
pub use process::KernelMigrationRefusal;
pub use process::KernelMigrationSurvey;
pub use process::KernelMigrationSurveyError;
pub use process::KernelProcessDefinition;
pub use process::KernelProcessEngine;
pub use process::KernelProcessFailureCode;
pub use process::KernelProcessInput;
pub use process::KernelRecordedSettings;
pub use process::KernelRunPolicy;
pub use process::KernelSettingsError;
pub use process::KernelStateMigration;
pub use process::LASH_VM_ENGINE_KIND;
pub use process::PlannedMigration;
pub use process::ProcessReplayRequest;
pub use process::RefusedKernelCell;
pub use process::RefusedKernelProcess;
pub use process::ReplayFault;
pub use process::ReplaySegments;
pub use process::SegmentAnswer;
pub use process::SegmentReplay;
pub use process::StoredSourceMap;
pub use process::StoredWorkflow;
pub use process::TOOL_FAILED;
pub use process::UnmigratedKernelSession;
pub use process::Unreplayable;
pub use process::Unretained;
pub use process::WorkflowAdmissionOutcome;
pub use process::WorkflowAdmissionRefusal;
pub use process::WorkflowAdmissionRequest;
pub use process::WorkflowDocument;
pub use process::WorkflowDocumentError;
pub use process::WorkflowEnvironment;
pub use process::WorkflowEnvironmentRequest;
pub use process::admit_kernel_process;
pub use process::carry_parked_checkpoint;
pub use process::carry_snapshot;
pub use process::definition_draft;
pub use process::definition_of_entry;
pub use process::entry_signature;
pub use process::kernel_process_engine_registration;
pub use process::migrate_function_value;
pub use process::migrate_saved_function;
pub use process::migration_refusal;
pub use process::plan_adoption;
pub use process::plan_migration;
pub use process::survey_kernel_processes;
pub use process_plugin::{
    DIALECT_SERVICE_EXECUTION, KERNEL_SERVICES_CONFIG, KERNEL_SERVICES_OWNER,
    KernelProcessPluginFactory, KernelServicesComponent, KernelServicesPluginFactory,
};

/// Statement observation requested by recorded kernel settings.
pub use lash_kernel_vm::Bounds as KernelBounds;
pub use lash_vocabulary::observation::RunObservation;

pub mod trace_adapter;
