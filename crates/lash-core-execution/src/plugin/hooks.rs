use crate::SessionId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::*;

pub type PluginFuture<T> = Pin<Box<dyn Future<Output = Result<T, PluginError>> + Send>>;
pub type PluginLifecycleFuture<'run> =
    Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'run>>;
pub type PluginLifecycleEventHook =
    Arc<dyn for<'run> Fn(PluginLifecycleEvent<'run>) -> PluginLifecycleFuture<'run> + Send + Sync>;
pub type PluginSessionTask = PluginFuture<()>;
pub type SessionConfigMutator = Arc<
    dyn Fn(SessionConfigChangedContext, SessionPolicy) -> PluginFuture<SessionPolicy> + Send + Sync,
>;
pub type BeforeTurnHook =
    Arc<dyn Fn(TurnHookContext) -> PluginFuture<Vec<TurnPluginDirective>> + Send + Sync>;
/// Inspects a tool call before dispatch and returns directives for the runtime to apply.
///
/// A hook may be invoked more than once for one call when a later hook replaces the arguments.
/// That bounded reinspection honors only restrictive terminal directives (`AbortTurn` and denied
/// or cancelled `ShortCircuitTool`); side effects already applied from the initial pass are not
/// applied again. A replacement emitted during reinspection is rejected as a typed composition
/// error.
pub type BeforeToolCallHook = Arc<
    dyn Fn(ToolCallHookContext) -> PluginFuture<Vec<BeforeToolCallPluginDirective>> + Send + Sync,
>;
/// Inspects a tool result after execution and returns directives for the runtime.
///
/// A hook may be invoked more than once for one call when a later hook successfully replaces the
/// result. Earlier hooks then reinspect that candidate once before the chain continues with the
/// effective first-emitted replacement. Reinspection honors only restrictive terminal directives
/// (`AbortTurn` and denied or cancelled `ShortCircuitTool`); side effects are not applied again. A
/// successful replacement emitted during reinspection is rejected as a typed composition error.
pub type AfterToolCallHook = Arc<
    dyn Fn(ToolResultHookContext) -> PluginFuture<Vec<AfterToolCallPluginDirective>> + Send + Sync,
>;
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

/// A registered presentation step, run in registration order inside the
/// journaled `PresentToolResult` boundary.
pub type ToolPresentationStep =
    Arc<dyn Fn(ToolPresentationInput) -> PluginFuture<crate::ModelToolReturn> + Send + Sync>;

/// The impure capability a presentation step may need, journaled by the
/// `PresentToolResult` boundary that runs the chain: a blob retained here is
/// `put` once on the first run and the recorded `crate::AttachmentRef` is what
/// replay serves.
pub trait ToolPresentationArtifacts: Send + Sync {
    /// Retain `text` under `label`, returning the content-addressed reference
    /// the session now references.
    fn retain_text<'a>(
        &'a self,
        label: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<crate::AttachmentRef, PluginError>> + Send + 'a>>;

    /// The refs retained so far, in retain order. The runtime journals them on
    /// the recorded presentation outcome; a step reads its own `retain_text`
    /// return, never this.
    fn retained(&self) -> Vec<crate::AttachmentRef> {
        Vec::new()
    }
}

/// A presentation context that retains nothing: `retain_text` answers a typed
/// refusal so a step that requires artifacts fails into the chain's recorded
/// fallback rather than pretending a retention happened.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPresentationArtifacts;

impl ToolPresentationArtifacts for NoPresentationArtifacts {
    fn retain_text<'a>(
        &'a self,
        _label: &'a str,
        _text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<crate::AttachmentRef, PluginError>> + Send + 'a>> {
        Box::pin(async move {
            Err(PluginError::Session(
                "this presentation context retains no artifacts".to_string(),
            ))
        })
    }
}
pub type AfterTurnHook =
    Arc<dyn Fn(TurnResultHookContext) -> PluginFuture<Vec<AfterTurnPluginDirective>> + Send + Sync>;
pub type CheckpointHook =
    Arc<dyn Fn(CheckpointHookContext) -> PluginFuture<Vec<TurnPluginDirective>> + Send + Sync>;
pub type PromptContributor =
    Arc<dyn Fn(PromptHookContext) -> PluginFuture<Vec<PromptContribution>> + Send + Sync>;
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
/// [`AssistantResponseHookContext::response`]: same input, same
/// [`AssistantResponseTransform`], no ambient side effects. Side effects belong
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
/// Out of contract, stated so it is not discovered: the journal records the
/// *phase*, not the hook set that produced it. Registering a response hook on a
/// session whose earlier drives had none makes phase 2 run live against a
/// replayed completion and append a new entry; removing the last one orphans an
/// already-journaled entry and silently uses the raw response. Change the hook
/// set between drives of the same session only when both readings are
/// acceptable.
pub type AssistantResponseHook = Arc<
    dyn Fn(AssistantResponseHookContext) -> PluginFuture<AssistantResponseTransform> + Send + Sync,
