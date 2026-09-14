//! Read projection of a session's durable graph.

use crate::facade_support::SessionGraphFacadeOps;
use crate::{RuntimeSessionState, SessionId, SessionPolicy, SessionSnapshot};
use lash_sansio::core_support::MessageSequenceCoreSupport;
use std::sync::Arc;
use std::sync::OnceLock;

#[derive(Clone, Debug)]
pub struct SessionReadView(Arc<SessionReadState>);
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
            turn_failure_settlements: Arc::new(Vec::new()),
        }))
    }

    /// Builds a `SessionReadView` from snapshot data for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn from_snapshot(
        snapshot: &SessionSnapshot,
    ) -> Result<Self, crate::SessionGraphScopeError> {
        let read_model = snapshot.read_model()?;
        Ok(Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_snapshot_ref(snapshot),
            graph: SessionReadGraph::Owned(snapshot.session_graph.clone()),
            read_model,
            chronological_projection: OnceLock::new(),
            turn_failure_settlements: Arc::new(Vec::new()),
        })))
    }

    /// Builds a `SessionReadView` from persisted state data for store and durable-substrate
    /// implementors while validating and applying durable session transitions.
    pub fn from_persisted_state(
        state: &RuntimeSessionState,
    ) -> Result<Self, crate::SessionGraphScopeError> {
        let graph = state.session_graph.clone();
        let read_model = state.read_model()?;
        Ok(Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_persisted_ref(state),
            graph: SessionReadGraph::Owned(graph),
            read_model,
            chronological_projection: OnceLock::new(),
            turn_failure_settlements: Arc::new(Vec::new()),
        })))
    }

    pub(crate) fn from_persisted_state_with_relation_and_failures(
        state: &RuntimeSessionState,
        relation: crate::SessionRelation,
        turn_failure_settlements: Vec<crate::TurnFailureSettlement>,
    ) -> Result<Self, crate::SessionGraphScopeError> {
        let graph = state.session_graph.clone();
        let read_model = state.read_model()?;
        Ok(Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_persisted_ref(state).with_durable_relation(relation),
            graph: SessionReadGraph::Owned(graph),
            read_model,
            chronological_projection: OnceLock::new(),
            turn_failure_settlements: Arc::new(turn_failure_settlements),
        })))
    }

    pub fn from_runtime_state(
        state: &RuntimeSessionState,
        policy: SessionPolicy,
        protocol_turn_options: crate::ProtocolTurnOptions,
    ) -> Result<Self, crate::SessionGraphScopeError> {
        let graph = state.session_graph.clone();
        let read_model = state.read_model()?;
        Ok(Self(Arc::new(SessionReadState {
            meta: SessionReadMeta::from_persisted_ref(state)
                .with_policy(policy)
                .with_protocol_turn_options(protocol_turn_options),
            graph: SessionReadGraph::Owned(graph),
            read_model,
            chronological_projection: OnceLock::new(),
            turn_failure_settlements: Arc::new(Vec::new()),
        })))
    }

    pub fn derived_from_persisted_state(
        state: &RuntimeSessionState,
        policy: SessionPolicy,
        turn_index: usize,
        protocol_turn_options: crate::ProtocolTurnOptions,
        base_graph: Arc<crate::SessionGraph>,
        messages: crate::MessageSequence,
    ) -> Result<Self, crate::SessionGraphScopeError> {
        let active_events = base_graph
            .read_model(state.current_frame_node_id.as_ref())?
            .active_events;
        Ok(Self::from_graph_message_sequence_meta(
            SessionReadMeta::from_persisted_ref(state)
                .with_policy(policy)
                .with_turn_index(turn_index)
                .with_protocol_turn_options(protocol_turn_options),
            base_graph,
            messages,
            active_events,
        ))
    }

    /// Exposes session graph to store and durable-substrate implementors while validating and
    /// applying durable session transitions.
    pub fn session_graph(&self) -> &crate::SessionGraph {
        match &self.0.graph {
            SessionReadGraph::Owned(graph) => graph,
            SessionReadGraph::Derived { cache, base_graph } => cache.get_or_init(|| {
                let mut graph = (**base_graph).clone();
                let frame_node_id = base_graph
                    .nearest_frame_node_id(base_graph.leaf_node_id.as_deref())
                    .map(|frame_node_id| {
                        crate::FrameNodeId::new(frame_node_id)
                            .expect("a graph node identity selected as a frame is non-empty")
                    });
                graph
                    .rewrite_active_read_tail(
                        frame_node_id.as_ref(),
                        self.0.read_model.messages.as_slice(),
                    )
                    .expect("derived frame must resolve in its source session graph");
                graph
            }),
        }
    }

    /// Exposes session id to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn.
    pub fn session_id(&self) -> &str {
        &self.0.meta.session_id
    }

    /// Borrows the complete relation from durable session metadata, when that
    /// metadata was available to this view's projection.
    ///
    /// `None` means the projection had no durable session metadata. Views
    /// projected directly from standalone snapshots or live runtime state
    /// therefore return `None`.
    pub fn durable_relation(&self) -> Option<&crate::SessionRelation> {
        self.0.meta.durable_relation.as_ref()
    }

    /// Durable failure evidence owned by settled turn records.
    ///
    /// This collection is structurally separate from messages, protocol
    /// events, and prompt contributions. Reading it cannot alter model context.
    pub fn turn_failure_settlements(&self) -> &[crate::TurnFailureSettlement] {
        self.0.turn_failure_settlements.as_slice()
    }

    /// Exposes policy to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn.
    pub fn policy(&self) -> &SessionPolicy {
        &self.0.meta.policy
    }

    /// Exposes messages to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn.
    pub fn messages(&self) -> &[crate::Message] {
        self.0.read_model.messages.as_slice()
    }

    /// Borrows active-path protocol events in graph order for protocol implementors materializing
    /// the next turn; events on inactive branches are excluded.
    pub fn active_events(&self) -> &[crate::SessionHistoryRecord] {
        self.0.read_model.active_events.as_slice()
    }

    /// Interleaves active messages and protocol events by graph position for protocol implementors
    /// building a chronological turn view.
    pub fn chronological_projection(&self) -> crate::ChronologicalProjection {
        crate::ChronologicalProjection::from_read_model(&self.0.read_model)
    }

    pub fn shared_chronological_projection(&self) -> Arc<crate::ChronologicalProjection> {
        Arc::clone(self.0.chronological_projection.get_or_init(|| {
            Arc::new(crate::ChronologicalProjection::from_read_model(
                &self.0.read_model,
            ))
        }))
    }

    /// Projects all message nodes, including inactive branches, for protocol and conformance
    /// embedders that need the session's branch structure.
    pub fn message_tree(&self) -> Vec<crate::SessionMessageTreeNode> {
        self.session_graph().message_tree()
    }

    /// Exposes turn index to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn.
    pub fn turn_index(&self) -> usize {
        self.0.meta.turn_index
    }

    /// Borrows the session's accumulated prompt and completion usage for protocol and observation
    /// embedders without recomputing it from message history.
    pub fn token_usage(&self) -> &crate::TokenUsage {
        &self.0.meta.token_usage
    }

    /// Returns the prompt usage basis pinned for the current logical turn, or the latest
    /// completed turn's usage outside a turn. The pinned basis may be `None`.
    /// Current-call feedback is available through `ProtocolBeforeLlmCallContext.latest_prompt_usage`.
    pub fn last_prompt_usage(&self) -> Option<&crate::runtime::PromptUsage> {
        self.0.meta.last_prompt_usage.as_ref()
    }

    /// Exposes protocol turn options to protocol and process-engine implementors while
    /// materializing protocol-specific session and turn state.
    pub fn protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        &self.0.meta.protocol_turn_options
    }

    /// Projects this `SessionReadView` into snapshot form for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn to_snapshot(&self) -> SessionSnapshot {
        self.0.meta.to_snapshot(self.session_graph().clone())
    }
}

#[derive(Debug)]
pub struct SessionReadState {
    meta: SessionReadMeta,
    graph: SessionReadGraph,
    read_model: crate::session_graph::SessionReadModel,
    chronological_projection: OnceLock<Arc<crate::ChronologicalProjection>>,
    turn_failure_settlements: Arc<Vec<crate::TurnFailureSettlement>>,
}
#[derive(Clone, Debug)]
pub struct SessionReadMeta {
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
pub enum SessionReadGraph {
    Owned(crate::SessionGraph),
    Derived {
        cache: OnceLock<crate::SessionGraph>,
        base_graph: Arc<crate::SessionGraph>,
    },
}
