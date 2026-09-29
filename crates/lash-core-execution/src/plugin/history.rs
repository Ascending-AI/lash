//! Prompt-view transforms and explicit context compaction plugin contracts.
//!
//! All types keep their original module path via `pub use` in `plugin/mod.rs`.

pub use lash_core_store::session_read_view::SessionReadView;

use crate::SessionId;
use futures_util::future::BoxFuture;
use std::sync::Arc;

use super::PluginError;

/// Lazily renders the system prompt a compaction completion carries.
///
/// Deferred so plugin prompt hooks run only when a compaction actually
/// happens — a context-pressure hook summarizes rarely, and resolving the
/// prompt eagerly on every turn prepare would fire prompt hooks for prompts
/// that are never sent.
pub type CompactionSystemPrompt =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Option<Arc<str>>, PluginError>> + Send + Sync>;

/// Emits trace events and nothing else.
///
/// The one observable side channel a context hook holds: transforms,
/// compactors and context-pressure hooks write nothing durable, so they get a
/// trace emitter instead of a session service (ADR 0001, ADR 0105 §6).
#[derive(Clone)]
pub struct PluginTraceEmitter {
    sink: Arc<dyn Fn(lash_trace::TraceContext, lash_trace::TraceEvent) + Send + Sync>,
}

impl PluginTraceEmitter {
    pub fn new(
        sink: impl Fn(lash_trace::TraceContext, lash_trace::TraceEvent) + Send + Sync + 'static,
    ) -> Self {
        Self {
            sink: Arc::new(sink),
        }
    }

    /// An emitter that drops every event.
    pub fn discard() -> Self {
        Self::new(|_, _| {})
    }

    pub fn emit(&self, context: lash_trace::TraceContext, event: lash_trace::TraceEvent) {
        (self.sink)(context, event);
    }
}

impl std::fmt::Debug for PluginTraceEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginTraceEmitter").finish_non_exhaustive()
    }
}

/// Context passed to a turn-context transform.
///
/// A transform is a Prompt View transform (ADR 0001): its output is
/// ephemeral and it holds no write service, so it can neither append nodes
/// nor open a frame. Durable context decisions belong to a
/// [`ContextPressureHook`], whose decision core writes.
#[derive(Clone)]
pub struct TurnTransformContext<'run> {
    pub session_id: SessionId,
    pub state: SessionReadView,
    pub prompt_usage: Option<crate::TokenUsage>,
    pub max_context_tokens: Option<usize>,
    pub traces: PluginTraceEmitter,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
}

/// Context passed to an explicit compactor.
///
/// A compactor returns seed nodes and core opens the frame; it holds no
/// write service.
#[derive(Clone)]
pub struct CompactionContext<'run> {
    pub session_id: SessionId,
    pub instructions: Option<String>,
    pub state: SessionReadView,
    pub traces: PluginTraceEmitter,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
    /// The system prompt the compaction completion carries: the same
    /// capability, core, and session prompt layers a turn on this session
    /// would resolve, minus the turn layer and every tool-gated contribution
    /// (the request ships no tools, so a gated contribution could never be
    /// honored). `None` when the resolved stack renders empty.
    pub system_prompt: Option<Arc<str>>,
}

/// Context passed to a [`ContextPressureHook`].
///
/// Everything in it is recorded state or a journaled effect: the committed
/// read view (which carries every durable plugin record, a pending
/// overflow-recovery marker included), the previous provider-reported prompt
/// usage and the context window. The hook holds no write service: it returns
/// a [`ContextPressureDecision`] and core writes it.
#[derive(Clone)]
pub struct ContextPressureContext<'run> {
    pub session_id: SessionId,
    /// The committed session, as it stood when the turn was admitted.
    pub state: SessionReadView,
    /// The previous turn's provider-reported prompt usage, if any.
    pub prompt_usage: Option<crate::TokenUsage>,
    /// The context window the turn's model runs under.
    pub max_context_tokens: Option<usize>,
    pub traces: PluginTraceEmitter,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
    /// The system prompt a summarizer completion carries: the same
    /// capability, core, and session prompt layers a turn on this session
    /// would resolve, minus the turn layer and every tool-gated contribution.
    /// Lazy — resolved only if the hook summarizes; `None` when this session
    /// cannot build one at all.
    pub system_prompt: Option<CompactionSystemPrompt>,
}

