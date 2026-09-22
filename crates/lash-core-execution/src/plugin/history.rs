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
/// happens — an in-transform overflow recovery is rare, and resolving the
/// prompt eagerly on every turn prepare would fire prompt hooks for prompts
/// that are never sent.
pub type CompactionSystemPrompt =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Option<Arc<str>>, PluginError>> + Send + Sync>;

/// Context passed to a turn-context transform.
#[derive(Clone)]
pub struct TurnTransformContext<'run> {
    pub session_id: SessionId,
    pub state: SessionReadView,
    pub prompt_usage: Option<crate::TokenUsage>,
    pub max_context_tokens: Option<usize>,
    pub sessions: Arc<dyn super::SessionStateService>,
    pub session_lifecycle: Arc<dyn super::SessionLifecycleService>,
    pub session_graph: Arc<dyn super::SessionGraphService>,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
    /// The system prompt an in-transform recovery completion carries: the
    /// same capability, core, and session prompt layers a turn on this
    /// session would resolve, minus the turn layer and every tool-gated
    /// contribution. Lazy — resolved only if recovery runs; `None` when this
    /// session cannot build a recovery prompt at all.
    pub system_prompt: Option<CompactionSystemPrompt>,
}

/// Context passed to an explicit compactor.
#[derive(Clone)]
pub struct CompactionContext<'run> {
    pub session_id: SessionId,
    pub instructions: Option<String>,
    pub state: SessionReadView,
    pub sessions: Arc<dyn super::SessionStateService>,
    pub session_lifecycle: Arc<dyn super::SessionLifecycleService>,
    pub session_graph: Arc<dyn super::SessionGraphService>,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
    /// The system prompt the compaction completion carries: the same
    /// capability, core, and session prompt layers a turn on this session
    /// would resolve, minus the turn layer and every tool-gated contribution
    /// (the request ships no tools, so a gated contribution could never be
    /// honored). `None` when the resolved stack renders empty.
    pub system_prompt: Option<Arc<str>>,
}

#[derive(Debug, thiserror::Error, Clone)]
#[non_exhaustive]
pub enum ContextError {
    #[error("context pipeline error: {0}")]
    Pipeline(String),
    #[error("context session error: {0}")]
    Session(String),
}

impl From<PluginError> for ContextError {
    fn from(value: PluginError) -> Self {
        Self::Session(value.to_string())
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

/// Prepares the ephemeral turn context presented to the model.
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
