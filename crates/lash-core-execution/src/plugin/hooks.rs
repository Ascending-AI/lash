use crate::SessionId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::*;

pub type PluginFuture<T> = Pin<Box<dyn Future<Output = Result<T, PluginError>> + Send>>;
pub type PluginLifecycleFuture = PluginFuture<()>;
pub type PluginLifecycleEventHook =
    Arc<dyn Fn(PluginLifecycleEvent) -> PluginLifecycleFuture + Send + Sync>;
pub type PluginSessionTask = PluginFuture<()>;
/// A before-turn observer: what it contributes to the turn being prepared.
pub type BeforeTurnHook =
    Arc<dyn Fn(TurnHookContext) -> PluginFuture<TurnContributions> + Send + Sync>;
/// One composable presentation step (ADR 0099 §6 presentation boundary,
/// FIG-3420): folds the previous step's `ModelToolReturn` with the recorded
/// settlement into the next. Pure over its inputs; anything impure it needs
/// (retaining bytes) goes through
/// [`ToolResultProjectionContext::artifacts`], which is journaled.
pub struct ToolPresentationInput {
    /// The return the chain has produced so far — `ModelToolReturn::from_output`
    /// before the first step, then each prior step's answer.
    pub previous: crate::ModelToolReturn,
    /// The settlement the presented result is being recorded into. Read-only
    /// evidence for the step: its `model_return` is the pre-chain baseline.
    pub settlement: Arc<crate::runtime::effect::ToolSettlement>,
    pub context: ToolResultProjectionContext,
}

/// A decision-only presentation transform, run in the recorded plan's order
/// inside `PresentToolResult`. It proposes no namespace state commands.
/// Only sequential before/after-turn, checkpoint and after-tool result checks
/// may propose commands; their publication belongs to the Run coordinator.
pub type ToolPresentationStep =
    Arc<dyn Fn(ToolPresentationInput) -> PluginFuture<crate::ModelToolReturn> + Send + Sync>;
pub type ToolPresentationPresenter = Arc<
    dyn Fn(
            ToolPresentationInput,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            crate::ModelToolReturn,
                            crate::runtime::effect::RuntimeEffectControllerError,
                        >,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// The one retention capability of a tool presentation (FIG-3420, FIG-1643),
/// journaled by the `PresentToolResult` boundary that runs the chain: a blob
/// retained here is `put` on the first run and the recorded
/// `crate::AttachmentRef` is what replay serves.
///
/// The standard renderer retains a cut output through it, a step may retain
/// what it presents, and the boundary itself retains whatever the folded
/// return still carries past [`Self::retention_policy`]. A retention that
/// fails ends the presentation with its typed attachment-store cause, even
/// when a step catches it. Transient faults retry the uncommitted derivation;
/// permanent refusals record [`OutputRetentionRefused`](crate::RuntimeErrorCode::OutputRetentionRefused).
/// A presentation never records a retention failure as text.
pub trait ToolPresentationArtifacts: Send + Sync {
    /// Retain `text` under `label`, returning the content-addressed reference
    /// the session now references.
    fn retain_text<'a>(
        &'a self,
        label: &'a str,
        text: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::AttachmentRef, crate::AttachmentStoreError>>
                + Send
                + 'a,
        >,
    >;

    /// The byte policy the boundary measures the folded return against. The
    /// recorded presentation journals it, so a replay under another policy
    /// serves the decision this one made.
    fn retention_policy(&self) -> crate::OutputRetentionPolicy {
        crate::OutputRetentionPolicy::DEFAULT
    }

    /// The refs retained so far, in retain order. The runtime journals them on
    /// the recorded presentation outcome; a step reads its own `retain_text`
    /// return, never this.
    fn retained(&self) -> Vec<crate::AttachmentRef> {
        Vec::new()
    }

    /// Why the first retention that failed while the chain ran failed, if
    /// one did.
    fn retention_failure(&self) -> Option<crate::RuntimeEffectControllerError> {
        None
    }
}

/// A presentation context that retains nothing: `retain_text` answers a
/// refusal, and the boundary fails the presentation with a typed retention
/// failure rather than pretending a retention happened.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPresentationArtifacts;

const NO_PRESENTATION_ARTIFACTS: &str = "this presentation context retains no artifacts";

impl ToolPresentationArtifacts for NoPresentationArtifacts {
    fn retain_text<'a>(
        &'a self,
        _label: &'a str,
        _text: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::AttachmentRef, crate::AttachmentStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Err(crate::AttachmentStoreError::Contract(
                NO_PRESENTATION_ARTIFACTS.to_string(),
            ))
        })
    }
}
/// An after-turn observer: what it contributes before the turn commits.
pub type AfterTurnHook =
    Arc<dyn Fn(TurnResultHookContext) -> PluginFuture<AfterTurnContributions> + Send + Sync>;
