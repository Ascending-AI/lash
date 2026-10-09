//! App-facing embedding facade for Lash.
//!
//! `lash` is intentionally a small layer above the lower-level
//! `lash-core` runtime crate. Host applications own providers, persistence,
//! app state, HTTP protocols, auth, and frontend streaming; this crate
//! owns only the ergonomic core/session/turn API.
//!
//! # Three verbs for one session
//!
//! A session id reaches Lash three ways, and the choice is the first thing to
//! make deliberately:
//!
//! * `core.session(id).create(creation).await` — the only verb that
//!   **creates**, and the only one that takes session config. It writes the
//!   session's catalog entry and its initial config head — the
//!   [`SessionCreation`]'s spec, parent and plugin options — in one store
//!   transaction and returns its [`DurableSession`], without building a
//!   runtime. An id that already exists is refused with
//!   [`EmbedError::SessionAlreadyExists`], always.
//! * `core.session(id).open().await` — the **live session**
//!   ([`LashSession`]). It builds a runtime: plugins, tool registry, protocol
//!   restore, lifecycle events, process admission. Submit input through
//!   [`LashSession::send`] and observe its handle; the engine executes the run. It
//!   never creates: the session runs with the config it recorded.
//! * `core.session(id).durable().await` — the **Durable Session**
//!   ([`DurableSession`]). It builds nothing and creates nothing: the
//!   session's queue and settled reads, answered from its store, correct while
//!   another process runs the session live. Use it to enqueue,
//!   list, cancel or reconcile.
//!
//! Every verb but `create` resolves an existing session and writes no catalog
//! row: an id the catalog has never created is refused with
//! [`EmbedError::UnknownSession`]. A host that means create-or-open writes it
//! out, so the arm where its config does not apply is visible:
//!
//! ```ignore
//! match core.session(id.clone()).create(creation).await {
//!     Ok(_) | Err(EmbedError::SessionAlreadyExists { .. }) => {}
//!     Err(error) => return Err(error),
//! }
//! let session = core.session(id).open().await?;
//! ```
//!
//! Polling a queue through `open()` costs a whole runtime per poll and, on a
//! core that does not carry the session's tool sources, orphans them. Reach
//! for `durable()` whenever no turn is being run. An open session exposes the
//! same operations through [`LashSession::durable`], so there is one behaviour
//! either way. See [`DurableSession`].
//!
//! Every public name has exactly one home. The crate root carries the daily
//! core/session/turn path; each domain module ([`tools`], [`persistence`],
//! [`plugins`], [`observe`], [`attachments`], ...) carries its own
//! vocabulary. [`prelude`] is the curated daily-use subset of the crate root.
//!
//! # Every type a facade signature names is nameable here
//!
//! A type that appears anywhere in a signature this crate exports -- a
//! function's parameters and return, a field, a variant, a trait's members and
//! supertraits, a generic bound, an alias -- has a `lash::` path. When a host
//! genuinely needs the type, it is exported in the module of the item that
//! names it. When no host needs it, the signature is narrowed instead
//! (`pub(crate)`, a changed signature, or a move into a hidden support module
//! such as `lash_core::core_internal`). `#[doc(hidden)]` is not a way out: it
//! is reserved for explicitly test- or support-only items, and a hidden item's
//! own signature is not part of the host API.
//!
//! `scripts/facade_completeness.py` enforces the rule over the rustdoc JSON of
//! this crate and of every first-party library in its dependency closure. It
//! has no allowlist and no exemption mechanism: a gap is fixed by exporting or
//! by narrowing, never by listing it.

/// Administrative facade handles and operations.
pub mod admin;
mod artifacts;
mod change_page;
mod core;
mod data_retention;
pub use change_page::ChangePage;
/// The durable substrate's backend builder (ADR 0132 §1).
pub mod durable;
mod durable_session;
mod error;
pub mod formats;
mod language_observation;
mod observation_feed;
mod parked_work;
#[cfg(feature = "postgres")]
mod postgres_host;
#[cfg(feature = "postgres")]
mod postgres_live_replay;
#[cfg(feature = "postgres")]
mod postgres_process_replay;
pub mod preflight;
pub(crate) mod process_admin;
mod process_feed;
mod process_history;
mod process_lifecycle;
/// A session's config and the typed commands that change it (FIG-4379).
///
/// Every installed owner records its namespace when a session is created,
/// beside the core owner's share (provider, model, generation, turn
/// budget and tool access). After that the config changes only through a
/// [`ConfigTransaction`]: an ordered list of typed commands of any owners,
/// written against the config revision the caller read under a stable
/// [`ConfigWrite::id`], applied with
/// [`SessionConfigAdmin::apply`](crate::admin::SessionConfigAdmin::apply). It
/// applies all or nothing, with one revision step, and settles as a
/// [`ConfigTransactionOutcome`]: applied, stale, or refused by an owner.
/// [`SessionConfigAdmin::commands`](crate::admin::SessionConfigAdmin::commands)
/// lists every command the installed owners register.
///
/// The core owner's commands are here; a protocol's are in its module
/// ([`standard::SetStandardRender`] and `rlm::SetRlmRender`). The prompt is
/// keyed sections (ADR 0133): plugins register them, and the host orders and
/// places them with [`SetPromptPlan`].
pub mod config {
    pub use crate::admin::SessionConfigAdmin;
    pub use crate::admin::config_transactions::{ConfigSettlement, ConfigWrite};
    /// The owner of the core configuration the commands below change.
    pub use lash_core::CoreConfigOwner;
    pub use lash_core::plugin::config::core::{
        SetAttachmentAcceptance, SetChargeSafety, SetGeneration, SetLlmProfile, SetMaxToolCalls,
        SetNoProgressBudget, SetPromptPlan, SetReasoning, SetToolAccess, SetTurnBudget,
    };
    pub use lash_core::{
        CORE_CONFIG_OWNER, ConfigCommandCatalog, ConfigCommandDescriptor, ConfigCommandEntry,
        ConfigRefusal, ConfigRefusalReason, ConfigSubmitError, ConfigTransaction,
        ConfigTransactionOutcome, ConfigValueRole, CoreConfig, CoreConfigRefusal, RefusalSite,
    };
}
/// The standard protocol's host surface: its creation options, its recorded
/// namespace and the command that changes it.
///
/// The prompt is not config: the protocol registers keyed sections under
/// [`STANDARD_PROTOCOL_PLUGIN_ID`]
/// ([`standard_section_keys`](standard::standard_section_keys)), placed in the
/// initial instructions unless the host's [`SetPromptPlan`](crate::config::SetPromptPlan)
/// places them. A host adds its own text as sections of its own plugin, and
/// replaces or omits a built-in section by wrapping it (ADR 0133):
///
/// ```ignore
/// reg.prompt().section(
///     PromptSectionSpec::new(key("support-intro"), PromptPlacement::InitialInstructions),
///     Arc::new(|_: &PromptInput<'_>| {
///         Ok(SectionText::text("You are the support desk's assistant."))
///     }),
/// )?;
/// ```
///
/// A run's options are [`StandardRunOptions`].
pub mod standard {
    pub use lash_protocol_standard::{
        STANDARD_PROTOCOL_PLUGIN_ID, SetStandardRender, StandardConfigOwner, StandardConfigRefusal,
        StandardRecordedBehaviour, StandardRecordedConfig, StandardRenderRefusal,
        StandardRunOptions, StandardTurnOptions, section_keys as standard_section_keys,
    };
}
pub mod render {
    pub use lash_protocol_standard::render::{
        AuthoredViewPolicy, BuiltinToolOutputRenderer, ResolvedStandardRenderConfig,
        StandardRenderConfig, ToolOutputRenderer, ToolOutputRendererSlot, ToolRenderParams,
        ToolRenderPatch, resolve,
    };
    #[cfg(feature = "rlm")]
    pub use lash_render::*;
}
#[cfg(feature = "rlm")]
/// RLM-specific turn-builder extensions.
pub mod rlm;
/// Reusable contracts for agent scenarios.
#[cfg(feature = "testing")]
pub mod scenario_contracts;
#[cfg(feature = "rlm")]
/// Integration with the Lash VM execution substrate.
pub mod vm;
/// Standard-lock poison recovery traits for application code.
pub mod sync {
    pub use lash_core::sync::*;
}
mod send;
pub use lash_core::RunId;
mod session;
mod session_binding;
mod support;
#[cfg(test)]
mod tests;
mod tool_intent_ingress;
/// Turn builders, streams, activities, and output types.
pub mod turn;
pub mod usage;
#[cfg(feature = "rlm")]
pub mod workflow;

