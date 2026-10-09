//! Lash VM embedding, artifact, compilation and execution contracts.
//!
//! Structured authoring IR is available in [`ir`]. VM workers and runtime
//! integration are independent of the optional TypeScript frontend.

pub use lash_vm::effect_value;
pub use lash_vm::is_resolved_type_assignable;
pub use lash_vm::referenced_definition_ids;
pub use lash_vm::{
    AbilityOp, AbilityOutcome, AggregateConsumer, Await, BINDING_SUMMARY_MAX_CHARS,
    BindingSummaryConfig, CANCEL_CHECKPOINT_INSTRUCTIONS, CANCEL_CHECKPOINT_INTERVAL_CAP,
    CompiledLinkedProgram, CompiledProcessCache, CompiledProcessCacheKey, CompiledProgram,
    CompiledProgramCacheStats, ContinuationError, DurableBaseline, DurableFragment, DurableParts,
    EcmaErrorClass, Entry, ErrorTaxonomy, ExecutableIdentity, ExecutionBound,
    ExecutionBounds as VmExecutionBounds, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionMode, ExecutionOutcome, ExecutionScratch, FormatError, GlobalPatch,
    GlobalPatchOutcome, HeapId, INSTRUCTION_ACCOUNTING_VERSION, ImageValue,
    LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY, LASH_HOST_REQUIREMENTS_REF_KEY,
    LASH_MODULE_REF_KEY, LASH_PROCESS_NAME_KEY, LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY,
    LASH_TYPE_KEY, LinkedProgramCache, LinkedProgramCacheError, ListValue, PendingOperation,
    PendingOperationMap, ProcessStart, ProfileReport, ProfileStat, ProjectedBindingError,
    ProjectedBindings, ProjectedReadRequest, ProjectedReadResponse, ProjectedValue,
    ProjectionCatalog, ProjectionError, ProjectionProvider, ProjectionReadError, ProjectionReader,
    ProjectionRefusal, ProjectionType, Record, ResourceHandle, ResourceOperation,
    ResourceOperationBatch, ResourceOperationBatchLeaf, ResourceOperationBatchOutcome,
    ResourceOperationOutcome, ResourceRef, RuntimeError as VmRuntimeError, RuntimeFailure, Sleep,
    SleepKind, Snapshot, SnapshotDecodeError, State, StringValue, UnawaitedToolCall, Value, Vm,
    VmComplete, VmContinuation, VmExecutionStart, VmFinallyCompletionContinuation,
    VmFinallyContinuation, VmGuestError, VmHandlerContinuation, VmHeapContinuation, VmInstance,
    VmInterrupt, VmIteratorContinuation, VmIteratorCursor, VmLoopContinuation, VmLoopPhase,
    VmPacing, VmParkReason, VmParked, VmPendingErrorOriginContinuation, VmProfileContinuation,
    VmRequest, VmResume, VmResumePoint, VmRunConfig, VmRunOutcome, VmSiteOccurrenceCounter, VmStep,
    VmStepError, VmSuspended, VmSuspendedOperation, cancel_checkpoint_reached, compile, execute,
    from_json, is_javascript_builtin_global, is_process_handle, unwrap_type_value,
    with_projection_reader,
};
pub use lash_vm::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, CanonicalPathSegment,
    REGEXP_EXECUTION_FUEL, REGEXP_FUEL_PER_INSTRUCTION, REGEXP_MAX_NESTING,
    REGEXP_MAX_PATTERN_CODE_UNITS, RegExpValidationError, validate_canonical_messagepack_structure,
    validate_regexp, validate_regexp_shape,
};
pub use lash_vm::{
    ContentHash, HostRequirements, HostRequirementsRef, LASH_VM_COMPILER_VERSION, ModuleArtifact,
    ModuleArtifactBytes, ModuleArtifactError, ModuleExports, ModuleRef, ProcessRef,
    host_requirements_for_program,
};
pub use lash_vm::{DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, HEAP_GC_ALLOCATION_INTERVAL};
pub use lash_vm::{DEFAULT_HOST_MEMORY_LIMIT_BYTES, DEFAULT_MAX_VM_FRAME_DEPTH};
pub use lash_vm::{HostDescriptor, HostDescriptorError};
pub use lash_vm::{
    INSTANCE_STDLIB_SIGNATURES, LiteralReceivers, STATIC_STDLIB_SIGNATURES, StdlibSignature,
};
pub use lash_vm::{
    JsonSchemaError, X_LASH_KEYWORD, XLashParam, XLashSignature, XLashType,
    json_schema_to_type_expr, type_expr_to_json_schema, type_expr_to_schema_shape,
};
pub use lash_vm::{
    LANGUAGE_RUNTIME_MODULE_PATH, LANGUAGE_RUNTIME_NOW_OPERATION,
    LANGUAGE_RUNTIME_RANDOM_OPERATION, LANGUAGE_RUNTIME_RESOURCE_TYPE, builtin_names,
    format_link_diagnostic, format_runtime_diagnostic, format_source_diagnostic,
};
pub use lash_vm::{
    LashVmBranchSite, LashVmEffectFailure, LashVmExecutionCallSite, LashVmExecutionChild,
    LashVmExecutionFailure, LashVmExecutionObservation, LashVmExecutionSite,
    ProcessBranchSelection, process_ref_key,
};
pub use lash_vm::{
    LashVmHostCatalog, LashVmHostCatalogError, LashVmHostEnvironment, LashVmLanguageFeatures,
    LinkError, LinkedModule, ModuleInstanceCatalog, ModuleOperationBinding, NamedDataType,
    NamedDataTypeError, OperationContract, OutputFromInputBinding, ResolvedOperation,
    ResourceOperationBinding, ResourceTypeCatalog, ValueConstructorBinding,
};
pub use lash_vm::{
    ModuleCompileDiagnostic, ModuleCompileError, ModuleCompileOutput as VmModuleCompileOutput,
    ModuleCompileRequest, compile_module,
};
pub use lash_vm::{
    ModuleInstanceIntrospection, ModuleIntrospection, ModuleIntrospectionError,
    ModuleOperationIntrospection, NamedDataTypeIntrospection, ProcessInputIntrospection,
    ProcessIntrospection, ResourceOperationIntrospection, ResourceTypeIntrospection, TypeView,
    ValueConstructorIntrospection, referenced_module_call_paths, referenced_receiver_call_paths,
};
pub use lash_vm::{
    OpaqueStateRefusal, VmContract, VmContractComponent, VmContractReads, VmOwner, VmStateKind,
};
pub use lash_vm::{ProcessDefinitionIdentity, ProcessDefinitionIdentityError};
pub use lash_vm::{
    RESOURCE_OPERATION_EXECUTION_SITE_KIND, execution_site_descriptor, is_pure_expr,
};
pub use lash_vm::{VM_CONTINUATION_READ_RANGE, vm_contract_reads, vm_contract_versions};
pub use lash_vm::{
    WorkflowExecutionSite, WorkflowLoopFrame, WorkflowLoopPosition, WorkflowOccurrenceContext,
    WorkflowSitePath, WorkflowSiteRef, WorkflowSiteRole, WorkflowSiteSegment,
};
pub use lash_vm::{WorkflowLinkAnalysis, analyze_workflow_program};

