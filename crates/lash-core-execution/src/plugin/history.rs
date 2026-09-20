//! Prompt-view transforms and explicit context compaction plugin contracts.
//!
//! Split out of `plugin/mod.rs` purely for file size. All types keep
//! their original module path via `pub use` in `plugin/mod.rs`.

pub use lash_core_store::session_read_view::SessionReadView;

use crate::SessionId;
use std::sync::Arc;

use super::PluginError;

/// Context passed to a turn-context transform.
#[derive(Clone)]
pub struct TurnTransformContext<'run> {
    pub session_id: SessionId,
    pub state: SessionReadView,
    pub prompt_usage: Option<crate::runtime::PromptUsage>,
    pub max_context_tokens: Option<usize>,
    pub sessions: Arc<dyn super::SessionStateService>,
    pub session_lifecycle: Arc<dyn super::SessionLifecycleService>,
    pub session_graph: Arc<dyn super::SessionGraphService>,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
    pub direct_completions: crate::DirectCompletionClient<'run>,
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

/// Produces seed nodes for an explicit compaction Agent Frame.
#[async_trait::async_trait]
pub trait ContextCompactor: Send + Sync {
    fn id(&self) -> &'static str;
    async fn compact(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError>;
}