pub use crate::admin::{
    AdminMutation, AdvancedToolAdmin, Completions, SessionCommandAdmin, SessionCommandWithdrawal,
    ToolAdmin,
};
pub use crate::core::{
    DeploymentDrainStatus, LashCore, LashCoreBuilder, NodeDrainError, NodeDrainReport,
    SessionDeleteCompletion, SessionDeletion,
};
pub use crate::data_retention::DataRetention;
pub use crate::durable_session::DurableSession;
pub use crate::error::{EmbedError, Result, SendError};
pub use crate::parked_work::{ControlIntentPage, ControlIntentQuery, ParkedWork};
pub use crate::send::{
    BatchInput, CancelBuilder, CancelReceipt, CancelTarget, ParkedTurn, RunHandle,
    SendBatchBuilder, SendBuilder, SendHandle, SendOutcome, StalledDelivery, TurnEvents,
    TurnStatus,
};
pub use crate::session::{
    LashSession, ObservableSession, ParkedSession, SessionBuilder, SessionCreation,
    SessionParkRefused,
};
pub use crate::turn::{ReportSource, TurnActivityFanout, TurnOutput, TurnReport};
/// Re-exported so implementors of `#[async_trait]` facade traits (for example
/// [`tools::StaticToolExecute`]) apply the macro without carrying their own
/// `async-trait` dependency to keep version-aligned.
pub use lash_core::async_trait;
/// Store→engine delivery obligations (ADR 0109): what a stalled obligation
/// reports, and how this deployment competes for the recovery leader lease.
pub use lash_core::engine::{RecoveryLeaseConfig, RecoveryLeaseTimings, RecoveryPassBudget};
/// Why a session's actor parked, read by
/// [`DurableSession::park_reason`], and the state of its unfinished turn a
/// build refused to decode.
pub use lash_core::runtime::durable::session::{ParkedTurnState, SessionParkReason};
mod pacing;
pub use lash_core::facade_support::{
    TurnCancelAffectedInput, TurnCancelInputOutcome, TurnCancelMode,
    TurnCancelUndeliveredInputPolicy,
};
/// A plugin hook's [`HookKey`](plugins::HookKey) for a string literal,
/// validated at compile time.
pub use lash_core::hook_key;
pub use lash_core::runtime::ExternalCompletionError;
/// The immediate delivery verdict of an obligation relay's attempt.
pub use lash_core::runtime::obligations::relay::RelayVerdict;
/// How a turn coalesces its stream deltas into frames for the live feed
/// (`LashCoreBuilder::delta_coalescing`).
pub use lash_core::runtime::{DeltaCoalescing, DeltaCoalescingError};
pub use lash_core::store::{
    DeliveryError, ObligationId, ObligationKey, ObligationKind, ObligationState, SessionFault,
    SessionFaultOrigin, SessionFaultRecord, StallReason, StalledObligation, UndecodableObligation,
};
pub use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, BatchId, ChargeSafetyPolicy,
    ChargeSafetyRefusalEvidence, CommitBudget, CommitBudgetLimit, DrainMode, DrainModePolicy,
    EmptyLlmProfiles, FrameKey, InputId, InputItem, LlmCallRecord, LlmProfileConfig, LlmProfileKey,
    LlmProfileLimits, LlmProfileLimitsError, LlmProfileMetadata, LlmProfileMetadataBuilder,
    LlmProfileRegistry, LlmProfileUnavailable, LlmProfileUnavailableReason, LlmProfiles,
    MaxToolCalls, NoProgressBudget, NodeId, OmittedToolCalls, OutputTokenLimits, PendingTurnInput,
    PendingTurnInputCancelOutcome, PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget,
    PendingTurnInputRead, PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome,
    ProcessId, QueuedDrainCandidate, QueuedDrainFamily, QueuedDrainPolicy, QueuedDrainRequest,
    QueuedDrainSelection, QueuedWorkBatchingConfig, ReasoningRefused, RecordedLlmProfile,
    RegisteredLlmProfile, RegistrationError, Resolution, ResolveOutcome, RuntimeOwner,
    SessionCreateRequest, SessionEntry, SessionError, SessionId, SessionListFilter,
    SessionRelationKind, SessionStartPoint, SessionView, ToolCallLimitExceeded, ToolCallLimitScope,
    TurnActivity, TurnActivityId, TurnBudget, TurnEvent, TurnFailureEvidence,
    TurnFailurePartialOutput, TurnFailureSettlement, TurnId, TurnInput, TurnInputApplication,
    UnstatedSessionConfig, facade_support::GenerationOverlay, facade_support::PluginStack,
    facade_support::QueueWithdrawalPublisher, facade_support::SessionCommand,
    facade_support::SessionCommandReceipt, facade_support::SessionSpec,
    facade_support::SpecResolveError, facade_support::TurnActivitySink,
    facade_support::TurnAddress, facade_support::TurnAttach, facade_support::TurnCancelOutcome,
    facade_support::TurnCancelReceipt, facade_support::TurnCancelRequest,
    facade_support::TurnCancellationEvidence, facade_support::TurnExecutionMetrics,
    facade_support::TurnFinish, facade_support::TurnInputAcceptanceReceipt,
    facade_support::TurnOutcome, facade_support::TurnStop, facade_support::TurnTerminal,
    facade_support::TurnWorkDriver,
};
/// Lash's own execution bounds (`LashCoreBuilder::execution_budgets`), the
/// limit one executable stretch runs under, and the registration refusal of
/// a tool missing a bound its host must set.
pub use lash_core::{
    ExecutionBudgets, ExecutionBudgetsConfig, ExecutionBudgetsError, ExecutionLimit,
    ProviderAttemptLimits, RegistrationRefused,
};
pub use pacing::{
    CommitAdmissionPolicy, CommitAdmissionPolicyError, ObserverPacing, PollPacing, RecoveryPacing,
    RelayPolicy, RelayPolicyError, RuntimePacingPolicy, WorkCadencePolicy,
};
// A host's head write is a session command it submits, settles and may
// withdraw (FIG-4202): the settlement and the typed outcomes it carries.
/// The one recorded form of a refusal those outcomes and a run's refused
/// terminal keep: its code, message and typed cause (FIG-5391). A host reads
/// it back as the `RuntimeError` it was, with `RuntimeError::from`.
pub use lash_core::RecordedRefusal;
pub use lash_core::runtime::{
    CompactContextOutcome, OpenAgentFrameCommandOutcome, PluginOperationCommandOutcome,
    SessionCommandOutcome, SessionCommandSettlement,
};
pub use lash_core::store::SessionHeadOwner;
/// The one substrate a [`LashCore`] takes every persistence port and its
/// durable store from: the concrete durable backend over one [`StoreSet`]
/// (ADR 0132 §1). [`LashCore::builder`] requires one, which
/// [`durable::DurableBackendBuilder`] builds.
pub use lash_core::{Backend, StoreBindingId, StoreSet};
/// The shape a sent input runs under (FIG-3838): a [`RunSpec`] set on
/// [`SendBuilder::run`], or through its one-shot setters, and the
/// [`RunDefinition`]s a [`LashCoreBuilder`] registers for specs to name.
pub use lash_core::{
    BindingId, CapabilityRef, ContractRef, DefinitionRef, NoRunOptionsOwner, RenderRefusal,
    RunDefinition, RunDefinitionRefusal, RunOptionsOwner, RunOverrides, RunResolveError,
    RunShapeRefusal, RunSpec, SlotId,
};
/// Lash's identity for one tool call (ADR 0117): what a tool keys its
/// idempotency on, through [`tools::AttemptContext::call_id`].
pub use lash_core::{InvalidToolCallId, ToolCallId, ToolCallRootError};
pub use lash_core::{SessionAdministration, SessionDeleteContext, SessionDeleteExecution};
/// Cooperative cancellation handle; re-exported so embedders hold one
/// without depending on `tokio-util` themselves.
pub use tokio_util::sync::CancellationToken;
// The vocabulary this module's signatures name (the facade-completeness rule).
pub use lash_core::ConfigTransactionRecord;
pub use lash_core::runtime::ConfigTransactionSubmitError;
pub use lash_core::runtime::obligations::relay::{
    DeliveryFailure, ObligationDelivery, ObligationRelay,
};
pub use lash_core::store::{IngressTerminal, IngressTerminalCause};
pub use lash_core_store::session_identity::{OpenAgentFrameOutcome, OpenAgentFrameRequest};
pub use lash_core_store::turn_input_vocabulary::ResolvedRun;
pub use lash_sansio::llm::types::{
    AttemptRecord, ChargeSafetyDenialReason, LlmCallId, StreamBlockEvent, StreamBlockKind,
};
pub use lash_sansio::{
    BlankIdentity, ErrorEnvelope, ExecCodeFailure, FrameKeyError, InvalidProcessId, LlmCallError,
    ReportedFailure, RetryProgress, ToolCallPosition, ToolCallRoot,
};

/// `use lash::prelude::*;` brings in the daily core/session/turn vocabulary
/// without the lower-level integration types or domain modules also exposed
/// from the crate root.
pub mod prelude {
    pub use crate::{
        AdvancedToolAdmin, ChargeSafetyPolicy, DeploymentDrainStatus, DurableSession, EmbedError,
        InputItem, LashCore, LashCoreBuilder, LashSession, LlmProfileConfig, LlmProfileKey,
        LlmProfileLimits, LlmProfileLimitsError, LlmProfileMetadata, LlmProfileMetadataBuilder,
        LlmProfileRegistry, MaxToolCalls, NoProgressBudget, ObservableSession, ParkedSession,
        PendingTurnInputCancelOutcome, PluginStack, RegisteredLlmProfile, Result, SendBuilder,
        SendHandle, SendOutcome, SessionBuilder, SessionCommand, SessionCommandAdmin,
        SessionCommandReceipt, SessionCreateRequest, SessionCreation, SessionDeletion,
        SessionEntry, SessionListFilter, SessionParkRefused, SessionRelationKind, SessionSpec,
        SessionStartPoint, SessionView, ToolAdmin, TurnActivity, TurnActivityFanout,
        TurnActivityId, TurnActivitySink, TurnBudget, TurnEvent, TurnExecutionMetrics, TurnFinish,
        TurnInput, TurnInputAcceptanceReceipt, TurnOutcome, TurnOutput, TurnReport, TurnStatus,
        TurnStop,
    };
}

/// Session observation: cursors, resumable event streams, and live replay
/// recovery for host frontends. Entry point: [`LashSession::observe`] /
/// [`ObservableSession`].
pub mod observe {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    /// The stream trait a live replay subscription's tail and the session
    /// feed implement: a custom [`LiveReplayStore`] builds its
    /// [`LiveReplaySubscription`](crate::persistence::LiveReplaySubscription)
    /// from one.
    pub use futures_util::Stream;
    pub use lash_core::runtime::ParsedSessionCursor;

    pub use crate::observation_feed::SessionObservationEventId;
    pub use crate::session::{SessionObservationStream, SessionObservationStreamItem};
    pub use lash_core::{
        LiveReplayEventDraft, LiveReplayGapReason, LiveReplayStore, LiveReplayStoreError,
        LiveReplaySubscribeOutcome, SessionCursor, SessionObservationEvent,
        SessionObservationEventPayload, SessionProcessEventKind, SessionQueueEventKind,
        SessionRevision, facade_support::InMemoryLiveReplayStore,
        facade_support::InMemoryLiveReplayStoreConfig, facade_support::LiveReplayGap,
        facade_support::SessionObservation, facade_support::SessionObservationSubscription,
        facade_support::SessionResume,
    };
}

/// Tool definitions, providers, and execution types.
///
/// A tool's execution contract controls recovery. A started `Once` attempt
/// without a recorded outcome settles as `Interrupted` and never runs again;
/// a reported failure is not retried. `Repeatable` permits rerunning an
/// unrecorded attempt and retrying reported failures within its bounds, only
/// while both the pinned and current policies permit repetition. Lash makes
/// no exactly-once claim for external effects: a repeat can follow an effect
/// whose outcome did not commit. A repeatable tool keys its idempotency on
/// [`AttemptContext::call_id`](crate::tools::AttemptContext::call_id), the
/// `ToolCallId` lash mints for the call: it is the same on every run of one
/// logical call and different for every other call, whatever id the model's
/// provider sent.
/// [`AttemptContext::attempt_number`](crate::tools::AttemptContext::attempt_number)
/// counts the runs apart from it.
pub mod tools {
    #[cfg(feature = "rlm")]
    pub use lash_llm_tools::LlmToolsPluginFactory;
    pub use lash_sansio::ToolPresentationConfig;
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::{GetDefinitionIntent, PublishDefinitionIntent};
    pub use lash_sansio::{ModelTool, ToolCallStatus};