#[cfg(feature = "testing")]
pub use lash_vm::testing;

/// Structured compiler IR and its workflow projection vocabulary.
pub mod ir {
    pub use lash_vm::Span;
    pub use lash_vm::{
        AssignPathStep, AssignTarget, AstPath, AstRoot, AstString, BindingVisibility, CatchClause,
        CoercingBinaryOp, CoercingUnaryOp, Declaration, Expr, ExprFolder, ExprSlot,
        ExprSlotVisitor, ExprVisitor, FunctionDecl, FunctionExpr, FunctionParam, InvalidAst,
        LIFTED_PROCESS_NAME_PREFIX, LabelMetadata, MAX_AST_NESTING_DEPTH, MethodKey,
        NestingTooDeep, OperandLogicalOp, ProcessDecl, ProcessLiteralExpr, ProcessOrigin,
        ProcessParam, ProcessSignature as VmProcessSignature, ProcessSignatureError, ProcessType,
        Program, ResourceRefExpr, StructuralRole, TryExpr, TypeExpr, TypeField, UnionMembers,
        check_ast_nesting_depth, fold_expr_children, format_type_expr, lifted_process_identity,
        process_wrapper_run_path, validate_ast, walk_expr, walk_expr_slots,
    };
    pub use lash_vm::{
        AttributeAssignParts, AttributeStep, AttributeUpdate, CollectionTransformParts,
        ExprChildren, ExprChildrenMut, ProcessWrapperParts, UpdateOperator,
    };
    pub use lash_vm::{
        ListedStatement, WorkflowBody, WorkflowBodySlot, WorkflowGraphProjector, WorkflowStatement,
        else_if_chain, statement_list, workflow_graph_from_artifact, workflow_graph_from_program,
    };
    pub use lash_vm::{
        VariableVersion, WorkflowArgument, WorkflowContainer, WorkflowDeclaration,
        WorkflowDiagnosticClassification, WorkflowDiagnosticKind, WorkflowEdge, WorkflowEdgeKind,
        WorkflowEffectKind, WorkflowExpectedArgument, WorkflowGraphDecodeError,
        WorkflowGraphVersionRefusal, WorkflowNode, WorkflowNodeId, WorkflowNodeKind,
        WorkflowNodeNameSource, WorkflowNodePath, WorkflowNodeTypeFacets, WorkflowOwnership,
        WorkflowProcess, WorkflowProjection, WorkflowResultStep, WorkflowSlotPath,
        WorkflowSlotPathSegment, WorkflowSubgraph, WorkflowTerminalKind, WorkflowTypeDiagnostic,
        WorkflowTypedVariable, child_path, execution_sites,
    };
    pub use lash_vm::{
        projected_node_type_facets, workflow_call_from_ir, workflow_call_to_ir,
        workflow_effect_from_ir, workflow_effect_to_ir, workflow_node_id,
        workflow_slot_accepts_value, workflow_slot_value,
    };
}

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
/// A single-binary host calls [`worker_entry_with_frontend`] as its first action,
/// before runtime creation, credentials, stores or providers, and returns from
/// main when that call returns `true`. It selects [`WorkerEntry::reexec`].
/// `examples/worker_host.rs` proves this bootstrap with the TypeScript frontend.
/// The child starts with an empty environment and closes inherited descriptors.
/// The language bounds guest authority; the process contains native crashes.
/// A native escape still has the worker user's OS access.
pub use lash_vm_client::service::Service as WorkerService;
/// The worker pool [`WorkerService::pool`] starts, which a host prewarms at
/// startup, and the counts it reports.
pub use lash_vm_client::{PoolStats as WorkerPoolStats, WorkerPool};
#[cfg(feature = "rlm")]
pub use lash_vm_runtime::{
    LASH_VM_SURFACE_EXTENSION_ID, LanguageTraceHost, LashVmEngineSteps,
    LashVmProcessAdmissionRefusal, LashVmProcessEngine, LashVmProcessFailureCode,
    LashVmRecordedSettings, LashVmRunSettingsRecorder, LashVmRuntimeError, LashVmSurface,
    LashVmSurfaceContribution, ToolBindingError, VmSegmentPolicy, resolve_lash_vm_module_operation,
};

