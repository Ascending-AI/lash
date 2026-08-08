use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use crate::session_graph_integrity::{
    ancestry_indices, graph_node_indices, validate_graph_parent_topology,
};
use crate::session_model::{ConversationRecord, ProtocolEvent, SessionHistoryRecord};
use crate::{BaseRenderCache, Clock, Message, MessageRole, PromptUsage, TokenUsage};
use facade_ops::{SessionGraphFacadeOps, SessionNodeRecordFacadeOps};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RealizedNodeTimestamp {
    pub node_id: String,
    pub timestamp: String,
}

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`SessionNodeRecord`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait SessionNodeRecordFacadeOps {
        fn event(&self) -> Option<&SessionHistoryRecord>;

        fn message(&self) -> Option<Message>;

        fn plugin(&self) -> Option<(&str, &serde_json::Value)>;
    }

    impl SessionNodeRecordFacadeOps for SessionNodeRecord {
        fn event(&self) -> Option<&SessionHistoryRecord> {
            match &self.payload {
                SessionNodePayload::Event { event } => Some(event),
                SessionNodePayload::Plugin { .. } | SessionNodePayload::FrameOpen { .. } => None,
            }
        }

        fn message(&self) -> Option<Message> {
            match self.event()? {
                SessionHistoryRecord::Conversation(record) => Some(record.to_message()),
                _ => None,
            }
        }

        fn plugin(&self) -> Option<(&str, &serde_json::Value)> {
            match &self.payload {
                SessionNodePayload::Event { .. } | SessionNodePayload::FrameOpen { .. } => None,
                SessionNodePayload::Plugin { plugin_type, body } => {
                    Some((plugin_type.as_str(), body.as_ref()))
                }
            }
        }
    }

    /// Facade-internal operations for [`SessionGraph`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait SessionGraphFacadeOps {
        fn active_path_nodes(&self) -> Vec<&SessionNodeRecord>;

        fn nearest_frame_node_id(&self, leaf_node_id: Option<&str>) -> Option<&str>;

        fn agent_frame_records(&self, session_id: &str) -> Vec<crate::AgentFrameRecord>;

        fn extend_node_records<I>(&mut self, nodes: I)
        where
            I: IntoIterator<Item = SessionNodeRecord>;
    }

    impl SessionGraphFacadeOps for SessionGraph {
        fn active_path_nodes(&self) -> Vec<&SessionNodeRecord> {
            self.cache()
                .active_path_indices
                .iter()
                .map(|idx| &self.nodes[*idx])
                .collect()
        }

        fn nearest_frame_node_id(&self, leaf_node_id: Option<&str>) -> Option<&str> {
            let idx = self
                .nearest_ancestor_index(leaf_node_id, |node| {
                    matches!(node.payload, SessionNodePayload::FrameOpen { .. })
                })
                .ok()??;
            Some(self.nodes[idx].node_id.as_str())
        }

        fn agent_frame_records(&self, session_id: &str) -> Vec<crate::AgentFrameRecord> {
            self.try_agent_frame_records(session_id)
                .unwrap_or_else(|err| panic!("invalid resident session graph: {err}"))
        }

        fn extend_node_records<I>(&mut self, nodes: I)
        where
            I: IntoIterator<Item = SessionNodeRecord>,
        {
            self.data_mut().nodes.extend(nodes);
        }
    }
}

fn draft_node_id(namespace: &str, ordinal: u64) -> String {
    let preimage = format!("{}:{namespace}:{ordinal}", namespace.len());
    format!(
        "draft-node/v2/{}",
        crate::stable_hash::sha256_hex(preimage.as_bytes())
    )
}

/// Derive a durable frame identity before the surrounding operation commits.
///
/// Process provenance can capture the current frame scope immediately, so a
/// FrameOpen ID must be final before runtime effects begin. The host-provided
/// session id fixes the identity before store admission; binding must leave it
/// unchanged.
pub fn frame_node_id(session_id: &str, frame_key: &str) -> String {
    let preimage = format!(
        "{}:{session_id}:{}:{frame_key}",
        session_id.len(),
        frame_key.len()
    );
    format!(
        "frame-node/v2/{}",
        crate::stable_hash::sha256_hex(preimage.as_bytes())
    )
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionGraphData {
    #[serde(default)]
    pub nodes: Vec<SessionNodeRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_node_id: Option<String>,
}

#[derive(Debug)]
pub struct SessionGraph {
    inner: Arc<SessionGraphData>,
    cache: Arc<OnceLock<SessionGraphCache>>,
}

impl Default for SessionGraph {
    fn default() -> Self {
        Self {
            inner: Arc::new(SessionGraphData::default()),
            cache: Arc::new(OnceLock::new()),
        }
    }
}

impl Clone for SessionGraph {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            cache: Arc::clone(&self.cache),
        }
    }
}

impl serde::Serialize for SessionGraph {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.inner.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for SessionGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let inner = SessionGraphData::deserialize(deserializer)?;
        Ok(Self {
            inner: Arc::new(inner),
            cache: Arc::new(OnceLock::new()),
        })
    }
}

