//! Prompt-view transforms and explicit context compaction plugin contracts.
//!
//! Split out of `plugin/mod.rs` purely for file size. All types keep
//! their original module path via `pub use` in `plugin/mod.rs`.

use crate::facade_support::SessionGraphFacadeOps;
use lash_sansio::core_support::*;
use std::sync::{Arc, OnceLock};

use crate::runtime::RuntimeSessionState;
use crate::{SessionPolicy, SessionSnapshot};

use super::PluginError;

#[derive(Clone, Debug)]
pub struct SessionReadView(Arc<SessionReadState>);

#[derive(Debug)]
struct SessionReadState {
    meta: SessionReadMeta,
    graph: SessionReadGraph,
    read_model: crate::session_graph::SessionReadModel,
    chronological_projection: OnceLock<Arc<crate::ChronologicalProjection>>,
}

#[derive(Clone, Debug)]
struct SessionReadMeta {
    session_id: String,
    policy: SessionPolicy,
    turn_index: usize,
    token_usage: crate::TokenUsage,
    last_prompt_usage: Option<crate::runtime::PromptUsage>,
    protocol_turn_options: crate::ProtocolTurnOptions,
}

impl SessionReadMeta {
    fn from_snapshot_ref(snapshot: &SessionSnapshot) -> Self {
        Self {
            session_id: snapshot.session_id.clone(),
            policy: snapshot.policy.clone(),
            turn_index: snapshot.turn_index,
            token_usage: snapshot.token_usage.clone(),
            last_prompt_usage: snapshot.last_prompt_usage.clone(),
            protocol_turn_options: snapshot.protocol_turn_options.clone(),
        }
    }

    fn from_persisted_ref(state: &RuntimeSessionState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            policy: state.policy.clone(),
            turn_index: state.turn_index,
            token_usage: state.token_usage.clone(),
            last_prompt_usage: state.last_prompt_usage.clone(),
            protocol_turn_options: state.protocol_turn_options.clone(),
        }
    }

    fn with_policy(mut self, policy: SessionPolicy) -> Self {
        self.policy = policy;
        self
    }

    fn with_turn_index(mut self, turn_index: usize) -> Self {
        self.turn_index = turn_index;
        self
    }

    fn with_protocol_turn_options(
        mut self,
        protocol_turn_options: crate::ProtocolTurnOptions,
    ) -> Self {
        self.protocol_turn_options = protocol_turn_options;
        self
    }

    fn to_snapshot(&self, session_graph: crate::SessionGraph) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.session_id.clone(),
            policy: self.policy.clone(),
            agent_frames: Vec::new(),
            current_frame_node_id: None,
            session_graph,
            turn_index: self.turn_index,
            token_usage: self.token_usage.clone(),
            last_prompt_usage: self.last_prompt_usage.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
            tool_state_ref: None,
            tool_state_generation: None,
            plugin_snapshot_ref: None,
            plugin_snapshot_revision: None,
            execution_state_ref: None,
            token_ledger: Vec::new(),
            checkpoint_ref: None,
        }
    }
}

#[derive(Debug)]
enum SessionReadGraph {
    Owned(crate::SessionGraph),
    Derived {
        cache: OnceLock<crate::SessionGraph>,
        base_graph: Arc<crate::SessionGraph>,
    },
}

impl SessionReadView {
    fn from_graph_message_sequence_meta(
        meta: SessionReadMeta,
        base_graph: Arc<crate::SessionGraph>,
        messages: crate::MessageSequence,
        active_events: Arc<Vec<crate::SessionHistoryRecord>>,
    ) -> Self {
        Self(Arc::new(SessionReadState {
            meta,
            graph: SessionReadGraph::Derived {
                cache: OnceLock::new(),
                base_graph,
            },
            read_model: crate::session_graph::SessionReadModel {
                active_events,
                messages: messages.shared(),
                prompt_render_cache: Arc::new(crate::BaseRenderCache::new()),
            },
            chronological_projection: OnceLock::new(),
        }))
    }