/// Host-selected worker entry, pool bounds, and execution deadlines.
pub use lash_vm_client::{
    Deadlines as WorkerDeadlines, PoolConfig as WorkerPoolConfig, WorkerEntry, WorkerTuning,
};
/// A source frontend lives in the worker entry the dialect selects.
pub use lash_vm_worker::{
    Frontend as WorkerFrontend, FrontendRefusal as WorkerFrontendRefusal,
    worker_entry_with_frontend,
};

// The vocabulary this module's signatures name (the facade-completeness rule).
pub use lash_sansio::worker_limit::WorkerFrameKind;
pub use lash_vm_client::service::CompiledModule;
pub use lash_vm_client::{
    BootstrapFault, CodecRefusal, DecodeLimits, Detail, Exchange, ExecutionClass, ExecutionLease,
    ExecutionReceipt, FrameEpoch, HeaderRefusal, InfrastructureOutcome, InspectedArtifact,
    OwnerEpoch, PayloadKind, PoolCounters, PoolError, PoolFault, PoolMeasurements, ProcessMetadata,
    ProtocolBounds, ProtocolBreach, ProtocolVersionRefusal, RunInput, RunRefusal, SequenceFault,
    SupervisorEvidence, TransportSequence, VmLimits, WorkerDeploymentFault, WorkerLimit,
};

pub use lash_vm_runtime::lash_vm_surface_extension;