    pub use crate::tool_intent_ingress::{
        ToolIntentIngress, ToolIntentIngressKey, ToolIntentIngressOutcome, ToolIntentIngressRefusal,
    };
    /// Typed cancellation evidence constructed by tool implementors; pass it to
    /// [`ToolCallOutput::cancelled`] when a tool stops without completing.
    pub use lash_core::ToolCancellation;
    /// Turn flow control constructed by tool implementors; attach it with
    /// [`ToolCallOutput::with_control`] or [`ToolOutcome::with_control`].
    pub use lash_core::ToolControl;
    /// Source and owning plugin identity of tools registered with
    /// [`crate::LashCoreBuilder::tools`]. Use it in deferred grants for those tools.
    pub use lash_core::facade_support::PLUGIN_TOOL_SOURCE_ID;
    /// The pending model call passed to a tool's preparation hook.
    pub use lash_core::sansio::PendingToolCall;
    pub use lash_core::{
        AttemptContext, AttemptProcessReads, AttemptSessionReads, CancelHint, CancelProcessIntent,
        CompactToolContract, ExecutionOwner, IsolatedProcessBinding, IsolatedProcessRequest,
        PendingCompletion, PendingResolver, PreparedToolCall, StartProcessIntent,
        TOOL_INTENT_MAX_CANONICAL_BYTES, TOOL_INTENT_MAX_COUNT, TOOL_INTENT_MAX_PER_KIND,
        TOOL_INTENT_PROTOCOL_V3, ToolArgumentProjectionPolicy, ToolAttachmentClient,
        ToolAttemptOutcome, ToolCall, ToolCallOutcome, ToolCallOutput, ToolCallRecord,
        ToolCatalogEntry, ToolContract, ToolDefinition, ToolDirectCompletionClient, ToolDiscovery,
        ToolExecutionGrant, ToolFailure, ToolFailureCause, ToolFailureClass, ToolFailureSource,
        ToolIntent, ToolIntentCommandFailure, ToolIntentExecutionOutcome, ToolIntentIdentity,
        ToolIntentKind, ToolIntentRealized, ToolIntentRefusalReason, ToolIntentRuntimeFailure,
        ToolIntents, ToolManifest, ToolModule, ToolOutcome, ToolOutcomeDone, ToolOutputContract,
        ToolPrepareCall, ToolPrepareContext, ToolProvider, ToolRegistry, ToolSessionLlmProfile,
        ToolValue, ToolView, ToolViewBlock, ToolViewMeta, derive_tool_intent_identity,
        facade_support::ReconfigureError, facade_support::ToolSourceHandle,
        facade_support::ToolStateFacadeOps, turn_outcome_from_tool_control,
    };
    /// Per-call execution contract carried by [`ToolDefinition::with_execution_policy`].
    pub use lash_core::{Backoff, BoundedRetry, ExecutionPolicy, LimitCause};
    /// The three capabilities a tool declares with
    /// [`ToolDefinition::with_declaration`], what refuses a call at admission,
    /// and what refuses an outcome its declaration does not admit.
    pub use lash_core::{DeclarationRefusal, OutcomeShape, ToolAdmissionRefusal, ToolDeclaration};
    pub use lash_core::{DeclaredStart, DeclaredStartRefused};
    /// A tool's host-set bounds: [`ToolDefinition::with_execution`] bounds
    /// its body, [`ToolDefinition::with_park`] the park of a tool that may
    /// defer. Registration refuses a tool missing one, naming the bound.
    pub use lash_core::{ParkBound, ToolBound, ToolBounds};
    /// Tool-execution request batches, replies, and child-process observation hooks.
    pub use lash_core::{
        PreparedToolBatch, PreparedToolBatchCall, ToolChildExecutionTraceHook,
        ToolChildProcessStarted, facade_support::ToolInvocation,
        facade_support::ToolInvocationReply,
    };
    /// The dialect-agnostic tool binding and its one setter. The manifest key
    /// is lash's internal projection — hosts never read or write it, and which
    /// dialect executes a bound tool is decided inside lash.
    pub use lash_core::{TOOL_BINDING_KEY, ToolBinding, ToolDefinitionBindingExt};
    pub use lash_core::{
        ToolId, ToolState, facade_support::SupersededToolIdentity,
        facade_support::ToolMembershipUpdate, facade_support::ToolRestoreReport,
        facade_support::ToolSourcePolicy, facade_support::ToolStateChange,
        facade_support::ToolStateChangeOutcome, facade_support::ToolStateEntry,
    };
    /// Engine-owned tool-intent admission records used by process-registry integrators.
    pub use lash_core::{
        ToolIntentSubmissionAdmission, ToolIntentSubmissionOutcome, ToolIntentSubmissionRecord,
        ToolIntentSubmissionSettlement,
    };
    /// The whole tool-authoring support surface: [`StaticToolProvider`] /
    /// [`StaticToolExecute`] for fixed-set providers plus the shared helpers
    /// (`invalid_tool_args`, `object_schema`, `parse_optional_usize_arg`,
    /// `ToolBinding`, `ToolDefinitionBindingExt`, `TOOL_BINDING_KEY`,
    /// `LASH_VM_BINDINGS_ENABLED`) tools are built from. The glob keeps the
    /// facade complete as the crate grows; where it overlaps the explicit
    /// `rlm` re-exports above, those name the same items.
    pub use lash_tool_support::*;
    #[cfg(feature = "rlm")]
    pub use lash_vm_runtime::{
        CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
        DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, ToolBindingResolutionExt, ToolManifestBindingExt,
        catalogue_preview, catalogue_preview_entries_from_catalog_records,
        catalogue_preview_entries_from_manifests, catalogue_preview_entry_from_catalog_record,
        catalogue_preview_entry_from_manifest, required_tool_binding,
    };
    #[cfg(feature = "rlm")]
    pub use lash_vm_runtime::{
        DeferredLink, DeferredLinkError, DeferredResolutionError, DeferredResolutionLinkKey,
        DeferredResolveContext, DeferredToolResolver, RecordedGrantInstallError,
        Resolution as DeferredToolResolution, SharedDeferredToolResolver,
        ToolGrant as DeferredToolGrant, compile_with_deferred_resolution,
    };
}

/// Direct protocol transport types.
pub mod direct {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_sansio::llm::types::{
        GenerationProjectionProvenance, ProviderReplayMeta, ProviderReplayOriginConflict,
        ResponsePhase, ResponseTextMeta,
    };

    pub use lash_core::llm::types::{
        GenerationOptionOutcome, GenerationOptions, GenerationReceipt, LlmEventSender,
        LlmOutputPart, LlmStreamEvent, LlmTerminalReason, NonNegativeFiniteF64,
        NonNegativeFiniteF64Error, ProviderReasoningReplay, ProviderReplayDrop,
        ProviderReplayDropReason, ProviderReplayKind, ProviderRouteIdentity, StreamBlockIdentity,
    };
    pub use lash_core::{
        facade_support::DirectCompletion, facade_support::DirectJsonSchema,
        facade_support::DirectLlmClient, facade_support::DirectLlmCompletion,
        facade_support::DirectLlmError, facade_support::DirectLlmOutcome,
        facade_support::DirectMessage, facade_support::DirectOutputSpec,
        facade_support::DirectPart, facade_support::DirectRequest, facade_support::DirectRole,
    };
}

pub mod persistence {
    //! Store-author contracts, including the process registry and its row.
    //!
    //! Custom stores implement these ports against `lash` alone. `ProcessRecord`
    //! is the durable lifecycle fold stores persist and return; receipts and
    //! authorities here govern writes. Hosts and plugins read
    //! [`crate::process::ObservedProcess`] through `processes()` or their
    //! runtime-provided services instead.
    #[cfg(any(test, feature = "testing"))]
    pub use lash_core::{
        ConformanceProcessRegistry, ProcessEventLogTestSupport, ProcessRegistryTestSupport,
    };
    pub use lash_core::{
        NonTerminalProcessPage, ProcessCancelReceipt, ProcessChange, ProcessClockRebind,
        ProcessCompletionAuthority, ProcessCompletionOutcome, ProcessEventAppendRequest,
        ProcessEventLog, ProcessExecutionWriteAuthority, ProcessHandleView, ProcessLifecycle,
        ProcessListFilter, ProcessLiveReferenceView, ProcessObserverRegistry, ProcessOpScope,
        ProcessQuery, ProcessRecord, ProcessRegistrar, ProcessRegistrationReceipt, ProcessRegistry,
        ProcessRetention, ProcessRosterRecords, ProcessStartOutcome, ProcessStartReceipt,
        ProcessToolIntents, StagedProcessStart,
    };

    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core_store::artifact_referrer::{
        ArtifactCarry, ArtifactCleanup, ArtifactReferrerError, ArtifactReferrerKind,
        ArtifactStoreId, AttachmentUploadId, ReferrerGuard, ReferrerStore, UploadReferrerId,
    };
    pub use lash_core_store::attachments::{AttachmentExecutionBinding, AttachmentHolder};
    pub use lash_core_store::compat::{CompatRefusal, CompatStamp};
    pub use lash_core_store::runtime_error::ExecutableGenerationRefusal;
    pub use lash_core_store::session_graph::{
        NodeTimestamp, NodeTimestampError, SessionGraphAppendBuilder, SessionGraphData,
        SessionNodeDraft, SessionReadModel,
    };
    pub use lash_core_store::session_identity::{
        SessionLineage, SessionObservedProcessOutcome, SessionObservedProcessReceipt,
        SessionObserverIntent,
    };
    pub use lash_core_store::session_state::{
        InstalledRunView, RuntimeSessionAuthority, SessionPluginStateSource,
    };
    /// Retained tool material: a store's dependency leases on bundles.
    pub use lash_core_store::store::ToolMaterialStore;
    pub use lash_core_store::store::commit_budget::RuntimeCommitBudgetMeasurement;
    pub use lash_core_store::store::{
        DurableRecord, EnumerationSource, FrameTransition, ReadWindow, StoreFault, StoreRefusal,
        StoredRunTerminal, SurfaceFormat, WriterPin,
    };
    pub use lash_core_store::{PersistedNodeIds, surface_format};
    /// The protocol-generic form [`SessionHistoryRecord`] specializes.
    pub use lash_sansio::SessionHistoryRecord as GenericSessionHistoryRecord;
    pub use lash_sansio::{AppendVec, BaseRenderCache, ConversationRecord};
    pub use lash_sansio::{VersionRange, VersionRangeError};

