//! Context-preparation vocabulary shared between the runtime and the
//! plugin host. Rolling strategies are dispatched through the
//! [`TurnContextTransform`](crate::plugin::TurnContextTransform) prompt-view
//! hook. Durable compaction is an explicit Agent Frame transition, not a
//! rewrite of this prepared context.

use std::sync::Arc;

use crate::ToolProvider;
use lash_sansio::PromptContribution;

/// Output of the per-turn context transform pipeline — the messages,
/// prompt contributions, and tool providers the runtime hands to the
/// LLM call.
#[derive(Clone)]
pub struct PreparedContext {
    pub messages: crate::MessageSequence,
    pub prompt_contributions: Vec<PromptContribution>,
    pub tool_providers: Vec<Arc<dyn ToolProvider>>,
    pub include_base_tools: bool,
}

impl Default for PreparedContext {
    fn default() -> Self {
        Self {
            messages: crate::MessageSequence::default(),
            prompt_contributions: Vec::new(),
            tool_providers: Vec::new(),
            include_base_tools: true,
        }
    }
}