impl Deref for SessionGraph {
    type Target = SessionGraphData;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionNodeRecord {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_node_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub payload: SessionNodePayload,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSessionNodeBody {
    timestamp: String,
    #[serde(flatten)]
    payload: SessionNodePayload,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionNodeDraft {
    payload: SessionNodeDraftPayload,
}

#[derive(Clone, Debug)]
enum SessionNodeDraftPayload {
    Message(Message),
    Plugin {
        plugin_type: String,
        body: serde_json::Value,
    },
    ProtocolEvent(ProtocolEvent),
}

impl SessionNodeDraft {
    pub(crate) fn message(message: Message) -> Self {
        Self {
            payload: SessionNodeDraftPayload::Message(message),
        }
    }

    pub(crate) fn plugin(plugin_type: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            payload: SessionNodeDraftPayload::Plugin {
                plugin_type: plugin_type.into(),
                body,
            },
        }
    }

    pub(crate) fn protocol_event(event: ProtocolEvent) -> Self {
        Self {
            payload: SessionNodeDraftPayload::ProtocolEvent(event),
        }
    }

    pub(crate) fn event(event: SessionHistoryRecord) -> Self {
        match event {
            SessionHistoryRecord::Conversation(record) => Self::message(record.to_message()),
            SessionHistoryRecord::Protocol(event) => Self::protocol_event(event),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SharedJsonValue(pub Arc<serde_json::Value>);

impl SharedJsonValue {
    pub fn new(value: serde_json::Value) -> Self {
        Self(Arc::new(value))
    }

    pub fn to_owned(&self) -> serde_json::Value {
        self.0.as_ref().clone()
    }
}

impl AsRef<serde_json::Value> for SharedJsonValue {
    fn as_ref(&self) -> &serde_json::Value {
        self.0.as_ref()
    }
}

impl serde::Serialize for SharedJsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for SharedJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// justification: persisted graph nodes retain their public inline payload shape across storage and replay.
#[allow(clippy::large_enum_variant)]
pub enum SessionNodePayload {
    Event {
        event: SessionHistoryRecord,
    },
    Plugin {
        plugin_type: String,
        body: SharedJsonValue,
    },
    FrameOpen {
        frame_key: String,
        reason: crate::AgentFrameReason,
        assignment: crate::AgentFrameAssignment,
        protocol_turn_options: crate::ProtocolTurnOptions,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PersistedSessionConfig {
    pub provider_id: String,
    pub model: crate::ModelSpec,
    pub turn_budget: crate::TurnBudget,
}

impl PersistedSessionConfig {
    /// Builds an empty persisted config carrying the required per-turn budget.
    ///
    /// Store implementors reading durable session heads populate the provider
    /// and model fields from the row; the budget has no default by doctrine,
    /// so every construction names `TurnBudget::Bounded(n)` or `Unbounded`
    /// explicitly.
    pub fn new(turn_budget: crate::TurnBudget) -> Self {
        Self {
            provider_id: String::new(),
            model: crate::ModelSpec::default(),
            turn_budget,
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PersistedTurnState {
    pub turn_index: usize,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_usage: Option<PromptUsage>,
    #[serde(default)]
    pub protocol_turn_options: crate::ProtocolTurnOptions,
}

#[derive(Clone, Debug)]
pub struct SessionMessageTreeNode {
    pub node_id: String,
    pub parent_message_node_id: Option<String>,
    pub message: Message,
    pub timestamp: String,
    pub children: Vec<SessionMessageTreeNode>,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveReadReplacement {
    pub(crate) leaf_node_id: Option<String>,
    pub(crate) new_tail_nodes: Vec<SessionNodeRecord>,
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveReadProjection {
    pub(crate) active_events: Vec<SessionHistoryRecord>,
    pub(crate) active_messages: Vec<Message>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionReadModel {
    pub(crate) active_events: Arc<Vec<SessionHistoryRecord>>,
    pub(crate) messages: Arc<Vec<Message>>,
    pub(crate) prompt_render_cache: Arc<BaseRenderCache>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionGraphAppendBuilder {
    existing_ids: HashSet<String>,
    leaf_node_id: Option<String>,
    draft_namespace: String,
    next_draft_ordinal: u64,
}

impl SessionGraphAppendBuilder {
    pub(crate) fn leaf_node_id(&self) -> Option<&String> {
        self.leaf_node_id.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn set_leaf_node_id(&mut self, leaf_node_id: Option<String>) {
        self.leaf_node_id = leaf_node_id;
    }

    pub(crate) fn remap_node_ids(&mut self, mapping: &[(String, String)]) {
        if mapping.is_empty() {
            return;
        }
        let mapping = mapping.iter().cloned().collect::<HashMap<_, _>>();
        self.existing_ids = self
            .existing_ids
            .drain()
            .map(|id| mapping.get(&id).cloned().unwrap_or(id))
            .collect();
        if let Some(leaf) = self.leaf_node_id.as_mut()
            && let Some(derived) = mapping.get(leaf)
        {
            *leaf = derived.clone();
        }
    }

    pub(crate) fn append_messages_at<I>(
        &mut self,
        messages: I,
        timestamp: String,
    ) -> Vec<SessionNodeRecord>
    where
        I: IntoIterator<Item = Message>,
    {
        self.append_drafts_at(
            messages.into_iter().map(SessionNodeDraft::message),
            timestamp,
        )
    }

    pub(crate) fn append_events_at<I>(
        &mut self,
        events: I,
        timestamp: String,
    ) -> Vec<SessionNodeRecord>
    where
        I: IntoIterator<Item = SessionHistoryRecord>,
    {
        self.append_drafts_at(events.into_iter().map(SessionNodeDraft::event), timestamp)
    }

    pub(crate) fn append_drafts_at<I>(
        &mut self,
        drafts: I,
        timestamp: String,
    ) -> Vec<SessionNodeRecord>
    where
        I: IntoIterator<Item = SessionNodeDraft>,
    {
        let mut nodes = Vec::new();
        for draft in drafts {
            let parent_node_id = self.leaf_node_id.clone();
            let (node_id, payload) = match draft.payload {
                SessionNodeDraftPayload::Message(message) => {
                    let node_id = self.next_draft_node_id();
                    (
                        node_id,
                        SessionNodePayload::Event {
                            event: SessionHistoryRecord::Conversation(
                                ConversationRecord::from_message(message),
                            ),
                        },
                    )
                }
                SessionNodeDraftPayload::Plugin { plugin_type, body } => {
                    let node_id = self.next_draft_node_id();
                    (
                        node_id,
                        SessionNodePayload::Plugin {
                            plugin_type,
                            body: SharedJsonValue::new(body),
                        },
                    )
                }
                SessionNodeDraftPayload::ProtocolEvent(event) => {
                    let node_id = self.next_draft_node_id();
                    (
                        node_id,
                        SessionNodePayload::Event {
                            event: SessionHistoryRecord::Protocol(event),
                        },
                    )
                }
            };
            self.existing_ids.insert(node_id.clone());
            self.leaf_node_id = Some(node_id.clone());
            nodes.push(SessionNodeRecord {
                node_id,
                parent_node_id,
                timestamp: timestamp.clone(),
                payload,
            });
        }
        nodes
    }

    fn next_draft_node_id(&mut self) -> String {
        loop {
            let candidate = draft_node_id(&self.draft_namespace, self.next_draft_ordinal);
            self.next_draft_ordinal += 1;
            if !self.existing_ids.contains(&candidate) {
                return candidate;
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SessionGraphCache {
    by_id: HashMap<String, usize>,
    active_path_indices: Vec<usize>,
    active_events: Arc<Vec<SessionHistoryRecord>>,
    active_messages: Arc<Vec<Message>>,
    /// Memoized render of `active_messages`. Shared with every
    /// `MessageSequence` built off this read model so the chat projector's
    /// per-iteration `render_prompt` walk only happens once per turn.
    /// Replaced (not invalidated in-place) whenever `active_messages`
    /// changes — the `Arc` identity tracks the cache's validity.
    prompt_render_cache: Arc<BaseRenderCache>,
}

impl SessionGraphCache {
    fn build(graph: &SessionGraph) -> Result<Self, crate::StoreError> {
        let by_id = graph_node_indices(graph)?;
        let mut active_path_indices =
            ancestry_indices(graph, &by_id, graph.leaf_node_id.as_deref())?;
        active_path_indices.reverse();

        let mut cache = Self {
            by_id,
            active_path_indices,
            active_events: Arc::new(Vec::new()),
            active_messages: Arc::new(Vec::new()),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
        };
        cache.rebuild_read_model(graph);
        Ok(cache)
    }

    fn rebuild_read_model(&mut self, graph: &SessionGraph) {
        let mut active_messages = Vec::with_capacity(self.active_path_indices.len());
        let mut active_events = Vec::with_capacity(self.active_path_indices.len());
        for idx in &self.active_path_indices {
            let node = &graph.nodes[*idx];
            if let Some(event) = node.event() {
                active_events.push(event.clone());
            }
            if let Some(message) = node.message() {
                if !message.is_transient() {
                    active_messages.push(message);
                }
                continue;
            }
        }
        self.active_messages = Arc::new(active_messages);
        self.active_events = Arc::new(active_events);
        self.prompt_render_cache = Arc::new(BaseRenderCache::new());
    }

    fn read_model_for_frame(&self, graph: &SessionGraph, frame_node_id: &str) -> SessionReadModel {
        let mut active_messages = Vec::with_capacity(self.active_path_indices.len());
        let mut active_events = Vec::with_capacity(self.active_path_indices.len());
        let mut in_frame = false;
        for idx in &self.active_path_indices {
            let node = &graph.nodes[*idx];
            if node.node_id == frame_node_id {
                in_frame = true;
            } else if in_frame && matches!(node.payload, SessionNodePayload::FrameOpen { .. }) {
                break;
            }
            if !in_frame {
                continue;
            }
            if let Some(event) = node.event() {
                active_events.push(event.clone());
            }
            if let Some(message) = node.message() {
                if !message.is_transient() {
                    active_messages.push(message);
                }
                continue;
            }
        }
        SessionReadModel {
            active_events: Arc::new(active_events),
            messages: Arc::new(active_messages),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
        }
    }

    fn append_node(
        &mut self,
        node_index: usize,
        node: &SessionNodeRecord,
        previous_leaf_node_id: Option<&str>,
    ) {
        self.by_id.insert(node.node_id.clone(), node_index);
        let parent_matches_leaf = node.parent_node_id.as_deref() == previous_leaf_node_id;
        if !parent_matches_leaf {
            return;
        }
        self.active_path_indices.push(node_index);
        if let Some(event) = node.event() {
            Arc::make_mut(&mut self.active_events).push(event.clone());
        }
        if let Some(message) = node.message()
            && !message.is_transient()
        {
            let messages = Arc::make_mut(&mut self.active_messages);
            messages.push(message);
            self.prompt_render_cache = Arc::new(BaseRenderCache::new());
        }
    }

    fn reserve_append_capacity(&mut self, additional_nodes: usize, additional_messages: usize) {
        self.by_id.reserve(additional_nodes);
        self.active_path_indices.reserve(additional_nodes);
        if additional_messages > 0 {
            Arc::make_mut(&mut self.active_messages).reserve(additional_messages);
        }
    }
}

impl SessionNodeRecord {
    /// Borrow the message identity carried by a conversation node.
    ///
    /// Protocol-event, plugin-state, and frame-boundary nodes return `None`.
    pub fn message_id(&self) -> Option<&str> {
        match &self.payload {
            SessionNodePayload::Event {
                event: SessionHistoryRecord::Conversation(message),
            } => Some(message.id.as_str()),
            SessionNodePayload::Event { .. }
            | SessionNodePayload::Plugin { .. }
            | SessionNodePayload::FrameOpen { .. } => None,
        }
    }

    /// Encode only immutable node content for `node_json`.
    ///
    /// Identity and graph structure are columns so SQL can index, join, and
    /// re-derive reachability without parsing an opaque JSON blob.
    pub fn encode_storage_body(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&StoredSessionNodeBody {
            timestamp: self.timestamp.clone(),
            payload: self.payload.clone(),
        })
    }

    /// Reassembles a node for store implementors from dedicated identity/parent columns and the
    /// immutable JSON body; malformed body JSON is returned as an error.
    pub fn decode_storage_body(
        node_id: String,
        parent_node_id: Option<String>,
        node_json: &str,
    ) -> Result<Self, serde_json::Error> {
        let body = serde_json::from_str::<StoredSessionNodeBody>(node_json)?;
        Ok(Self {
            node_id,
            parent_node_id,
            timestamp: body.timestamp,
            payload: body.payload,
        })
    }

    /// Borrows frame-boundary state for store and protocol implementors, returning `None` for event
    /// and plugin nodes.
    pub fn frame_open(
        &self,
    ) -> Option<(
        &crate::AgentFrameReason,
        &crate::AgentFrameAssignment,
        &crate::ProtocolTurnOptions,
    )> {
        match &self.payload {
            SessionNodePayload::FrameOpen {
                reason,
                assignment,
                protocol_turn_options,
                ..
            } => Some((reason, assignment, protocol_turn_options)),
            SessionNodePayload::Event { .. } | SessionNodePayload::Plugin { .. } => None,
        }
    }

    /// Provider and model captured by this frame boundary.
    pub fn frame_config(&self) -> Option<PersistedSessionConfig> {
        let (_, assignment, _) = self.frame_open()?;
        Some(PersistedSessionConfig {
            provider_id: assignment.policy.recorded_provider_id().to_string(),
            model: assignment.policy.model.clone(),
            turn_budget: assignment.policy.turn_budget,
        })
    }

    /// Decodes a plugin node body for store and protocol implementors, returning `None` when the
    /// node is not a plugin node or its body does not match the requested type.
    pub fn plugin_body<T>(&self) -> Option<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let (_, body) = self.plugin()?;
        T::deserialize(body).ok()
    }
}

impl SessionGraph {
    /// Appends non-transient messages after the active leaf in source order for protocol
    /// implementors applying a read-model delta.
    pub fn append_active_read_delta(&mut self, messages: &[Message]) {
        let appendable_messages = messages
            .iter()
            .filter(|message| !message.is_transient())
            .cloned()
            .collect::<Vec<_>>();

        self.reserve_append_capacity(appendable_messages.len(), appendable_messages.len());
        self.append_message_batch(appendable_messages);
    }

    pub(crate) fn append_active_conversation_messages_at(
        &mut self,
        messages: &[Message],
        timestamp: String,
    ) {
        let appendable_messages = messages
            .iter()
            .filter(|message| !message.is_transient())
            .cloned()
            .collect::<Vec<_>>();
        self.reserve_append_capacity(appendable_messages.len(), appendable_messages.len());
        self.append_message_batch_at(appendable_messages, timestamp);
    }

    /// Builds and structurally validates a graph from node data.
    ///
    /// Graph integrity follows the same pair-assertion rule as durable state: validate before
    /// writing and validate again after reading. Store implementations must map an error returned
    /// here while decoding durable rows to their typed stored-data-corruption variant. A supplied
    /// leaf must resolve, but leaf presence is a resident-graph invariant enforced by
    /// `validate_resident_integrity` at the resident seam.
    pub fn from_nodes(
        nodes: Vec<SessionNodeRecord>,
        leaf_node_id: Option<String>,
    ) -> Result<Self, crate::StoreError> {
        let graph = Self::from_validated_nodes(nodes, leaf_node_id);
        graph.validate_structural_integrity()?;
        Ok(graph)
    }

    /// Builds a graph from nodes whose integrity is already guaranteed by the caller.
    ///
    /// This is deliberately crate-private and named for auditability. It is only appropriate when
    /// the nodes are derived from an already-validated resident graph by an operation that
    /// preserves identity, parent topology, and leaf membership.
    pub(crate) fn from_validated_nodes(
        nodes: Vec<SessionNodeRecord>,
        leaf_node_id: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(SessionGraphData {
                nodes,
                leaf_node_id,
            }),
            cache: Arc::new(OnceLock::new()),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn from_unchecked_nodes_for_testing(
        nodes: Vec<SessionNodeRecord>,
        leaf_node_id: Option<String>,
    ) -> Self {
        Self::from_validated_nodes(nodes, leaf_node_id)
    }

    pub(crate) fn validate_resident_integrity(&self) -> Result<(), crate::StoreError> {
        if !self.nodes.is_empty() && self.leaf_node_id.is_none() {
            return Err(crate::StoreError::InvalidGraphLeaf { leaf_node_id: None });
        }
        self.validate_structural_integrity()
    }

    fn validate_structural_integrity(&self) -> Result<(), crate::StoreError> {
        let by_id = graph_node_indices(self)?;
        ancestry_indices(self, &by_id, self.leaf_node_id.as_deref())?;
        validate_graph_parent_topology(self, &by_id)
    }

    pub(crate) fn append_builder(&self) -> SessionGraphAppendBuilder {
        let namespace = self.leaf_node_id.as_deref().map_or_else(
            || "unscoped-root".to_string(),
            |leaf| format!("unscoped:{leaf}"),
        );
        self.append_builder_in_namespace(namespace)
    }

    pub(crate) fn append_builder_in_namespace(
        &self,
        draft_namespace: impl Into<String>,
    ) -> SessionGraphAppendBuilder {
        SessionGraphAppendBuilder {
            existing_ids: self.nodes.iter().map(|node| node.node_id.clone()).collect(),
            leaf_node_id: self.leaf_node_id.clone(),
            draft_namespace: draft_namespace.into(),
            next_draft_ordinal: 0,
        }
    }

    fn invalidate_cache(&mut self) {
        self.cache = Arc::new(OnceLock::new());
    }

    pub(crate) fn data_mut(&mut self) -> &mut SessionGraphData {
        self.invalidate_cache();
        Arc::make_mut(&mut self.inner)
    }

    pub(crate) fn remap_node_ids(&mut self, _session_id: &str, mapping: &[(String, String)]) {
        if mapping.is_empty() {
            return;
        }
        let mapping = mapping.iter().cloned().collect::<HashMap<_, _>>();
        let data = self.data_mut();
        for node in &mut data.nodes {
            if let Some(derived) = mapping.get(&node.node_id) {
                node.node_id = derived.clone();
            }
            if let Some(parent) = node.parent_node_id.as_mut()
                && let Some(derived) = mapping.get(parent)
            {
                *parent = derived.clone();
            }
        }
        if let Some(leaf) = data.leaf_node_id.as_mut()
            && let Some(derived) = mapping.get(leaf)
        {
            *leaf = derived.clone();
        }
    }

    pub(crate) fn apply_realized_node_timestamps(&mut self, realized: &[RealizedNodeTimestamp]) {
        if realized.is_empty() {
            return;
        }
        let timestamps = realized
            .iter()
            .map(|node| (node.node_id.as_str(), node.timestamp.as_str()))
            .collect::<HashMap<_, _>>();
        for node in &mut self.data_mut().nodes {
            if let Some(timestamp) = timestamps.get(node.node_id.as_str()) {
                node.timestamp = (*timestamp).to_string();
            }
        }
    }

    fn reserve_append_capacity(&mut self, additional_nodes: usize, additional_messages: usize) {
        if additional_nodes == 0 {
            return;
        }
        self.detach_initialized_cache_for_append();
        Arc::make_mut(&mut self.inner)
            .nodes
            .reserve(additional_nodes);
        if let Some(cache_lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = cache_lock.get_mut()
        {
            cache.reserve_append_capacity(additional_nodes, additional_messages);
        }
    }

    fn detach_initialized_cache_for_append(&mut self) {
        if Arc::get_mut(&mut self.cache).is_some() {
            return;
        }
        let Some(cache) = self.cache.get().cloned() else {
            self.invalidate_cache();
            return;
        };
        let lock = OnceLock::new();
        let _ = lock.set(cache);
        self.cache = Arc::new(lock);
    }

    fn try_cache(&self) -> Result<&SessionGraphCache, crate::StoreError> {
        if let Some(cache) = self.cache.get() {
            return Ok(cache);
        }
        let cache = SessionGraphCache::build(self)?;
        let _ = self.cache.set(cache);
        Ok(self
            .cache
            .get()
            .expect("session graph cache was initialized"))
    }

    /// Returns the cache for a graph that passed construction-time validation.
    ///
    /// The panic is a last-resort invariant for defects introduced by later in-memory mutation;
    /// durable rows are rejected by [`Self::from_nodes`] before they can reach this reader.
    fn cache(&self) -> &SessionGraphCache {
        self.try_cache()
            .unwrap_or_else(|err| panic!("invalid resident session graph: {err}"))
    }

    fn append_message_batch(&mut self, messages: Vec<Message>) {
        self.append_message_batch_at(messages, crate::SystemClock.timestamp_rfc3339());
    }

    fn append_message_batch_at(&mut self, messages: Vec<Message>, timestamp: String) {
        if messages.is_empty() {
            return;
        }
        self.append_node_drafts_at_inner(
            None,
            messages.into_iter().map(SessionNodeDraft::message),
            timestamp,
        );
    }

    fn append_prebuilt_nodes(&mut self, nodes: Vec<SessionNodeRecord>) {
        if nodes.is_empty() {
            return;
        }

        self.detach_initialized_cache_for_append();
        if let Some(cache_lock) = Arc::get_mut(&mut self.cache)
            && let Some(cache) = cache_lock.get_mut()
        {
            let data = Arc::make_mut(&mut self.inner);
            for node in nodes {
                let previous_leaf = data.leaf_node_id.clone();
                let node_id = node.node_id.clone();
                data.nodes.push(node);
                cache.append_node(
                    data.nodes.len() - 1,
                    data.nodes.last().expect("just appended graph node"),
                    previous_leaf.as_deref(),
                );
                data.leaf_node_id = Some(node_id);
            }
            return;
        }

        let data = self.data_mut();
        for node in nodes {
            data.leaf_node_id = Some(node.node_id.clone());
            data.nodes.push(node);
        }
    }

    /// Appends one message after the active leaf for protocol implementors and returns its
    /// content-derived node ID.
    pub fn append_message(&mut self, message: Message) -> String {
        self.append_node_draft(SessionNodeDraft::message(message))
    }

    /// Appends a plugin payload after the active leaf for protocol implementors extending session
    /// history, returning the content-derived node ID.
    pub fn append_plugin(
        &mut self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
    ) -> String {
        self.append_node_draft(SessionNodeDraft::plugin(plugin_type, body))
    }

    fn try_active_path_nodes(&self) -> Result<Vec<&SessionNodeRecord>, crate::StoreError> {
        Ok(self
            .try_cache()?
            .active_path_indices
            .iter()
            .map(|idx| &self.nodes[*idx])
            .collect())
    }

    pub(crate) fn read_model(&self) -> SessionReadModel {
        let cache = self.cache();
        SessionReadModel {
            active_events: Arc::clone(&cache.active_events),
            messages: Arc::clone(&cache.active_messages),
            prompt_render_cache: Arc::clone(&cache.prompt_render_cache),
        }
    }

    pub(crate) fn read_model_for_frame(&self, frame_node_id: &str) -> SessionReadModel {
        if frame_node_id.is_empty() {
            return self.read_model();
        }
        self.cache().read_model_for_frame(self, frame_node_id)
    }

    /// Resolve the canonical current frame for `leaf_node_id`.
    ///
    /// The head caches this answer for bounded reads, but ancestry remains the
    /// truth and is used to validate every stored pointer.
    pub fn append_protocol_event(&mut self, event: ProtocolEvent) -> String {
        self.append_node_draft(SessionNodeDraft::protocol_event(event))
    }

    pub(crate) fn append_node_draft(&mut self, draft: SessionNodeDraft) -> String {
        self.append_node_drafts([draft])
            .into_iter()
            .next()
            .expect("single draft append must create one node")
    }

    pub(crate) fn append_node_drafts<I>(&mut self, drafts: I) -> Vec<String>
    where
        I: IntoIterator<Item = SessionNodeDraft>,
    {
        self.append_node_drafts_at_inner(None, drafts, crate::SystemClock.timestamp_rfc3339())
    }

    pub(crate) fn append_frame_open_with_id_at(
        &mut self,
        frame_node_id: String,
        frame_key: String,
        reason: crate::AgentFrameReason,
        assignment: crate::AgentFrameAssignment,
        protocol_turn_options: crate::ProtocolTurnOptions,
        timestamp: String,
    ) -> bool {
        if self.find_node(&frame_node_id).is_some() {
            return false;
        }
        self.append_prebuilt_nodes(vec![SessionNodeRecord {
            node_id: frame_node_id,
            parent_node_id: self.leaf_node_id.clone(),
            timestamp,
            payload: SessionNodePayload::FrameOpen {
                frame_key,
                reason,
                assignment,
                protocol_turn_options,
            },
        }]);
        true
    }

    pub(crate) fn try_agent_frame_records(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::AgentFrameRecord>, crate::StoreError> {
        let mut previous_frame_node_id = None;
        let mut frames = Vec::new();
        for node in self.try_active_path_nodes()? {
            let Some((reason, assignment, protocol_turn_options)) = node.frame_open() else {
                continue;
            };
            frames.push(crate::AgentFrameRecord::new_at(
                node.node_id.clone(),
                session_id.to_string(),
                previous_frame_node_id.clone(),
                reason.clone(),
                assignment.clone(),
                protocol_turn_options.clone(),
                node.timestamp.clone(),
            ));
            previous_frame_node_id = Some(node.node_id.clone());
        }
        Ok(frames)
    }

    pub(crate) fn append_node_drafts_at<I>(
        &mut self,
        draft_namespace: &str,
        drafts: I,
        timestamp: String,
    ) -> Vec<String>
    where
        I: IntoIterator<Item = SessionNodeDraft>,
    {
        self.append_node_drafts_at_inner(Some(draft_namespace), drafts, timestamp)
    }

    fn append_node_drafts_at_inner<I>(
        &mut self,
        draft_namespace: Option<&str>,
        drafts: I,
        timestamp: String,
    ) -> Vec<String>
    where
        I: IntoIterator<Item = SessionNodeDraft>,
    {
        let mut builder = draft_namespace.map_or_else(
            || self.append_builder(),
            |namespace| self.append_builder_in_namespace(namespace),
        );
        let nodes = builder.append_drafts_at(drafts, timestamp);
        let node_ids = nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        self.append_prebuilt_nodes(nodes);
        node_ids
    }

    /// Exposes user message count to store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn user_message_count(&self) -> usize {
        self.nodes
            .iter()
            .filter_map(SessionNodeRecord::message)
            .filter(|message| matches!(message.role, MessageRole::User))
            .count()
    }

    /// Returns normalized search text from the first user message in storage order for protocol
    /// embedders, or an empty string when none exists.
    pub fn first_user_message(&self) -> String {
        self.nodes
            .iter()
            .filter_map(SessionNodeRecord::message)
            .find(|message| matches!(message.role, MessageRole::User))
            .map(|message| first_message_search_text(&message))
            .unwrap_or_default()
    }

    /// Updates leaf node id state for store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn set_leaf_node_id(&mut self, node_id: Option<String>) {
        self.data_mut().leaf_node_id = node_id;
    }

    /// Updates node record state for store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
    pub fn push_node_record(&mut self, node: SessionNodeRecord) {
        self.data_mut().nodes.push(node);
    }

    /// Tests branch liveness for store and protocol implementors against the current leaf ancestry.
    ///
    /// The panic is a last-resort invariant for post-construction mutation defects. Durable data
    /// is validated by [`Self::from_nodes`] before a resident graph is returned.
    pub fn active_path_contains(&self, node_id: &str) -> bool {
        self.try_active_path_contains(node_id)
            .unwrap_or_else(|err| panic!("invalid resident session graph: {err}"))
    }

    pub(crate) fn try_active_path_contains(
        &self,
        node_id: &str,
    ) -> Result<bool, crate::StoreError> {
        let cache = self.try_cache()?;
        let Some(node_index) = cache.by_id.get(node_id) else {
            return Ok(false);
        };
        Ok(cache.active_path_indices.contains(node_index))
    }

    /// Return a resident graph containing only the current ancestry path.
    ///
    /// This is a memory-residency trim. It does not create a durable fork or
    /// move a persisted session head. The caller must have validated the source graph.
    pub fn trim_to_active_path(&self) -> SessionGraph {
        let path = self.active_path_nodes();
        // Selecting the ancestry of a validated graph preserves unique ids, complete parents, and
        // the existing leaf, so repeating the full validation on this hot read projection is
        // unnecessary.
        SessionGraph::from_validated_nodes(
            path.into_iter().cloned().collect(),
            self.leaf_node_id.clone(),
        )
    }

    pub(crate) fn try_trim_to_active_path(&self) -> Result<SessionGraph, crate::StoreError> {
        let by_id = graph_node_indices(self)?;
        let mut path = ancestry_indices(self, &by_id, self.leaf_node_id.as_deref())?;
        path.reverse();
        SessionGraph::from_nodes(
            path.into_iter()
                .map(|index| self.nodes[index].clone())
                .collect(),
            self.leaf_node_id.clone(),
        )
    }

    /// Looks up any resident node by ID for store and protocol implementors, including nodes
    /// outside the active path; an unknown ID returns `None`.
    pub fn find_node(&self, node_id: &str) -> Option<&SessionNodeRecord> {
        self.cache()
            .by_id
            .get(node_id)
            .and_then(|idx| self.nodes.get(*idx))
    }

    /// Rewrites the active readable tail and moves the resident leaf while retaining historical
    /// branches and excluding transient replacement messages.
    ///
    /// The resulting graph is a read projection. It must never be committed against an existing
    /// session head because the rewritten tail is not parented from that durable head.
    pub fn rewrite_active_read_tail(&mut self, messages: &[Message]) {
        self.rewrite_active_read_tail_from(None, messages);
    }

    pub(crate) fn rewrite_active_read_tail_for_frame(
        &mut self,
        frame_node_id: &str,
        messages: &[Message],
    ) {
        self.rewrite_active_read_tail_from(Some(frame_node_id), messages);
    }

    fn rewrite_active_read_tail_from(&mut self, frame_node_id: Option<&str>, messages: &[Message]) {
        let active_path = self.active_path_nodes();
        let current_nodes = frame_node_id.map_or(active_path.as_slice(), |frame_node_id| {
            active_path
                .iter()
                .position(|node| node.node_id == frame_node_id)
                .map_or(&[][..], |index| &active_path[index..])
        });
        if frame_node_id.is_some() && current_nodes.is_empty() {
            return;
        }
        let existing_ids = self
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<HashSet<_>>();
        let replacement = build_active_read_replacement(
            current_nodes.iter().copied(),
            &existing_ids,
            &format!(
                "unscoped-replacement:{}",
                self.leaf_node_id.as_deref().unwrap_or("root")
            ),
            messages,
            crate::SystemClock.timestamp_rfc3339(),
        );
        let data = self.data_mut();
        data.leaf_node_id = replacement.leaf_node_id;
        data.nodes.extend(replacement.new_tail_nodes);
    }

    /// Builds a `SessionGraph` from active read state data for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn from_active_read_state(messages: &[Message]) -> Self {
        let mut graph = Self::default();
        graph.rewrite_active_read_tail(messages);
        graph
    }

    /// Exposes message tree to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn.
    pub fn message_tree(&self) -> Vec<SessionMessageTreeNode> {
        let active_node_ids = self
            .active_path_nodes()
            .into_iter()
            .filter(|node| node.message().is_some())
            .map(|node| node.node_id.clone())
            .collect::<HashSet<_>>();

        let message_nodes = self
            .nodes
            .iter()
            .filter_map(|node| {
                let message = node.message()?.clone();
                let parent_message_node_id =
                    self.nearest_message_ancestor(node.parent_node_id.as_deref());
                Some(SessionMessageTreeNode {
                    node_id: node.node_id.clone(),
                    parent_message_node_id,
                    message,
                    timestamp: node.timestamp.clone(),
                    children: Vec::new(),
                    active: active_node_ids.contains(&node.node_id),
                })
            })
            .collect::<Vec<_>>();

        build_tree(message_nodes)
    }

    fn nearest_message_ancestor(&self, node_id: Option<&str>) -> Option<String> {
        let idx = self
            .nearest_ancestor_index(node_id, |node| node.message().is_some())
            .ok()??;
        Some(self.nodes[idx].node_id.clone())
    }

    fn nearest_ancestor_index(
        &self,
        node_id: Option<&str>,
        predicate: impl FnMut(&SessionNodeRecord) -> bool,
    ) -> Result<Option<usize>, crate::StoreError> {
        if let Some(cache) = self.cache.get() {
            return nearest_ancestor_index(self, &cache.by_id, node_id, predicate);
        }
        let by_id = graph_node_indices(self)?;
        nearest_ancestor_index(self, &by_id, node_id, predicate)
    }
}

fn nearest_ancestor_index(
    graph: &SessionGraph,
    by_id: &HashMap<String, usize>,
    node_id: Option<&str>,
    mut predicate: impl FnMut(&SessionNodeRecord) -> bool,
) -> Result<Option<usize>, crate::StoreError> {
    let mut current = node_id.and_then(|node_id| by_id.get(node_id).copied());
    let mut remaining = graph.nodes.len();
    while let Some(idx) = current {
        let node = &graph.nodes[idx];
        if predicate(node) {
            return Ok(Some(idx));
        }
        if remaining == 0 {
            return Err(crate::StoreError::InvalidGraphParent {
                node_id: node.node_id.clone(),
                expected: None,
                actual: node.parent_node_id.clone(),
            });
        }
        remaining -= 1;
        current = node
            .parent_node_id
            .as_ref()
            .and_then(|parent| by_id.get(parent).copied());
    }
    Ok(None)
}

fn build_tree(mut nodes: Vec<SessionMessageTreeNode>) -> Vec<SessionMessageTreeNode> {
    let mut children_by_parent = HashMap::<Option<String>, Vec<SessionMessageTreeNode>>::new();
    for node in nodes.drain(..) {
        children_by_parent
            .entry(node.parent_message_node_id.clone())
            .or_default()
            .push(node);
    }
    let mut roots = build_tree_children(None, &mut children_by_parent);
    sort_tree(&mut roots);
    roots
}

fn sort_tree(nodes: &mut [SessionMessageTreeNode]) {
    nodes.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    for node in nodes {
        sort_tree(&mut node.children);
    }
}

fn build_tree_children(
    parent_id: Option<String>,
    children_by_parent: &mut HashMap<Option<String>, Vec<SessionMessageTreeNode>>,
) -> Vec<SessionMessageTreeNode> {
    let mut children = children_by_parent.remove(&parent_id).unwrap_or_default();
    for child in &mut children {
        child.children = build_tree_children(Some(child.node_id.clone()), children_by_parent);
    }
    children
}

pub(crate) fn build_active_read_replacement<'a>(
    current_nodes: impl IntoIterator<Item = &'a SessionNodeRecord>,
    existing_node_ids: &HashSet<String>,
    draft_namespace: &str,
    messages: &[Message],
    timestamp: String,
) -> ActiveReadReplacement {
    let target = messages
        .iter()
        .filter(|message| !message.is_transient())
        .collect::<Vec<_>>();

    let mut target_idx = 0usize;
    let mut leaf_node_id = None;
    for node in current_nodes {
        if node
            .message()
            .map(|message| message.is_transient())
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(current_message) = node.message() {
            let Some(target_item) = target.get(target_idx) else {
                break;
            };
            if serde_json::to_value(&current_message).ok() != serde_json::to_value(target_item).ok()
            {
                break;
            }
            leaf_node_id = Some(node.node_id.clone());
            target_idx += 1;
        } else {
            leaf_node_id = Some(node.node_id.clone());
        }
    }

    let mut new_node_ids = HashSet::new();
    let mut new_tail_nodes = Vec::new();

    for message in target.into_iter().skip(target_idx) {
        let parent_node_id = leaf_node_id.clone();
        let node_id =
            next_replacement_draft_node_id(existing_node_ids, &new_node_ids, draft_namespace);
        let node = SessionNodeRecord {
            node_id,
            parent_node_id,
            timestamp: timestamp.clone(),
            payload: SessionNodePayload::Event {
                event: SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                    message.clone(),
                )),
            },
        };
        new_node_ids.insert(node.node_id.clone());
        leaf_node_id = Some(node.node_id.clone());
        new_tail_nodes.push(node);
    }

    ActiveReadReplacement {
        leaf_node_id,
        new_tail_nodes,
    }
}

pub(crate) fn build_active_read_projection<'a>(
    current_nodes: impl IntoIterator<Item = &'a SessionNodeRecord>,
    messages: &[Message],
) -> ActiveReadProjection {
    let target = messages
        .iter()
        .filter(|message| !message.is_transient())
        .collect::<Vec<_>>();

    let mut active_events = Vec::new();
    let mut active_messages = Vec::new();
    let mut target_idx = 0usize;
    for node in current_nodes {
        if node
            .message()
            .map(|message| message.is_transient())
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(current_message) = node.message() {
            let Some(target_item) = target.get(target_idx) else {
                break;
            };
            if serde_json::to_value(&current_message).ok() != serde_json::to_value(target_item).ok()
            {
                break;
            }
            push_active_read_node(node, &mut active_events, &mut active_messages);
            target_idx += 1;
        } else {
            push_active_read_node(node, &mut active_events, &mut active_messages);
        }
    }

    for message in target.into_iter().skip(target_idx) {
        active_events.push(SessionHistoryRecord::Conversation(
            ConversationRecord::from_message(message.clone()),
        ));
        active_messages.push(message.clone());
    }

    ActiveReadProjection {
        active_events,
        active_messages,
    }
}

fn push_active_read_node(
    node: &SessionNodeRecord,
    active_events: &mut Vec<SessionHistoryRecord>,
    active_messages: &mut Vec<Message>,
) {
    if let Some(event) = node.event() {
        active_events.push(event.clone());
    }
    if let Some(message) = node.message()
        && !message.is_transient()
    {
        active_messages.push(message);
    }
}

fn next_replacement_draft_node_id(
    existing_ids: &HashSet<String>,
    new_ids: &HashSet<String>,
    draft_namespace: &str,
) -> String {
    for ordinal in 0_u64.. {
        let candidate = draft_node_id(draft_namespace, ordinal);
        if !existing_ids.contains(&candidate) && !new_ids.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("draft node id space exhausted")
}

fn first_message_search_text(message: &Message) -> String {
    message
        .parts
        .iter()
        .filter_map(|part| match part.kind {
            crate::PartKind::ToolCall | crate::PartKind::ToolResult => None,
            crate::PartKind::Attachment => Some("[Attachment]".to_string()),
            _ => (!part.content.trim().is_empty()).then(|| part.content.clone()),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string()
}

#[cfg(test)]
#[path = "session_graph_tests.rs"]
mod tests;
