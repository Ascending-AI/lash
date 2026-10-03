//! Context-preparation vocabulary shared between the runtime and the
//! plugin host. Rolling strategies are dispatched through the
//! [`TurnContextTransform`](crate::plugin::TurnContextTransform) prompt-view
//! hook. Durable compaction is an explicit Agent Frame transition, not a
//! rewrite of this prepared context.

/// Output of the per-turn context transform pipeline — the messages and
/// tool providers the runtime hands to the
/// LLM call.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PreparedContext {
    pub messages: crate::MessageSequence,
    pub tool_providers: Vec<crate::plugin::PluginCallbackIdentity>,
}
