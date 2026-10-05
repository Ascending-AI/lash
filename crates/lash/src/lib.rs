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
//! [`plugins`], [`observe`], [`triggers`], [`attachments`], ...) carries its own
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

// SDK fixture macros emit absolute `::restate_sdk` paths. Keep their private
// test bindings on the same SDK re-export as the facade's Restate adapter.
#[cfg(test)]
extern crate self as restate_sdk;
#[cfg(test)]
use lash_restate::restate_sdk::{context, discovery, endpoint, errors, ingress, service};

/// Administrative facade handles and operations.
pub mod admin;
mod artifacts;
mod change_page;
mod core;
pub use change_page::ChangePage;
mod durable_session;
mod error;
pub mod formats;
mod observation_feed;
mod parked_work;
mod parked_work_verbs;
pub use parked_work_verbs::{
    ControlIntentPage, ControlIntentQuery, ForkedTurn, ParkCancelled, ParkVerbRefused,
    RedriveAccepted, RunRedriveAccepted,
};
pub mod preflight;
pub(crate) mod process_admin;
mod process_lifecycle;
mod process_observation;
pub mod recoverable_chat;
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
/// ([`standard::SetStandardPrompt`], [`standard::SetStandardRender`],
/// `rlm::SetRlmPrompt` and `rlm::SetRlmRender`). The core has no prompt
/// command: a session's system prompt is its protocol plugin's recorded
/// config (FIG-4586).
pub mod config {
    pub use crate::admin::SessionConfigAdmin;
    pub use crate::admin::config_transactions::{ConfigSettlement, ConfigWrite};
    /// The owner of the core configuration the commands below change.
    pub use lash_core::CoreConfigOwner;
    pub use lash_core::plugin::config::core::{
        SetAttachmentAcceptance, SetAutonomy, SetChargeSafety, SetGeneration, SetLlmProfile,
        SetMaxToolCalls, SetNoProgressBudget, SetReasoning, SetToolAccess, SetTurnBudget,
    };
    pub use lash_core::{
        CORE_CONFIG_OWNER, ConfigCommandCatalog, ConfigCommandDescriptor, ConfigCommandEntry,
        ConfigRefusal, ConfigRefusalReason, ConfigSubmitError, ConfigTransaction,
        ConfigTransactionOutcome, ConfigValueRole, CoreConfig, CoreConfigRefusal, RefusalSite,
    };
}
/// The standard protocol's host surface: its creation options, its recorded
/// namespace, its prompt config and the commands that change it.
///
/// A session's system prompt is recorded config of its protocol plugin
/// (FIG-4586). A host states the standard protocol's at creation, in the
/// session spec's plugin options under [`STANDARD_PROTOCOL_PLUGIN_ID`]:
///
/// ```ignore
/// let spec = SessionSpec::new(profile_key, turn_budget, max_tool_calls).plugin(
///     lash::standard::STANDARD_PROTOCOL_PLUGIN_ID,
///     lash::standard::StandardTurnOptions {
///         prompt: Some(lash::standard::StandardPrompt {
///             intro: Some("You are the support desk's assistant.".to_string()),
///             ..Default::default()
///         }),
///         render: None,
///     },
/// )?;
/// ```
///
/// The spec is one session's ([`SessionCreation::spec`](crate::SessionCreation::spec)):
/// a core keeps no default, so a host that wants every session to state the
/// same prompt keeps the spec value and passes it to each creation.
/// After creation the prompt changes only through [`SetStandardPrompt`] and
/// [`SetStandardPromptContext`], which reach the next run. A run's options
/// are [`StandardRunOptions`]: they cannot state the prompt.
pub mod standard {
    pub use lash_protocol_standard::{
        STANDARD_PROTOCOL_PLUGIN_ID, SetStandardPrompt, SetStandardPromptContext,
        SetStandardRender, StandardConfigOwner, StandardConfigRefusal, StandardPrompt,
        StandardRecordedBehaviour, StandardRecordedConfig, StandardRenderRefusal,
        StandardRunOptions, StandardTurnOptions,
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
pub mod scenario_contracts;
/// Standard-lock poison recovery traits for application code.
pub mod sync {
    pub use lash_core::sync::*;
}
mod run_id;
mod send;
pub use run_id::RunId;
mod session;
mod session_binding;
mod support;
#[cfg(test)]
mod tests;
mod tool_catalog;
mod tool_intent_ingress;
/// Turn builders, streams, activities, and output types.
pub mod turn;
pub mod usage;

pub use crate::admin::{
    AdvancedToolAdmin, Completions, CoreTriggerAdmin, PluginOperations, SessionCommandAdmin,
    SessionCommandWithdrawal, SessionTriggerAdmin, ToolAdmin,
};
pub use crate::core::{
    DeploymentDrainStatus, GenerationDrainStatus, LashCore, LashCoreBuilder, SessionClosing,
    SessionDeleteCompletion, SessionDeleteFailure, SessionDeleteReport, SessionDeleteWait,
    SessionDeletion, drain_generation,
};
pub use crate::durable_session::DurableSession;
pub use crate::error::{EmbedError, Result, SendError};
pub use crate::parked_work::{
    ParkedKinds, ParkedWork, ParkedWorkCursor, ParkedWorkEvent, ParkedWorkEventPage,
    ParkedWorkEventsCursor, ParkedWorkPage, ParkedWorkQuery, ParkedWorkRecord, ParkedWorkRef,
    ParkedWorkReport,
};
pub use crate::send::{
    BatchInput, CancelBuilder, CancelReceipt, CancelTarget, ParkedTurn, RunHandle,
    SendBatchBuilder, SendBuilder, SendHandle, SendOutcome, StalledDelivery, TurnEvents,
    TurnStatus,
};
pub use crate::session::{
    LashSession, ObservableSession, ParkedSession, SessionBuilder, SessionCreation,
    SessionParkRefused,
};
pub use crate::tool_catalog::ToolCatalogMiss;
pub use crate::turn::{
    ReportSource, TurnActivityFanout, TurnOutput, TurnReport, message_role, message_text,
};
/// Re-exported so implementors of `#[async_trait]` facade traits (for example
/// [`tools::StaticToolExecute`]) apply the macro without carrying their own
/// `async-trait` dependency to keep version-aligned.
pub use lash_core::async_trait;
/// The one substrate a [`LashCore`] takes every persistence port and its
/// effect host from: one [`EffectEngine`] over one store set (ADR 0104).
/// [`LashCore::builder`] requires one: a `lash::restate::RestateEngine` over a
/// SQLite or PostgreSQL store set.
pub use lash_core::engine::BuildGeneration;
/// The slot an engine holds its build generation in, and the typed refusals
/// of reading it before a core bound it and of binding it twice (FIG-4744).
pub use lash_core::engine::{EngineGeneration, GenerationRebound, GenerationUnbound};
/// Store→engine delivery obligations (ADR 0109): what a stalled obligation
/// reports, and how this deployment competes for the recovery leader lease.
pub use lash_core::engine::{RecoveryLeaseConfig, RecoveryLeaseTimings, RecoveryPassBudget};
pub use lash_core::facade_support::{
    TurnCancelAffectedInput, TurnCancelAffectedWake, TurnCancelClosureAuthorization,
    TurnCancelClosureAuthorizationOutcome, TurnCancelClosureProposal, TurnCancelClosureSettlement,
    TurnCancelInputOutcome, TurnCancelIntentSnapshot, TurnCancelMode, TurnCancelRequestRecord,
    TurnCancelUndeliveredInputPolicy,
};
/// A plugin hook's [`HookKey`](plugins::HookKey) for a string literal,
/// validated at compile time.
pub use lash_core::hook_key;
pub use lash_core::runtime::ExternalCompletionError;
/// How a turn coalesces its stream deltas into frames for the live feed
/// (`LashCoreBuilder::delta_coalescing`).
pub use lash_core::runtime::{DeltaCoalescing, DeltaCoalescingError};
/// The immediate delivery verdict carried by a session deletion's wait.
pub use lash_core::shift::relay::RelayVerdict;
pub use lash_core::store::{
    DeliveryError, ObligationId, ObligationKey, ObligationKind, ObligationState, SessionFault,
    SessionFaultOrigin, SessionFaultRecord, StallReason, StalledObligation, UndecodableObligation,
    session_delete::SessionCleanup,
};
pub use lash_core::{
    AdmissionRefusal as IngressAdmissionRefusal, AwaitEventKey, AwaitEventWaitIdentity, BatchId,
    ChargeSafetyPolicy, ChargeSafetyRefusalEvidence, CommitBudget, CommitBudgetLimit, DrainMode,
    DrainModePolicy, EmptyLlmProfiles, FrameKey, InputId, InputItem, LlmCallRecord,
    LlmProfileConfig, LlmProfileKey, LlmProfileLimits, LlmProfileLimitsError, LlmProfileMetadata,
    LlmProfileMetadataBuilder, LlmProfileRegistry, LlmProfileUnavailable,
    LlmProfileUnavailableReason, LlmProfiles, MaxToolCalls, NoProgressBudget, NodeId,
    OmittedToolCalls, OutputTokenLimits, PendingTurnInput, PendingTurnInputCancelOutcome,
    PendingTurnInputCancelReceipt, PendingTurnInputCancelTarget, PendingTurnInputRead,
    PendingTurnInputReadStatus, PendingTurnInputSuffixCancelOutcome, ProcessId,
    QueuedDrainCandidate, QueuedDrainFamily, QueuedDrainPolicy, QueuedDrainRequest,
    QueuedDrainSelection, QueuedWorkBatchingConfig, ReasoningRefused, RecordedLlmProfile,
    RegisteredLlmProfile, RegistrationError, Resolution, ResolveOutcome, RuntimeOwner,
    SessionCreateRequest, SessionEntry, SessionError, SessionId, SessionListFilter,
    SessionRelationKind, SessionStartPoint, SessionView, ToolCallLimitExceeded, ToolCallLimitScope,
    TurnActivity, TurnActivityId, TurnBudget, TurnCause, TurnEvent, TurnFailureEvidence,
    TurnFailurePartialOutput, TurnFailureSettlement, TurnId, TurnInput, TurnInputApplication,
    UnstatedSessionConfig, facade_support::GenerationOverlay, facade_support::PluginStack,
    facade_support::SessionCommand, facade_support::SessionCommandReceipt,
    facade_support::SessionSpec, facade_support::SpecResolveError,
    facade_support::TurnActivitySink, facade_support::TurnAddress, facade_support::TurnAttach,
    facade_support::TurnCancelOutcome, facade_support::TurnCancelReceipt,
    facade_support::TurnCancelRequest, facade_support::TurnCancellationEvidence,
    facade_support::TurnExecutionMetrics, facade_support::TurnFinish,
    facade_support::TurnInputAcceptanceReceipt, facade_support::TurnOutcome,
    facade_support::TurnStop, facade_support::TurnTerminal, facade_support::TurnWorkDriver,
};
// A host's head write is a session command it submits, settles and may
// withdraw (FIG-4202): the settlement and the typed outcomes it carries.
pub use lash_core::runtime::{
    CompactContextOutcome, OpenAgentFrameCommandOutcome, PluginOperationCommandOutcome,
    PluginTaskCancelRequest, SessionCommandOutcome, SessionCommandSettlement,
};
pub use lash_core::store::SessionHeadOwner;
/// The one substrate a [`LashCore`] takes every persistence port and its
/// effect host from: one [`EffectEngine`] over one [`StoreSet`] (ADR 0104).
/// [`LashCore::builder`] requires one; the engine crates behind the
/// feature-gated modules (`restate`, `sqlite`, `postgres`) build one.
pub use lash_core::{Backend, EffectEngine, StoreBindingId, StoreSet};
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
pub use lash_core::SessionPluginInit;
pub use lash_core::runtime::ConfigTransactionSubmitError;
pub use lash_core::session_close::SessionCloseServices;
pub use lash_core::session_delete::SessionDeleteStores;
pub use lash_core::shift::relay::{DeliveryFailure, ObligationDelivery, ObligationRelay};
pub use lash_core::shift::{ObligationRelayUnavailable, RelayNeed};
pub use lash_core::store::{IngressTerminal, IngressTerminalCause};
pub use lash_core_store::build_generation::BuildGenerationParseError;
pub use lash_core_store::session_identity::{OpenAgentFrameOutcome, OpenAgentFrameRequest};
pub use lash_core_store::turn_input_vocabulary::ResolvedRun;
pub use lash_sansio::llm::types::{
    AttemptRecord, ChargeSafetyDenialReason, LlmCallId, StreamBlockKind,
};
pub use lash_sansio::{
    BlankIdentity, ErrorEnvelope, ExecCodeFailure, FrameKeyError, InvalidProcessId, LlmCallError,
    ToolCallPosition, ToolCallRoot,
};

/// `use lash::prelude::*;` brings in the daily core/session/turn vocabulary
/// without the lower-level integration types or domain modules also exposed
/// from the crate root.
pub mod prelude {
    pub use crate::{
        AdvancedToolAdmin, ChargeSafetyPolicy, CoreTriggerAdmin, DeploymentDrainStatus,
        DurableSession, EmbedError, InputItem, LashCore, LashCoreBuilder, LashSession,
        LlmProfileConfig, LlmProfileKey, LlmProfileLimits, LlmProfileLimitsError,
        LlmProfileMetadata, LlmProfileMetadataBuilder, LlmProfileRegistry, MaxToolCalls,
        NoProgressBudget, ObservableSession, ParkedSession, PendingTurnInputCancelOutcome,
        PluginOperations, PluginStack, RegisteredLlmProfile, Result, SendBuilder, SendHandle,
        SendOutcome, SessionBuilder, SessionCommand, SessionCommandAdmin, SessionCommandReceipt,
        SessionCreateRequest, SessionCreation, SessionDeleteReport, SessionDeletion, SessionEntry,
        SessionListFilter, SessionParkRefused, SessionRelationKind, SessionSpec, SessionStartPoint,
        SessionTriggerAdmin, SessionView, ToolAdmin, TurnActivity, TurnActivityFanout,
        TurnActivityId, TurnActivitySink, TurnBudget, TurnCause, TurnEvent, TurnExecutionMetrics,
        TurnFinish, TurnInput, TurnInputAcceptanceReceipt, TurnOutcome, TurnOutput, TurnReport,
        TurnStatus, TurnStop, message_role, message_text,
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

    pub use crate::session::{
        RemoteSessionObservationEventStream, RemoteSessionObservationStream,
        RemoteSessionObservationStreamItem, RemoteSessionObservationSubscription,
        SessionObservationStream, SessionObservationStreamItem,
    };
    pub use lash_core::{
        LiveReplayEventDraft, LiveReplayGapReason, LiveReplayStore, LiveReplayStoreError,
        LiveReplaySubscribeOutcome, PreparedLiveReplayPublication, SessionCursor,
        SessionObservationEvent, SessionObservationEventPayload, SessionProcessEventKind,
        SessionQueueEventKind, SessionRevision, facade_support::InMemoryLiveReplayStore,
        facade_support::InMemoryLiveReplayStoreConfig, facade_support::LiveReplayGap,
        facade_support::SessionObservation, facade_support::SessionObservationSubscription,
        facade_support::SessionResume,
    };
}

/// Entry points: [`LashCore::triggers`] and
/// [`SessionAdmin::triggers`](admin::SessionAdmin::triggers) through [`LashSession::admin`].
///
/// Mutations go through the store contract below:
/// [`TriggerCommand`](crate::triggers::TriggerCommand) executed by
/// [`TriggerStore::execute_command`](crate::triggers::TriggerStore::execute_command), the only
/// supported way to change a subscription.
/// The tables a durable store keeps (`lash_*` in the first-party SQL backends) are private to
/// lash; raw SQL against them is unsupported for reads and writes alike.
pub mod triggers {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::TriggerLifecycleColumnError;
    pub use lash_core::triggers::TriggerEventKey;

    /// Trigger catalog state exposed to protocol and engine integrators.
    pub use lash_core::TriggerEventCatalog;
    /// The deterministic source key a sourced occurrence is emitted under
    /// ([`TriggerOccurrenceRequest::with_source`]): the host that emits
    /// against a subscription carrying `source` params derives the same key
    /// the router derives (ADR 0021).
    pub use lash_core::facade_support::default_trigger_source_key;
    pub use lash_core::facade_support::deterministic_subscription_id;
    /// The fenced, receipted verb vocabulary for subscription mutation,
    /// including [`TriggerCommand::Enable`] for re-enable, executed by
    /// [`TriggerStore::execute_command`] on the host's trigger store.
    pub use lash_core::{TriggerCommand, TriggerStore, trigger_handle};
    pub use lash_core::{
        TriggerCommandOutcome, TriggerDeliveryReservation, TriggerDeliveryRetentionCandidate,
        TriggerEffectResult, TriggerHandle, TriggerIngressReceipt, TriggerInputBinding,
        TriggerMutationOutcome, TriggerMutationReceipt, TriggerOccurrenceFilter,
        TriggerOccurrenceOutcome, TriggerOccurrenceReclamationReport,
        TriggerOccurrenceReclamationResult, TriggerOccurrenceRecord, TriggerOccurrenceRequest,
        TriggerOperationError, TriggerOwnerScope, TriggerProviderRoute,
        TriggerRetentionReconciliationReport, TriggerRouteRefusal, TriggerRouteRestore,
        TriggerRouteRestorer, TriggerSourceCapture, TriggerSubscriptionChange,
        TriggerSubscriptionChangeCursor, TriggerSubscriptionDraft, TriggerSubscriptionFilter,
        TriggerSubscriptionLifecycle, TriggerSubscriptionRecord,
        facade_support::TriggerDeliveryEmitOutcome, facade_support::TriggerDeliveryEmitReceipt,
        facade_support::TriggerEmitReport, facade_support::TriggerEvent,
        facade_support::TriggerEventType, facade_support::TriggerRegistration,
        facade_support::TriggerTarget, facade_support::empty_trigger_source_key,
    };
}

/// Tool definitions, providers, and execution types.
///
/// Tools are at-least-once — lash makes no exactly-once claim for a tool's
/// external effects: a crash between a tool's effect and the durable record
/// of its outcome runs the call again, and a reported failure may be
/// retried. A tool keys its idempotency on
/// [`AttemptContext::call_id`](crate::tools::AttemptContext::call_id), the
/// `ToolCallId` lash mints for the call: it is the same on every run of one
/// logical call and different for every other call, whatever id the model's
/// provider sent.
/// [`AttemptContext::attempt_number`](crate::tools::AttemptContext::attempt_number)
/// counts the runs apart from it.
pub mod tools {
    #[cfg(feature = "rlm")]
    pub use lash_llm_tools::LlmToolsPluginFactory;
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::{GetDefinitionIntent, PublishDefinitionIntent, RegisterTriggerIntent};
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
    /// Per-tool retry policy carried by [`ToolDefinition::with_retry_policy`].
    pub use lash_core::ToolRetryPolicy;
    /// The pending model call passed to a tool's preparation hook.
    pub use lash_core::sansio::PendingToolCall;
    /// Collected replies returned by a runtime tool batch.
    pub use lash_core::session::ToolBatchReplies;
    pub use lash_core::tool_dispatch::ToolTriggerEffectOutcome;
    pub use lash_core::{
        AttemptContext, AttemptProcessReads, AttemptSessionReads, CancelHint, CancelProcessIntent,
        CompactToolContract, EmitProcessEventIntent, EmitTriggerIntent, ExecutionOwner,
        IsolatedProcessBinding, IsolatedProcessRequest, PendingAnnouncement, PendingCompletion,
        PendingResolver, PreparedToolCall, SignalProcessIntent, StartProcessIntent,
        TOOL_INTENT_MAX_CANONICAL_BYTES, TOOL_INTENT_MAX_COUNT, TOOL_INTENT_MAX_PER_KIND,
        TOOL_INTENT_PROTOCOL_V3, ToolArgumentProjectionPolicy, ToolAttachmentClient,
        ToolAttemptOutcome, ToolCall, ToolCallOutcome, ToolCallOutput, ToolCallRecord,
        ToolCatalogEntry, ToolContract, ToolDefinition, ToolDirectCompletionClient, ToolDiscovery,
        ToolExecutionGrant, ToolFailure, ToolFailureCause, ToolFailureClass, ToolFailureSource,
        ToolIntent, ToolIntentCommandFailure, ToolIntentExecutionOutcome, ToolIntentIdentity,
        ToolIntentKind, ToolIntentRealized, ToolIntentRefusalReason, ToolIntentRuntimeFailure,
        ToolIntents, ToolManifest, ToolModule, ToolOutcome, ToolOutcomeDone, ToolOutputContract,
        ToolPrepareCall, ToolPrepareContext, ToolProvider, ToolRegistry, ToolRetryStatus,
        ToolSessionLlmProfile, ToolValue, ToolView, ToolViewBlock, ToolViewMeta,
        derive_tool_intent_identity, facade_support::ReconfigureError,
        facade_support::ToolSourceHandle, facade_support::ToolStateFacadeOps,
        turn_outcome_from_tool_control,
    };
    /// The three capabilities a tool declares with
    /// [`ToolDefinition::with_declaration`], what refuses a call at admission,
    /// and what refuses an outcome its declaration does not admit.
    pub use lash_core::{DeclarationRefusal, OutcomeShape, ToolAdmissionRefusal, ToolDeclaration};
    pub use lash_core::{DeclaredStart, DeclaredStartRefused};
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
        ToolId, ToolState, facade_support::PLUGIN_TOOL_SOURCE_ID,
        facade_support::SupersededToolIdentity, facade_support::ToolRestoreReport,
        facade_support::ToolSourcePolicy, facade_support::ToolStateEntry,
    };
    /// Engine-owned tool-intent admission records used by process-registry integrators.
    pub use lash_core::{
        ToolIntentSubmissionAdmission, ToolIntentSubmissionOutcome, ToolIntentSubmissionRecord,
        ToolIntentSubmissionSettlement,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
        DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, RemoteToolGrantBindingExt,
        ToolBindingResolutionExt, ToolManifestBindingExt, catalogue_preview,
        catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
        catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
        required_tool_binding,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        DeferredLink, DeferredLinkError, DeferredResolutionError, DeferredResolutionLinkKey,
        DeferredResolveContext, DeferredToolResolver, RecordedGrantInstallError,
        Resolution as DeferredToolResolution, SharedDeferredToolResolver,
        ToolGrant as DeferredToolGrant, compile_with_deferred_resolution,
    };
    /// The whole tool-authoring support surface: [`StaticToolProvider`] /
    /// [`StaticToolExecute`] for fixed-set providers plus the shared helpers
    /// (`invalid_tool_args`, `object_schema`, `parse_optional_usize_arg`,
    /// `ToolBinding`, `ToolDefinitionBindingExt`, `TOOL_BINDING_KEY`,
    /// `LASHLANG_BINDINGS_ENABLED`) tools are built from. The glob keeps the
    /// facade complete as the crate grows; where it overlaps the explicit
    /// `rlm` re-exports above, those name the same items.
    pub use lash_tool_support::*;
}

/// Direct protocol transport types.
pub mod direct {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_sansio::llm::types::{
        GenerationProjectionProvenance, ProviderReplayMeta, ProviderReplayOriginConflict,
        ResponsePhase, ResponseTextMeta,
    };

    pub use lash_core::llm::types::{
        AttachmentSource, GenerationOptionOutcome, GenerationOptions, GenerationReceipt,
        LlmEventSender, LlmOutputPart, LlmStreamEvent, LlmTerminalReason, LlmUsage,
        NonNegativeFiniteF64, NonNegativeFiniteF64Error, ProviderFileScope,
        ProviderReasoningReplay, ProviderReplayDrop, ProviderReplayDropReason, ProviderReplayKind,
        ProviderRouteIdentity, StreamBlockIdentity,
    };
    pub use lash_core::{
        facade_support::DirectCompletion, facade_support::DirectJsonSchema,
        facade_support::DirectLlmClient, facade_support::DirectLlmCompletion,
        facade_support::DirectLlmError, facade_support::DirectLlmOutcome,
        facade_support::DirectMessage, facade_support::DirectOutputSpec,
        facade_support::DirectPart, facade_support::DirectRequest, facade_support::DirectRole,
    };
}

/// Session persistence types and services.
pub mod persistence {
    // The vocabulary this module's signatures name (the facade-completeness rule).

    pub use lash_core_store::artifact_referrer::{
        ArtifactCarry, ArtifactCleanup, ArtifactReferrerError, ArtifactReferrerKind,
        ArtifactStoreId, AttachmentUploadId, ReferrerGuard, ReferrerStore, SubscriptionRevisionId,
        UploadReferrerId,
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
        DurableRecord, EnumerationSource, FollowOnRecovery, FrameTransition, ReadWindow,
        StoreFault, StoreRefusal, StoredRunTerminal, SurfaceFormat, WriterPin,
    };
    pub use lash_core_store::{PersistedNodeIds, surface_format};
    /// The protocol-generic form [`SessionHistoryRecord`] specializes.
    pub use lash_sansio::SessionHistoryRecord as GenericSessionHistoryRecord;
    pub use lash_sansio::{AppendVec, BaseRenderCache, ConversationRecord};

    pub use lash_core::CheckpointKind;
    /// The store halves a [`StoreSet`](crate::StoreSet) hands out as trait
    /// objects, nameable so a host can decorate a store set (FIG-4373).
    pub use lash_core::ProcessDefinitionStore;
    pub use lash_core::RunSpecHash;
    pub use lash_core::attachments::{
        AttachmentRootPage, AttachmentRootSource, CompleteAttachmentRoots,
    };
    /// The engine's evidence that a run's execution is lost, which
    /// `DeploymentStore::end_lost_run` ends the run on.
    pub use lash_core::engine::RunLoss;
    /// Logical run references returned by a run store, and an open run
    /// as the store's recovery page lists it.
    pub use lash_core::engine::{OpenRun, RunRef};
    pub use lash_core::facade_support::FileAttachmentStore;
    /// Durable session-store inputs and outputs exposed to storage integrators.
    pub use lash_core::runtime::{
        ActiveTurnIngress, AdmissionBoundary, AdmittedQueuedWork, AdmittedTurnInputs,
        DeliveryPolicy, DeploymentStore, DeploymentStoreDecorator, ForkSessionReceipt,
        ForkSessionRequest, LiveReplayOutcome, LiveReplaySubscription, PROCESS_WAKE_MERGE_KEY,
        PendingTurnInputBatch, PendingTurnInputDraft, ProcessWakeSource, QueuedCheckpointTurnInput,
        QueuedCheckpointWork, QueuedWorkAuthority, QueuedWorkBatch, QueuedWorkBatchDraft,
        QueuedWorkCompletion, QueuedWorkEnqueueOutcome, QueuedWorkKind, QueuedWorkPayload,
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
    /// A process park write accepted by a custom registry.
    pub use lash_core::store::ProcessParkWrite;
    /// Head and usage values returned by custom session stores.
    pub use lash_core::store::SessionHeadRef;
    /// A build generation's drain marks and remaining work (FIG-3799): the
    /// store half a [`StoreSet`](crate::StoreSet) supplies for
    /// [`LashCore::drain_generation`](crate::LashCore::drain_generation).
    pub use lash_core::store::generation_drain::{
        DrainingGeneration, GenerationDrainStore, GenerationWork,
    };
    pub use lash_core::store::worker_recovery::{
        WorkerRecoveryClaim, WorkerRecoveryError, WorkerRecoveryLimits, WorkerRecoveryStore,
        WorkerRecoveryTotals,
    };
    /// The store halves a storage integrator's [`StoreSet`](crate::StoreSet)
    /// supplies: the obligation ledgers and the recovery leader lease
    /// (ADR 0109 §1.3, §1.6).
    pub use lash_core::store::{
        ClaimToken, ClaimedObligation, HolderId, KeyColumn, KeyColumnType, LeaseAnswer, LeaseClaim,
        LeaseName, LeaseRow, ObligationLedger, ObligationSettlement, RecoveryLeaderStore,
        SettleOutcome,
        session_delete::{SessionDeleteLedger, SessionDeleteObligation},
    };
    pub use lash_core::store::{
        EngineWaitKind, PreparedRunAdmission, StoreTransition, ToolCompletionReceipt,
        ToolRequestReceipt, TurnTraceReceipt, WaitReceiptStore, WaitRequestReceipt,
        WaitResolutionReceipt,
    };
    /// Artifact ownership supplied to protocol engines and effect controllers.
    pub use lash_core::{
        ArtifactName, ArtifactReferrer, FrameEnvironmentId, ReferrerClaim, ResolvedArtifactCleanup,
    };
    pub use lash_core::{AttachmentReferrers, AttachmentWrite, SessionReferrerState};
    /// Queued-work ordering values and admission-selection helpers.
    pub mod queued_work {
        // The vocabulary this module's signatures name (the facade-completeness rule).
        pub use lash_core_store::store::TurnWorkPrefix;
        pub use lash_core_store::store::queued_work::TurnLaneCandidate;

        /// Stable queued-work ordering values and selection helpers for store implementations.
        pub use lash_core::store::queued_work::{
            PendingSessionWorkOrdering, PendingWorkOrderingKey, QueuedWorkClass,
            admission_scan_limit, derive_batch_id, select_leading_session_command,
            select_turn_work_prefix,
        };
    }
    /// The canonical queued-work draft for a delivered process wake under a
    /// chosen delivery boundary, so a host replaying a wake through
    /// [`QueuedWorkStore::enqueue_queued_work_with_outcome`] submits the
    /// draft the runtime itself would build (ADR 0101).
    pub use lash_core::runtime::process_wake_batch_draft_with_delivery_policy;
    pub use lash_core::session_graph::WindowAnchor;
    pub use lash_core::store::PluginWriterRangesFuture;
    pub use lash_core::store::plugin_writers::{
        AdmittedPlugin, PluginAdmission, PluginPublication, PluginWriterRanges,
        PluginWriterRegistration, PluginWriterStamp,
    };
    /// The shift epoch a session shift's seal raises (FIG-3600): one segment
    /// of [`RuntimeStore`], implemented by every store a runtime executes,
    /// and the fence it yields, the one authority every shift write presents.
    pub use lash_core::store::{
        AdmissionId, RunHold, RunStartNonce, ShiftEpochSeal, ShiftEpochStore, ShiftFence,
        ShiftRaise, StoredShiftEpoch,
    };
    /// A run's recorded admission of the turn-lane run it executes and the
    /// execution that runs it, what its checkpoints admit, how a commit
    /// settles the rows its run holds, and the session's one unfinished
    /// run (FIG-3927, FIG-4403).
    pub use lash_core::store::{
        AdmitRunRequest, AdmittedHead, CheckpointAdmission, CheckpointAdmissionRequest,
        IngressRowId, IngressSettlement, RUN_ADMISSION_STEP, RunAdmission, RunAdmissionAnswer,
        RunAdmissionRefusal, RunExecutor, ShiftAdmissionPreparation, ShiftAdmissionReceipt,
        ShiftAdmissionSelection, ShiftAdmissionWrite, TurnCancellationBinding, UnfinishedRun,
    };
    /// The multi-session store's catalog and bounded history segments, the
    /// one-session view runtime code holds, and the window loaders (ADR 0112).
    pub use lash_core::store::{
        AnchorUnavailable, FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor,
        HistoryBudget, HistoryCursor, HistoryNode, HistoryPage, HistoryStop, LineageStamp,
        LoadedSessionWindow, QueuedWorkStore, RuntimeStore, SessionCatalogStore,
        SessionHistoryStore, SessionLookup, SessionStore, SessionWindowRead, TurnInputStore,
        WindowAnchorViolation, WindowSelector, load_session_read_view, load_session_window_state,
        refresh_session_window,
    };
    pub use lash_core::store::{
        AppendRequestIdentity, CheckpointComponentDescriptor, FollowOnWork, GraphAppend,
        HydratedCheckpointComponent, HydratedSessionCheckpoint, InterruptedTurnClosure,
        OperationId, ParkCancelCause, ParkEventColumns, ParkEventKind, ParkFeedCursor,
        ParkFeedEvent, ParkFeedPage, ParkId, ParkReason, ParkReasonCode, ParkReport,
        PendingFollowOn, PhysicalTurn, ProcessPark, ProcessParkKey, ProcessParkQuery,
        RunContinuation, RunOpenerState, RuntimeCommit, RuntimeCommitReceipt,
        RuntimeStoreDecorator, RuntimeTurnCommitStamp, SemanticBoundaryOperation,
        SessionCheckpoint, SessionHeadMeta, SessionHeadPayload, SuspendedCell, TurnChange,
        TurnChangeCursor, TurnChangeKind, TurnChangePage, TurnCommitFailureCause,
        TurnCommitOutcome, TurnPark, TurnParkOrigin, TurnParkQuery, TurnParkTarget, TurnParkWrite,
        TurnProjectionWatermark, UnparkCause, UnsettledTurnCounts, commit_runtime_state_verified,
        validate_turn_commit_outcome_code,
    };
    /// A logical run's durable terminal evidence and the store segment that
    /// answers and binds runs (FIG-3600 S7, FIG-3607 item 8), and the
    /// control intents a session's close and a parked run's verbs record.
    pub use lash_core::store::{
        CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind,
        ControlIntentState, ControlIntentStore, EnginePark, IntentApplication, IntentObligation,
        IntentSettle, RunCommittedOutcome, RunEndOutcome, RunIntentRefused, RunIntentRequest,
        RunStore, RunTerminal, RunTerminalCause, RunTerminalKind, RunTerminalWrite, RunVerb,
        TurnCommitId,
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
        AttachmentCondemnationRecord, AttachmentCondemnationSettlement, AttachmentDeleteArming,
        AttachmentDeleteStallReason, AttachmentReadPolicy, AttachmentReclamationPolicy,
        AttachmentRetentionFailure, AttachmentRetentionStoreFailure, AttachmentRootSet,
        AttachmentSettlementOutcome, AttachmentStore, AttachmentStoreError,
        AttachmentStoreFailureClass, AttachmentStorePersistence, AttachmentSweepGeneration,
        AttachmentWriteFence, AttachmentWritePermit, AttachmentWriteToken, EmptyRootSetPolicy,
        MAX_ATTACHMENT_DELETE_ATTEMPTS, ProcessExecutionEnvStore, StoredAttachment, StoredBlobRef,
        attachments::AttachmentReclamationFailure, facade_support::AttachmentGcFence,
        facade_support::AttachmentReclamationReport, facade_support::RuntimeAttachmentStore,
        facade_support::reclaim_unreferenced_attachments,
    };
    /// The Lashlang module-artifact port a backend's store set supplies.
    pub use lash_core::{
        ArtifactStoreError, DurabilityTier, ModuleArtifactAstRefusal, ModuleArtifactCorruption,
        ModuleArtifactGeneration, ModuleArtifactRefusal, ModuleArtifactStore,
    };
    pub use lash_core::{
        BlobRef, CURRENT_SESSION_STATE_VERSION, DurableItem, DurablePayload, DurableScan,
        DurableScanPage, DurableSurface, ExecutedCall, ExecutedCallOutcome, ExecutedCallRecord,
        FLEET_FORMAT_VERSION, FleetFormat, FleetFormatState, FleetFormatStore, GcReport,
        LeaseOwnerIdentity, MaintenanceFailure, MaintenanceRefusal, MaintenanceReport,
        MaintenanceResult, MaintenanceStop, MaintenanceSweep,
        OLDEST_SUPPORTED_SESSION_STATE_VERSION, PersistedSessionConfig, PersistedTurnState,
        ProtocolEvent, RetentionBound, RetentionReport, ScanCoverage, SessionAdmission,
        SessionBlobReclaimReport, SessionCommitStore, SessionGraph, SessionHistoryRecord,
        SessionMeta, SessionNodePayload, SessionNodeRecord, SessionReadView, SessionRelation,
        SessionStateAdmission, StoreBackend, StoreComponentVersion, StoreError, StoreMaintenance,
        StorePreflight, StoreReleaseStamp, StoreReleaseState, StoreSchemaDatabase,
        StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict, TurnInputAdmission,
        VacuumReport, facade_support::SessionNodeProjection,
    };
    pub use lash_core::{
        facade_support::ChronologicalEntry, facade_support::ChronologicalPayload,
        facade_support::ChronologicalProjection,
    };
    /// The typed view an RLM host reads and writes its module artifacts through.
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::LashlangArtifacts;
}

/// Plugin contracts, manifests, and operation types.
pub mod plugins {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::ConfigRegistry;
    pub use lash_core::plugin::{
        AssistantProseProjectorPlugin, AssistantStreamFinishedHook, CompactionSystemPrompt,
        DecidedContextPressure, PluginFuture, PluginLifecycleEventHook, PluginLifecycleFuture,
        ResolvedToolSurface, ToolCatalogContributor, ToolPresentationArtifacts,
        ToolPresentationFacts, ToolPresentationInput, ToolPresentationStep,
        TranscriptRowProjectorPlugin,
    };
    pub use lash_core::runtime::ToolAttemptEffectOutcome;
    pub use lash_core::runtime::{
        AttemptStream, AttemptStreamChannel, AttemptStreamEvent, AttemptStreamTruncation,
        DecodedStreamEvent, ToolAttemptCapture,
    };
    pub use lash_core::session::{
        CompletedProtocolToolCall, Incorporated, IncorporationLedger, SettlementSource,
        ToolAggregateConsumer, ToolAggregateLeaf, ToolAggregateLeafReply, ToolAggregateOutcome,
        ToolAggregateRequest, ToolRunAggregateCursor, ToolRunAggregatePoll,
    };
    pub use lash_core::tool_dispatch::{
        ArmedResolver, LaunchReceipt, PendingToolDispatchOutcome, ToolCallIds, ToolDispatchOutcome,
        ToolPreparationOutcome,
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
        Observation, PendingWork, ProjectorContext, ResponseToolCalls, SessionStreamEvent,
        StreamMessageKind, ToolCatalogBuildError, ToolContractResolver, ToolExpansionPlan,
        TurnMachineConfig, TurnProtocol, UnitTurnProtocol, WriterFormats,
    };
    /// The protocol-generic forms [`TurnDriverConfig`] and
    /// [`TurnDriverPreamble`] specialize to the host's turn protocol.
    pub use lash_sansio::{
        TurnDriverConfig as GenericTurnDriverConfig,
        TurnDriverPreamble as GenericTurnDriverPreamble,
    };

    /// Host-specialized driver configuration required by every [`TurnDriverPreamble`].
    pub use lash_core::TurnDriverConfig;
    /// Durable session-lifecycle operations a hook context carries, alongside
    /// [`SessionStateService`] and [`SessionGraphService`]; runtime-implemented.
    pub use lash_core::facade_support::SessionLifecycleService;
    /// The schema crate config wire types derive with, so an owner's
    /// namespace, commands and refusals generate the schemas the config
    /// command catalog publishes: derive
    /// `#[derive(lash::plugins::schemars::JsonSchema)]` with
    /// `#[schemars(crate = "lash::plugins::schemars")]`.
    pub use lash_core::facade_support::schemars;
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
    /// What a plugin factory declares about itself: its behaviour revision
    /// and the formats it reads and writes. The build generation is computed
    /// from every registered factory's declaration, in hook order.
    pub use lash_core::plugin::{
        BehaviorRevision, FormatVersion, PluginCallbackIdentity, PluginComposition,
        PluginDeclaration, PluginDeclarationError, PluginDefinition, PluginExecutionRefusal,
        PluginId, PluginMetadata, PluginRevision,
    };
    /// Protocol and process-engine contracts, including their complete runtime-owned state closure.
    pub use lash_core::plugin::{
        CheckpointApplication, CheckpointComponentKey, CodeExecutionOutcome, CodeExecutorPlugin,
        ExecutionLeafName, ExecutionStateCapture, HydratedExecutionState, InvalidExecutionLeafName,
        LeafChange, PluginAbort, PluginNamespaceState, PluginSessionMaterializationRequest,
        PluginSessionRequest, PluginState, PluginStateEffect, PluginTransitionBase,
        PluginTransitionId, PluginTransitionRecord, PluginTransitionRequest,
        ProtocolBeforeLlmCallContext, ProtocolDriverPlugin, ProtocolLlmCallAction,
        ProtocolSessionContext, ProtocolSessionPlugin, ProtocolSessionRestoreView,
        RecordedCallbackPhase, RecordedTurnContribution, SessionAuthorityContext,
        SystemPromptContext, SystemPromptPurpose, TurnFinalization, TurnPreparation,
    };
    /// The registration groups [`PluginRegistrar`]'s accessors return
    /// (`reg.tools()`, `reg.session()`, ...), nameable so a helper can take
    /// one as a parameter.
    pub use lash_core::plugin::{
        ContextRegistrations, ExecutionRegistrations, OutputRegistrations,
        PluginOperationRegistrations, ProtocolRegistrations, SessionRegistrations,
        ToolCallRegistrations, ToolCatalogRegistrations, ToolRegistrations,
        ToolResultRegistrations, TriggerEventRegistrations, TurnRegistrations,
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
        StateCommand, StateCommandOrigin, StateCommandRefusal, StateCommands, StateFrontier,
        StateReducer, StateReduction, StateResolution, StateResolutionOutcome,
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
    pub use lash_core::runtime::{
        ProcessEngineProcessContext, ProcessEngineRegistry, ProcessEngineRunGuard,
        ProcessEngineRuntimeContext,
    };
    /// Engine-extension contracts for Run-owned admission, attempts, aggregates
    /// and continuation. Effect hosts implement these recorded transitions;
    /// hosts submit work through session handles.
    pub use lash_core::tool_dispatch::{
        BeforeCheckReply, DecidedCall, DeclaredStartObligation, DeclaredStartObligationRefusal,
        IntentRealizationContext, IsolatedProcessDescriptor, IsolatedStartRefusal,
        IsolatedToolStart, IssuedRealization, RealizationDispatch, RealizationPayload,
        RealizationReceipt, RealizationRequest, RecordedIsolatedStart, RunAggregateOutcome,
        RunBodies, RunCoordinator, RunCutRefusal, RunSelectKey, RunSelectValue, RunSelectable,
        SelectKey, SingletonAttempt, SingletonBodyOutcome, SingletonCapture, SingletonDrift,
        SingletonPreparedRequest, SingletonPresentationError, SingletonRunError, SingletonStart,
        SingletonTerminal, SingletonToolCall, SingletonToolHandlers, ToolRealizer,
    };
    /// A source seal's typed refusal, distinct from engine admission refusal.
    pub use lash_core::tool_run::SealRefusal as SourceSealRefusal;
    pub use lash_core::tool_run::run_event::{
        AttemptResult, CallDecision, PendingStart, RealizationKey, ResultSource, RunAttemptEntry,
        RunEvent, RunEventOrdinal, RunEventRefusal, RunJournalEntry, RunLedger, RunRecord,
        RunTraceFacts,
    };
    pub use lash_core::tool_run::{
        AdmissionRefusal as ToolRunAdmissionRefusal, AdmittedBinding, AdmittedCall, AdmittedRound,
        AdoptedRun, AfterCheckVerdict, AggregateConsumer, AggregateLeaf, AggregatePlan,
        AttributedVerdict, BeforeCheckVerdict, BeforeSelection, CapacityScope, Cut, CutPhase,
        RecordedRetryPolicy, RoundAdmission, RunLifecycle, RunTransfer, RuntimeCallPolicy,
        SegmentOrdinal, TransferBundle,
    };
    /// Recorded Run data needed by engine extensions and effect-host journals.
    pub use lash_core::tool_run::{
        BusinessReceipt, CheckRecord, ContinuationRefusal, InvalidMaterialDigest, LogicalTerminal,
        MaterialBundle, MaterialDigest, MaterialEntry, MaterialHolder, MaterialLocation,
        MaterialOwner, MaterialPayload, MaterialRef, MaterialRefusal, MaterialRetentionError,
        MaterialRole, ObservationPermit, ObservedFact, OperationRun, RetainedBundle, RunInputKind,
        SealOutcome, SealWriter, SourceAuthority, SourceDescriptor, SourceRefusal, SourceSeal,
        SourceSubscription,
    };
    /// A session's recorded plugin configuration and the owner contract that
    /// creates and changes it (FIG-4379): each installed plugin registers the
    /// owner of its namespace and the typed config commands that change it,
    /// and reads the recorded value on every open and in every scoped hook.
    pub use lash_core::{
        AdmittedPluginConfig, CandidateFacts, ConfigCommand, ConfigFault, ConfigOwner,
        ConfigRegistrar, ConfigRegistrationError, ConfigWire, CreationConfigError, CreationFacts,
        NoRunOptions, OwnerChange, PluginConfig, RecordedNamespaceCorrupt, RenderFault,
    };
    /// Protocol-driver and process-engine inputs that core owns independently of plugin storage.
    pub use lash_core::{
        AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, HostTurnProtocol,
        PersistedSegmentHandover, PhysicalProcessWorker, ProcessEngine, ProcessEngineAdmission,
        ProcessEngineRegistration, ProcessEngineRunContext, ProcessExecutionBoundary,
        ProcessInfraError, ProcessRunOutcome, ProcessSegmentKey, ProtocolBuildInput,
        ProtocolDriverState, ProtocolTurnOptionsError, SegmentHandover, SegmentStartMarker,
        SessionPluginSource, TurnDriverPreamble, WorkerCommand, WorkerProcessEngine,
        WorkerTerminationReceipt,
    };
    /// The session services a hook context hands a plugin: read-through state
    /// access ([`SessionStateService`]) and durable graph appends
    /// ([`SessionGraphService`]), plus the append request/result vocabulary.
    /// Both are runtime-implemented — a plugin receives one, never writes one.
    pub use lash_core::{
        AppendSessionNodesOutcome, AppendSessionNodesRequest, PluginExtensions, SessionAppendNode,
        SessionGraphService, SessionStateService, SessionToolAccess, SessionToolAccessError,
        SubagentSessionContext,
    };
    /// Code-executor request, response, and runtime capability context.
    pub use lash_core::{
        CellFailure, CellFailureKind, ExecRequest, ExecResponse, RuntimeExecutionContext,
    };
    pub use lash_core::{CompletedToolCall, PluginOptions};
    /// Executable identity and terminal rendering returned by protocol integrators.
    pub use lash_core::{ExecutableGeneration, RecordedRender};
    pub use lash_core::{
        PluginError, PluginErrorClass, PluginMessage, PluginRuntimeEvent, ToolCatalog,
        facade_support::PluginFactory, facade_support::PluginHost, facade_support::PluginRegistrar,
        facade_support::PluginSession, facade_support::PluginSessionContext,
        facade_support::PluginSpec, facade_support::PluginSpecFactory,
        facade_support::SessionPlugin, facade_support::ToolCatalogContribution,
        facade_support::TurnHookContext, facade_support::TurnResultHookContext,
    };
    /// Lifecycle observation: what a `reg.session().on_event(..)` hook receives
    /// once durable session state has advanced, and the contexts each event
    /// carries. [`PluginLifecycleEvent::TurnPersisted`] fires after the commit it
    /// describes, so a hook observes a session whose head may already have moved
    /// on.
    pub use lash_core::{
        facade_support::PluginLifecycleEvent, facade_support::SessionConfigChangedContext,
        facade_support::SessionStateChangedContext,
    };
    /// Per-turn context assembly: the prepared messages, prompt contributions,
    /// and tool providers a [`TurnContextTransform`] may rewrite before the
    /// model call, and the read-only context the transform is handed.
    pub use lash_core::{
        facade_support::PreparedContext, facade_support::TurnContextTransform,
        facade_support::TurnTransformContext,
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
    /// JSON value in integrator signatures, without a second direct dependency.
    pub use serde_json::Value as JsonValue;
}

/// Attachment values: identity, media type, and the metadata that travels with
/// bytes. This is the vocabulary shared by the three places a host meets an
/// attachment — [`InputItem::attachment`](crate::InputItem), the direct-LLM
/// [`AttachmentSource`](crate::direct::AttachmentSource), and the
/// [`AttachmentStore`](crate::persistence::AttachmentStore) contract — so it
/// has its own home rather than being duplicated into each.
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
    /// Output kept out of session history (FIG-1643): the byte policy
    /// [`LashCoreBuilder::output_retention`](crate::LashCoreBuilder::output_retention)
    /// configures, the witness and reference history keeps in an oversized
    /// output's place, and a value that is one or the other.
    pub use lash_core::{OutputRetentionPolicy, OutputValue, RetainedOutput};
    pub use lash_sansio::{InvalidAttachmentId, InvalidMediaType};
}

/// Secret-handling values for host-owned configuration structs.
pub mod secrets {
    /// A string wrapper whose `Debug`/`Display` render `[redacted]`, so a
    /// provider key held in a host config struct cannot leak through logs.
    pub use lash_sansio::Redacted;
}

/// Wire-format DTOs for executing lash across a process boundary, sub-namespaced
/// by protocol domain. Only the cross-cutting envelope
/// ([`Envelope`](remote::Envelope),
/// [`REMOTE_PROTOCOL_VERSION`](remote::REMOTE_PROTOCOL_VERSION)) and the
/// protocol error type live at this module root; everything else has exactly one
/// home in a domain sub-namespace.
pub mod remote;

/// Durable process definitions, handles, and events.
pub mod process {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::facade_support::ProcessEventSinkRegistration;
    pub use lash_core::{
        ConsumerHold, ProcessDefinitionStoredError, ProcessRegistrationProbe,
        ProcessSpawnProvenance, ProcessStartDeclaration,
    };
    pub use lash_core_store::effect_opener::EffectOpenerError;
    pub use lash_sansio::{HandleTarget, ObservedProcessFailure};

    pub use crate::admin::SessionProcessAdmin;
    pub use crate::artifacts::{HostArtifactPin, HostArtifacts};
    pub use crate::process_admin::Processes;
    pub use crate::process_observation::{
        ProcessCursor, ProcessCursorError, ProcessCursorReference, ProcessDurableCompleteness,
        ProcessDurableGapReason, ProcessDurableSnapshot, ProcessEventsFrom, ProcessEventsRead,
        ProcessLiveIncompleteness, ProcessObservationCompleteness, ProcessObservationConfig,
        ProcessObservationGapReason, ProcessObservationHub, ProcessObservationItem,
        ProcessObservationProjection, ProcessObservationSnapshot, ProcessObservationSubscription,
    };
    /// The origin of a lifecycle cancellation submitted to a registry.
    pub use lash_core::CancelOrigin;
    pub use lash_core::SessionTurnOutcome;
    /// Materialized event semantics returned to custom process registries.
    pub use lash_core::runtime::ProcessEventSemantics;
    /// Process-registry and event types that complete the store and engine signature closure.
    pub use lash_core::runtime::{
        ParentEndPlan, ProcessChange, ProcessCompletionOutcome, ProcessExecutionWriteAuthority,
        ProcessOutcome, ProcessStartOutcome, ProcessTerminalSemantics, ProcessTerminalSpec,
        ProcessTombstone, WaitKind, WaitState, WakeDelivery, WakeDeliveryBlockedGroup,
        WakeDeliveryClaimOutcome, WakeDeliveryLifecycle, WakeDeliveryReport, WakeDeliveryState,
        WakeDiscardReason,
    };
    /// The one lifecycle state a process record holds, and the outcome a
    /// terminal one ends in.
    pub use lash_core::runtime::{
        ProcessLifecycleState, ProcessOutcomeNotRetained, ProcessTerminal,
    };
    /// Registry admission receipts and lifecycle write outcomes.
    pub use lash_core::runtime::{
        ProcessRegistrationReceipt, ProcessRegistryBinding, StoreRealization,
    };
    pub use lash_core::{
        AbandonEvidence, AbandonWriter, AdmittedProcessIdentity, Ancestry, CausalRef,
        DeclaredProcessIdentity, HandleId, InvalidProcessDefinitionId, InvalidStartKey, Lifetime,
        LifetimeDecision, LifetimePolicy, MAX_NON_TERMINAL_PROCESS_PAGE_SIZE, NoProcessWork,
        NonTerminalProcessPage, PROCESS_EFFECT_OCCURRENCE_CAP, PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
        PROCESS_EFFECT_OUTCOME_EVENT_TYPE, PROCESS_EVENT_VOCABULARY_VERSION, PinnedTriggerDelivery,
        ProcessAwaitOutput, ProcessCancelReceipt, ProcessChangeCursor, ProcessClockRebind,
        ProcessCompletionAuthority, ProcessContinuationStore, ProcessDefinition,
        ProcessDefinitionDraft, ProcessDefinitionDraftError, ProcessDefinitionId,
        ProcessDefinitionRef, ProcessDefinitionRefusal, ProcessDefinitionResolution,
        ProcessDefinitionTarget, ProcessDefinitionValue, ProcessEffectNodeReport,
        ProcessEffectOccurrence, ProcessEffectOmissions, ProcessEffectOmittedCounts,
        ProcessEffectOutcomeClass, ProcessEffectReport, ProcessEffectReportError,
        ProcessEngineKind, ProcessEvent, ProcessEventAppendReceipt, ProcessEventAppendRequest,
        ProcessEventHistoryRetention, ProcessEventLite, ProcessEventLog, ProcessEventPage,
        ProcessEventPageEvents, ProcessEventPageMore, ProcessEventQueryMode,
        ProcessEventReadOutcome, ProcessEventRelease, ProcessEventType, ProcessExecutionContext,
        ProcessExecutionEnvRef, ProcessExecutionEnvSpec, ProcessExternalRef, ProcessHandleView,
        ProcessIdentity, ProcessInput, ProcessLifecycle, ProcessLineage, ProcessListFilter,
        ProcessListMode, ProcessLiveReferenceView, ProcessObserverBy, ProcessObserverRegistry,
        ProcessOpScope, ProcessOriginator, ProcessOriginatorFilter, ProcessProvenance,
        ProcessPruneReport, ProcessQuery, ProcessRecord, ProcessRegistrar, ProcessRegistration,
        ProcessRegistrationOutcome, ProcessRegistry, ProcessRegistryCursor, ProcessResumeRefusal,
        ProcessRetention, ProcessService, ProcessSessionDeleteReport, ProcessSignal,
        ProcessSignalIdentity, ProcessSignalWaitBinding, ProcessSignature, ProcessStartOptions,
        ProcessStartReceipt, ProcessStartRegistration, ProcessStartRequest, ProcessStartTarget,
        ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessTerminalPublication,
        ProcessTerminalWait, ProcessToolIntents, ProcessWakeDelivery, ProcessWakeOutbox,
        ProcessWakeSpec, ProcessWorkSubstrate, ProcessWorkWiring, ProjectionWatermark,
        RetiredProcessStatus, SCOPE_STORAGE_PAYLOAD_VERSION, ScopeGrant, ScopeId, ScopeRef,
        ScopeStorageError, SessionScope, StartCx, StartCxError, StartKey, TerminalProcessStatus,
        TriggerDeliveryPin, WakeId, WatchedRegistry, facade_support::ObservedProcess,
        facade_support::ObservedProcessEvent, facade_support::ObservedProcessEventLite,
        facade_support::ObservedProcessEventPage, facade_support::ObservedProcessEventReadOutcome,
        facade_support::ObservedWorkItem, facade_support::ObservedWorkItemState,
        facade_support::ProcessChangeHub, facade_support::ProcessChangeSubscription,
        facade_support::ProcessEventSink, facade_support::ProcessRuntimeHost,
        facade_support::ProcessToolVisibilityFilter, facade_support::ProcessWake,
        facade_support::ProcessWorkObserver, facade_support::ProcessWorkSnapshot,
        facade_support::SessionScopeId, facade_support::watch_process_registry,
        facade_support::watch_process_registry_with_sink, lifetime,
    };
    /// Test-only registry probes and the conformance-suite registry type that
    /// carries them (`testing` feature only; no production trait requires them).
    #[cfg(any(test, feature = "testing"))]
    pub use lash_core::{
        ConformanceProcessRegistry, ProcessEventLogTestSupport, ProcessRegistryTestSupport,
    };
    /// Event semantics a registration declares for its extra event types: which
    /// occurrences wake the process ([`ProcessWakeSpec`]) and how a payload is
    /// projected into the wake input ([`ProcessValueSelector`]).
    pub use lash_core::{ProcessEventSemanticsSpec, ProcessValueSelector};
    /// Wake redelivery. A host that owns its own [`ProcessRegistry`] also owns
    /// the redelivery loop that turns pending wakes into queued work; an
    /// embedded core executes one for you.
    /// [`process_wake_source_key`] is the queued-work source key a delivered
    /// wake lands under, so a host can correlate the two.
    pub use lash_core::{
        WakeDeliveryConfig, facade_support::WakeDeliveryDriveReport,
        facade_support::WakeDeliveryDriver, facade_support::process_wake_source_key,
    };
    #[cfg(feature = "rlm")]
    pub use lash_lashlang_runtime::{
        LASHLANG_ENGINE_KIND, LashlangProcessInput, TraceLanguageExecutionMapError,
        lashlang_process_event_types, lashlang_process_signal_event_types,
        trace_lashlang_process_map, trace_lashlang_process_map_snapshot,
    };
}

/// Durability configuration and backend contracts.
pub mod durability {
    pub use lash_core::{EffectAttempt, RecordedEffectExecution};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::RecordedKeys;
    pub use lash_core::runtime::process_start::ProcessStartRelay;
    pub use lash_core::{PreparedProcessRegistration, SegmentHandoverCommit};
    pub use lash_core_store::attachments::{
        AttachmentProducer, AttachmentSourcePolicy, AttachmentSourcePolicyError,
    };
    pub use lash_core_store::effect_opener::EffectOpener;
    pub use lash_sansio::{CancelRequest, ToolCallAdmission};

    /// Effect-host inputs, replay projections, and local execution capabilities.
    pub use lash_core::runtime::{
        BoundaryReason, CanonicalRuntimeEffectEnvelope, EffectJournalIdentity,
        EffectJournalRetirement, EffectRetirementGate, HostStartAdmission, ProcessLocalExecution,
        ProcessOutcomeObserver, ProcessTurnCancellation, RuntimeAwaitEventOptions,
        RuntimeEffectReplayTrace, RuntimeReplay, RuntimeReplayAttribution, RuntimeSleepOptions,
        RuntimeSubject, SegmentProgress, ToolAttemptLaunch, TriggerLocalExecution,
    };
    /// Durable group and journal values returned by effect-host implementors.
    pub use lash_core::runtime::{
        JournalReplay, ProcessDriveStep, RecordedJournal, RecordedKeyRange, RunRecordStep,
    };
    pub use lash_core::tool_dispatch::{
        RunAttemptBody, RunAttemptHandle, RunAttemptStep, RunRetryTimer, RunSelectKey,
        RunSelectValue, RunSelectable, RunStartPrepareStep, RunStartPrepared, RunStepHandle,
        SelectKey,
    };
    pub use lash_core::{
        EffectHost, TurnCancellationAuthority, facade_support::LeaseTimings,
        facade_support::LeaseTimingsError, facade_support::RuntimeEnvironment,
        facade_support::RuntimeHostConfig, facade_support::TerminationPolicy,
    };
    pub use lash_core_worker::{DurableProcessWorker, DurableProcessWorkerConfig};
}

/// Runtime events, errors, and execution controls.
pub mod runtime {
    pub use lash_core::IngressReservedSourceKeyRefusal;
    pub use lash_core::engine::{ObservationSink, ObservedEvent, ReplayKey, ShiftObservation};
    pub use lash_core::facade_support::TraceBoundaryReceipt;
    pub use lash_core::runtime::{AttemptStreamRecorder, DeclaredStartPhase, StartCancelDecision};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::engine::{
        AdmitRequest, AdmitVerdict, EngineAck, EngineCursor, EnginePage, EngineParkRecorded,
        EngineRefusal, ParkReconcileReport, ParkRecoveryWriter, ParkRef, ParkTarget, RefusalClass,
        ScopeCloseSink, SealRefusal, SealVerdict, SessionControlEngine, ShiftAbort,
        StalledExecution,
    };
    pub use lash_core::runtime::ProcessDefinitionLocalExecution;
    pub use lash_core::runtime::SessionTurnAdmission;
    pub use lash_core::runtime::{
        AdmittedHeadVerdict, CompactionBase, PresentationBinding, ToolPresentation,
    };
    pub use lash_core::shift::relay::RelayPolicy;
    pub use lash_core::tool_dispatch::ToolAttemptLineage;
    /// The cancellation policy pinned in a Run-owned source descriptor.
    pub use lash_core::tool_run::ExternalCancelPolicy;
    pub use lash_core::triggers::TriggerDeliveryAdmission;

    pub use lash_core::{ConfigResolution, ConfigResolutionDecision};
    pub use lash_core::{
        ProtocolSessionExtension, ScopeBoundController, ServedOnly, TurnControlAttachment,
    };
    pub use lash_core_store::runtime_error::EffectErrorJournalPolicy;
    pub use lash_core_store::store::FollowOnRecoveryAnswer;
    pub use lash_core_store::turn_control_binding::{
        TurnControlBindingId, TurnControlBindingIdError,
    };
    pub use lash_core_store::turn_input_vocabulary::RunDefinitions;
    pub use lash_sansio::sansio::{
        ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure,
        ExecutionEnvironmentSyncFailureKind, ProjectorTurnInputs,
    };
    pub use lash_sansio::{CheckpointDelivery, EffectIdentityError};

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
    /// Assistant-output state exposed by assembled runtime turns.
    pub use lash_core::facade_support::OutputState;
    /// Wall-clock milliseconds since the Unix epoch, as the runtime stamps its
    /// own process records. A host that mints a record the runtime will compare
    /// against uses the same reading rather than its own.
    pub use lash_core::runtime::current_epoch_ms;
    pub use lash_core::runtime::{
        AdmittedScope, AssembledTurn, AssistantResponseHookEvents, AssistantResponsePlan,
        AssistantStreamHookState, AwaitEventResolver, CheckpointAdmittedSet,
        CompletionKeyPreparation, DirectCompletionClient, EffectAddress, EmbeddedRuntimeHost,
        EventSink, ExecutionScope, LlmRequestSpec, LlmStreamRecord, NoSessionWork, NoopEventSink,
        NoopTurnActivitySink, ProcessCommand, ProcessEffectOutcome, ProcessListSelection,
        RunAggregateWakePolicy, RuntimeAttribution, RuntimeControlConfig, RuntimeDurabilityConfig,
        RuntimeEffectCommand, RuntimeEffectController, RuntimeEffectControllerError,
        RuntimeEffectEnvelope, RuntimeEffectInvocation, RuntimeEffectKind,
        RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeEffectReplayMismatchReport,
        RuntimeEnvironmentBuilder, RuntimeError, RuntimeErrorCode, RuntimeInvocation,
        RuntimeProviderConfig, ScopedEffectController, SessionWorkEngine, SleepSpec, TraceEmitter,
        TraceRuntime, TurnCancelWait, TurnContext, TurnControlBinding, TurnPrelude,
        WorkCadenceError, WorkCadencePolicy,
    };
    /// The host clock a [`Backend`](crate::Backend) is opened on, used
    /// for runtime sleeps and store timestamps. [`SystemClock`] is the
    /// wall-clock default; tests open a backend on their own to make expiry
    /// deterministic.
    pub use lash_core::{Clock, ClockWallTime, facade_support::SystemClock};
    /// The session extension handle and turn options exposed to runtime integrators.
    pub use lash_core::{
        ProtocolSessionExtensionHandle, ProtocolTurnOptions, SessionPolicy, SessionSnapshot,
        facade_support::SessionHandle, facade_support::render_turn_causes_prompt,
    };
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
    pub use lash_core_store::store::fleet_finalize::{
        FinalizeError, FinalizeHold, FinalizeRefusal, FleetEpochFlip,
    };

    pub use lash_sqlite_store::*;
}

/// PostgreSQL durable store backend.
#[cfg(feature = "postgres")]
pub mod postgres {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core_store::store::fleet_finalize::FinalizeMode;

    pub use lash_postgres_store::*;
}

/// S3 attachment store backend.
#[cfg(feature = "s3")]
pub mod s3 {
    pub use lash_s3_store::*;
}

/// Restate durable-execution substrate: [`RestateEngine`] over a SQLite or
/// PostgreSQL store set is the backend a [`LashCore::builder`](crate::LashCore::builder)
/// takes (ADR 0104).
///
/// [`RestateEngine`]: lash_restate::RestateEngine
#[cfg(feature = "restate")]
pub mod restate {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::SessionShifts;
    pub use lash_core::engine::{
        Admitted, AdmittedWork, ReconcileCursor, RunEnd, RunOutcome, ShiftHold, ShiftLoop,
        ShiftOutcome, ShiftRequest, ShiftRequestId, ShiftStop,
    };
    pub use lash_core_store::compat::ComponentId;
    pub use lash_core_store::store::fleet_finalize::{
        DeploymentRegistry, DeploymentRegistryError, RetainedDeployment,
    };
    pub use lash_sansio::VersionRangeError;

    use crate::formats::{
        DurableFormat, DurableFormatEntry, EngineFormat, FormatProbe, FormatVersion,
    };
    pub use crate::send::restate::{RestateWait, RestateWaitContext};

    pub use lash_restate::*;

    /// The durable-format rows this engine registers with the facade's
    /// format table, projected onto the table's own row shape
    /// (ADR 0104 §2). The engine owns its formats — [`durable_formats`] is
    /// its registry — so the facade names them through the engine-neutral
    /// `DurableFormat::Engine` handle rather than variants spelled for the
    /// engine.
    pub(crate) fn durable_format_entries() -> impl Iterator<Item = DurableFormatEntry> {
        durable_formats().map(|format| DurableFormatEntry {
            format: DurableFormat::Engine(EngineFormat {
                id: format.id,
                name: format.name,
                unwalkable_reason: format.unwalkable_reason,
                upgrade_policy: format.upgrade_policy,
            }),
            version: FormatVersion::Counter(format.version),
            owning_crate: "lash-restate",
            constant: format.constant,
            probe: FormatProbe::Comparable,
        })
    }
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

/// First-party process-control tools: `start_process`, `signal_process`,
/// `emit_process_event`, `get_process_definition`, `list_process_handles`,
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

/// Subagent spawning plugin.
#[cfg(feature = "subagents")]
pub mod subagents {
    pub use lash_subagents::*;
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
    #[cfg(any(feature = "anthropic", feature = "google", feature = "openai"))]
    pub use lash_llm_transport::ExtraHeaders;
    #[cfg(any(feature = "google", feature = "openai"))]
    pub use lash_provider_auth::{OAuthError, OAuthTokenErrorCode, OAuthTokens};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_core::llm::transport::HttpFailureContext;
    pub use lash_sansio::llm::types::{
        LlmProviderTraceEvent, LlmProviderTraceSender, ProviderReasoningRetentionSupport,
    };

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
        ProviderRateLimitPermit, ProviderRateLimitPolicy, ProviderRateLimiter, ProviderReliability,
        ProviderRetryPolicy, RequestTimeout,
    };
    pub use lash_core::{
        AnthropicThinkingRetention, AttachmentAcceptanceRule, AttachmentAcceptor,
        AttachmentCapabilitySnapshot, AttachmentMimeSource, CacheControlDialect, GoogleDialect,
        InstructionRole, LlmProfileCapability, OpenAiReasoningContext, ReasoningCapability,
        ReasoningEncoding, ReasoningIntent, ReasoningRetentionCapability, ReasoningRetentionPolicy,
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
        AttemptOutcome, AttemptUsageOutcome, ExecutionEvidence,
        ExecutionEvidenceCollectionInterruption, ExecutionEvidenceMergeError, LlmRequest,
        LlmRequestScope, LlmResponse, LlmStreamEvidence, NormalizedError, ProtocolPosition,
        ProviderEndpointError, facade_support::LlmTransportError,
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

/// Canonical committed chat rows and protocol-neutral display contracts.
pub mod transcript {
    pub use lash_core::transcript::{
        RowContent, RowId, RowOrdinal, RowProvenance, RowTool, SuppressionReason,
        TranscriptProjection, TranscriptProjectionOptions, TranscriptProjectionOutcome,
        TranscriptRow, TranscriptRowKind, TranscriptRowRecord,
    };
}