/// What a [`ContextPressureHook`] decided for the turn being prepared.
///
/// Core applies it as the folded outcome of the turn's prepare step (ADR
/// 0105 §6). Nothing is written until the turn commits, and the turn's
/// commit carries every node and the frame.
#[derive(Clone, Debug, PartialEq)]
pub enum ContextPressureDecision {
    /// Leave the session as it is.
    Continue,
    /// Append plugin records to the current frame. They fold at the turn's
    /// first boundary, after the turn's own input.
    Record {
        nodes: Vec<crate::SessionAppendNode>,
    },
    /// Open a compaction frame the turn runs in.
    ///
    /// `records` are appended to the frame being left, then core opens a
    /// frame seeded with `seed` before the turn runs, so the turn's own
    /// messages follow the seed. Core derives the frame key from the turn's
    /// scope and the current frame. `task` names the compaction. A turn
    /// opens at most one frame: a turn that opened one and then ends in a
    /// `continue_as` is refused typed.
    OpenFrame {
        records: Vec<crate::SessionAppendNode>,
        task: String,
        seed: Vec<crate::SessionAppendNode>,
    },
}

/// The decision a named hook returned, as core applies it.
#[derive(Clone, Debug)]
pub struct DecidedContextPressure {
    pub hook_id: &'static str,
    pub decision: ContextPressureDecision,
}

#[derive(Debug, thiserror::Error, Clone)]
#[non_exhaustive]
pub enum ContextError {
    #[error("context pipeline error: {0}")]
    Pipeline(String),
    #[error("context session error: {0}")]
    Session(String),
    /// A plugin-seam failure the context step ran into, kept whole so a live
    /// fault inside it keeps its own cause (FIG-3575).
    #[error(transparent)]
    Plugin(PluginError),
}

impl From<PluginError> for ContextError {
    fn from(value: PluginError) -> Self {
        Self::Plugin(value)
    }
}

impl ContextError {
    /// Settles a context step's failure by its cause (FIG-3575): a plugin
    /// failure settles as [`PluginError::into_turn_failure`] does, and the
    /// context step's own pipeline or session refusal is an outcome spelled as
    /// `refusal`.
    pub fn into_turn_failure(self, refusal: crate::RuntimeErrorCode) -> crate::RuntimeError {
        match self {
            Self::Plugin(error) => error.into_turn_failure(refusal),
            other @ (Self::Pipeline(_) | Self::Session(_)) => {
                crate::RuntimeError::new(refusal, other.to_string())
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ContextCompaction {
    pub initial_nodes: Vec<crate::SessionAppendNode>,
}

impl ContextCompaction {
    pub fn new(initial_nodes: Vec<crate::SessionAppendNode>) -> Self {
        Self { initial_nodes }
    }

    pub fn is_empty(&self) -> bool {
        self.initial_nodes.is_empty()
    }
}

/// Prepares the ephemeral turn context presented to the model (a Prompt
/// View transform, ADR 0001).
#[async_trait::async_trait]
pub trait TurnContextTransform: Send + Sync {
    fn id(&self) -> &'static str;
    async fn transform(
        &self,
        ctx: &TurnTransformContext<'_>,
        input: crate::session_model::context::PreparedContext,
    ) -> Result<crate::session_model::context::PreparedContext, ContextError>;
}

#[async_trait::async_trait]
pub trait ContextCompactor: Send + Sync {
    fn id(&self) -> &'static str;
    async fn compact(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError>;
}

/// Decides, once per turn and before the Prompt View transforms, whether the
/// session's context needs a durable change: plugin records, or a compaction
/// frame the turn then runs in.
///
/// The hook owns the strategy (the threshold, the cut, the summarizer
/// prompt, overflow recovery) and may run effects, such as one journaled
/// summarizer completion. It never writes: core performs the write its
/// decision names. A replay calls it again over the same recorded inputs,
/// and the journaled completion answers the same way, so it reaches the same
/// decision.
#[async_trait::async_trait]
pub trait ContextPressureHook: Send + Sync {
    fn id(&self) -> &'static str;
    async fn decide(
        &self,
        ctx: &ContextPressureContext<'_>,
    ) -> Result<ContextPressureDecision, ContextError>;
}
