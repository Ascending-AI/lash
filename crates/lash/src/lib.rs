//! App-facing embedding facade for Lash.
//!
//! Recovery loads committed state; uncommitted computation and diagnostics may
//! repeat. Committed effect outcomes do not re-execute; unfinished calls follow
//! their execution policy. Hosts deduplicate external effects on `call_id`.
//! See [What survives a crash](https://github.com/Ascending-AI/lash/blob/main/README.md#what-survives-a-crash)
//! for the storage assumption, `Once`/`Repeatable` rules and model resend windows.
//!
//! `lash` is intentionally a small layer above the lower-level
//! `lash-core` runtime crate. Host applications own providers, persistence,
//! app state, HTTP protocols, auth, and frontend streaming; this crate
//! owns only the ergonomic core/session/turn API.
//!
//! Tool failures expose their plain model-facing message through
//! [`tools::ToolFailure::model_text`]. Never stringify a failure's
//! `value_for_projection()` as model text: it projects typed JSON for host views.
//! [`ToolLiveFilter`] and [`ToolLiveItem`] Serde JSON are stable host wire
//! contracts, internally tagged by `kind`. Their typed JSON is the readable
//! host form; hosts may exchange it directly without a wire mirror.
//!
//! With `workflow-view`, `workflow::view::JSON_SCHEMA` is self-contained:
//! every `$ref` has the form `#/$defs/<name>` and resolves within its root
//! `$defs`, with no external or `$id`-relative references. Definition names
//! are stable within a lash version, so hosts may splice the definitions
//! into their own OpenAPI schemas.
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
//! The two stable root re-exports are deliberate second homes:
//! `StreamBlockIdentity` also lives at [`direct::StreamBlockIdentity`] and
//! `LlmUsage` at [`usage::LlmUsage`] (FIG-6044).
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

#![deny(clippy::future_not_send)]