    pub fn from_snapshot(snapshot: &SessionSnapshot) -> Self {
        let read_model = snapshot.read_model();
        Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_snapshot_ref(snapshot),
            graph: SessionReadGraph::Owned(snapshot.session_graph.clone()),
            read_model,
            chronological_projection: OnceLock::new(),
        }))
    }

    pub fn from_persisted_state(state: &RuntimeSessionState) -> Self {
        let graph = state.session_graph.clone();
        let read_model = state.read_model();
        Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_persisted_ref(state),
            graph: SessionReadGraph::Owned(graph),
            read_model,
            chronological_projection: OnceLock::new(),
        }))
    }

    pub(crate) fn from_runtime_state(
        state: &RuntimeSessionState,
        policy: SessionPolicy,
        protocol_turn_options: crate::ProtocolTurnOptions,
    ) -> Self {
        let graph = state.session_graph.clone();
        let read_model = state.read_model();
        Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_persisted_ref(state)
                .with_policy(policy)
                .with_protocol_turn_options(protocol_turn_options),
            graph: SessionReadGraph::Owned(graph),
            read_model,
            chronological_projection: OnceLock::new(),
        }))
    }

    pub(crate) fn derived_from_persisted_state(
        state: &RuntimeSessionState,
        policy: SessionPolicy,
        turn_index: usize,
        protocol_turn_options: crate::ProtocolTurnOptions,
        base_graph: Arc<crate::SessionGraph>,
        messages: crate::MessageSequence,
    ) -> Self {
        let active_events = state
            .current_frame_node_id
            .as_deref()
            .map_or_else(
                || base_graph.read_model(),
                |frame_node_id| base_graph.read_model_for_frame(frame_node_id),
            )
            .active_events;
        Self::from_graph_message_sequence_meta(
            SessionReadMeta::from_persisted_ref(state)
                .with_policy(policy)
                .with_turn_index(turn_index)
                .with_protocol_turn_options(protocol_turn_options),
            base_graph,
            messages,
            active_events,
        )
    }

    pub fn session_graph(&self) -> &crate::SessionGraph {
        match &self.0.graph {
            SessionReadGraph::Owned(graph) => graph,
            SessionReadGraph::Derived { cache, base_graph } => cache.get_or_init(|| {
                let mut graph = (**base_graph).clone();
                if let Some(frame_node_id) =
                    base_graph.nearest_frame_node_id(base_graph.leaf_node_id.as_deref())
                {
                    graph.replace_active_read_state_for_frame(
                        frame_node_id,
                        self.0.read_model.messages.as_slice(),
                    );
                } else {
                    graph.replace_active_read_state(self.0.read_model.messages.as_slice());
                }
                graph
            }),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.0.meta.session_id
    }

    pub fn policy(&self) -> &SessionPolicy {
        &self.0.meta.policy
    }

    pub fn messages(&self) -> &[crate::Message] {
        self.0.read_model.messages.as_slice()
    }

    pub fn active_events(&self) -> &[crate::SessionHistoryRecord] {
        self.0.read_model.active_events.as_slice()
    }

    pub fn chronological_projection(&self) -> crate::ChronologicalProjection {
        crate::ChronologicalProjection::from_read_model(&self.0.read_model)
    }

    pub(crate) fn shared_chronological_projection(&self) -> Arc<crate::ChronologicalProjection> {
        Arc::clone(self.0.chronological_projection.get_or_init(|| {
            Arc::new(crate::ChronologicalProjection::from_read_model(
                &self.0.read_model,
            ))
        }))
    }

    pub fn message_tree(&self) -> Vec<crate::SessionMessageTreeNode> {
        self.session_graph().message_tree()
    }

    pub fn turn_index(&self) -> usize {
        self.0.meta.turn_index
    }

    pub fn token_usage(&self) -> &crate::TokenUsage {
        &self.0.meta.token_usage
    }

    pub fn last_prompt_usage(&self) -> Option<&crate::runtime::PromptUsage> {
        self.0.meta.last_prompt_usage.as_ref()
    }

    pub fn protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        &self.0.meta.protocol_turn_options
    }

    pub fn to_snapshot(&self) -> SessionSnapshot {
        self.0.meta.to_snapshot(self.session_graph().clone())
    }
}

/// Context passed to a turn-context transform.
#[derive(Clone)]
pub struct TurnTransformContext<'run> {
    pub session_id: String,
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
    pub session_id: String,
    pub instructions: Option<String>,
    pub state: SessionReadView,
    pub sessions: Arc<dyn super::SessionStateService>,
    pub session_lifecycle: Arc<dyn super::SessionLifecycleService>,
    pub session_graph: Arc<dyn super::SessionGraphService>,
    pub scoped_effect_controller: crate::ScopedEffectController<'run>,
}

#[derive(Debug, thiserror::Error, Clone)]
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