>;
pub type AssistantStreamFinishedHook =
    Arc<dyn Fn(AssistantStreamFinishedContext) -> PluginFuture<()> + Send + Sync>;

#[derive(Clone)]
pub struct PromptHookContext {
    pub session_id: SessionId,
    pub sessions: Arc<dyn SessionStateService>,
    pub state: SessionReadView,
    pub protocol_turn_options: ProtocolTurnOptions,
    pub turn_context: crate::TurnContext,
}

#[derive(Clone)]
pub struct TurnHookContext {
    pub session_id: SessionId,
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionStateService>,
    pub turn_context: crate::TurnContext,
}

#[derive(Clone)]
pub struct SessionConfigChangedContext {
    pub session_id: SessionId,
    pub previous: SessionPolicy,
    pub current: SessionPolicy,
    pub sessions: Arc<dyn SessionStateService>,
}

#[derive(Clone)]
pub struct SessionStateChangedContext<'run> {
    pub session_id: SessionId,
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
}

#[derive(Clone)]
pub enum PluginLifecycleEvent<'run> {
    TurnFinalized(Arc<AssembledTurn>),
    /// Best-effort observer hook emitted after durable session state advances.
    ///
    /// Hook failures cannot affect the commit, which has already completed, but
    /// they are returned to the host as `lifecycle_hook_failed` turn issues.
    TurnPersisted(Box<SessionStateChangedContext<'run>>),
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
pub struct ToolCallHookContext {
    pub session_id: SessionId,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub argument_projection: crate::ToolArgumentProjectionPolicy,
    pub turn_context: crate::TurnContext,
    pub(crate) sessions: Arc<dyn SessionStateService>,
}

impl ToolCallHookContext {
    pub fn new(
        session_id: SessionId,
        tool_name: String,
        args: serde_json::Value,
        argument_projection: crate::ToolArgumentProjectionPolicy,
        turn_context: crate::TurnContext,
        sessions: Arc<dyn SessionStateService>,
    ) -> Self {
        Self {
            session_id,
            tool_name,
            args,
            argument_projection,
            turn_context,
            sessions,
        }
    }

    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(&self.session_id).await
    }

    pub async fn set_tool_membership(
        &self,
        names: &[String],
        present: bool,
    ) -> Result<u64, PluginError> {
        self.sessions
            .set_tool_membership(&self.session_id, names, present)
            .await
    }
}

#[derive(Clone)]
pub struct ToolResultHookContext {
    pub session_id: SessionId,
    /// The durable identity of the prepared call this observation belongs to:
    /// the same value the attempt body saw as [`crate::AttemptContext::tool_call_id`]
    /// and the executed-call record carries as [`crate::ToolCallRecord::call_id`].
    /// A host correlating this observation with its own records — an effect
    /// ledger, an audit trail — keys on this rather than on tool name or args.
    /// It is a correlator, not a receipt: retry and reinspection invoke the
    /// hook more than once for one call, so observations deduplicate on it.
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: ToolOutcome,
    pub duration_ms: u64,
    pub turn_context: crate::TurnContext,
    pub(crate) sessions: Arc<dyn SessionStateService>,
}

impl ToolResultHookContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
        result: ToolOutcome,
        duration_ms: u64,
        turn_context: crate::TurnContext,
        sessions: Arc<dyn SessionStateService>,
    ) -> Self {
        Self {
            session_id,
            call_id,
            tool_name,
            args,
            result,
            duration_ms,
            turn_context,
            sessions,
        }
    }

    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(&self.session_id).await
    }

    pub async fn set_tool_membership(
        &self,
        names: &[String],
        present: bool,
    ) -> Result<u64, PluginError> {
        self.sessions
            .set_tool_membership(&self.session_id, names, present)
            .await
    }
}

#[derive(Clone)]
pub struct ToolResultProjectionContext {
    pub session_id: SessionId,
    pub call_id: String,
    pub tool_name: String,
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
    pub turn: Arc<TurnHookReport>,
    pub sessions: Arc<dyn SessionStateService>,
}

#[derive(Clone)]
pub struct CheckpointHookContext {
    pub session_id: SessionId,
    pub checkpoint: CheckpointKind,
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
}

#[derive(Clone)]
pub struct AssistantStreamHookContext {
    pub session_id: SessionId,
    pub chunk: String,
}

#[derive(Clone, Debug, Default)]
pub struct AssistantStreamTransform {
    pub chunk: String,
    pub reasoning_deltas: Vec<String>,
    pub events: Vec<PluginRuntimeEvent>,
    /// When `true`, the runtime cancels the in-flight LLM call the
    /// moment this hook returns and finalizes the turn using whatever
    /// text has been streamed so far. Any plugin may set this — the
    /// first to raise it wins. Used by protocol plugins to enforce
    /// one-block-per-turn contracts (for example, aborting as soon as
    /// the first protocol-owned code fence closes).
    pub abort_stream: bool,
}

#[derive(Clone)]
pub struct AssistantResponseHookContext {
    pub session_id: SessionId,
    pub response: crate::LlmResponse,
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
    pub reason: AssistantStreamFinishReason,
}
