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
/// Facts a plugin may use to contribute repair text to a dialect failure.
/// Admitted grants and searchable deferred names are separate: discovery
/// names carry no authority to call a tool.
#[derive(Clone, Debug)]
pub struct CellFailureDiagnostic<'a> {
    pub code: &'a str,
    /// Byte offsets in the authored cell source.
    pub span: Option<(usize, usize)>,
    /// The name at the span for an unknown-name diagnostic, otherwise absent.
    pub unknown_name: Option<&'a str>,
    /// Each offered operation, by its effect name (`tools.lookup`), to its
    /// callable spelling in this cell's dialect.
    pub grants: &'a std::collections::BTreeMap<String, String>,
    /// The run's available deferred provider's searchable names to its resolver
    /// call paths, under the run's configuration and capabilities. Names only:
    /// this contains no definitions, resolved matches or authority to call them.
    pub deferred: &'a std::collections::BTreeMap<String, String>,
}

/// Pure repair contributions, appended in registration order to the cell's
/// failure feedback. Implementations must be deterministic and perform no I/O
/// or state changes. The cell records the resulting feedback with its failure;
/// replay reads that result. No installed hooks means no added repair text.
pub type CellFailureFeedbackHook =
    Arc<dyn Fn(&CellFailureDiagnostic<'_>) -> Vec<String> + Send + Sync>;

/// Realized declarations available to presentation callbacks.
#[derive(Clone, Debug, Default)]
pub struct ToolPresentationFacts {
    pub intent_outcomes:
        Vec<lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome>,
}

/// One presentation step folds the previous return with recorded declaration facts.
/// Retaining bytes goes through the journaled artifact port.
pub struct ToolPresentationInput {
    /// The return the chain has produced so far — `ModelToolReturn::from_output`
    /// before the first step, then each prior step's answer.
    pub previous: crate::ModelToolReturn,
    /// Read-only evidence of the declarations realized before presentation.
    pub facts: Arc<ToolPresentationFacts>,
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
    fn retention_policy(&self) -> crate::OutputRetentionPolicy;

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
/// failure rather than pretending a retention happened. It measures outputs
/// against [`OutputRetentionPolicy::STANDARD`](crate::OutputRetentionPolicy::STANDARD),
/// so an output a standard host would retain is refused here.
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

    fn retention_policy(&self) -> crate::OutputRetentionPolicy {
        crate::OutputRetentionPolicy::STANDARD
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
/// Transforms each visible chunk of a model call's assistant text as it
/// streams. **It must be replay-safe.** The chunks it was handed are recorded
/// in the call's receipt (`model.answer`), and a derivation feeds them to it
/// again, in order, on whichever worker derives: on the worker that streamed,
/// after a crash, or after a derivation fault. A replayed chunk's
/// [`AssistantStreamTransform`] is discarded: its chunk, reasoning and events
/// were published while the call streamed, and its stop was already honoured.
/// So the hook's only lasting effect is the per-stream state its
/// [`AssistantStreamFinishedHook`] reads, which must be a pure function of the
/// chunks it was handed since its last finish.
pub type AssistantStreamHook =
    Arc<dyn Fn(AssistantStreamHookContext) -> PluginFuture<AssistantStreamTransform> + Send + Sync>;
/// **This hook is at-least-once, and idempotency is your obligation.** It
/// derives the response of a durable turn's model call from that call's
/// receipt: the provider's raw outcome, committed under `model.answer` and
/// acknowledged before this hook, or any stream hook's finish, runs. A
/// durably recorded response is never purchased again; the price is that a
/// crash, an abort or a hook failure before the derived response commits
/// derives again from the *same* receipt, calling this hook again. Treat
/// every invocation as a pure derivation of
/// [`AssistantResponseHookContext::response`] and
/// [`AssistantResponseHookContext::stream_state`]: same input, same
/// [`AssistantResponseTransform`], no ambient side effects (no clock, random
/// value, mutable external state or unkeyed external write). In particular
/// the hook never reads state its plugin's stream hooks left in memory: the
/// derivation may run on another worker, or after a restart, from the
/// receipt alone. What the stream hooks learned reaches it only as the state
/// its [`AssistantStreamFinishedHook`] returns when the derivation finishes
/// the receipt's recorded stream. Nothing this hook returns is recorded on
/// its own: its response and events commit with the turn's next phase
/// (`model.done`, a cell's first save or `turn.commit`), and a derivation
/// that did not reach that commit runs again. Side effects belong on a
/// durable effect seam, not in this hook, where a second invocation would
/// repeat them.
///
/// Returning `Err` is not data loss: it marks the derivation incomplete, and
/// the receipt remains to be derived again.
///
/// The receipt records an ordered
/// [`AssistantResponsePlan`](crate::runtime::AssistantResponsePlan) of callback
/// keys and owning plugin ids selected before the paid call. Every derivation
/// follows that plan. An empty plan serves the raw completion even if hooks
/// were installed later. A derivation resolves every recorded callback before
/// invoking any, and parks if a key is unavailable or its stream-state format
/// is unreadable.
pub type AssistantResponseHook = Arc<
    dyn Fn(AssistantResponseHookContext) -> PluginFuture<AssistantResponseTransform> + Send + Sync,
>;
/// Ends a model call's stream for a plugin's stream hooks. A derivation runs
/// it once the call's receipt is acknowledged, after replaying the receipt's
/// recorded chunks through the stream hooks; it never runs between the
/// provider's answer and that acknowledgement. The value it returns for a
/// stream that produced a response ([`AssistantStreamFinishReason::Complete`]
/// or [`AssistantStreamFinishReason::Aborted`]) is handed to each response
/// callback of its plugin that names this callback's key as its
/// `stream_state_from`. The receiving callback's key and owning plugin id
/// identify that state in [`AssistantResponseHookContext::stream_state`].
/// Return `None` when the response callback needs nothing from the stream.
/// [`AssistantStreamFinishReason::AttemptReset`] discards whatever the hooks
/// hold, which the runtime also asks for before each stream and each
/// derivation. The hook should leave no per-stream state behind: the next
/// stream starts from nothing.
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
    /// Draft decision input: a later committed-state read cannot reproduce it (rule A).
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionReadService>,
    pub turn_context: crate::TurnContext,
    /// Where this hook stands in the trace: under the turn's span, bound
    /// from the registered hook that runs and revoked when it returns.
    pub trace: crate::trace::ExtensionTrace,
}

/// The policy fields changed by a committed configuration transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionConfigField {
    Model,
    AttachmentAcceptance,
    TurnBudget,
    MaxToolCalls,
    NoProgressBudget,
    ChargeSafety,
    Generation,
}

impl SessionConfigField {
    /// Compare the committed transition, including when it supersedes an owed one.
    pub fn between(
        previous: &SessionPolicy,
        current: &SessionPolicy,
    ) -> std::collections::BTreeSet<Self> {
        let SessionPolicy {
            model,
            attachment_acceptance,
            turn_budget,
            max_tool_calls,
            no_progress_budget,
            charge_safety,
            generation,
        } = previous;
        [
            (Self::Model, model != &current.model),
            (
                Self::AttachmentAcceptance,
                attachment_acceptance != &current.attachment_acceptance,
            ),
            (Self::TurnBudget, turn_budget != &current.turn_budget),
            (
                Self::MaxToolCalls,
                max_tool_calls != &current.max_tool_calls,
            ),
            (
                Self::NoProgressBudget,
                no_progress_budget != &current.no_progress_budget,
            ),
            (Self::ChargeSafety, charge_safety != &current.charge_safety),
            (Self::Generation, generation != &current.generation),
        ]
        .into_iter()
        .filter_map(|(field, changed)| changed.then_some(field))
        .collect()
    }
}

/// The logical model call and the provider attempt a hook observes or derives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCallAddress {
    pub session_id: SessionId,
    pub turn_id: crate::TurnId,
    pub call_id: crate::LlmCallId,
    /// None before dispatch; otherwise the ordinal on the provider's request scope.
    pub attempt: Option<u32>,
}