/// A checkpoint observer: what it contributes at the checkpoint.
pub type CheckpointHook =
    Arc<dyn Fn(CheckpointHookContext) -> PluginFuture<TurnContributions> + Send + Sync>;
pub type ToolCatalogContributor =
    Arc<dyn Fn(ToolCatalogContext) -> Result<ToolCatalogContribution, PluginError> + Send + Sync>;
pub type AssistantStreamHook =
    Arc<dyn Fn(AssistantStreamHookContext) -> PluginFuture<AssistantStreamTransform> + Send + Sync>;
/// **This hook is at-least-once, and idempotency is your obligation.** It runs
/// as phase 2 of a staged effect boundary: the raw completion is journaled
/// first, so it is durable before this hook is ever called and is never
/// re-bought from the provider. The price of that guarantee is that a crash,
/// redrive, or hook failure replays the *same* raw completion into this hook
/// again. Treat every invocation as a pure derivation of
/// [`AssistantResponseHookContext::response`] and
/// [`AssistantResponseHookContext::stream_state`]: same input, same
/// [`AssistantResponseTransform`], no ambient side effects. In particular the
/// hook never reads state its plugin's stream hooks left in memory: phase 2
/// may run on another worker, or after a restart, from the journal alone. What
/// the stream hooks learned reaches it only as the state its
/// [`AssistantStreamFinishedHook`] returned. Side effects belong
/// on the effect seam, where they are journaled — not in this hook, where a
/// second invocation would repeat them.
///
/// Returning `Err` is not data loss: it marks the derivation incomplete, and
/// the paid completion remains in the journal to be derived again.
///
/// Events returned in [`AssistantResponseTransform::events`] are journaled with
/// this phase's outcome and served from it on replay, so they keep their
/// placement and are never re-emitted by a redrive that replays the entry.
///
/// Phase 1 records an ordered
/// [`AssistantResponsePlan`](crate::runtime::AssistantResponsePlan) of callback
/// keys and owning revisions selected before the paid call. Replay and redrive
/// follow that plan. An empty plan serves the raw completion even if hooks
/// were installed later. A completed derivation serves its recorded outcome
/// without invoking hooks. An unfinished derivation resolves every recorded
/// callback before invoking any, and parks if a key or revision is unavailable.
pub type AssistantResponseHook = Arc<
    dyn Fn(AssistantResponseHookContext) -> PluginFuture<AssistantResponseTransform> + Send + Sync,
>;
/// Runs once the provider stream of an LLM call finished, in phase 1 of the
/// staged boundary. The value it returns for a stream that produced a
/// response ([`AssistantStreamFinishReason::Complete`] or
/// [`AssistantStreamFinishReason::Aborted`]) is recorded with the raw completion
/// for each response callback of its plugin that names this callback's key
/// as its `stream_state_from`. The receiving callback's key and owning
/// revision identify that state in [`AssistantResponseHookContext::stream_state`].
/// Return `None` when phase 2 needs nothing from the stream. The hook should
/// leave no per-stream state behind: the next stream starts from nothing.
pub type AssistantStreamFinishedHook = Arc<
    dyn Fn(AssistantStreamFinishedContext) -> PluginFuture<Option<serde_json::Value>> + Send + Sync,
>;

#[derive(Clone)]
pub struct TurnHookContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionStateService>,
    pub turn_context: crate::TurnContext,
}

#[derive(Clone)]
pub struct SessionConfigChangedContext {
    pub session_id: SessionId,
    pub previous: SessionPolicy,
    pub current: SessionPolicy,
    pub sessions: Arc<dyn SessionReadService>,
}

#[derive(Clone)]
pub struct SessionStateChangedContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionReadService>,
}

#[derive(Clone)]
pub enum PluginLifecycleEvent {
    TurnFinalized(Arc<AssembledTurn>),
    /// Best-effort observer hook emitted after durable session state advances.
    ///
    /// Hook failures cannot affect the commit, which has already completed, but
    /// they are returned to the host as advisory `lifecycle_hook_failed` turn issues.
    TurnPersisted(Box<SessionStateChangedContext>),
    SessionRestored(SessionReadView),
    SessionConfigChanged(Box<SessionConfigChangedContext>),
}