    pub use lash_core::CheckpointKind;
    /// The store halves a [`StoreSet`](crate::StoreSet) hands out as trait
    /// objects, nameable so a host can decorate a store set (FIG-4373).
    pub use lash_core::ProcessDefinitionStore;
    pub use lash_core::RunSpecHash;
    pub use lash_core::attachments::{
        AttachmentRootPage, AttachmentRootSource, CompleteAttachmentRoots,
    };
    /// The logical run reference a run store ends a run by.
    pub use lash_core::engine::RunRef;
    /// Durable session-store inputs and outputs exposed to storage integrators.
    pub use lash_core::runtime::{
        ActiveTurnIngress, AdmissionBoundary, AdmittedQueuedWork, AdmittedTurnInputs,
        DeliveryPolicy, DeploymentStore, DeploymentStoreDecorator, ForkSessionReceipt,
        ForkSessionRequest, LiveReplayOutcome, LiveReplaySubscription, PendingTurnInputBatch,
        PendingTurnInputDraft, QueuedCheckpointTurnInput, QueuedWorkAuthority, QueuedWorkBatch,
        QueuedWorkBatchDraft, QueuedWorkCompletion, QueuedWorkEnqueueOutcome, QueuedWorkPayload,
        RuntimeCheckpointComponents, RuntimeSessionState, SessionCreationHead, SessionCursorError,
        SessionStoreCreateRequest, TurnInputAdmissionMode, TurnInputCheckpointBoundary,
        TurnInputCompletion, TurnInputCompletionData, TurnInputIngress, TurnInputState,
        TurnInputStateKind, TurnLaneAdmissionPolicy,
    };
    pub use lash_core::session_graph::RealizedNodeTimestamp;
    /// The artifact-cleanup ledger a [`StoreSet`](crate::StoreSet) hands out
    /// as a trait object (FIG-4373).
    pub use lash_core::store::ArtifactCleanupLedger;
    /// The current state of an obligation a custom ledger exposes.
    pub use lash_core::store::ObligationStanding;
    /// Head and usage values returned by custom session stores.
    pub use lash_core::store::SessionHeadRef;
    /// The store halves a storage integrator's [`StoreSet`](crate::StoreSet)
    /// supplies: the obligation ledgers and the recovery leader lease
    /// (ADR 0109 §1.3, §1.6).
    pub use lash_core::store::{
        ClaimToken, ClaimedObligation, HolderId, KeyColumn, KeyColumnType, LeaseAnswer, LeaseClaim,
        LeaseName, LeaseRow, ObligationLedger, ObligationSettlement, RecoveryLeaderStore,
        SettleOutcome,
    };
    pub use lash_core::store::{StoreTransition, TurnTraceReceipt};
    /// Artifact ownership supplied to protocol engines and effect controllers.
    pub use lash_core::{
        ArtifactName, ArtifactReferrer, FrameEnvironmentId, ReferrerClaim, ResolvedArtifactCleanup,
    };
    pub use lash_core::{AttachmentReferrers, AttachmentWrite, SessionReferrerState};
    /// Queued-work ordering values and admission-selection helpers.
    pub mod queued_work {
        // The vocabulary this module's signatures name (the facade-completeness rule).
        pub use lash_core_store::store::queued_work::TurnLaneCandidate;

        /// Stable queued-work ordering values and selection helpers for store implementations.
        pub use lash_core::store::queued_work::{
            PendingSessionWorkOrdering, PendingWorkOrderingKey, admission_scan_limit,
            derive_batch_id, select_leading_session_command,
        };
    }
    pub use lash_core::attachments::{
        AttachmentReclamationRetryPolicy, AttachmentReclamationRetryPolicyError,
    };
    pub use lash_core::session_graph::WindowAnchor;
    pub use lash_core::store::PluginWriterRangesFuture;
    /// A session's fault record (ADR 0109 §9): one segment of
    /// [`RuntimeStore`], implemented by every store a runtime executes.
    pub use lash_core::store::SessionFaultStore;
    pub use lash_core::store::plugin_writers::{
        AdmittedPlugin, PluginAdmission, PluginPublication, PluginWriterRanges,
        PluginWriterRegistration, PluginWriterStamp,
    };
    /// A run's admission, what its checkpoints admit, how a commit settles
    /// the rows its run holds, and the session's one unfinished run
    /// (FIG-3927, FIG-4403, FIG-5221).
    pub use lash_core::store::{
        AdmittedHead, AdmittedInputIds, AdmittedTurnRows, CheckpointAdmission,
        CheckpointAdmissionRequest, EmptyInputAdmission, IngressRowId, IngressSettlement,
        RUN_ADMISSION_STEP, RunAdmissionRecord, UnfinishedRun,
    };
    pub use lash_core::store::{
        AnchorUnavailable, FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor,
        HistoryBudget, HistoryCursor, HistoryNode, HistoryPage, HistoryStop, LineageStamp,
        LoadedSessionWindow, QueuedWorkStore, RuntimeStore, SessionCatalogStore,
        SessionHistoryStore, SessionLookup, SessionStore, SessionWindowRead, TurnInputStore,
        WindowAnchorViolation, WindowSelector, load_session_read_view, load_session_window_state,
        refresh_session_window,
    };
    pub use lash_core::store::{
        AppendRequestIdentity, CheckpointComponentDescriptor, GraphAppend,
        HydratedCheckpointComponent, HydratedSessionCheckpoint, OperationId, ParkCancelCause,
        ParkEventColumns, ParkEventKind, ParkFeedCursor, ParkFeedEvent, ParkFeedPage, ParkId,
        ParkReason, ParkReasonCode, ParkReport, PhysicalTurn, RuntimeCommit, RuntimeCommitReceipt,
        RuntimeStoreDecorator, RuntimeTurnCommitStamp, SemanticBoundaryOperation,
        SessionCheckpoint, SessionHeadMeta, SessionHeadPayload, TurnChange, TurnChangeCursor,
        TurnChangeKind, TurnChangePage, TurnCommitFailureCause, TurnCommitOutcome,
        TurnProjectionWatermark, UnparkCause, UnsettledTurnCounts, commit_runtime_state_verified,
        validate_turn_commit_outcome_code,
    };
    /// A logical run's durable terminal evidence and the store segment that
    /// answers and binds runs (FIG-3600 S7, FIG-3607 item 8), and the
    /// control intent a session's close records.
    pub use lash_core::store::{
        CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind,
        ControlIntentState, ControlIntentStore, EnginePark, RunCommittedOutcome, RunEndOutcome,
        RunStore, RunTerminal, RunTerminalCause, RunTerminalKind, RunTerminalWrite, TurnCommitId,
    };
    /// The multi-session store's catalog and bounded history segments, the
    /// one-session view runtime code holds, and the window loaders (ADR 0112).
    /// A session's committed turns in commit order: the cursor a host keeps,
    /// and the store-level page a backend answers (FIG-5297).
    pub use lash_core::store::{
        CommittedTurnCursor, CommittedTurnNodes, CommittedTurnNodesPage, CommittedTurnReceipt,
    };
    /// Test-only store hooks and the conformance-suite handle types that
    /// carry them (`testing` feature only; no production trait requires them).
    #[cfg(any(test, feature = "testing"))]
    pub use lash_core::store::{
        ConformanceDeployment, ConformanceStore, DecodedRowCounts, GraphRowCorruption,
        StoreTestSupport,
    };
    /// Attachment ownership, retention and reclamation contracts.
    pub use lash_core::{
        AdoptedAttachmentCondemnation, AttachmentCondemnation, AttachmentCondemnationAdoption,
        AttachmentCondemnationPhase, AttachmentCondemnationProvenance,
        AttachmentCondemnationRecord, AttachmentCondemnationSettlement, AttachmentContentMismatch,
        AttachmentDeleteArming, AttachmentDeleteStallReason, AttachmentPolicy,
        AttachmentReadPolicy, AttachmentReclamationPolicy, AttachmentRetentionFailure,
        AttachmentRetentionStoreFailure, AttachmentRootSet, AttachmentSettlementOutcome,
        AttachmentStore, AttachmentStoreError, AttachmentStoreFailureClass,
        AttachmentStorePersistence, AttachmentSweepGeneration, AttachmentWriteFence,
        AttachmentWritePermit, AttachmentWriteToken, EmptyRootSetPolicy,
        MAX_ATTACHMENT_DELETE_ATTEMPTS, ProcessExecutionEnvStore, StoredAttachment, StoredBlobRef,
        TurnPreludeStore, attachments::AttachmentReclamationFailure,
        facade_support::AttachmentGcFence, facade_support::AttachmentReclamationReport,
        facade_support::RuntimeAttachmentStore, facade_support::reclaim_unreferenced_attachments,
    };
    /// The Lash VM module-artifact port a backend's store set supplies.
    pub use lash_core::{
        ArtifactStoreError, DurabilityTier, ModuleArtifactAstRefusal, ModuleArtifactCorruption,
        ModuleArtifactGeneration, ModuleArtifactRefusal, ModuleArtifactStore,
    };
    pub use lash_core::{
        BlobRef, DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface,
        ExecutedCall, ExecutedCallOutcome, FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState,
        FleetFormatStore, GcReport, LeaseIncarnationId, LeaseOwnerId, LeaseOwnerIdentity,
        MaintenanceFailure, MaintenanceRefusal, MaintenanceReport, MaintenanceResult,
        MaintenanceStop, MaintenanceSweep, OLDEST_SUPPORTED_SESSION_STATE_VERSION,
        PersistedSessionConfig, PersistedTurnState, ProtocolEvent, RetentionBound, RetentionReport,
        ScanCoverage, SessionAdmission, SessionBlobReclaimReport, SessionCommitStore, SessionGraph,
        SessionHistoryRecord, SessionMeta, SessionNodePayload, SessionNodeRecord, SessionReadView,
        SessionRelation, SessionStateAdmission, StoreBackend, StoreComponentVersion, StoreError,
        StoreMaintenance, StorePreflight, StoreReleaseStamp, StoreReleaseState,
        StoreSchemaDatabase, StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict,
        TurnInputAdmission, UndeliveredConfigChange, VacuumReport,
        facade_support::SessionNodeProjection,
    };
    pub use lash_core::{
        facade_support::ChronologicalEntry, facade_support::ChronologicalPayload,
        facade_support::ChronologicalProjection,
    };
    /// Content validation and optional provider-file delivery behind the host store.
    pub use lash_core_store::attachments::{
        ContentMismatchDetail, ProviderFileCacheLimits, ProviderFileDelivery, ProviderFileUploader,
        UploadedProviderFile,
    };
    /// The typed view an RLM host reads and writes its module artifacts through.
    #[cfg(feature = "rlm")]
    pub use lash_vm_runtime::LashVmArtifacts;
}

