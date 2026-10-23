/// Coherent environment sources and mailbox changes. Version adoption and
/// retention remain owned by the engine; hosts only publish source facts.
pub mod environment {
    pub use lash_core::plugin::environment_sources::{
        EnvironmentChangeSink, EnvironmentSource, EnvironmentSources, ImmutableEnvironmentSource,
        PublishedEnvironmentSource, SourceCollectionError, provider_source_id,
    };
    pub use lash_core_store::environment::{
        EnvironmentChange, EnvironmentFragment, InputFrontier, SourceDescriptor, SourceId,
        SourceMode, SourceRevision, SourceSnapshot,
    };
    pub use lash_vocabulary::{claims::*, names::*, package::*};
}

/// Host-only live and retained plugin evidence.
pub mod diagnostics {
    pub use crate::plugin_diagnostics::*;
}
pub use lash_core::plugin::ToolProviderHandle;
pub use lash_core::plugin::{
    CommittedTurnReadPort, CurrentSessionSnapshotPort, NamedSessionSnapshotPort,
    ProcessAbandonHoldPort, ProcessAwaitPort, ProcessBoundCancelPort, ProcessBoundStartPort,
    ProcessCancelPort, ProcessListForAttemptPort, ProcessListPort, ProcessRecordedCancelPort,
    ProcessRecordedStartPort, ProcessReleaseHoldPort, ProcessStartFromRequestPort,
    ProcessStartPort, ProcessTransferPort, ProcessValidateVisiblePort, RecordedToolCatalogPort,
    ServiceBindings, SessionCreatePort, SessionGraphAppendPort, TurnScopePort,
};
pub use lash_core_store::tool_run::StateCommandLimits;
pub use lash_vocabulary::providers::StateAuthority;
// The vocabulary this module's signatures name (the facade-completeness rule).
pub use lash_core::ArtifactReferrerPorts;
pub use lash_core::CommandJournalGuard;
pub use lash_core::CommandReplayKey;
pub use lash_core::ConfigRegistry;
pub use lash_core::RecordedKeyFence;
pub use lash_core::ReferrerAcquisition;
pub use lash_core::RefusedWriteRange;
pub use lash_core::ServedOnlyRange;
pub use lash_core::WeakProcessEngineRegistry;
pub use lash_core::plugin::{
    AssistantProseProjectorPlugin, AssistantStreamFinishedHook, DecidedContextPressure,
    PluginFuture, PluginLifecycleEventHook, PluginLifecycleFuture, ResolvedToolSurface,
    ToolCatalogContributor, ToolPresentationArtifacts, ToolPresentationFacts,
    ToolPresentationInput, ToolPresentationStep, TranscriptDecoderPlugin,
};
/// The neutral composition and state sink shared by execution owners.
pub use lash_core::plugin::{
    CallbackSlot, ContributionDescriptor, ContributionDetails, ContributionKey, ContributionKind,
    ContributionReadiness, ExecutionComposition, ExecutionState, InstalledInventory,
    InventoryScope, MaterializedInventoryReader, PluginRecipe, PluginRecipeFactory, PreflightIssue,
    ProcessRecipe, Proposal, RequiredPort, Requirements, RetiredConfigObservers, ServiceKind,
    ToolProviderKey, ToolProviderKeyError, TraceScopeOf, WeakMaterializedInventoryReader,
};
pub use lash_core::runtime::ToolAttemptEffectOutcome;
pub use lash_core::runtime::{
    AttemptStream, AttemptStreamChannel, AttemptStreamEvent, AttemptStreamTruncation,
    DecodedStreamEvent,
};
pub use lash_core::session::{
    CompletedProtocolToolCall, Incorporated, IncorporationLedger, SettlementSource,
};
pub use lash_core::tool_dispatch::{
    LaunchReceipt, ToolCallIds, ToolDispatchOutcome, ToolPreparationOutcome,
};
pub use lash_core_store::session_identity::FrameNodeIdError;
pub use lash_core_store::transcript::CellCallLedger;
pub use lash_execution_contracts::process::DefinitionAcquisition;
pub use lash_execution_contracts::process::ProcessEnvLoadError;
pub use lash_execution_contracts::process::ResolvedProcessDefinition;
pub use lash_protocol_standard::BatchSugar;
pub use lash_protocol_types::AttachmentMaterializationNotice;
pub use lash_protocol_types::CompletionCandidate;
pub use lash_protocol_types::CompletionDisposition;
pub use lash_protocol_types::DecidedCompletion;
pub use lash_protocol_types::DegradedBinding;
pub use lash_protocol_types::ModelToolOutcome;
pub use lash_protocol_types::ModelToolReturn;
pub use lash_protocol_types::SessionStreamEvent;
pub use lash_protocol_types::StreamMessageKind;
pub use lash_protocol_types::ToolContractResolver;
pub use lash_sansio::CheckpointResumeAction;
pub use lash_sansio::DriverAction;
pub use lash_sansio::DriverContextView;
pub use lash_sansio::EffectId as DriverEffectId;
pub use lash_sansio::ExpandedRow;
pub use lash_sansio::ExpandedWrapper;
pub use lash_sansio::ModelToolCalls;
pub use lash_sansio::PendingWork;
pub use lash_sansio::ProjectorContext;
pub use lash_sansio::ResponseToolCalls;
pub use lash_sansio::ToolControlOrigin;
pub use lash_sansio::ToolExpansionPlan;
pub use lash_sansio::ToolWave;
pub use lash_sansio::TurnMachineConfig;
pub use lash_sansio::TurnProtocol;
pub use lash_sansio::UndecodableDriverState;
pub use lash_sansio::UnitTurnProtocol;
pub use lash_sansio::WriterFormats;
/// The protocol-generic forms [`TurnDriverConfig`] and
/// [`TurnDriverPreamble`] specialize to the host's turn protocol.
pub use lash_sansio::{
    TurnDriverConfig as GenericTurnDriverConfig, TurnDriverPreamble as GenericTurnDriverPreamble,
};
pub use lash_vocabulary::ToolCatalogBuildError;