/// Administrative facade handles and operations.
pub mod admin;
mod artifacts;
mod change_page;
mod core;
mod data_retention;
pub use change_page::ChangePage;
pub use lash_core::facade_support::RuntimeDataLimits;
pub use lash_core_store::plugin_state::NameByteLimit;
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
#[cfg(feature = "postgres")]
mod postgres_replay;
pub mod preflight;
pub(crate) mod process_admin;
mod process_authority;
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
/// [`SessionConfigAdmin::apply`](crate::config::SessionConfigAdmin::apply). It
/// applies all or nothing, with one revision step, and settles as a
/// [`ConfigTransactionOutcome`]: applied, stale, or refused by an owner.
/// [`SessionConfigAdmin::commands`](crate::config::SessionConfigAdmin::commands)
/// lists every command the installed owners register.
///
/// The core owner's commands are here; a protocol's are in its module
/// ([`standard::SetStandardRender`] and `rlm::SetRlmRender`). The prompt is
/// keyed sections (ADR 0133): plugins register them, and the host orders and
/// places them with [`SetPromptPlan`].
pub mod config {
    pub use crate::admin::config_transactions::{
        ConfigSettlement, ConfigWrite, SessionConfigAdmin,
    };
    pub use lash_core::CORE_CONFIG_OWNER;
    pub use lash_core::ConfigCommandCatalog;
    pub use lash_core::ConfigCommandDescriptor;
    pub use lash_core::ConfigCommandEntry;
    pub use lash_core::ConfigRefusal;
    pub use lash_core::ConfigRefusalReason;
    pub use lash_core::ConfigSubmitError;
    pub use lash_core::ConfigTransaction;
    pub use lash_core::ConfigTransactionOutcome;
    pub use lash_core::ConfigValueRole;
    pub use lash_core::CoreConfig;
    /// The owner of the core configuration the commands below change.
    pub use lash_core::CoreConfigOwner;
    pub use lash_core::RefusalSite;
    pub use lash_core::plugin::config::core::{
        SetAttachmentAcceptance, SetChargeSafety, SetGeneration, SetLlmProfile, SetMaxToolCalls,
        SetNoProgressBudget, SetPromptPlan, SetReasoning, SetTurnBudget,
    };
    pub use lash_execution_contracts::config_error::CoreConfigRefusal;
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
/// A run's options are [`StandardRunOptions`], which a send states under
/// [`STANDARD_RUN_OPTIONS`](standard::STANDARD_RUN_OPTIONS)
/// ([`SendBuilder::options`](crate::SendBuilder::options), FIG-6021).
pub mod standard {
    pub use lash_protocol_standard::history;
    pub use lash_protocol_standard::{
        STANDARD_PROTOCOL_PLUGIN_ID, STANDARD_RUN_OPTIONS, SetStandardRender, StandardConfigOwner,
        StandardConfigRefusal, StandardRecordedBehaviour, StandardRecordedConfig,
        StandardRenderRefusal, StandardRunOptions, StandardTurnOptions,
        section_keys as standard_section_keys,
    };
    /// The compact tool presenter standard sessions ship with, and its
    /// building blocks: the compact contract of one tool and the compact
    /// spelling of a shape.
    pub use lash_tool_presenter_standard::{
        CompactNoteRow, CompactToolContract, STANDARD_TOOL_PRESENTER_ID, StandardPresenterConfig,
        StandardToolPresenter, compact_contract, compact_contract_with_pool, compact_definition,
        compact_name_allocator, compact_note_line, compact_note_rows, compact_signature,
        compact_type,
    };
}
pub mod render {
    pub use lash_core::session_model::format_tool_output_content;
    pub use lash_protocol_standard::render::{
        AuthoredViewPolicy, BuiltinToolOutputRenderer, ResolvedStandardRenderConfig,
        StandardRenderConfig, ToolOutputRenderer, ToolOutputRendererSlot, ToolRenderParams,
        ToolRenderPatch, resolve, tool_failure_body,
    };
    #[cfg(feature = "rlm")]
    pub use lash_render::*;
}
#[cfg(feature = "codemode")]
mod kernel_migration;
#[cfg(feature = "codemode")]
mod packages;
#[cfg(feature = "rlm")]
/// RLM-specific turn-builder extensions.
pub mod rlm;
/// Reusable contracts for agent scenarios.
#[cfg(feature = "testing")]
pub mod scenario_contracts;
#[cfg(feature = "codemode")]
/// Code mode: kernel execution, worker pools and host boundaries (`codemode`).
pub mod vm;
/// Packages (FIG-6917): what a host installs as one keyed artifact of
/// operations and code beside lash's own six, registered once as a
/// [`package::HeldPackages`] from which the parent's admission and every
/// worker's registry are derived. A host builds its package against lash's
/// (`Package::build(path, release, &sources, &[kernel])`, with `kernel`
/// from `package::lash_packages` when code mode is enabled), installs both, and gives its workers
/// natives only, by function identity.
#[cfg(any(feature = "codemode", feature = "workflow-view"))]
pub mod package {
    /// The rows a node installs before it serves, and the artifacts of its
    /// own set (FIG-6918): for a host that runs its own node runner.
    pub use crate::packages::{install_rows, own_artifacts};
    pub use lash_kernel_package::*;
    /// lash's own packages as this build holds them, and why they did not
    /// assemble: a defect of the build.
    #[cfg(feature = "codemode")]
    pub use lash_vm_runtime::{LibraryError, lash_packages};
}
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
/// Read, draw, edit, publish and observe kernel workflows (`codemode`,
/// `workflow-view`, `workflow-authoring`).
#[cfg(any(feature = "codemode", feature = "workflow-view"))]
pub mod workflow;

pub use crate::admin::host_commands::{SessionCommandAdmin, SessionCommandWithdrawal};
pub use crate::admin::{AdminMutation, Completions};
pub use crate::core::{
    DeploymentDrainStatus, LashCore, LashCoreBuilder, NodeDrainError, NodeDrainReport,
    SessionDeleteCompletion, SessionDeletion,
};
pub use crate::data_retention::DataRetention;
pub use crate::durable_session::DurableSession;
pub use crate::error::{EmbedError, Result, RetiredConfigObserverRefusal, SendError};
pub use crate::parked_work::{ControlIntentPage, ControlIntentQuery, ParkedWork};
pub use crate::send::{
    BatchInput, CancelBuilder, CancelReceipt, CancelTarget, Forsaking, ParkedTurn, RunHandle,
    SendBatchBuilder, SendBuilder, SendHandle, SendOutcome, TurnEvents, TurnStatus,
};
pub use crate::session::{
    LashSession, ObservableSession, ParkedSession, SessionBuilder, SessionCreation,
    SessionParkRefused,
};
pub use crate::turn::{ToolCallRead, TurnActivityFanout, TurnOutput, TurnToolCallRef};
/// Re-exported so implementors of `#[async_trait]` facade traits (for example
/// [`tools::StaticToolExecute`]) apply the macro without carrying their own
/// `async-trait` dependency to keep version-aligned.
pub use lash_core::async_trait;
/// Store→engine delivery obligations (ADR 0109): the budget of each
/// obligation delivery attempt.
pub use lash_core::engine::RecoveryPassBudget;
/// Why a session's actor parked, read by
/// [`DurableSession::park_reason`], and the state of its unfinished turn a
/// build refused to decode.
pub use lash_core::runtime::durable::session::{
    ParkedTurnState, SessionParkReason, SessionPassFailure,
};
mod pacing;
pub use lash_core::AwaitEventKey;
pub use lash_core::AwaitEventWaitIdentity;
pub use lash_core::BatchId;
pub use lash_core::ChargeSafetyPolicy;
pub use lash_core::ChargeSafetyRefusalEvidence;
pub use lash_core::CommitBudget;
pub use lash_core::CommitBudgetLimit;
pub use lash_core::DrainMode;
pub use lash_core::DrainModePolicy;
pub use lash_core::EmptyLlmProfiles;
pub use lash_core::FrameKey;
pub use lash_core::InputId;
pub use lash_core::InputItem;
pub use lash_core::LlmCallRecord;
pub use lash_core::LlmProfileConfig;
pub use lash_core::LlmProfileKey;
pub use lash_core::LlmProfileLimits;
pub use lash_core::LlmProfileLimitsError;
pub use lash_core::LlmProfileMetadata;
pub use lash_core::LlmProfileMetadataBuilder;
pub use lash_core::LlmProfileRegistry;
pub use lash_core::LlmProfileUnavailable;
pub use lash_core::LlmProfileUnavailableReason;
pub use lash_core::LlmProfiles;
pub use lash_core::LlmUsage;
pub use lash_core::MaxToolCalls;
pub use lash_core::NoProgressBudget;
pub use lash_core::NodeId;
pub use lash_core::OutputTokenLimits;
pub use lash_core::PendingTurnInput;
pub use lash_core::PendingTurnInputCancelOutcome;
pub use lash_core::PendingTurnInputCancelReceipt;
pub use lash_core::PendingTurnInputCancelTarget;
pub use lash_core::PendingTurnInputRead;
pub use lash_core::PendingTurnInputReadStatus;
pub use lash_core::PendingTurnInputSuffixCancelOutcome;
pub use lash_core::ProcessId;
pub use lash_core::QueuedDrainCandidate;
pub use lash_core::QueuedDrainFamily;
pub use lash_core::QueuedDrainPolicy;
pub use lash_core::QueuedDrainRequest;
pub use lash_core::QueuedDrainSelection;
pub use lash_core::QueuedWorkBatchingConfig;
pub use lash_core::ReasoningRefused;
pub use lash_core::RecordedLlmProfile;
pub use lash_core::RegisteredLlmProfile;
pub use lash_core::RegistrationError;
pub use lash_core::Resolution;
pub use lash_core::ResolveOutcome;
pub use lash_core::RuntimeOwner;
pub use lash_core::SessionEntry;
pub use lash_core::SessionError;
pub use lash_core::SessionId;
pub use lash_core::SessionListFilter;
pub use lash_core::SessionRelationKind;
pub use lash_core::SessionView;
pub use lash_core::SubmittedBody;
pub use lash_core::ToolCallLimitExceeded;
pub use lash_core::ToolCallLimitScope;
pub use lash_core::TurnActivity;
pub use lash_core::TurnActivityId;
pub use lash_core::TurnBudget;
pub use lash_core::TurnEvent;
pub use lash_core::TurnFailureEvidence;
pub use lash_core::TurnFailurePartialOutput;
pub use lash_core::TurnFailureSettlement;
pub use lash_core::TurnId;
pub use lash_core::TurnInput;
pub use lash_core::TurnInputApplication;
pub use lash_core::facade_support::GenerationOverlay;
pub use lash_core::facade_support::OperationCancelOutcome;
pub use lash_core::facade_support::PluginStack;
pub use lash_core::facade_support::PluginStage;
pub use lash_core::facade_support::QueueWithdrawalPublisher;
pub use lash_core::facade_support::SessionCommand;
pub use lash_core::facade_support::SessionCommandReceipt;
pub use lash_core::facade_support::TurnActivitySink;
pub use lash_core::facade_support::TurnAddress;
pub use lash_core::facade_support::TurnAttach;
pub use lash_core::facade_support::TurnCancelOutcome;
pub use lash_core::facade_support::TurnCancelReceipt;
pub use lash_core::facade_support::TurnCancelRequest;
pub use lash_core::facade_support::TurnCancellationEvidence;
pub use lash_core::facade_support::TurnErrorCause;
pub use lash_core::facade_support::TurnErrorCode;
pub use lash_core::facade_support::TurnExecutionMetrics;
pub use lash_core::facade_support::TurnFinish;
pub use lash_core::facade_support::TurnInputAcceptanceReceipt;
pub use lash_core::facade_support::TurnOutcome;
pub use lash_core::facade_support::TurnStop;
pub use lash_core::facade_support::TurnTerminal;
pub use lash_core::facade_support::TurnWorkDriver;
pub use lash_core::facade_support::{
    TurnCancelAffectedInput, TurnCancelInputOutcome, TurnCancelMode,
    TurnCancelUndeliveredInputPolicy,
};
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
/// Lash's own execution bounds (`LashCoreBuilder::execution_budgets`), the
/// limit one executable stretch runs under, and the registration refusal of
/// a tool missing a bound its host must set.
pub use lash_core::{
    ExecutionBudgets, ExecutionBudgetsConfig, ExecutionBudgetsError, ExecutionLimit,
    ProviderAttemptLimits, RegistrationRefused,
};
/// A plugin hook's [`HookKey`](plugins::HookKey) for a string literal,
/// validated at compile time.
pub use lash_core::{hook_key, tool_provider_key};
/// The bounds on execution evidence, diagnostics and composition memory:
/// sealed policies whose refusals name the field, value and range.
pub use lash_core_store::execution_limits::{
    CompositionCacheLimits, CompositionCacheLimitsBuilder, CompositionCacheLimitsError,
    CompositionCacheLimitsField, DiagnosticLimits, DiagnosticLimitsBuilder, DiagnosticLimitsError,
    DiagnosticLimitsField, EvidenceLimits, EvidenceLimitsBuilder, EvidenceLimitsError,
    EvidenceLimitsField, LimitBounds,
};
pub use lash_execution_contracts::session_spec::SessionSpec;
pub use lash_execution_contracts::session_spec::SpecResolveError;
pub use lash_execution_contracts::session_types::SessionCreateRequest;
pub use lash_execution_contracts::session_types::UnstatedSessionConfig;
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
/// The shape a sent input runs under (FIG-3838): a [`RunSpec`] set on
/// [`SendBuilder::run`], or through its one-shot setters, the typed run
/// options a send states for a config owner under the [`RunOptionsKey`] its
/// plugin crate exports (FIG-6021), and the [`RunDefinition`]s a
/// [`LashCoreBuilder`] registers for specs to name.
pub use lash_core::{
    BindingId, CapabilityRef, ContractRef, DefinitionRef, NoRunOptionsOwner, OwnerRunOptions,
    RenderRefusal, RunDefinition, RunDefinitionRefusal, RunOptionsEncodeError, RunOptionsKey,
    RunOptionsOwner, RunOverrides, RunResolveError, RunShapeRefusal, RunSpec, SlotId,
};
/// Lash's identity for one tool call (ADR 0117): what a tool keys its
/// idempotency on, through [`tools::AttemptContext::call_id`].
pub use lash_core::{InvalidToolCallId, ToolCallId, ToolCallRootError};
pub use lash_core::{SessionAdministration, SessionDeleteContext, SessionDeleteExecution};
/// The one substrate a [`LashCore`] takes every persistence port and its
/// durable store from: the concrete durable backend over one [`StoreSet`]
/// (ADR 0132 §1). [`LashCore::builder`] requires one, which
/// [`durable::DurableBackendBuilder`] builds.
pub use lash_execution_contracts::backend::Backend;
pub use lash_execution_contracts::backend::StoreBindingId;
pub use lash_execution_contracts::backend::StoreSet;
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
pub use lash_protocol_types::{
    BlankIdentity, ErrorEnvelope, ExecCodeFailure, FrameKeyError, InvalidProcessId, LlmCallError,
    ReportedFailure, RetryProgress, ToolCallPosition, ToolCallRoot,
};
pub use lash_protocol_types::{
    llm::types::AttemptRecord, llm::types::ChargeSafetyDenialReason, llm::types::LlmCallId,
    llm::types::StreamBlockEvent, llm::types::StreamBlockIdentity, llm::types::StreamBlockKind,
};

/// `use lash::prelude::*;` brings in the daily core/session/turn vocabulary
/// without the lower-level integration types or domain modules also exposed
/// from the crate root.
pub mod prelude {
    pub use crate::{
        ChargeSafetyPolicy, DeploymentDrainStatus, DurableSession, EmbedError, InputItem, LashCore,
        LashCoreBuilder, LashSession, LlmProfileConfig, LlmProfileKey, LlmProfileLimits,
        LlmProfileLimitsError, LlmProfileMetadata, LlmProfileMetadataBuilder, LlmProfileRegistry,
        MaxToolCalls, NoProgressBudget, ObservableSession, ParkedSession,
        PendingTurnInputCancelOutcome, PluginStack, RegisteredLlmProfile, Result, SendBuilder,
        SendHandle, SendOutcome, SessionBuilder, SessionCommand, SessionCommandAdmin,
        SessionCommandReceipt, SessionCreateRequest, SessionCreation, SessionDeletion,
        SessionEntry, SessionListFilter, SessionParkRefused, SessionRelationKind, SessionSpec,
        SessionView, TurnActivity, TurnActivityFanout, TurnActivityId, TurnActivitySink,
        TurnBudget, TurnEvent, TurnExecutionMetrics, TurnFinish, TurnInput,
        TurnInputAcceptanceReceipt, TurnOutcome, TurnOutput, TurnStatus, TurnStop,
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
///
/// A provider that forwards calls to a remote tool host has that host answer
/// the typed [`ToolValue`](crate::tools::ToolValue), not presentation parts:
/// see its section on remote tool hosts.
pub mod tools {
    #[cfg(feature = "codemode")]
    pub use lash_llm_tools::{LLM_TOOLS_CONFIG, LlmToolsComponent, LlmToolsPluginFactory};
    pub use lash_protocol_types::schema_contract::OutputSchemaLimits;
    /// How an execution mode's tools read to a model: the replaceable
    /// presenter a mode holds in its slot, what it is handed and what it
    /// answers, the record a session keeps of it, and a wrapper that
    /// decorates one. [`presenter_listing`] is the catalogue listing the
    /// shipped presenters share.
    pub use lash_tool_presenter_api::{
        PresentationRefusal, PresentedToolSection, RecordedToolPresenter, ToolDisclosure,
        ToolPresentationEntry, ToolPresentationRequest, ToolPresenter, ToolPresenterSlot,
        WrappedToolPresenter, admit_over_standard, default_attachment_omission_text, import_schema,
        inline_schema, inline_shape, listing as presenter_listing, read_config,
    };
    /// The shape a presenter reads a tool's JSON Schemas as.
    pub use lash_tool_shape::{
        CanonicalIdentity, DefinitionId, ExtraKeys, ImportLimits, ObjectShape, ProcessParamShape,
        ProcessShape, ReadableNameAllocator, SchemaDefinition, SchemaDocument, SchemaShape,
        ShapeConstraints, ShapeField, ShapeImportRefusal, ShapeKind, ShapeRow, X_LASH_KEYWORD,
        XLashParam, XLashSignature, XLashType, is_named_type_reference,
    };
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_execution_contracts::tool_intent::GetDefinitionIntent;
    pub use lash_execution_contracts::tool_intent::PublishDefinitionIntent;
    pub use lash_protocol_types::{ModelTool, ToolCallStatus};

    pub use crate::tool_intent_ingress::{
        ToolIntentIngress, ToolIntentIngressKey, ToolIntentIngressOutcome, ToolIntentIngressRefusal,
    };
    pub use lash_core::AttemptContext;
    pub use lash_core::AttemptProcessReads;
    pub use lash_core::AttemptSessionReads;
    pub use lash_core::CancelHint;
    pub use lash_core::ExecutionOwner;
    pub use lash_core::IntoOperation;
    pub use lash_core::IsolatedProcessRequest;
    pub use lash_core::IsolatedStart;
    pub use lash_core::PendingCompletion;
    pub use lash_core::PendingResolver;
    pub use lash_core::PreparedToolCall;
    pub use lash_core::QualifiedOperation;
    pub use lash_core::ResolveTargetError;
    pub use lash_core::SurfaceTools;
    pub use lash_core::ToolArgumentProjectionPolicy;
    pub use lash_core::ToolAttachmentClient;
    pub use lash_core::ToolAttemptOutcome;
    pub use lash_core::ToolCall;
    pub use lash_core::ToolCallOutcome;
    pub use lash_core::ToolCallOutput;
    pub use lash_core::ToolCallRecord;
    /// Typed cancellation evidence constructed by tool implementors; pass it to
    /// [`ToolCallOutput::cancelled`] when a tool stops without completing.
    pub use lash_core::ToolCancellation;
    pub use lash_core::ToolCatalogEntry;
    pub use lash_core::ToolContract;
    pub use lash_core::ToolDefinition;
    pub use lash_core::ToolDirectCompletionClient;
    pub use lash_core::ToolDiscovery;
    pub use lash_core::ToolDraft;
    pub use lash_core::ToolExecutionGrant;
    pub use lash_core::ToolFailure;
    pub use lash_core::ToolFailureCause;
    pub use lash_core::ToolFailureClass;
    pub use lash_core::ToolFailureSource;
    pub use lash_core::ToolId;
    pub use lash_core::ToolIntentActivityOutcome;
    pub use lash_core::ToolIntentIdentity;
    pub use lash_core::ToolIntentKind;
    pub use lash_core::ToolIntentRealizedRef;
    pub use lash_core::ToolManifest;
    pub use lash_core::ToolOutcome;
    pub use lash_core::ToolOutcomeDone;
    pub use lash_core::ToolOutputContract;
    pub use lash_core::ToolPrepareCall;
    pub use lash_core::ToolPrepareContext;
    pub use lash_core::ToolProvider;
    pub use lash_core::ToolRegistry;
    pub use lash_core::ToolSessionLlmProfile;
    pub use lash_core::ToolValue;
    pub use lash_core::ToolView;
    pub use lash_core::ToolViewBlock;
    pub use lash_core::ToolViewMeta;
    pub use lash_core::facade_support::ReconfigureError;
    /// The pending model call passed to a tool's preparation hook.
    pub use lash_core::sansio::PendingToolCall;
    pub use lash_core::standard_name;
    pub use lash_core::turn_stop_from_tool_control;
    /// Per-call execution contract carried by [`ToolDefinition::with_execution_policy`].
    pub use lash_core::{Backoff, BoundedRetry, ExecutionPolicy, LimitCause};
    /// Where a tool's contract id comes from: a package's declared id, or
    /// one derived from the tool's semantic facts and its host's tag. The
    /// id itself is [`crate::workflow::document::ContractId`].
    pub use lash_core::{ContractSource, HostTag};
    /// The three capabilities a tool declares with
    /// [`ToolDefinition::with_declaration`], and what refuses a declaration
    /// or an outcome it does not admit.
    pub use lash_core::{DeclarationRefusal, OutcomeShape, ToolDeclaration};
    /// The deferred provider and what it records: a [`DeferredMatch`] per
    /// call path, committed once, with the [`ExecutionBinding`] that routes a
    /// resolved tool's calls.
    pub use lash_core::{
        DeferredMatch, DeferredOutcome, DeferredResolutionPolicy, DeferredResolveContext,
        DeferredToolProvider, DeferredToolProviderRegistration, ExecutionBinding,
        ExecutionBindingUnreadable,
    };
    /// Turn flow control constructed by tool implementors; attach it with
    /// [`ToolCallOutput::with_control`] or [`ToolOutcome::with_control`].
    pub use lash_core::{
        FinishDeclaration, NextRun, ToolControl, TurnControl, TurnControlKind, TurnControls,
    };
    /// A finish tool: its call ends the turn with its whole input as the
    /// turn's value, typed by the schema it is built with.
    pub use lash_core::{FinishToolProvider, finish_tool, finish_tool_id};
    /// The one pinned record of how a run's operation is served, in session
    /// surfaces, deferred matches and process envs alike, and the one judge
    /// every pinned call meets: the live tool's contract id must equal the
    /// pinned one.
    pub use lash_core::{Judgement, LinkSource, OperationLink, judge, operation_contract_changed};
    /// What a run's tool surface records and a provider reads: the provider
    /// keys tools are matched to, the surface entries, the run's config each
    /// provider method reads, and the refusal of a surface whose names collide.
    pub use lash_core::{
        OwnerConfigView, ProviderKey, RecordedSurface, SurfaceCollision, SurfaceDuplicateTool,
        SurfaceEntry, SurfaceRefusal, ToolSurfaceDrift, ToolSurfaceDriftKind,
    };
    /// A tool's host-set bounds: [`ToolDefinition::with_execution`] bounds
    /// its body, [`ToolDefinition::with_declaration`] the park of a tool that
    /// may defer. A tool missing one cannot be defined: the refusal names
    /// the bound.
    pub use lash_core::{ParkBound, ToolBound, ToolBounds};
    /// Tool-execution request batches, replies, and child-process observation hooks.
    pub use lash_core::{
        PreparedToolBatch, PreparedToolBatchCall, ToolChildExecutionTraceHook,
        ToolChildProcessStarted, facade_support::ToolInvocation,
        facade_support::ToolInvocationReply,
    };
    /// What answers an [`ExecutionPolicy::Read`] tool's calls inside a code
    /// cell: the reader a provider registers beside itself, with the
    /// recipe's `provider_with_reads` (ADR 0144).
    pub use lash_core::{ReadCall, ReadError, ReadRoute, ToolProviderRegistration, ToolReads};
    pub use lash_execution_contracts::DeclaredStart;
    pub use lash_execution_contracts::DeclaredStartRefused;
    pub use lash_execution_contracts::plugin_error::ToolIntentCommandFailure;
    pub use lash_execution_contracts::plugin_error::ToolIntentRuntimeFailure;
    pub use lash_execution_contracts::tool_intent::CancelProcessIntent;
    pub use lash_execution_contracts::tool_intent::StartProcessIntent;
    pub use lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_CANONICAL_BYTES;
    pub use lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_COUNT;
    pub use lash_execution_contracts::tool_intent::TOOL_INTENT_MAX_PER_KIND;
    pub use lash_execution_contracts::tool_intent::TOOL_INTENT_PROTOCOL_V3;
    pub use lash_execution_contracts::tool_intent::ToolIntent;
    /// Engine-owned tool-intent admission records used by process-registry integrators.
    pub use lash_execution_contracts::tool_intent::ToolIntentLedgerColumns;
    pub use lash_execution_contracts::tool_intent::ToolIntentLimits;
    pub use lash_execution_contracts::tool_intent::ToolIntentSubmissionAdmission;
    pub use lash_execution_contracts::tool_intent::ToolIntentSubmissionOutcome;
    pub use lash_execution_contracts::tool_intent::ToolIntentSubmissionPayload;
    pub use lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord;
    pub use lash_execution_contracts::tool_intent::ToolIntentSubmissionSettlement;
    pub use lash_execution_contracts::tool_intent::ToolIntents;
    pub use lash_execution_contracts::tool_intent::derive_tool_intent_identity;
    pub use lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome;
    pub use lash_execution_contracts::tool_intent_outcome::ToolIntentRealized;
    pub use lash_execution_contracts::tool_intent_outcome::ToolIntentRefusalReason;
    #[cfg(feature = "rlm")]
    pub use lash_protocol_rlm::DeferredResolutionError;
    #[cfg(feature = "rlm")]
    pub use lash_protocol_rlm::{
        CataloguePreviewEntry, CataloguePreviewOptions, DEFAULT_CATALOGUE_PREVIEW_CALL_NAME_LIMIT,
        DEFAULT_CATALOGUE_PREVIEW_MODULE_LIMIT, catalogue_preview,
        catalogue_preview_entries_from_catalog_records, catalogue_preview_entries_from_manifests,
        catalogue_preview_entry_from_catalog_record, catalogue_preview_entry_from_manifest,
    };
    /// The whole tool-authoring support surface: [`StaticToolProvider`] /
    /// [`StaticToolExecute`] for fixed-set providers plus the shared helpers
    /// (`invalid_tool_args`, `object_schema`, `parse_optional_usize_arg`,
    /// `LASH_VM_BINDINGS_ENABLED`) tools are built from. The glob keeps the
    /// facade complete as the crate grows; where it overlaps the explicit
    /// `rlm` re-exports above, those name the same items.
    pub use lash_tool_support::*;
}

/// Direct protocol transport types.
pub mod direct {
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_protocol_types::{
        llm::types::GenerationProjectionProvenance, llm::types::ProviderReplayMeta,
        llm::types::ProviderReplayOriginConflict, llm::types::ResponsePhase,
        llm::types::ResponseTextMeta,
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

pub mod persistence;

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
///         "text": ctx.content_policy().capture(lash::tracing::ContentCategory::Extension, || text.to_owned()),
///     })
/// }
/// ```
///
/// The accessor follows the current deployment's policy, including on
/// reopen. Hosts can drop or redact custom records before export by wrapping
/// their [`TraceSink`](crate::tracing::TraceSink) and installing it through
/// [`LashCoreBuilder::trace_sink`](crate::core::LashCoreBuilder::trace_sink).
mod plugin_diagnostics;

pub mod plugins;

/// Protocol message and content types.
pub mod messages {
    pub use lash_protocol_types::AppendVec;
    pub use lash_protocol_types::session_model::message::RenderedPrompt;
    pub use lash_protocol_types::session_model::message::{BaseRenderCache, MessageSequenceIter};
    // The vocabulary this module's signatures name (the facade-completeness rule).
    pub use lash_protocol_types::ModelToolReturnPart;

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
    /// place, a code mode value retained with its shape (FIG-1439), and a
    /// value that is inline or retained.
    pub use lash_core::{
        OutputRetentionPolicy, OutputValue, RetainedOutput, RetainedValue, ValueShape,
        ValueShapeLimits,
    };
    pub use lash_core_store::attachments::{AttachmentByteDelivery, AttachmentPutReceipt};
    pub use lash_protocol_types::FieldShape;
    pub use lash_protocol_types::{InvalidAttachmentId, InvalidMediaType};
    pub use lash_protocol_types::{
        llm::attachment_delivery::AttachmentPosition, llm::attachment_delivery::Delivery,
        llm::attachment_delivery::DeliveryContext, llm::attachment_delivery::DeliveryFetchHorizon,
        llm::attachment_delivery::DeliveryForms, llm::attachment_delivery::DeliveryLimits,
        llm::attachment_delivery::DeliverySecret, llm::attachment_delivery::ProviderAccepts,
        llm::attachment_delivery::ProviderFileScope,
    };
}

/// Secret-handling values for host-owned configuration structs.
pub mod secrets {
    /// A string wrapper whose `Debug`/`Display` render `[redacted]`, so a
    /// provider key held in a host config struct cannot leak through logs.
    pub use lash_protocol_types::Redacted;
}

/// Host process observation, definitions and lifecycle commands.
///
/// Hosts and plugins read [`ObservedProcess`]. Raw rows and their store ports
/// live in [`crate::persistence`].
pub mod process;

/// Store-author durability configuration and backend contracts.
///
/// These ports may consume stored [`crate::persistence::ProcessRecord`] rows.
/// Host observations use [`crate::process::ObservedProcess`].
pub mod durability;

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
    /// An exhausted host resource and whether its execution can be redriven.
    pub use lash_core_store::runtime_error::HostResource;
    pub use lash_core_store::turn_input_vocabulary::RunDefinitions;
    pub use lash_protocol_types::EffectIdentityError;
    pub use lash_sansio::CheckpointDelivery;
    pub use lash_sansio::sansio::{
        ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure,
        ExecutionEnvironmentSyncFailureKind,
    };

    /// The one effect context of an actor's activation (ADR 0132 §1).
    pub use lash_core::ActorContext;
    pub use lash_core::ExecutionDebts;
    /// The lazy binding of a recorded model that
    /// [`RuntimeEffectLocalExecutor::direct`] takes: bound only when an
    /// unjournaled completion's body runs.
    pub use lash_core::LlmProfileBinding;
    /// The durable session extension and turn options exposed to runtime integrators.
    pub use lash_core::ProtocolSessionExtension;
    pub use lash_core::ProtocolTurnOptions;
    /// Structured cause carried by a [`RuntimeError`], so a host distinguishes
    /// an expected retirement (a deleted session) from a real fault.
    pub use lash_core::RuntimeErrorCause;
    pub use lash_core::SessionPolicy;
    pub use lash_core::SessionSnapshot;
    /// The session-state generations an admission refused, carried in-process
    /// on the error a refused call returns (FIG-3619).
    pub use lash_core::SessionStateVersionRefusal;
    /// The record kind and diagnostic carried by a stored-data corruption cause.
    pub use lash_core::StoredDataCorruption;
    /// How a failed turn settles (FIG-3575): an outcome is recorded, a live
    /// fault aborts. A host minting a foreign error code chooses its class.
    pub use lash_core::TurnFailureCause;
    pub use lash_core::runtime::AdmittedDirectSend;
    pub use lash_core::runtime::AdmittedScope;
    pub use lash_core::runtime::AssistantResponseHookEvents;
    pub use lash_core::runtime::AssistantResponsePlan;
    pub use lash_core::runtime::AssistantStreamCapsule;
    pub use lash_core::runtime::AssistantStreamHookState;
    pub use lash_core::runtime::CheckpointAdmittedSet;
    pub use lash_core::runtime::DirectCompletionClient;
    pub use lash_core::runtime::EffectAddress;
    pub use lash_core::runtime::EmbeddedRuntimeHost;
    pub use lash_core::runtime::EventSink;
    pub use lash_core::runtime::ExecutionScope;
    pub use lash_core::runtime::LlmRequestSpec;
    pub use lash_core::runtime::LlmStreamRecord;
    pub use lash_core::runtime::NoopEventSink;
    pub use lash_core::runtime::NoopTurnActivitySink;
    pub use lash_core::runtime::ProcessCommand;
    pub use lash_core::runtime::ProcessEffectOutcome;
    pub use lash_core::runtime::ProcessListSelection;
    pub use lash_core::runtime::RunAggregateWakePolicy;
    pub use lash_core::runtime::RuntimeAttribution;
    pub use lash_core::runtime::RuntimeControlConfig;
    pub use lash_core::runtime::RuntimeDurabilityConfig;
    pub use lash_core::runtime::RuntimeEffectCommand;
    pub use lash_core::runtime::RuntimeEffectControllerError;
    pub use lash_core::runtime::RuntimeEffectEnvelope;
    pub use lash_core::runtime::RuntimeEffectInvocation;
    pub use lash_core::runtime::RuntimeEffectKind;
    pub use lash_core::runtime::RuntimeEffectLocalExecutor;
    pub use lash_core::runtime::RuntimeEffectOutcome;
    pub use lash_core::runtime::RuntimeEffectReplayMismatchReport;
    pub use lash_core::runtime::RuntimeEnvironmentBuilder;
    pub use lash_core::runtime::RuntimeError;
    pub use lash_core::runtime::RuntimeErrorCode;
    pub use lash_core::runtime::RuntimeInvocation;
    pub use lash_core::runtime::RuntimeProviderConfig;
    pub use lash_core::runtime::SleepSpec;
    pub use lash_core::runtime::TraceEmitter;
    pub use lash_core::runtime::TraceRuntime;
    pub use lash_core::runtime::TurnCancelWait;
    pub use lash_core::runtime::TurnContext;
    pub use lash_core::runtime::TurnPrelude;
    pub use lash_core::runtime::WorkCadenceError;
    /// Wall-clock milliseconds since the Unix epoch, as the runtime stamps its
    /// own process records. A host that mints a record the runtime will compare
    /// against uses the same reading rather than its own.
    pub use lash_core::runtime::current_epoch_ms;
    /// The host clock a [`Backend`](crate::Backend) is opened on, used
    /// for runtime sleeps and store timestamps. [`SystemClock`] is the
    /// wall-clock default; tests open a backend on their own to make expiry
    /// deterministic.
    pub use lash_core::{Clock, ClockWallTime, facade_support::SystemClock};
    pub use lash_execution_contracts::session_types::SessionHandle;
    pub use lash_execution_contracts::turn_prelude::TurnPreludeRef;
}

/// Structural admission of host JSON before DTO allocation. The budget is
/// independent of fixed wire-validity limits and Serde's recursion guard.
pub mod json_decode {
    pub use lash_protocol_types::{
        json_decode::JsonDecodeError, json_decode::JsonDecodeLimits, json_decode::JsonDecodeUsage,
    };
}

/// Trace context, events, and sink configuration.
pub mod tracing;

/// Fixture constructors and test support for non-shipped targets only.
///
/// Cargo features are additive: keep `testing` out of shipped dependency
/// feature sets, including features enabled by other workspace members.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// JSON-schema contracts, projection policies, and provider dialect
/// projection. This is the vocabulary a tool schema and a provider request
/// share: [`SchemaContract`] declares what a schema promises, and
/// [`project_for_dialect`] renders it for one provider dialect.
pub mod schema {
    /// Shipped JSON Schema and TypeScript text for every host contract.
    pub mod host;
    pub use lash_protocol_types::schema_contract::*;
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
/// decides the lifetime of every process a model's `start_process` declares,
/// and its [`ProcessEnvPolicy`](crate::process::ProcessEnvPolicy) the
/// environment and engine settings. A process whose environment names
/// [`PROCESS_CONTROLS_CONFIG`]'s component gets the same tools through
/// [`ProcessControlsComponent`].
pub mod process_controls {
    pub use lash_plugin_process_controls::{
        PROCESS_CONTROLS_CONFIG, PROCESS_CONTROLS_PROVIDER, ProcessControlTool,
        ProcessControlsComponent, SESSION_PROCESS_CONTROLS_PROVIDER,
        SessionProcessAdminPluginFactory, process_tool_definition,
    };
}

/// The dialect seam of the kernel: what a worker installs for a language
/// (its front end, printer and helper functions), the library a front end
/// names functions through, and the diagnostics both answer.
#[cfg(feature = "codemode")]
/// Integrator/read/edit seam: frontend operations expose typed documents;
/// retain runnable host artifacts as `workflow::document::DocumentBytes`.
pub mod dialect {
    pub use lash_kernel_dialect::*;
}

/// The TypeScript dialect of the kernel: its front end, its printer and
/// their diagnostics.
#[cfg(feature = "typescript")]
pub mod typescript {
    pub use lash_dialect_typescript::*;
}

/// Python source operations: authored cell lowering, exact generated printing
/// and library-free generated compilation. Decoding never admits a workflow.
#[cfg(feature = "python")]
pub mod python {
    pub use lash_dialect_python::*;
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
    /// Attachment delivery contracts. Their read limits live at
    /// [`crate::persistence::AttachmentReadPolicy`], beside the host's attachment policy.
    pub use lash_core::provider::{
        AttachmentDeliveryError, AttemptEndReason, AttemptTerminal, CanonicalDeliveries,
        InlineDeliveries, LiveCallHorizon, NoSlotDeliveries, ProviderAttempt,
        ProviderAttemptControl, ProviderCancellationGuard, ProviderTerminalPolicy, SlotDeliveries,
    };
    /// Read the request a canonical body says, for an in-process model that
    /// decides from what a call asks.
    pub use lash_core::provider::{canonical_call, canonical_request};
    /// The admitted request template and the transient body filled for one attempt
    /// (ADR 0133 §6). Only literals, refs, acceptance and codecs are recorded.
    pub use lash_protocol_types::{
        llm::types::AttachmentSlot, llm::types::BodyPart, llm::types::LiveRequestBody,
        llm::types::LoweredItem, llm::types::RecordedRequestTemplate, llm::types::RequestSegment,
        llm::types::RequestTemplateBuilder, llm::types::SegmentKind, llm::types::SegmentPart,
        llm::types::SlotCodec, llm::types::TemplateError, llm::types::TemplateJson,
        llm::types::TransientJson, llm::types::WireChunk,
    };
    pub use lash_protocol_types::{
        llm::types::LlmProviderTraceDirection, llm::types::LlmProviderTraceEvent,
        llm::types::LlmProviderTraceSender, llm::types::ProviderReasoningRetentionSupport,
    };
    /// Reuse of what a provider lowered before: each unit of history by the
    /// identity of the messages it was lowered from (FIG-6443).
    pub use lash_protocol_types::{
        llm::types::LoweredUnit, llm::types::SEGMENT_CACHE_BUDGET_BYTES, llm::types::SegmentCache,
        llm::types::SegmentCacheStats, llm::types::SegmentContext, llm::types::SegmentContextKey,
        llm::types::SegmentKey,
    };
    /// What a provider's `send` reads its response under, beside the body:
    /// the call's scope, its recorded contract and the send's live senders.
    pub use lash_protocol_types::{
        llm::types::ResponseContext, llm::types::ResponseContract, llm::types::ToolCallContract,
    };
    pub use lash_transport_failure::{
        HttpFailureContext, LlmTransportError, TransportRetryVerdict, retry_after_from_headers,
    };

    /// Typed provider-failure classification surfaced on
    /// [`TurnIssue`](crate::turn::TurnIssue) and session error envelopes.
    pub use lash_core::ProviderFailureKind;
    /// Why a host-supplied [`LlmProfileCapability`] rejected a reasoning-effort
    /// selection. The snake_case [`LlmProfileEffortValidationCategory`] codes are a
    /// stable contract a capability catalog can branch on.
    pub use lash_core::facade_support::LlmProfileEffortValidationCategory;
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
        AttemptOutcome, AttemptUsageOutcome, ChargeSafetyDecision, DiscardReason,
        DiscardedAfterCell, ExecutionEvidence, ExecutionEvidenceCollectionInterruption,
        ExecutionEvidenceMergeError, LlmRequest, LlmRequestOwner, LlmRequestScope, LlmResponse,
        LlmStreamEvidence, LlmTurnScope, NormalizedError, ProtocolPosition, ProviderEndpointError,
        RetryClass, RetryDecision, RetryDeclineCause, RetryWait,
    };
    /// The namespaced failure code carried on
    /// [`LlmTransportError`] and attempt
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
    pub use crate::turn::CommittedTurnsWithCallsPage;
    pub use lash_core::transcript::{
        CommittedTurn, CommittedTurnWithCalls, CommittedTurnsPage, EntryId, EntryProvenance,
        SessionTranscript, SuppressionReason, ToolResultBlock, TranscriptBlock, TranscriptDecoders,
        TranscriptEntry, TranscriptItem, TranscriptMessage, TranscriptRole,
    };
    /// One executed code cell: the record its protocol committed, which a
    /// [`TranscriptItem::Cell`] returns, a code executor's response reports
    /// the prints and result of, and a completion activity carries.
    pub use lash_core::{BindingChanges, CellControl, CellOutcome, CellPrint, CellRecord};
}

/// Presentation cuts for runtime value replies and raw transcript errors.
pub use lash_core::session_model::RuntimeOutputCuts;

pub use lash_core::tool_live::{
    CallAddress, TOOL_LIVE_ENVELOPE_BYTES, ToolLive, ToolLiveCursor, ToolLiveDiagnostics,
    ToolLiveEmission, ToolLiveEmissionId, ToolLiveError, ToolLiveFact, ToolLiveFaultSource,
    ToolLiveFilter, ToolLiveGapReason, ToolLiveItem, ToolLiveLogLevel, ToolLiveMemoryWindow,
    ToolLiveOmission, ToolLiveOwnerOmission, ToolLivePayload, ToolLivePolicy, ToolLivePolicyBound,
    ToolLivePolicyError, ToolLivePolicyField, ToolLivePolicyRelation, ToolLivePolicySnapshot,
    ToolLivePolicyValue, ToolLivePublisher, ToolLiveReadFault, ToolLiveRecord, ToolLiveReplay,
    ToolLiveRetention, ToolLiveSubscription,
};