/// Prompt sections (ADR 0133): the host's plan and the records of what a
/// model call composed.
///
/// Every piece of model-facing instruction text is a keyed section owned by
/// the plugin that registered it ([`PromptSectionId`]). The host owns the
/// [`PromptPlan`], recorded as session config and changed by
/// [`SetPromptPlan`](crate::config::SetPromptPlan): the section order and each
/// section's [`PromptPlacement`], which overrides the plugin's default. Lash
/// sets no placement policy. [`PromptPlacement::InitialInstructions`] puts a
/// section in the provider's instruction field, at the head of the request,
/// so a section that changes between calls changes the cached request
/// prefix. [`PromptPlacement::CurrentContext`] puts it late, after the
/// conversation and outside its history, which keeps the history prefix
/// stable but may reach the model in a different role.
/// [`PromptPlacement::Excluded`] drops it: its renderer never runs.
///
/// [`SessionPromptAdmin`] reads a session's recorded plan and registered
/// catalog, and previews the plan's resolution for a call without admitting
/// one. Its `snapshot(run, call)` reads a retained model call's snapshot
/// with its exact text, without rendering again.
///
/// A call records its [`ResolvedPromptPlan`] and a version-1
/// [`PromptSnapshot`]: each section's base text, each wrapper's output and
/// the final text, as content-addressed [`PromptTextRef`]s.
pub mod prompt {
    pub use crate::admin::prompt::SessionPromptAdmin;
    pub use lash_core::durable_port::domain::{ModelCallId, PromptCallKey};
    pub use lash_core::plugin::prompt::{
        AdmittedCallLoadError, LoadedPromptSnapshot, PromptCompositionError, PromptRenderSite,
    };
    pub use lash_core::prompt_sections::{
        AppliedPromptWrap, PROMPT_KEY_MAX_BYTES, PlacementSource, PromptKeyError, PromptLimits,
        PromptPlacement, PromptPlan, PromptPlanError, PromptPurpose, PromptSectionId,
        PromptSectionKey, PromptSectionPlacement, PromptSnapshot, PromptSnapshotVersion,
        PromptTextRef, PromptWrapId, PromptWrapKey, ProviderBodyError, RecordedSectionText,
        RenderedPromptSection, ResolvedPromptPlan, ResolvedPromptSection, ResolvedPromptWrap,
    };
}

/// Plugin contracts, manifests, and operation types.
///
/// Compare [`SessionReadView::current_frame()`](crate::persistence::SessionReadView::current_frame)
/// in your before-turn hook to detect compaction or a frame switch.
///
/// A plugin must honour the host's telemetry content policy for content it
/// puts in custom trace payloads. Lash applies that policy to built-in
/// telemetry and passes plugin-authored custom payloads without inspecting
/// or classifying them. Read the policy in your factory's context:
///
/// ```
/// use lash::plugins::PluginSessionContext;
///
/// fn custom_payload(ctx: &PluginSessionContext, text: &str) -> serde_json::Value {
///     serde_json::json!({
///         "text": ctx.telemetry_content().capture(|| text.to_owned()),
///     })
/// }
/// ```
///
/// The accessor follows the current deployment's policy, including on
/// reopen. Hosts can drop or redact custom records before export by wrapping
/// their [`TraceSink`](crate::tracing::TraceSink) and installing it through
/// [`LashCoreBuilder::trace_sink`](crate::core::LashCoreBuilder::trace_sink).
pub mod plugins {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::ConfigRegistry;
    pub use lash_core::plugin::{
        AssistantProseProjectorPlugin, AssistantStreamFinishedHook, DecidedContextPressure,
        PluginFuture, PluginLifecycleEventHook, PluginLifecycleFuture, ResolvedToolSurface,
        ToolCatalogContributor, ToolPresentationArtifacts, ToolPresentationFacts,
        ToolPresentationInput, ToolPresentationStep, TranscriptDecoderPlugin,
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
    pub use lash_core::{
        ArtifactReferrerPorts, CommandJournalGuard, CommandReplayKey, DeclaredModuleArtifact,
        DefinitionAcquisition, RecordedKeyFence, ReferrerAcquisition, RefusedWriteRange,
        ResolvedProcessDefinition, ServedOnlyRange, WeakProcessEngineRegistry,
    };
    pub use lash_core_store::session_identity::FrameNodeIdError;
    pub use lash_core_worker::execution::runtime::ProcessExecutionEnvLoadError;
    pub use lash_protocol_standard::BatchSugar;
    pub use lash_sansio::{
        AttachmentMaterializationNotice, CheckpointResumeAction, DegradedBinding, DriverAction,
        DriverContextView, EffectId, ExpandedRow, ExpandedWrapper, ModelToolCalls, ModelToolReturn,
        PendingWork, ProjectorContext, ResponseToolCalls, SessionStreamEvent, StreamMessageKind,
        ToolCatalogBuildError, ToolContractResolver, ToolExpansionPlan, TurnMachineConfig,
        TurnProtocol, UndecodableDriverState, UnitTurnProtocol, WriterFormats,
    };
    /// The protocol-generic forms [`TurnDriverConfig`] and
    /// [`TurnDriverPreamble`] specialize to the host's turn protocol.
    pub use lash_sansio::{
        TurnDriverConfig as GenericTurnDriverConfig,
        TurnDriverPreamble as GenericTurnDriverPreamble,
    };

    /// Host-specialized driver configuration required by every [`TurnDriverPreamble`].
    pub use lash_core::TurnDriverConfig;
    /// A turn's prepared request history, as its prelude records it.
    pub use lash_core::facade_support::PreparedContext;
    /// Durable session-lifecycle operations a hook context carries, alongside
    /// [`SessionStateService`] and [`SessionGraphService`]; runtime-implemented.
    pub use lash_core::facade_support::SessionLifecycleService;
    /// The schema crate config wire types derive with, so an owner's
    /// namespace, commands and refusals generate the schemas the config
    /// command catalog publishes: derive
    /// `#[derive(lash::plugins::schemars::JsonSchema)]` with
    /// `#[schemars(crate = "lash::plugins::schemars")]`.
    pub use lash_core::facade_support::schemars;
    pub use lash_core::plugin::prompt::PromptRenderPoolConfig;
    /// Prompt sections (ADR 0133): a plugin registers keyed sections and
    /// trusted wrappers through `reg.prompt()`. A renderer reads only a
    /// [`PromptInput`]: the call's committed cut, its own frozen namespace
    /// and its admitted config. The host's plan and the recorded snapshots
    /// are in [`prompt`](crate::prompt).
    pub use lash_core::plugin::prompt::{
        CommittedPluginNamespace, OfferedTools, ProjectedHistoryStats, PromptCall,
        PromptFamilySection, PromptInput, PromptModel, PromptRegistrations, PromptRenderError,
        PromptSection, PromptSectionFamilySpec, PromptSectionSource, PromptSectionSpec,
        PromptSectionWrap, PromptWrapSpec, PromptWrapTarget, SectionText,
    };
    /// A session's registered sections, families and wrappers, read back
    /// and previewed against a plan, and the facts a protocol states for its
    /// own sections. Only the runtime builds a call's cut and composes it, at
    /// the call's admission; a test composes through
    /// `lash::testing::prompt`.
    pub use lash_core::plugin::prompt::{
        PromptCatalog, PromptSectionFamilyInfo, PromptSectionInfo, ProtocolPromptFacts,
    };
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
        CheckpointHook, CheckpointHookContext, CompactionContext, ContextCompaction,
        ContextCompactor, ContextError, ContextPressureContext, ContextPressureDecision,
        ContextPressureHook, HookKey, PluginExtensionContribution, PluginRecordContribution,
        PluginSessionMaterialization, PluginSpecBuilder, PluginTraceEmitter, SessionContributions,
        StaticPluginFactory, ToolCatalogContext, ToolMembershipContribution,
        ToolPresentationPresenter, ToolResultProjectionContext, TurnContributions, TurnHookReport,
    };
    /// Protocol and process-engine contracts, including their complete runtime-owned state closure.
    pub use lash_core::plugin::{
        AfterTurnDecisions, CheckpointApplication, CheckpointComponentKey, CodeExecutionOutcome,
        CodeExecutorPlugin, ExecutionLeafName, ExecutionStateCapture, HydratedExecutionState,
        InvalidExecutionLeafName, LeafChange, NamespaceEntry, NamespaceValues, PluginAbort,
        PluginNamespaceState, PluginSessionMaterializationRequest, PluginSessionRequest,
        PluginState, PluginStateEffect, PluginTransitionBase, PluginTransitionId,
        PluginTransitionRecord, PluginTransitionRequest, ProtocolBeforeLlmCallContext,
        ProtocolDriverPlugin, ProtocolLlmCallAction, ProtocolSessionContext, ProtocolSessionPlugin,
        ProtocolSessionRestoreView, RecordedCallbackPhase, RecordedTurnContribution,
        SessionAuthorityContext, TurnPreparation,
    };
    /// The attachment-omission history policy (ADR 0133): a plugin names the
    /// attachments of a turn's projected history its request omits, and core
    /// omits them with one placeholder. It cannot add text or touch history.
    pub use lash_core::plugin::{
        AttachmentOmissionContext, AttachmentOmissionPolicy, HistoryPartId,
        OMITTED_ATTACHMENT_PLACEHOLDER,
    };
    /// What a plugin factory declares about itself: its behaviour revision
    /// and the formats it reads and writes. The build generation is computed
    /// from every registered factory's declaration, in hook order.
    pub use lash_core::plugin::{
        BehaviorRevision, FormatVersion, PluginCallbackIdentity, PluginComposition,
        PluginDeclaration, PluginDeclarationError, PluginDefinition, PluginExecutionRefusal,
        PluginId, PluginMetadata, PluginRevision,
    };
    /// The registration groups [`PluginRegistrar`]'s accessors return
    /// (`reg.tools()`, `reg.session()`, ...), nameable so a helper can take
    /// one as a parameter.
    pub use lash_core::plugin::{
        ContextRegistrations, ExecutionRegistrations, OutputRegistrations,
        PluginOperationRegistrations, ProtocolRegistrations, SessionRegistrations,
        ToolCallRegistrations, ToolCatalogRegistrations, ToolRegistrations,
        ToolResultRegistrations, TurnRegistrations,
    };
    /// Host-mediated JSON state: a plugin reads its namespace through a
    /// read-only [`PluginStateView`] and changes it only by returning
    /// [`StateCommands`] from a tool body or a before-turn, after-turn,
    /// checkpoint or after-tool callback. Their recorded resolution is
    /// published once durable and persisted at boundary commits.
    pub use lash_core::plugin::{
        FormatNamespace, FormatRefusal, FrontierRefusal, FrontierStep, HookCause, HookOccurrence,
        KeyRejection, NamespaceFrontierRefusal, PluginConfigNamespace, PluginStateError,
        PluginStateView, PublicationOrdinal, ResolvedStateChange, SessionReadyContext,
        StateCommand, StateCommandOrigin, StateCommandRefusal, StateCommands, StateFork,
        StateFrontier, StateReducer, StateReduction, StateResolution, StateResolutionOutcome,
    };
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
    pub use lash_core::plugin::{
        PluginCommand, PluginCommandContext, PluginFailureClass, PluginFailureOrigin,
        PluginHookFailure, PluginOperation, PluginOperationDef, PluginOperationFailure,
        PluginOperationInvokeError, PluginOperationKind, PluginOperationOutcome,
        PluginOperationReceipt, PluginOwned, PluginQuery, PluginQueryContext,
        PluginRuntimeDirective, PluginTask, PluginTaskContext, ProcessReadService, SessionParam,
        SessionReadService,
    };
    /// What [`PluginFactory::process_engine_contributions`] is handed: a host
    /// factory that wraps another (the RLM factory, say) forwards it so the
    /// wrapped factory's process engines are still contributed (FIG-4373).
    pub use lash_core::plugin::{PluginExecutionTrace, ProcessEngineContributionContext};
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
        RealizationReceipt, RunCutRefusal, SingletonAttempt, SingletonBodyOutcome,
        SingletonCapture, SingletonPreparedRequest, SingletonPresentationError, SingletonRunError,
        SingletonToolCall, SingletonToolHandlers, StartLaunch,
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
        MaterialHolder, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef,
        MaterialRefusal, MaterialRetentionError, MaterialRole, OperationRun, RetainedBundle,
        RunInputKind,
    };
    /// A session's recorded plugin configuration and the owner contract that
    /// creates and changes it (FIG-4379): each installed plugin registers the
    /// owner of its namespace and the typed config commands that change it,
    /// and reads the recorded value on every open and in every scoped hook.
    pub use lash_core::{
        AdmittedPluginConfig, CandidateFacts, ConfigCommand, ConfigFault, ConfigOwner,
        ConfigRegistrar, ConfigRegistrationError, ConfigWire, CreationConfigError, NoRunOptions,
        OwnerChange, PluginConfig, RecordedNamespaceCorrupt, RenderFault,
    };
    /// Protocol-driver and process-engine inputs that core owns independently of plugin storage.
    pub use lash_core::{
        AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, HostTurnProtocol,
        InspectedProcessDefinition, ProcessDocument, ProcessDocumentProvider, ProcessDocumentRead,
        ProcessDocumentRefRead, ProcessEngine, ProcessEngineAdmission, ProcessEngineRegistration,
        ProcessExecutionDocumentRead, ProcessInfraError, ProcessRunOutcome, ProtocolBuildInput,
        ProtocolDriverState, ProtocolTurnOptionsError, TurnDriverPreamble,
    };
    /// The session services a hook context hands a plugin: read-through state
    /// access ([`SessionStateService`]) and durable graph appends
    /// ([`SessionGraphService`]), plus the append request/result vocabulary.
    /// Both are runtime-implemented — a plugin receives one, never writes one.
    pub use lash_core::{
        AppendSessionNodesOutcome, AppendSessionNodesRequest, PluginExtensions, SessionAppendNode,
        SessionGraphService, SessionStateService, SessionToolAccess, SessionToolAccessError,
    };
    /// Code-executor request, response, and runtime capability context.
    pub use lash_core::{
        CellFailure, CellFailureKind, ExecRequest, ExecResponse, RuntimeExecutionContext,
    };
    pub use lash_core::{CompletedToolCall, PluginOptions};
    /// A host process engine's state machine (ADR 0132 §6): the state it
    /// keeps, the events lash delivers to `advance`, and the action it
    /// answers with.
    pub use lash_core::{
        EngineAction, EngineEvent, EngineState, EngineStateFormat, EngineStepKind,
        EngineStepRefusal, EngineStepRun, EngineSteps, KeyName, Material, NamesMaterial,
        SettledOutput, SettledOutputRefusal, StepEffectSite, StepName, StepRequest,
    };
    /// Executable identity and terminal rendering returned by protocol integrators.
    pub use lash_core::{ExecutableGeneration, RecordedRender};
    pub use lash_core::{
        PluginError, PluginErrorClass, PluginMessage, PluginRuntimeEvent, ToolCatalog,
        ToolCheckConflict, ToolCheckPhase, ToolCheckReply, ToolCheckVerdictKind,
        facade_support::PluginFactory, facade_support::PluginHost, facade_support::PluginRegistrar,
        facade_support::PluginSession, facade_support::PluginSessionContext,
        facade_support::PluginSpec, facade_support::PluginSpecFactory,
        facade_support::SessionPlugin, facade_support::ToolCatalogContribution,
        facade_support::TurnHookContext, facade_support::TurnResultHookContext,
    };
    /// Lifecycle observation: what a `reg.session().on_event(..)` hook receives
    /// when a turn is finalized, a session is restored, or its configuration
    /// changes, and the contexts each event carries.
    ///
    /// Hosts that need durable post-commit work keep a durable progress cursor
    /// and page through [`DurableSession::history`](crate::DurableSession::history)
    /// across frame and fork boundaries. Advance progress after successful,
    /// idempotent output and schedule reconciliation independently of hooks.
    pub use lash_core::{
        facade_support::PluginLifecycleEvent, facade_support::SessionConfigChangedContext,
    };
    pub use lash_plugin_standard_compaction::{
        StandardCompactionConfig, StandardCompactionPluginFactory,
    };
    pub use lash_protocol_standard::{StandardProtocolConfig, StandardProtocolPluginFactory};
    /// Default chat projector installed by [`TurnDriverConfig::chat`].
    pub use lash_sansio::ChatContextProjector;
    pub use lash_sansio::CompletedToolCall as GenericCompletedToolCall;
    /// Projection contract stored by [`TurnDriverConfig`] when a protocol supplies a custom
    /// context projector.
    pub use lash_sansio::ContextProjector;
    /// Sans-I/O protocol handle accepted by [`TurnDriverConfig::chat`]; custom host drivers use
    /// [`HostTurnProtocol`] as its protocol parameter.
    pub use lash_sansio::ProtocolDriverHandle;
    /// Model-facing tool declaration carried by [`TurnDriverPreamble::tool_specs`].
    pub use lash_sansio::llm::types::LlmToolSpec;
}

