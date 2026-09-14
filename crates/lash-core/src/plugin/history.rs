//! Prompt-view transforms and explicit context compaction plugin contracts.
//!
//! Split out of `plugin/mod.rs` purely for file size. All types keep
//! their original module path via `pub use` in `plugin/mod.rs`.

use crate::SessionId;
use crate::facade_support::SessionGraphFacadeOps;
use lash_sansio::core_support::*;
use std::sync::{Arc, OnceLock};

use crate::runtime::RuntimeSessionState;
use crate::{SessionPolicy, SessionSnapshot};

use super::PluginError;



#[derive(Debug)]
struct SessionReadState {
    meta: SessionReadMeta,
    graph: SessionReadGraph,
    read_model: crate::session_graph::SessionReadModel,
    chronological_projection: OnceLock<Arc<crate::ChronologicalProjection>>,
    turn_failure_settlements: Arc<Vec<crate::TurnFailureSettlement>>,
}

#[derive(Clone, Debug)]
struct SessionReadMeta {
    session_id: SessionId,
    durable_relation: Option<crate::SessionRelation>,
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
            durable_relation: None,
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
            durable_relation: None,
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

    fn with_durable_relation(mut self, relation: crate::SessionRelation) -> Self {
        self.durable_relation = Some(relation);
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
            plugin_state_ref: None,
            plugin_state_generations: Default::default(),
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