/// A session's recorded plugin configuration and the owner contract that
/// creates and changes it (FIG-4379): each installed plugin registers the
/// owner of its namespace and the typed config commands that change it,
/// and reads the recorded value on every open and in every scoped hook.
pub use lash_core::AdmittedPluginConfig;
/// Protocol-driver and process-engine inputs that core owns independently of plugin storage.
pub use lash_core::AgentFrameAssignment;
pub use lash_core::AgentFrameReason;
pub use lash_core::AgentFrameRecord;
pub use lash_core::CandidateFacts;
pub use lash_core::ConfigCommand;
pub use lash_core::ConfigFault;
pub use lash_core::ConfigOwner;
pub use lash_core::ConfigRegistrar;
pub use lash_core::ConfigWire;
pub use lash_core::CreationConfigError;
pub use lash_core::FrameNodeId;
pub use lash_core::HostTurnProtocol;
pub use lash_core::NoRunOptions;
pub use lash_core::OwnerChange;
pub use lash_core::PluginConfig;
pub use lash_core::PluginMessage;
pub use lash_core::PluginRuntimeEvent;
pub use lash_core::ProcessEngineRegistration;
/// What an engine registry holds beside its engines: the process
/// components installed on the deployment, and the environment ports a
/// start's launch is held through (FIG-6034).
pub use lash_core::ProcessEnvPorts;
pub use lash_core::ProtocolBuildInput;
pub use lash_core::ProtocolDriverState;
pub use lash_core::ProtocolTurnOptionsError;
pub use lash_core::RecordedNamespaceCorrupt;
pub use lash_core::RenderFault;
pub use lash_core::ToolCatalog;
pub use lash_core::ToolCheckConflict;
pub use lash_core::ToolCheckPhase;
pub use lash_core::ToolCheckReply;
pub use lash_core::ToolCheckVerdictKind;
/// Host-specialized driver configuration required by every [`TurnDriverPreamble`].
pub use lash_core::TurnDriverConfig;
pub use lash_core::TurnDriverPreamble;
pub use lash_core::facade_support::PluginFactory;
pub use lash_core::facade_support::PluginHost;
pub use lash_core::facade_support::PluginRegistrar;
pub use lash_core::facade_support::PluginSession;
pub use lash_core::facade_support::PluginSessionContext;
pub use lash_core::facade_support::PluginSpec;
/// A turn's prepared request history, as its prelude records it.
pub use lash_core::facade_support::PreparedContext;
/// Durable session-lifecycle operations a hook context carries, alongside
/// [`SessionStateService`] and [`SessionGraphService`]; runtime-implemented.
pub use lash_core::facade_support::SessionLifecycleService;
pub use lash_core::facade_support::SessionPlugin;
pub use lash_core::facade_support::ToolCatalogContribution;
pub use lash_core::facade_support::TurnHookContext;
pub use lash_core::facade_support::TurnResultHookContext;
/// The schema crate config wire types derive with, so an owner's
/// namespace, commands and refusals generate the schemas the config
/// command catalog publishes: derive
/// `#[derive(lash::plugins::schemars::JsonSchema)]` with
/// `#[schemars(crate = "lash::plugins::schemars")]`.
pub use lash_core::facade_support::schemars;
/// Host-mediated JSON state: a plugin reads its namespace through a
/// read-only [`PluginStateView`] and changes it only by returning
/// [`StateCommands`] from a tool body or a before-turn, after-turn,
/// checkpoint or after-tool callback. Their recorded resolution is
/// published once durable and persisted at boundary commits.
pub use lash_core::plugin::FormatNamespace;
pub use lash_core::plugin::FormatRefusal;
/// Plugin identities, namespace formats and readable stream-state formats.
pub use lash_core::plugin::FormatVersion;
pub use lash_core::plugin::FrontierRefusal;
pub use lash_core::plugin::FrontierStep;
pub use lash_core::plugin::HookCause;
pub use lash_core::plugin::HookOccurrence;
pub use lash_core::plugin::KeyRejection;
pub use lash_core::plugin::NamespaceFrontierRefusal;
pub use lash_core::plugin::PluginCallbackIdentity;
/// Plugin operations: the query / command / task vocabulary. A plugin
/// author declares an operation by implementing [`PluginOperation`] plus
/// one of [`PluginQuery`], [`PluginCommand`] or [`PluginTask`], registers a
/// handler through
/// [`PluginRegistrar::operations`](lash_core::plugin::PluginRegistrar::operations)
/// or [`PluginSpec`], and receives the matching context
/// ([`PluginQueryContext`], [`PluginCommandContext`], [`PluginTaskContext`]).
/// Command and task handlers return a [`PluginOperationOutcome`], which is
/// how a plugin asks the runtime to do something on its behalf — today one
/// [`PluginRuntimeDirective`]. Hosts invoke operations through
/// [`PluginOperations`](crate::admin::PluginOperations) and read the
/// resulting [`PluginOperationReceipt`].
///
/// This is authoring surface in full: writing a plugin that carries
/// operations needs no `lash-core` dependency (ADR 0051).
pub use lash_core::plugin::PluginCommand;
pub use lash_core::plugin::PluginCommandContext;
pub use lash_core::plugin::PluginComposition;
pub use lash_core::plugin::PluginConfigNamespace;
pub use lash_core::plugin::PluginDeclaration;
pub use lash_core::plugin::PluginFailureClass;
pub use lash_core::plugin::PluginFailureOrigin;
pub use lash_core::plugin::PluginHookFailure;
pub use lash_core::plugin::PluginId;
pub use lash_core::plugin::PluginMetadata;
pub use lash_core::plugin::PluginOperation;
pub use lash_core::plugin::PluginOperationDef;
pub use lash_core::plugin::PluginOperationFailure;
pub use lash_core::plugin::PluginOperationInvokeError;
pub use lash_core::plugin::PluginOperationKind;
pub use lash_core::plugin::PluginOperationOutcome;
pub use lash_core::plugin::PluginOperationReceipt;
pub use lash_core::plugin::PluginQuery;
pub use lash_core::plugin::PluginQueryContext;
pub use lash_core::plugin::PluginRuntimeDirective;
pub use lash_core::plugin::PluginStateError;
pub use lash_core::plugin::PluginStateView;
pub use lash_core::plugin::PluginTask;
pub use lash_core::plugin::PluginTaskContext;
pub use lash_core::plugin::ProcessReadService;
pub use lash_core::plugin::PublicationOrdinal;
pub use lash_core::plugin::RecordedCallbackUnavailable;
pub use lash_core::plugin::ResolvedStateChange;
pub use lash_core::plugin::RetirementRefusal;
pub use lash_core::plugin::SessionParam;
pub use lash_core::plugin::SessionReadService;
pub use lash_core::plugin::SessionReadyContext;
pub use lash_core::plugin::StateCommand;
pub use lash_core::plugin::StateCommandOrigin;
pub use lash_core::plugin::StateCommandRefusal;
pub use lash_core::plugin::StateFork;
pub use lash_core::plugin::StateFrontier;
pub use lash_core::plugin::StateReducer;
pub use lash_core::plugin::StateReduction;
pub use lash_core::plugin::StateResolution;
pub use lash_core::plugin::StateResolutionOutcome;
pub use lash_core::plugin::StreamStateFormatRefusal;
pub use lash_core::plugin::prompt::PromptRenderPoolConfig;
/// Prompt sections (ADR 0133): a plugin registers keyed sections and
/// trusted wrappers through `reg.prompt()`. A renderer reads only a
/// [`PromptInput`]: the call's committed cut, its own frozen namespace
/// and its admitted config. The host's plan and the recorded snapshots
/// are in [`prompt`](crate::prompt).
pub use lash_core::plugin::prompt::{
    CommittedPluginNamespace, OfferedTools, ProjectedHistoryStats, PromptCall, PromptFamilySection,
    PromptInput, PromptModel, PromptRegistrations, PromptRenderError, PromptSection,
    PromptSectionFamilySpec, PromptSectionSource, PromptSectionSpec, PromptSectionWrap,
    PromptWrapSpec, PromptWrapTarget, SectionText,
};
/// A session's registered sections, families and wrappers, read back
/// and previewed against a plan, and the facts a protocol states for its
/// own sections. Only the runtime builds a call's cut and composes it, at
/// the call's admission; a test composes through
/// `lash::testing::prompt`.
pub use lash_core::plugin::prompt::{
    PromptCatalog, PromptCatalogSnapshot, PromptSectionFamilyInfo, PromptSectionInfo,
    ProtocolPromptFacts,
};
/// Which registry a [`PluginRegistrar`] declares for, as a type
/// (`Session` or `Process`), and the contribution table that decides
/// which of its methods each registry offers and judges descriptors
/// offered to another registry (FIG-6273).
pub use lash_core::plugin::registries;
pub use lash_core::plugin::registries::{Process, Session};
/// The tool hook phases (ADR 0128): argument transforms, before-checks
/// over the prepared call, result transforms, and after-checks over the
/// final result.
pub use lash_core::plugin::{
    AfterToolContributions, AfterToolDecision, AttemptOrdinal, BeforeToolDecision,
    CachedToolSuccess, CheckRank, PreparedCallReadView, RankedVerdict, ToolArgsCheckHook,
    ToolArgsCheckInput, ToolArgsTransformHook, ToolArgsTransformInput, ToolHookContext,
    ToolHookOccurrence, ToolHookPhase, ToolResultCandidate, ToolResultCheckHook,
    ToolResultCheckInput, ToolResultTransformHook, ToolResultTransformInput,
};
/// Hook contracts and reports used by plugin authors. Every hook
/// registers under a [`HookKey`] (see [`hook_key!`](crate::hook_key));
/// ADR 0128 is the composition table every seam follows.
pub use lash_core::plugin::{
    AfterTurnContributions, AfterTurnHook, AssistantResponseHook, AssistantResponseHookContext,
    AssistantResponseTransform, AssistantStreamFinishReason, AssistantStreamFinishedContext,
    AssistantStreamHook, AssistantStreamHookContext, AssistantStreamTransform, BeforeTurnHook,
    CellFailureDiagnostic, CellFailureFeedbackHook, CheckpointHook, CheckpointHookContext,
    CommittedTurnReadError, CompactionContext, ContextCompaction, ContextCompactor, ContextError,
    ContextPressureContext, ContextPressureDecision, ContextPressureHook, ControlPhaseExpired,
    HookKey, PluginExtensionContribution, PluginRecordContribution, PluginSessionMaterialization,
    PluginTraceEmitter, SessionContributions, StaticPluginFactory, ToolCatalogContext,
    ToolPresentationPresenter, ToolResultProjectionContext, TurnContributions, TurnFinalized,
    TurnHookReport,
};
/// Protocol and process-engine contracts, including their complete runtime-owned state closure.
pub use lash_core::plugin::{
    AfterTurnDecisions, CellCallReferences, CheckpointApplication, CheckpointComponentKey,
    CodeExecutionOutcome, CodeExecutorPlugin, ExecutionCandidate, ExecutionGraphCapture,
    ExecutionGraphPurpose, HydratedExecutionGraph, NamespaceBody, NamespaceEncoding,
    NamespaceEntry, NamespaceValues, PluginAbort, PluginNamespaceState,
    PluginSessionMaterializationRequest, PluginSessionRequest, PluginState, PluginStateEffect,
    PluginTransitionBase, PluginTransitionId, PluginTransitionRecord, PluginTransitionRequest,
    ProtocolBeforeLlmCallContext, ProtocolDriverPlugin, ProtocolLlmCallAction,
    ProtocolSessionContext, ProtocolSessionPlugin, ProtocolSessionRestoreView,
    RecordedCallbackPhase, RecordedTurnContribution, SessionAuthorityContext, SessionPageReader,
    SessionPages, StoredCell, TurnPreparation,
};
/// The attachment-omission history policy (ADR 0133): a plugin names the
/// attachments of a turn's projected history its request omits, and core
/// substitutes the plugin-authored placeholder. Stored history stays unchanged.
pub use lash_core::plugin::{
    AttachmentOmissionContext, AttachmentOmissionPolicy, AttachmentOmissions, HistoryPartId,
};
/// The registration groups [`PluginRegistrar`]'s accessors return
/// (`reg.tools()`, `reg.session()`, ...), nameable so a helper can take
/// one as a parameter.
pub use lash_core::plugin::{
    ContextRegistrations, ExecutionRegistrations, OutputRegistrations,
    PluginOperationRegistrations, ProtocolRegistrations, SessionRegistrations,
    ToolCallRegistrations, ToolCatalogRegistrations, ToolRegistrations, ToolResultRegistrations,
    TurnRegistrations,
};
/// What an execution runs under: its model, generation, charge safety,
/// attachment rules, tool-call limit and render, and what one tool call
/// records of them at its admission (FIG-6034).
pub use lash_core::plugin::{ExecutionFacts, RecordedCallFacts};
/// What a recipe's process-engine binders receive. A host factory that
/// wraps another preserves its recipe's engine entries (FIG-4373).
pub use lash_core::plugin::{PluginExecutionTrace, ProcessEngineContributionContext};
pub use lash_core::plugin::{ProcessComponentRegistry, ProcessComposition, StepLink};
/// Engine registry and narrowed execution contexts used to host custom process engines.
pub use lash_core::runtime::ProcessEngineRegistry;
/// Engine-extension contracts for a tool call's admission, attempts,
/// decision and presentation. A call runs in memory inside the admitted
/// execution that makes it durable; hosts submit work through session
/// handles.
pub use lash_core::tool_dispatch::{
    AdmittedToolCall, AttemptEnd, BeforeCheckReply, CallEnd, DeclaredStartObligation,
    DeclaredStartObligationRefusal, IntentRealizationContext, IsolatedBinding,
    IsolatedProcessDescriptor, IsolatedStartRefusal, IsolatedToolStart, Realization,
    RealizationReceipt, RunCutRefusal, SingletonAttempt, SingletonBodyOutcome, SingletonCapture,
    SingletonPreparedRequest, SingletonPresentationError, SingletonRunError, SingletonToolCall,
    SingletonToolHandlers, StartLaunch,
};
/// One recorded Run attempt's outcome, distinct from the provider's
/// [`crate::provider::AttemptOutcome`].
pub use lash_core::tool_run::run_event::AttemptOutcome as RunAttemptOutcome;
pub use lash_core::tool_run::run_event::{
    AvailableEvidence, CallDecision, CompletionSource, KnownFailure, KnownFailureReason,
    ResultSource,
};
pub use lash_core::tool_run::{
    AdmissionRefusal as ToolRunAdmissionRefusal, AdmittedBinding, AdmittedCall, AdmittedRound,
    AfterCheckVerdict, AttributedVerdict, BeforeCheckVerdict, BeforeSelection, CapacityScope,
    RoundAdmission, RuntimeCallPolicy, SegmentOrdinal,
};
/// Recorded Run data needed by engine extensions and effect-host journals.
pub use lash_core::tool_run::{
    CheckRecord, InvalidMaterialDigest, MaterialBundle, MaterialDigest, MaterialEntry,
    MaterialHolder, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef, MaterialRefusal,
    MaterialRetentionError, MaterialRole, OperationRun, RetainedBundle, RunInputKind,
};
/// The session services a hook context hands a plugin: read-through state
/// access ([`SessionStateService`]) and durable graph appends
/// ([`SessionGraphService`]), plus the append request/result vocabulary.
/// Both are runtime-implemented — a plugin receives one, never writes one.
pub use lash_core::{
    AppendSessionNodesOutcome, AppendSessionNodesRequest, PluginExtensions, SessionAppendNode,
    SessionGraphAppendResult, SessionGraphService, SessionStateService,
};
/// Code-executor request, response, and runtime capability context.
pub use lash_core::{
    CellDefect, CellFailure, CellFailureKind, CellFailureRepair, ExecRequest, ExecResponse,
    RuntimeExecutionContext,
};
pub use lash_core::{CompletedToolCall, PluginOptions};
/// Executable identity and terminal rendering returned by protocol integrators.
pub use lash_core::{ExecutableGeneration, RecordedRender};
/// Lifecycle observation: what a `reg.lifecycle().on_event(..)` hook receives
/// when a turn is finalized, a session is restored, or its configuration
/// changes, and the contexts each event carries.
///
/// Hosts that need durable post-commit work keep a durable progress cursor
/// and page through [`DurableSession::history`](crate::DurableSession::history)
/// across frame and fork boundaries. Advance progress after successful,
/// idempotent output and schedule reconciliation independently of hooks.
pub use lash_core::{
    facade_support::ModelCallAddress, facade_support::PluginLifecycleEvent,
    facade_support::SessionConfigChangedContext, facade_support::SessionConfigField,
};
/// The committed wait identity and deadline of a pending operation.
pub use lash_core_store::tool_run::run_event::CompletionTimer;
pub use lash_core_store::tool_run::{Material, NamesMaterial, SettledOutput, SettledOutputRefusal};
pub use lash_execution_contracts::config_error::ConfigRegistrationError;
pub use lash_execution_contracts::declaration_error::PluginDeclarationError;
pub use lash_execution_contracts::plugin_error::PluginError;
pub use lash_execution_contracts::plugin_error::PluginErrorClass;
/// A host process engine's state machine (ADR 0132 §6): the state it
/// keeps, the events lash delivers to `advance`, and the action it
/// answers with.
pub use lash_execution_contracts::process::CapacityCause;
pub use lash_execution_contracts::process::CapacityVerdict;
pub use lash_execution_contracts::process::EngineAction;
pub use lash_execution_contracts::process::EngineEvent;
pub use lash_execution_contracts::process::EngineState;
pub use lash_execution_contracts::process::EngineStateFormat;
pub use lash_execution_contracts::process::EngineStatus;
pub use lash_execution_contracts::process::EngineStatusProjection;
pub use lash_execution_contracts::process::EngineStepAnswer;
pub use lash_execution_contracts::process::EngineStepKind;
pub use lash_execution_contracts::process::EngineStepRefusal;
pub use lash_execution_contracts::process::EngineSteps;
/// How a host process engine carries a process onto a successor pair its
/// host names (FIG-6121, D2).
pub use lash_execution_contracts::process::EngineSuccession;
pub use lash_execution_contracts::process::EngineWake;
pub use lash_execution_contracts::process::InlineDefinition;
pub use lash_execution_contracts::process::InlineStarts;
pub use lash_execution_contracts::process::InspectedProcessDefinition;
pub use lash_execution_contracts::process::KeyName;
pub use lash_execution_contracts::process::ProcessDocument;
pub use lash_execution_contracts::process::ProcessDocumentProvider;
pub use lash_execution_contracts::process::ProcessDocumentRead;
pub use lash_execution_contracts::process::ProcessDocumentRefRead;
pub use lash_execution_contracts::process::ProcessEngine;
pub use lash_execution_contracts::process::ProcessEngineAdmission;
pub use lash_execution_contracts::process::ProcessExecutionDocumentRead;
pub use lash_execution_contracts::process::ProcessInfraError;
pub use lash_execution_contracts::process::ProcessRunOutcome;
pub use lash_execution_contracts::process::ProcessTraceClass;
pub use lash_execution_contracts::process::SettledRan;
pub use lash_execution_contracts::process::StepCandidate;
pub use lash_execution_contracts::process::StepManifest;
pub use lash_execution_contracts::process::StepName;
pub use lash_execution_contracts::process::StepPages;
pub use lash_execution_contracts::process::StepPublication;
pub use lash_execution_contracts::process::StepRequest;
pub use lash_execution_contracts::process::SuccessionError;
pub use lash_execution_contracts::process::SuccessorCarry;
pub use lash_execution_contracts::process::{EngineEffectReads, EngineReadError, EngineStepRun};
pub use lash_execution_contracts::session_types::PluginOwned;
pub use lash_execution_contracts::state_commands::StateCommands;
pub use lash_plugin_standard_compaction::{
    OMITTED_ATTACHMENT_PLACEHOLDER, StandardCompactionConfig, StandardCompactionPluginFactory,
};
pub use lash_protocol_standard::{StandardProtocolConfig, StandardProtocolPluginFactory};
/// Model-facing tool declaration carried by [`TurnDriverPreamble::tool_specs`].
pub use lash_protocol_types::llm::types::LlmToolSpec;
pub use lash_protocol_types::session_model::message::HistoryPresentation;
/// Default chat projector installed by [`TurnDriverConfig::chat`].
pub use lash_sansio::ChatContextProjector;
pub use lash_sansio::CompletedToolCall as GenericCompletedToolCall;
/// Projection contract stored by [`TurnDriverConfig`] when a protocol supplies a custom
/// context projector.
pub use lash_sansio::ContextProjector;
/// Sans-I/O protocol handle accepted by [`TurnDriverConfig::chat`]; custom host drivers use
/// [`HostTurnProtocol`] as its protocol parameter.
pub use lash_sansio::ProtocolDriverHandle;
/// A claim token: what an actor's outstanding work requires and a
/// node's installed registrations serve (S7).
pub use lash_vocabulary::claims::ObligationToken;
pub use lash_vocabulary::claims::RegistryKind;

// Types named by the plugin extension contracts.
pub use lash_core::facade_support::{ResolutionError, ResolutionValidator};
pub use lash_core::{EndedCell, LinkedExecution};
pub use lash_execution_contracts::process::{
    InlineCode, InlineRequest, SuccessionContext, UseCorrespondence,
};