/// Protocol message and content types.
pub mod messages {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_sansio::ModelToolReturnPart;

    pub use lash_core::session_graph::SharedJsonValue;
    pub use lash_core::{
        InternalPartKind, Message, MessageOrigin, MessageRole, Part, PartKind, TurnOutputSource,
        TurnReply, facade_support::MessageSequence, session_model::message::PartAttachment,
    };
    /// JSON map in integrator signatures, without a second direct dependency.
    pub use serde_json::Map as JsonMap;
    /// JSON value in integrator signatures, without a second direct dependency.
    pub use serde_json::Value as JsonValue;
}

/// Attachment identity, metadata, acceptance and transient delivery values.
/// Hosts upload through [`LashSession::put_attachment`](crate::LashSession::put_attachment)
/// before sending an [`InputItem::attachment`](crate::InputItem::attachment).
/// The durable ref also names attachments in direct model calls and tool results;
/// the host store chooses a delivery form for each provider attempt.
/// Admission and journal records hold request templates, never delivered values.
/// Provider text can echo a URL or file id; hosts should keep signed URL lifetimes short.
///
/// Where the bytes live is a persistence concern:
/// [`persistence`] carries the store trait, its errors, and reclamation.
pub mod attachments {
    /// The canonical content address of a byte payload, so a host
    /// [`AttachmentStore`](crate::persistence::AttachmentStore) can key stored
    /// bytes by their content id.
    pub use lash_core::attachments::content_id;
    pub use lash_core::{
        AttachmentCreateMeta, AttachmentId, AttachmentRef, AttachmentTypeMetadata, MediaType,
    };
    /// Output kept out of session history (FIG-1643): the byte policy a host
    /// states in [`DataRetention::attachments`](crate::DataRetention::attachments),
    /// the witness and reference history keeps in an oversized output's
    /// place, and a value that is one or the other.
    pub use lash_core::{OutputRetentionPolicy, OutputValue, RetainedOutput};
    pub use lash_sansio::llm::attachment_delivery::{
        AttachmentPosition, Delivery, DeliveryContext, DeliveryFetchHorizon, DeliveryForms,
        DeliveryLimits, DeliverySecret, ProviderAccepts, ProviderFileScope,
    };
    pub use lash_sansio::{InvalidAttachmentId, InvalidMediaType};
}

/// Secret-handling values for host-owned configuration structs.
pub mod secrets {
    /// A string wrapper whose `Debug`/`Display` render `[redacted]`, so a
    /// provider key held in a host config struct cannot leak through logs.
    pub use lash_sansio::Redacted;
}

/// Host process observation, definitions and lifecycle commands.
///
/// Hosts and plugins read [`ObservedProcess`]. Raw rows and their store ports
/// live in [`crate::persistence`].
pub mod process {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::{
        ConsumerHold, ProcessDefinitionStoredError, ProcessSpawnProvenance, ProcessStartDeclaration,
    };
    pub use lash_core_store::effect_opener::EffectOpenerError;
    pub use lash_sansio::HandleTarget;