#[derive(Clone)]
pub struct SessionConfigChangedContext {
    pub session_id: SessionId,
    /// The config revision the change reached: the change's identity. A
    /// change delivered again carries the same revision, so an observer that
    /// must act on a change once dedupes on it (FIG-5397).
    pub revision: u64,
    /// Fields changed by this revision, including on redelivery or supersession.
    pub changed: std::collections::BTreeSet<SessionConfigField>,
    /// Read the policy served for this delivery through `snapshot_current()`.
    pub sessions: Arc<dyn SessionReadService>,
}

/// The committed turn identified by an advisory finalization notification.
/// Read its transcript and tool-call ledger through
/// [`SessionReadService::committed_turn`]; billing belongs at the provider seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFinalized {
    pub session_id: SessionId,
    pub turn_id: crate::TurnId,
    /// The same outcome kind the committed-turn reader returns.
    pub outcome: crate::store::TurnCommitOutcome,
}

#[derive(Clone)]
pub enum PluginLifecycleEvent {
    /// Advisory delivery after the turn's commit is acknowledged. The caller
    /// may receive the committed outcome before observers run; observer
    /// failures are reported and cannot change that outcome. Delivery is
    /// not durable: an owner lost after commit may never deliver the event.
    /// Observation only; hosts bill from provider attempt terminals (ADR 0127).
    TurnFinalized(TurnFinalized),
    /// The restored head's identity; revision zero is a durable create/fork
    /// baseline, and absence means no checkpoint was committed. Read restored
    /// state through the session's normal read API after restore completes;
    /// the callback may run before the refreshed resident state is installed.
    SessionRestored {
        session_id: SessionId,
        revision: Option<crate::SessionRevision>,
    },
    /// A config transaction changed the session's policy. Observers see it
    /// once its change is committed, never for a transaction that was stale
    /// or refused. On a durable session the commit that applies the change
    /// records it as owed in the session's head (FIG-5397), and a build of
    /// the session's plugins delivers it: the command run's own, past its
    /// commit, then the next one wherever the session runs (a turn's, a
    /// command's, a replay of the settled command) while the change is
    /// still owed. A build that fails leaves the change applied and owed
    /// (the run logs a warning). The head commit after a delivery retires
    /// it, so delivery is at least once: a node lost between a delivery and
    /// that commit delivers it again, with the same
    /// [`revision`](SessionConfigChangedContext::revision). A change a later
    /// one supersedes before it is delivered is delivered as the later one.
    SessionConfigChanged(Box<SessionConfigChangedContext>),
}