#[derive(Clone, Debug)]
pub struct TurnHookReport {
    pub outcome: crate::TurnOutcome,
    pub assistant_output: crate::runtime::AssistantOutput,
    pub execution: crate::runtime::TurnExecutionMetrics,
    pub token_usage: crate::TokenUsage,
    pub tool_calls: Arc<Vec<crate::ToolCallRecord>>,
    pub omitted: Option<crate::OmittedToolCalls>,
    pub errors: Arc<Vec<crate::runtime::TurnIssue>>,
}

impl TurnHookReport {
    pub fn from_assembled(turn: &AssembledTurn) -> Self {
        Self {
            outcome: turn.outcome.clone(),
            assistant_output: turn.assistant_output.clone(),
            execution: turn.execution.clone(),
            token_usage: turn.token_usage.clone(),
            tool_calls: Arc::new(turn.tool_calls.clone()),
            omitted: turn.omitted.clone(),
            errors: Arc::new(turn.errors.clone()),
        }
    }
}

#[derive(Clone)]
pub struct ToolResultProjectionContext {
    /// Who the call runs for: a session, or a process runtime.
    pub owner: crate::RuntimeOwner,
    pub call_id: crate::ToolCallId,
    pub tool_id: crate::ToolId,
    pub tool_name: String,
    pub render: Option<crate::RecordedRender>,
    pub args: serde_json::Value,
    pub output: crate::ToolCallOutput,
    pub duration_ms: u64,
    /// The journaled artifact capability a presentation step retains bytes
    /// through (FIG-3420); [`NoPresentationArtifacts`] where the boundary
    /// supplies none.
    pub artifacts: Arc<dyn ToolPresentationArtifacts>,
}

#[derive(Clone)]
pub struct TurnResultHookContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub turn: Arc<TurnHookReport>,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_graph: Arc<dyn SessionGraphService>,
}

#[derive(Clone)]
pub struct CheckpointHookContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub checkpoint: CheckpointKind,
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
}

#[derive(Clone)]
pub struct AssistantStreamHookContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub chunk: String,
}

#[derive(Clone, Debug, Default)]
pub struct AssistantStreamTransform {
    pub chunk: String,
    pub reasoning_deltas: Vec<String>,
    pub events: Vec<PluginRuntimeEvent>,
    /// When `true`, the runtime cancels the in-flight LLM call the
    /// moment the chunk's hooks return and finalizes the turn using
    /// whatever text has been streamed so far. The stop is sticky: once
    /// any hook raises it for a chunk, the stream stops, whatever later
    /// hooks return. Used by protocol plugins to enforce
    /// one-block-per-turn contracts (for example, aborting as soon as
    /// the first protocol-owned code fence closes).
    pub abort_stream: bool,
}

#[derive(Clone)]
pub struct AssistantResponseHookContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub response: crate::LlmResponse,
    /// The state the paired [`AssistantStreamFinishedHook`] returned when
    /// the completion's stream finished. Phase 1 records it under this exact
    /// response callback identity and revision. `None` when the paired hook
    /// returned nothing or the completion did not stream.
    pub stream_state: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct AssistantResponseTransform {
    pub response: crate::LlmResponse,
    pub events: Vec<PluginRuntimeEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssistantStreamFinishReason {
    /// The provider abandoned one streaming attempt and will retry the same
    /// logical request from an empty response state.
    AttemptReset,
    Complete,
    Aborted,
    Cancelled,
    ProviderError,
}

#[derive(Clone)]
pub struct AssistantStreamFinishedContext {
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub reason: AssistantStreamFinishReason,
}

/// The session `owner` names, or [`PluginError::NotASessionRuntime`] naming
/// `operation` for a process runtime.
pub fn require_session_owner<'a>(
    owner: &'a crate::RuntimeOwner,
    operation: &'static str,
) -> Result<&'a SessionId, PluginError> {
    match owner {
        crate::RuntimeOwner::Session(session_id) => Ok(session_id),
        crate::RuntimeOwner::Process(process_id) => {
            Err(crate::runtime::not_a_session_runtime(operation, process_id))
        }
    }
}

/// The trace context of work `owner` does: its session, or none for a
/// process runtime.
pub(crate) fn owner_trace_context(owner: &crate::RuntimeOwner) -> lash_trace::TraceContext {
    match owner {
        crate::RuntimeOwner::Session(session_id) => {
            lash_trace::TraceContext::default().for_session(session_id.clone())
        }
        crate::RuntimeOwner::Process(_) => lash_trace::TraceContext::default(),
    }
}