    pub use crate::admin::SessionProcessAdmin;
    pub use crate::artifacts::{HostArtifactPin, HostArtifacts, ProcessDefinitions};
    pub use crate::process_admin::Processes;
    pub use crate::process_feed::{
        ObservableProcess, ProcessObservationEventId, ProcessObservationStream,
        ProcessObservationStreamItem,
    };
    pub use crate::process_history::{ProcessEventsRead, ProcessHistoryContinuation};
    /// The origin of a lifecycle cancellation submitted to a registry.
    pub use lash_core::CancelOrigin;
    pub use lash_core::SessionTurnOutcome;
    /// Registry admission receipts and lifecycle write outcomes.
    pub use lash_core::runtime::StoreRealization;
    /// Process-registry and event types that complete the store and engine signature closure.
    pub use lash_core::runtime::{
        ParentEndPlan, ProcessOutcome, ProcessParkReason, ProcessParkState, ProcessTombstone,
        ProcessWaits, WaitKind, WaitState,
    };
    /// The one lifecycle state a process record holds, and the outcome a
    /// terminal one ends in.
    pub use lash_core::runtime::{
        ProcessLifecycleState, ProcessOutcomeNotRetained, ProcessTerminal,
    };
    pub use lash_core::{
        AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry, CausalRef,
        DeclaredProcessIdentity, HandleId, InvalidProcessDefinitionId, InvalidStartKey, Lifetime,
        LifetimeDecision, LifetimePolicy, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE,
        MAX_PROCESS_ROSTER_PAGE_SIZE, NoProcessWork, PROCESS_EFFECT_OCCURRENCE_CAP,
        PROCESS_EFFECT_OMISSIONS_EVENT_TYPE, PROCESS_EFFECT_OUTCOME_EVENT_TYPE, ProcessAwaitOutput,
        ProcessChangeBounds, ProcessChangeCursor, ProcessDefinition, ProcessDefinitionDraft,
        ProcessDefinitionDraftError, ProcessDefinitionId, ProcessDefinitionRef,
        ProcessDefinitionRefusal, ProcessDefinitionResolution, ProcessDefinitionTarget,
        ProcessDefinitionValue, ProcessEffectNodeReport, ProcessEffectOccurrence,
        ProcessEffectOmissions, ProcessEffectOmittedCounts, ProcessEffectOutcomeClass,
        ProcessEffectReport, ProcessEffectReportError, ProcessEngineKind, ProcessEvent,
        ProcessEventAppendReceipt, ProcessEventHistoryRetention, ProcessEventKind,
        ProcessEventLite, ProcessEventPage, ProcessEventPageEvents, ProcessEventPageMore,
        ProcessEventQueryMode, ProcessEventReadOutcome, ProcessEventRelease,
        ProcessExecutionContext, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
        ProcessExternalRef, ProcessIdentity, ProcessInput, ProcessLifecycleFact, ProcessLineage,
        ProcessListMode, ProcessObserverBy, ProcessOriginator, ProcessOriginatorFilter,
        ProcessProvenance, ProcessPruneReport, ProcessRegistration, ProcessRegistrationOutcome,
        ProcessRegistryCursor, ProcessResumeRefusal, ProcessRosterCursor, ProcessService,
        ProcessSessionDeleteReport, ProcessSignature, ProcessStartOptions,
        ProcessStartRegistration, ProcessStartRequest, ProcessStartTarget, ProcessStarted,
        ProcessStatus, ProcessStatusFilter, ProcessTerminalWait, ProcessWorkSubstrate,
        ProcessWorkWiring, ProjectionWatermark, RetiredProcessStatus, ScopeGrant, ScopeId,
        ScopeRef, ScopeStorageError, SessionScope, StagedPluginState, StartCx, StartCxError,
        StartKey, StoreLocalEffect, StoreLocalRows, TerminalProcessStatus, WatchedRegistry,
        facade_support::CanonicalProcessEventAppend, facade_support::ObservedProcess,
        facade_support::ObservedProcessChange, facade_support::ObservedProcessEvent,
        facade_support::ObservedProcessEventLite, facade_support::ObservedProcessEventPage,
        facade_support::ObservedProcessEventReadOutcome, facade_support::ObservedWorkItem,
        facade_support::ObservedWorkItemState, facade_support::ProcessChangeHub,
        facade_support::ProcessChangeSubscription, facade_support::ProcessEventSink,
        facade_support::ProcessRosterPage, facade_support::ProcessRuntimeHost,
        facade_support::ProcessToolVisibilityFilter, facade_support::ProcessWorkObserver,
        facade_support::ProcessWorkSnapshot, facade_support::SessionScopeId,
        facade_support::watch_process_registry, lifetime,
    };
    pub use lash_core::{ArgsMismatch, ArgsMode};
    /// Process observation's contract: the snapshot, cursor, stream events,
    /// typed gaps and the replay store behind a process feed.
    pub use lash_core::{
        InMemoryProcessReplayStore, InMemoryProcessReplayStoreConfig, LanguageExecutionObservation,
        ParsedProcessObservationCursor, ProcessDocumentIdentity, ProcessEffectCoverage,
        ProcessEffectEvidence, ProcessEffectGapReason, ProcessObservation,
        ProcessObservationCursor, ProcessObservationCursorError, ProcessObservationEnd,
        ProcessObservationEvent, ProcessObservationEventPayload, ProcessObservationGapCause,
        ProcessObservationIdentity, ProcessObservationReplacement, ProcessReadView,
        ProcessReplayEventDraft, ProcessReplayGapReason, ProcessReplayOutcome,
        ProcessReplayPublishLimits, ProcessReplayStore, ProcessReplayStoreError,
        ProcessReplaySubscribeOutcome, ProcessReplaySubscription, ProcessSequence,
        RetainedProcessView, StepBodyStartedObservation,
    };
    #[cfg(feature = "rlm")]
    pub use lash_vm_runtime::{LASH_VM_ENGINE_KIND, LashVmProcessInput};
}

/// Store-author durability configuration and backend contracts.
///
/// These ports may consume stored [`crate::persistence::ProcessRecord`] rows.
/// Host observations use [`crate::process::ObservedProcess`].
pub mod durability {
    pub use lash_core::{EffectAttempt, RecordedEffectExecution};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::PreparedProcessRegistration;
    pub use lash_core::RecordedKeys;
    pub use lash_core_store::effect_opener::EffectOpener;
    pub use lash_sansio::{CancelRequest, ToolCallAdmission};

    /// Effect-host inputs, replay projections, and local execution capabilities.
    pub use lash_core::runtime::{
        CanonicalRuntimeEffectEnvelope, EffectJournalIdentity, EffectJournalRetirement,
        EffectRetirementGate, HostStartAdmission, ProcessLocalExecution, ProcessOutcomeObserver,
        ProcessTurnCancellation, RuntimeEffectReplayTrace, RuntimeReplay, RuntimeReplayAttribution,
        RuntimeSleepOptions, RuntimeSubject, SegmentProgress, ToolAttemptLaunch,
        validate_replayed_effect_envelope,
    };
    /// Durable group and journal values returned by effect-host implementors.
    pub use lash_core::runtime::{JournalReplay, ProcessDriveStep, RecordedKeyRange};
    pub use lash_core::{
        facade_support::DataRetentionConfig, facade_support::RuntimeEnvironment,
        facade_support::RuntimeHostConfig,
    };
    pub use lash_core_worker::{DurableProcessWorker, DurableProcessWorkerConfig};
    /// The session work a node runs `SessionTurn` processes with, which
    /// [`DurableProcessWorker`] implements (FIG-5208).
    pub use lash_core_worker::{SessionTurnCancel, SessionTurnMail, SessionTurns};
}

/// Runtime events, errors, and execution controls.
pub mod runtime {
    pub use lash_core::IngressReservedSourceKeyRefusal;
    pub use lash_core::engine::{ObservationSink, ObservedEvent, ReplayKey, ShiftObservation};
    pub use lash_core::facade_support::TraceBoundaryReceipt;
    pub use lash_core::runtime::{AttemptStreamRecorder, DeclaredStartPhase, StartCancelDecision};
    #[cfg(any(test, feature = "testing"))]
    pub use lash_core::{ObservationSource, work_with_observations};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::engine::{EngineRefusal, RefusalClass};
    pub use lash_core::runtime::ProcessDefinitionLocalExecution;
    pub use lash_core::runtime::SessionTurnAdmission;
    pub use lash_core::runtime::{CompactionBase, PresentationBinding, ToolPresentation};
    pub use lash_core::tool_dispatch::ToolAttemptLineage;
    /// The cancellation policy pinned in a Run-owned source descriptor.
    pub use lash_core::tool_run::ExternalCancelPolicy;

    pub use lash_core::ServedOnly;
    pub use lash_core::{ConfigResolution, ConfigResolutionDecision};
    pub use lash_core_store::runtime_error::EffectErrorJournalPolicy;
    pub use lash_core_store::turn_input_vocabulary::RunDefinitions;
    pub use lash_sansio::sansio::{
        ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure,
        ExecutionEnvironmentSyncFailureKind,
    };
    pub use lash_sansio::{CheckpointDelivery, EffectIdentityError};

    /// The one effect context of an actor's activation (ADR 0132 §1).
    pub use lash_core::ActorContext;
    /// The lazy binding of a recorded model that
    /// [`RuntimeEffectLocalExecutor::direct`] takes: bound only when an
    /// unjournaled completion's body runs.
    pub use lash_core::LlmProfileBinding;
    /// Structured cause carried by a [`RuntimeError`], so a host distinguishes
    /// an expected retirement (a deleted session) from a real fault.
    pub use lash_core::RuntimeErrorCause;
    /// The session-state generations an admission refused, carried in-process
    /// on the error a refused call returns (FIG-3619).
    pub use lash_core::SessionStateVersionRefusal;
    /// The record kind and diagnostic carried by a stored-data corruption cause.
    pub use lash_core::StoredDataCorruption;
    /// How a failed turn settles (FIG-3575): an outcome is recorded, a live
    /// fault aborts. A host minting a foreign error code chooses its class.
    pub use lash_core::TurnFailureCause;
    /// Wall-clock milliseconds since the Unix epoch, as the runtime stamps its
    /// own process records. A host that mints a record the runtime will compare
    /// against uses the same reading rather than its own.
    pub use lash_core::runtime::current_epoch_ms;
    pub use lash_core::runtime::{
        AdmittedDirectSend, AdmittedScope, AssembledTurn, AssistantResponseHookEvents,
        AssistantResponsePlan, AssistantStreamHookState, CheckpointAdmittedSet,
        DirectCompletionClient, EffectAddress, EmbeddedRuntimeHost, EventSink, ExecutionScope,
        LlmRequestSpec, LlmStreamRecord, NoopEventSink, NoopTurnActivitySink, ProcessCommand,
        ProcessEffectOutcome, ProcessListSelection, RunAggregateWakePolicy, RuntimeAttribution,
        RuntimeControlConfig, RuntimeDurabilityConfig, RuntimeEffectCommand,
        RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectInvocation,
        RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
        RuntimeEffectReplayMismatchReport, RuntimeEnvironmentBuilder, RuntimeError,
        RuntimeErrorCode, RuntimeInvocation, RuntimeProviderConfig, SleepSpec, TraceEmitter,
        TraceRuntime, TurnCancelWait, TurnContext, TurnPrelude, TurnPreludeRef, WorkCadenceError,
    };
    /// The host clock a [`Backend`](crate::Backend) is opened on, used
    /// for runtime sleeps and store timestamps. [`SystemClock`] is the
    /// wall-clock default; tests open a backend on their own to make expiry
    /// deterministic.
    pub use lash_core::{Clock, ClockWallTime, facade_support::SystemClock};
    /// The durable session extension and turn options exposed to runtime integrators.
    pub use lash_core::{
        ProtocolSessionExtension, ProtocolTurnOptions, SessionPolicy, SessionSnapshot,
        facade_support::SessionHandle,
    };
}

