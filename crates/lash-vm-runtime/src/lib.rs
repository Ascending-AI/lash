//! The lash side of the kernel machine's host boundary.
//!
//! Recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
//!
//! A kernel run performs ordinary package operations, bound as waited or
//! as Read. [`HostBoundary`] describes the accepted package surface, and
//! [`ParentHost`] answers a worker-hosted machine's read-bound performs.
//! Transcript data belongs to the RLM protocol's Read tools.

#![deny(clippy::future_not_send)]

mod authority;
mod boundary;
pub mod dialect_services;
mod formats;
mod host;
pub mod process;
mod process_plugin;
pub mod trace_adapter;

pub use authority::admit_publication;
pub use boundary::{
    HostBoundary, MAX_PROJECTED_TYPE_NODES, MAX_RESOLVED_REFERENCES, SchemaConflict,
    SchemaProjectionRefusal, effect_controls, type_of_schema,
};
pub use boundary::{tool_arguments, tool_input};
pub use formats::{
    KERNEL_DOCUMENT_SCHEMA_VERSION, KERNEL_PARKED_STATE_VERSION, KERNEL_SAVED_FUNCTION_VERSION,
    LASH_KERNEL_VERSION, previous_kernel_version, retired_kernel_version,
};
pub use host::{EffectReads, ParentHost};
/// Why a kernel migration does not carry a document or a parked run.
pub use lash_kernel_migrate::{AdoptionRefusal, DocumentRefusal, ParkedRefusal, Rewritten};
/// lash's own packages, registered once in a process: what a kernel
/// document is linked, admitted and migrated against when the host
/// installs no package of its own. The parent decodes them from the
/// build's images, so it links no dialect (FIG-5812, FIG-6917).
pub use lash_kernel_releases::LibraryError;
/// The names of the dialects lash ships: what a session or process records
/// at creation, and what a host passes where it names its dialect
/// (FIG-5990).
pub use lash_kernel_releases::{PYTHON as PYTHON_DIALECT, TYPESCRIPT as TYPESCRIPT_DIALECT};
pub use lash_vm_broker::kernel::loader::LoadRefusal;

/// The machine a worker hosts, as the broker drives it, and the run's
/// wire-level host.
pub use lash_vm_client::{RemoteMachine, RemoteMachines, RunHost};

pub use lash_kernel_check::{WorkflowDocument, WorkflowDocumentError};
pub use process::{
    EFFECT_ARGUMENTS, EFFECT_UNKNOWN, HelperAdoptionRefusal, KERNEL_RUN_STEP, KernelDocuments,
    KernelEngineSteps, KernelMigrationRefusal, KernelMigrationSurvey, KernelMigrationSurveyError,
    KernelProcessDefinition, KernelProcessEngine, KernelProcessFailureCode, KernelProcessInput,
    KernelRecordedSettings, KernelRunPolicy, KernelSettingsError, LASH_VM_ENGINE_KIND,
    PlannedMigration, ProcessReplayRequest, RefusedKernelCell, RefusedKernelProcess, ReplayFault,
    ReplayPath, ReplaySegments, SegmentAnswer, SegmentReplay, StoredSourceMap, TOOL_FAILED,
    TaskPath, UnmigratedKernelSession, Unreplayable, Unretained, WorkflowEnvironment,
    WorkflowEnvironmentRequest, WorkflowPublishRequest, admit_kernel_process,
    carry_parked_checkpoint, carry_snapshot, definition_draft, definition_of_start,
    entry_signature, kernel_process_engine_registration, map_link_refusal, migrate_function_value,
    migration_refusal, plan_adoption, plan_migration, survey_kernel_processes,
};

pub use process_plugin::{
    KERNEL_SERVICES_CONFIG, KERNEL_SERVICES_OWNER, KernelProcessPluginFactory,
    KernelServicesComponent, KernelServicesPluginFactory,
};

/// Statement observation requested by recorded kernel settings.
pub use lash_kernel_vm::{Bounds as KernelBounds, ObservationMode};

pub use dialect_services::{DIALECT_SERVICE_PACKAGES, TIMER_OPERATIONS};