/// Facts available before the turn's commit: its outcome and the tool calls
/// its rounds committed. Usage, execution metrics and issues are not part of
/// this report; this boundary never collected them.
#[derive(Clone, Debug)]
pub struct TurnHookReport {
    /// Decision input before the final commit, so a settled read cannot supply it.
    pub outcome: crate::TurnOutcome,
    /// Precommit decision input, including calls whose turn may never commit after an Immediate stop.
    pub tool_calls: Arc<Vec<crate::ToolCallRecord>>,
}

#[derive(Clone)]
pub struct ToolResultProjectionContext {
    /// Who the call runs for: a session, or a process runtime.
    pub owner: crate::RuntimeOwner,
    pub call_id: crate::ToolCallId,
    pub tool_id: crate::ToolId,
    /// Presentation decision input supplied before the call's settling commit (rule A).
    pub tool_name: String,
    /// Presentation decision input supplied before the call's settling commit (rule A).
    pub render: Option<crate::RecordedRender>,
    /// Presentation decision input supplied before the call's settling commit (rule A).
    pub args: serde_json::Value,
    /// Presentation decision input supplied before the call's settling commit (rule A).
    pub output: crate::ToolCallOutput,
    /// The journaled artifact capability a presentation step retains bytes
    /// through (FIG-3420); [`NoPresentationArtifacts`] where the boundary
    /// supplies none.
    pub artifacts: Arc<dyn ToolPresentationArtifacts>,
}

#[derive(Clone)]
pub struct TurnResultHookContext {
    /// Writer versions selected by this attempt's admitted fleet.
    pub writer_formats: Arc<dyn lash_sansio::WriterFormats>,
    pub session_id: SessionId,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub turn: Arc<TurnHookReport>,
    pub sessions: Arc<dyn SessionReadService>,
    /// Where this hook stands in the trace: under the turn's span, bound
    /// from the registered hook that runs and revoked when it returns.
    pub trace: crate::trace::ExtensionTrace,
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
    /// Draft decision input: a later committed-state read cannot reproduce it (rule A).
    pub state: SessionReadView,
    pub sessions: Arc<dyn SessionReadService>,
    /// Where this hook stands in the trace: under the turn's span, bound
    /// from the registered hook that runs and revoked when it returns.
    pub trace: crate::trace::ExtensionTrace,
}

#[derive(Clone)]
pub struct AssistantStreamHookContext {
    pub session_id: SessionId,
    pub address: ModelCallAddress,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    /// Admitted decision input: a current-head read cannot reproduce this configuration (rule A).
    pub plugin_config: super::AdmittedPluginConfig,
    /// Transform decision input, possibly rewritten by earlier callbacks (rule A).
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
    pub address: ModelCallAddress,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    /// Admitted decision input: a current-head read cannot reproduce this configuration (rule A).
    pub plugin_config: super::AdmittedPluginConfig,
    /// Transform decision input: earlier callbacks may have changed it (rule A).
    pub response: crate::LlmResponse,
    /// Derived decision input: a later committed-state read cannot reproduce it (rule A).
    /// The state the paired [`AssistantStreamFinishedHook`] returned when
    /// the derivation finished the receipt's recorded stream, handed to this
    /// exact response callback identity in its plugin's stream-state format.
    /// `None` when the paired hook returned nothing or the completion did
    /// not stream.
    pub stream_state: Option<serde_json::Value>,
    /// The format of `stream_state`, so a reader can interpret an older
    /// readable payload. `None` exactly when there is no stream state.
    pub stream_state_format: Option<super::FormatVersion>,
}

#[derive(Clone, Debug)]
pub struct AssistantResponseTransform {
    pub response: crate::LlmResponse,
    pub events: Vec<PluginRuntimeEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantStreamFinishReason {
    /// The provider abandoned one streaming attempt and will retry the same
    /// logical request from an empty response state.
    /// A synthetic reset opening receipt replay names the receipt's answering
    /// attempt; a live retry reset names the attempt whose state is discarded.
    /// The reset before dispatch names the upcoming call with no attempt yet.
    AttemptReset,
    Complete,
    Aborted,
    Cancelled,
    ProviderError,
}

#[derive(Clone)]
pub struct AssistantStreamFinishedContext {
    pub session_id: SessionId,
    pub address: ModelCallAddress,
    /// The plugin configuration this hook runs under (FIG-4379): the
    /// running run's admitted configuration and its revision, a process's
    /// captured one, or the head's outside a run — never today's head
    /// inside a run.
    /// Admitted decision input: a current-head read cannot reproduce this configuration (rule A).
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