/// Structural admission of host JSON before DTO allocation. The budget is
/// independent of fixed wire-validity limits and Serde's recursion guard.
pub mod json_decode {
    pub use lash_sansio::json_decode::{JsonDecodeError, JsonDecodeLimits, JsonDecodeUsage};
}

/// Trace context, events, and sink configuration.
pub mod tracing;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// JSON-schema contracts, projection policies, and provider dialect
/// projection. This is the vocabulary a tool schema and a provider request
/// share: [`SchemaContract`] declares what a schema promises, and
/// [`project_for_dialect`] renders it for one provider dialect.
pub mod schema {
    pub use lash_sansio::schema_contract::*;
}

/// SQLite durable store backend. Enable with `features = ["sqlite"]`.
#[cfg(feature = "sqlite")]
pub mod sqlite {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core_store::process_identity::ProcessIdMint;

    pub use lash_sqlite_store::*;
}

/// PostgreSQL durable store backend.
#[cfg(feature = "postgres")]
pub mod postgres {
    pub use lash_postgres_store::host::*;
    pub use lash_postgres_store::*;

    /// A host's whole PostgreSQL wiring from one validated configuration
    /// (FIG-5240).
    pub use crate::postgres_host::{PostgresHost, PostgresHostConnectError};
    /// The live replay store every replica of a host shares through one
    /// PostgreSQL database (FIG-5101).
    pub use crate::postgres_live_replay::{
        PostgresLiveReplayError, PostgresLiveReplaySchemaFinding, PostgresLiveReplaySchemaReport,
        PostgresLiveReplayStore,
    };
    /// The process replay store every replica of a host shares through one
    /// PostgreSQL database, apart from the live replay store (FIG-5568).
    pub use crate::postgres_process_replay::{
        PostgresProcessReplayError, PostgresProcessReplaySchemaFinding,
        PostgresProcessReplaySchemaReport, PostgresProcessReplayStore,
    };
}

/// S3 attachment store backend.
#[cfg(feature = "s3")]
pub mod s3 {
    pub use lash_s3_store::*;
}

/// OpenAI model provider. Enable with `features = ["openai"]`.
#[cfg(feature = "openai")]
pub mod openai {
    pub use lash_provider_openai::*;
}

/// Anthropic model provider. Enable with `features = ["anthropic"]`.
#[cfg(feature = "anthropic")]
pub mod anthropic {
    pub use lash_provider_anthropic::*;
}

/// Google model provider.
#[cfg(feature = "google")]
pub mod google {
    pub use lash_provider_google::*;
}

/// Model Context Protocol tool plugin.
#[cfg(feature = "mcp")]
pub mod mcp {
    pub use lash_plugin_mcp::*;
}

/// First-party process-control tools: `start_process`, `get_process_definition`, `list_process_handles`,
/// `await_process` and `cancel_process`.
///
/// A host installs [`SessionProcessAdminPluginFactory`] with
/// [`LashCoreBuilder::plugin`](crate::LashCoreBuilder::plugin) instead of
/// declaring these tools itself. Its [`Lifetime`](crate::process::Lifetime)
/// policy, for example [`lifetime::session_or_starter`](crate::process::lifetime::session_or_starter),
/// decides the lifetime of every process a model's `start_process` declares.
pub mod process_controls {
    pub use lash_plugin_process_controls::{
        ProcessControlTool, SessionProcessAdminPluginFactory, process_tool_definition,
    };
}

/// TypeScript process dialect.
#[cfg(feature = "typescript")]
pub mod typescript {
    pub use lash_typescript::*;
}

/// HTTP transport for provider and ingress traffic.
#[cfg(feature = "http-transport")]
pub mod http_transport {
    pub use lash_http_transport::*;
}

/// Model-provider configuration and request types.
pub mod provider {
    /// The host-owned credential seam: lash asks a host's [`TokenSource`]
    /// for a [`ProviderToken`] before every model-call attempt and runs no
    /// OAuth of its own. A fixed API key is a `ProviderToken`.
    pub use lash_core::provider::{
        ProviderToken, TokenError, TokenErrorKind, TokenRequest, TokenRequestReason, TokenSource,
    };
    #[cfg(any(feature = "anthropic", feature = "google", feature = "openai"))]
    pub use lash_llm_transport::{ExtraHeaders, TokenPolicy};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::llm::transport::HttpFailureContext;
    /// Read the request a canonical body says, for an in-process model that
    /// decides from what a call asks.
    pub use lash_core::provider::canonical_request;
    pub use lash_core::provider::{
        AttachmentDeliveryError, LiveCallHorizon, NoSlotDeliveries, SlotDeliveries,
    };
    /// The admitted request template and the transient body filled for one attempt
    /// (ADR 0133 §6). Only literals, refs, acceptance and codecs are recorded.
    pub use lash_sansio::llm::types::{
        AttachmentSlot, LiveRequestBody, RecordedRequestTemplate, RequestSegment,
        RequestTemplateBuilder, SlotCodec, TemplateError, TemplateJson, TransientJson,
    };
    pub use lash_sansio::llm::types::{
        LlmProviderTraceDirection, LlmProviderTraceEvent, LlmProviderTraceSender,
        ProviderReasoningRetentionSupport,
    };
    /// What a provider's `send` reads its response under, beside the body:
    /// the call's scope, its recorded contract and the send's live senders.
    pub use lash_sansio::llm::types::{ResponseContext, ResponseContract, ToolCallContract};

    /// Typed provider-failure classification surfaced on
    /// [`TurnIssue`](crate::turn::TurnIssue) and session error envelopes.
    pub use lash_core::ProviderFailureKind;
    /// Why a host-supplied [`LlmProfileCapability`] rejected a reasoning-effort
    /// selection. The snake_case [`LlmProfileEffortValidationCategory`] codes are a
    /// stable contract a capability catalog can branch on.
    pub use lash_core::facade_support::LlmProfileEffortValidationCategory;
    pub use lash_core::llm::transport::TransportRetryVerdict;
    pub use lash_core::llm::types::{
        LlmContentBlock, LlmJsonSchema, LlmMessage, LlmOutputSpec, LlmRole, LlmToolChoice,
    };
    pub use lash_core::provider::LlmProfileEffortValidationError;
    /// Provider completion, caching, failure, retry, and rate-limiting contracts.
    /// Hosts meter spend by decorating the Provider seam (ADR 0127).
    pub use lash_core::provider::{
        CacheRetention, DefaultProviderFailureClassifier, LlmProfileRequestDefaults,
        ProviderCompletion, ProviderCompletionError, ProviderFailureClassifier,
        ProviderRateLimitPermit, ProviderRateLimitPolicy, ProviderRateLimiter, ProviderRateWindow,
        ProviderReliability, ProviderRetryPolicy, RouteBound, RouteBoundAboveBudget,
    };
    pub use lash_core::{
        AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
        AttachmentCapabilitySnapshot, CacheControlDialect, GoogleDialect, InstructionRole,
        LlmProfileCapability, OpenAiReasoningContext, ReasoningCapability, ReasoningEncoding,
        ReasoningIntent, ReasoningRetentionCapability, ReasoningRetentionPolicy,
        ReasoningRetentionSelection, ReasoningRetentionValidationCategory,
        ReasoningRetentionValidationError, ReasoningSelection, SamplingCapability,
        StreamTermination, facade_support::GenerationRetryGuarantee, facade_support::LlmTimeouts,
        facade_support::Provider, facade_support::ProviderComponents,
        facade_support::ProviderHandle, facade_support::ProviderOptions,
    };
    /// Request/response/error vocabulary of [`Provider::complete`],
    /// re-exported so hosts can implement provider decorators (admission
    /// gates, metrics taps) against the facade alone.
    pub use lash_core::{
        AttemptOutcome, AttemptUsageOutcome, ChargeSafetyDecision, ExecutionEvidence,
        ExecutionEvidenceCollectionInterruption, ExecutionEvidenceMergeError, LlmRequest,
        LlmRequestOwner, LlmRequestScope, LlmResponse, LlmStreamEvidence, LlmTurnScope,
        NormalizedError, ProtocolPosition, ProviderEndpointError, RetryClass, RetryDecision,
        RetryDeclineCause, RetryWait, facade_support::LlmTransportError,
    };
    /// The namespaced failure code carried on
    /// [`LlmTransportError`](facade_support::LlmTransportError) and attempt
    /// journals: `lash:` codes are workspace-authored, `provider:` codes came
    /// off the provider wire, and a host names its own vocabulary through
    /// [`HostNamespace`] plus [`FailureCode::host`] — a host namespace is
    /// first-class, never `provider:`. [`Namespace::host`] plus
    /// [`FailureCode::foreign`] remain for namespaces only known at runtime
    /// or decoded off the wire.
    pub use lash_core::{FailureCode, HostNamespace, InvalidNamespace, Namespace};
}

pub use crate::core::ForkRequest;
/// What a pin or a fork names, what a session retains, and a retained point
/// (FIG-4731).
pub use lash_core::{RetainedRevision, Retention, Target};

/// The typed, decoded committed history of a session (ADR 0129): facts a
/// host renders however it likes.
pub mod transcript {
    pub use lash_core::transcript::{
        CommittedTurn, CommittedTurnsPage, EntryId, EntryProvenance, SessionTranscript,
        SuppressionReason, ToolResultBlock, TranscriptBlock, TranscriptDecoders, TranscriptEntry,
        TranscriptItem, TranscriptMessage, TranscriptRole,
    };
    /// One executed code cell: the record its protocol committed, which a
    /// [`TranscriptItem::Cell`] returns, a code executor's response reports
    /// the prints and result of, and a completion activity carries.
    pub use lash_core::{CellPrint, CellRecord, CellResult};
}

/// Presentation cuts for runtime value replies and raw transcript errors.
pub use lash_core::session_model::RuntimeOutputCuts;
