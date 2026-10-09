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
    LASH_KERNEL_VERSION,
};
pub use host::ParentHost;
pub use projection::{ProjectionCatalog, ProjectionProvider, ProjectionRefusal};

/// The machine a worker hosts, as the broker drives it, and the run's
/// wire-level host.
pub use lash_vm_client::{RemoteMachine, RemoteMachines, RunHost};

pub use process::{
    AdmittedWorkflow, DocumentStoreError, EFFECT_ARGUMENTS, EFFECT_UNKNOWN, KERNEL_RUN_STEP,
    KernelDocuments, KernelEngineSteps, KernelProcessDefinition, KernelProcessEngine,
    KernelProcessFailureCode, KernelProcessInput, KernelRecordedSettings, KernelRunPolicy,
    LASH_VM_ENGINE_KIND, TOOL_FAILED, WorkflowAdmissionOutcome, WorkflowAdmissionRefusal,
    WorkflowAdmissionRequest, WorkflowDocument, WorkflowDocumentError, WorkflowEnvironment,
    WorkflowEnvironmentRequest, admit_kernel_process, definition_draft, definition_of_entry,
    entry_signature, kernel_process_engine_registration, with_definitions,
};
