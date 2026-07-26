use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::{Arc, OnceLock};

use crate::session_model::{ConversationRecord, ProtocolEvent, SessionHistoryRecord};
use crate::{BaseRenderCache, Clock, Message, MessageRole, PromptUsage, TokenUsage};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<crate::CausalRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_frame_id: Option<crate::AgentFrameId>,
    pub timestamp: String,
    #[serde(flatten)]
    pub payload: SessionNodePayload,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionNodeDraft {
    payload: SessionNodeDraftPayload,
    caused_by: Option<crate::CausalRef>,
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
            caused_by: None,
        }
    }

    pub(crate) fn plugin(plugin_type: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            payload: SessionNodeDraftPayload::Plugin {
                plugin_type: plugin_type.into(),
                body,
            },
            caused_by: None,
        }
    }

    pub(crate) fn protocol_event(event: ProtocolEvent) -> Self {
        Self {
            payload: SessionNodeDraftPayload::ProtocolEvent(event),
            caused_by: None,
        }
    }

    pub(crate) fn event(event: SessionHistoryRecord) -> Self {
        match event {
            SessionHistoryRecord::Conversation(record) => Self::message(record.to_message()),
            SessionHistoryRecord::Protocol(event) => Self::protocol_event(event),
        }
    }

    pub(crate) fn with_caused_by(mut self, caused_by: Option<crate::CausalRef>) -> Self {
        self.caused_by = caused_by;
        self
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
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PersistedSessionConfig {
    pub provider_id: String,
    pub model: crate::ModelSpec,
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
    agent_frame_id: Option<crate::AgentFrameId>,
    next_draft_ordinal: u64,
}

impl SessionGraphAppendBuilder {
    pub(crate) fn with_agent_frame_id(
        mut self,
        agent_frame_id: impl Into<crate::AgentFrameId>,
    ) -> Self {
        self.agent_frame_id = Some(agent_frame_id.into());
        self
    }

    pub(crate) fn agent_frame_id(&self) -> Option<&str> {
        self.agent_frame_id.as_deref()
    }

    pub(crate) fn leaf_node_id(&self) -> Option<&String> {
        self.leaf_node_id.as_ref()
    }

    pub(crate) fn set_leaf_node_id(&mut self, leaf_node_id: Option<String>) {
        self.leaf_node_id = leaf_node_id;
    }

    pub(crate) fn register_existing_node_ids<'a>(
        &mut self,
        node_ids: impl IntoIterator<Item = &'a str>,
    ) {
        self.existing_ids
            .extend(node_ids.into_iter().map(ToOwned::to_owned));
    }

    pub(crate) fn existing_node_ids(&self) -> &HashSet<String> {
        &self.existing_ids
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
            let (node_id, caused_by, payload) = match draft.payload {
                SessionNodeDraftPayload::Message(message) => {
                    let node_id = self.next_draft_node_id();
                    let caused_by = draft
                        .caused_by
                        .or_else(|| causal_ref_from_message_origin(&message.origin));
                    (
                        node_id,
                        caused_by,
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
                        draft.caused_by,
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
                        draft.caused_by,
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
                caused_by,
                agent_frame_id: self.agent_frame_id.clone(),
                timestamp: timestamp.clone(),
                payload,
            });
        }
        nodes
    }

    fn next_draft_node_id(&mut self) -> String {
        loop {
            let candidate = format!("draft-node/v1/{}", self.next_draft_ordinal);
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
    fn build(graph: &SessionGraph) -> Self {
        let by_id = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.node_id.clone(), idx))
            .collect::<HashMap<_, _>>();
        let mut active_path_indices = Vec::new();
        let mut current = graph
            .leaf_node_id
            .as_ref()
            .and_then(|node_id| by_id.get(node_id).copied());
        while let Some(idx) = current {
            active_path_indices.push(idx);
            current = graph.nodes[idx]
                .parent_node_id
                .as_ref()
                .and_then(|node_id| by_id.get(node_id).copied());
        }
        active_path_indices.reverse();

        let mut cache = Self {
            by_id,
            active_path_indices,
            active_events: Arc::new(Vec::new()),
            active_messages: Arc::new(Vec::new()),
            prompt_render_cache: Arc::new(BaseRenderCache::new()),
        };
        cache.rebuild_read_model(graph);
        cache
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

    fn read_model_for_agent_frame(
        &self,
        graph: &SessionGraph,
        frame_id: &str,
        include_unscoped: bool,
    ) -> SessionReadModel {
        let mut active_messages = Vec::with_capacity(self.active_path_indices.len());
        let mut active_events = Vec::with_capacity(self.active_path_indices.len());
        for idx in &self.active_path_indices {
            let node = &graph.nodes[*idx];
            if !node_belongs_to_agent_frame(node, frame_id, include_unscoped) {
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
    pub fn event(&self) -> Option<&SessionHistoryRecord> {
        match &self.payload {
            SessionNodePayload::Event { event } => Some(event),
            SessionNodePayload::Plugin { .. } => None,
        }
    }

    pub fn message(&self) -> Option<Message> {
        match self.event()? {
            SessionHistoryRecord::Conversation(record) => Some(record.to_message()),
            _ => None,
        }
    }

    pub fn plugin(&self) -> Option<(&str, &serde_json::Value)> {
        match &self.payload {
            SessionNodePayload::Event { .. } => None,
            SessionNodePayload::Plugin { plugin_type, body } => {
                Some((plugin_type.as_str(), body.as_ref()))
            }
        }
    }

    pub fn plugin_body<T>(&self) -> Option<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let (_, body) = self.plugin()?;
        T::deserialize(body).ok()
    }
}

impl SessionGraph {
    pub fn append_active_read_delta(&mut self, messages: &[Message]) {
        self.append_active_read_delta_scoped(None, messages);
    }

    pub fn append_active_read_delta_for_agent_frame(
        &mut self,
        agent_frame_id: &str,
        messages: &[Message],
    ) {
        self.append_active_read_delta_scoped(Some(agent_frame_id), messages);
    }

    fn append_active_read_delta_scoped(
        &mut self,
        agent_frame_id: Option<&str>,
        messages: &[Message],
    ) {
        let appendable_messages = messages
            .iter()
            .filter(|message| !message.is_transient())
            .cloned()
            .collect::<Vec<_>>();

        self.reserve_append_capacity(appendable_messages.len(), appendable_messages.len());
        self.append_message_batch_scoped(agent_frame_id, appendable_messages);
    }

    pub(crate) fn append_active_conversation_messages_for_agent_frame(
        &mut self,
        agent_frame_id: &str,
        messages: &[Message],
    ) {
        self.append_active_conversation_messages_scoped(Some(agent_frame_id), messages);
    }

    fn append_active_conversation_messages_scoped(
        &mut self,
        agent_frame_id: Option<&str>,
        messages: &[Message],
    ) {
        self.append_active_conversation_messages_scoped_at(
            agent_frame_id,
            messages,
            crate::SystemClock.timestamp_rfc3339(),
        );
    }

    pub(crate) fn append_active_conversation_messages_for_agent_frame_at(
        &mut self,
        agent_frame_id: &str,
        messages: &[Message],
        timestamp: String,
    ) {
        self.append_active_conversation_messages_scoped_at(
            Some(agent_frame_id),
            messages,
            timestamp,
        );
    }

    fn append_active_conversation_messages_scoped_at(
        &mut self,
        agent_frame_id: Option<&str>,
        messages: &[Message],
        timestamp: String,
    ) {
        let appendable_messages = messages
            .iter()
            .filter(|message| !message.is_transient())
            .cloned()
            .collect::<Vec<_>>();
        self.reserve_append_capacity(appendable_messages.len(), appendable_messages.len());
        self.append_message_batch_scoped_at(agent_frame_id, appendable_messages, timestamp);
    }

    pub fn from_nodes(nodes: Vec<SessionNodeRecord>, leaf_node_id: Option<String>) -> Self {
        Self {
            inner: Arc::new(SessionGraphData {
                nodes,
                leaf_node_id,
            }),
            cache: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn append_builder(&self) -> SessionGraphAppendBuilder {
        SessionGraphAppendBuilder {
            existing_ids: self.nodes.iter().map(|node| node.node_id.clone()).collect(),
            leaf_node_id: self.leaf_node_id.clone(),
            agent_frame_id: None,
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

    pub(crate) fn remap_node_ids(&mut self, mapping: &[(String, String)]) {
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

    fn cache(&self) -> &SessionGraphCache {
        self.cache.get_or_init(|| SessionGraphCache::build(self))
    }

    fn append_message_batch_scoped(
        &mut self,
        agent_frame_id: Option<&str>,
        messages: Vec<Message>,
    ) {
        self.append_message_batch_scoped_at(
            agent_frame_id,
            messages,
            crate::SystemClock.timestamp_rfc3339(),
        );
    }

    fn append_message_batch_scoped_at(
        &mut self,
        agent_frame_id: Option<&str>,
        messages: Vec<Message>,
        timestamp: String,
    ) {
        if messages.is_empty() {
            return;
        }
        self.append_node_drafts_scoped_at(
            agent_frame_id,
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

    pub fn append_message(&mut self, message: Message) -> String {
        self.append_node_draft(SessionNodeDraft::message(message))
    }

    pub fn append_plugin(
        &mut self,
        plugin_type: impl Into<String>,
        body: serde_json::Value,
    ) -> String {
        self.append_node_draft(SessionNodeDraft::plugin(plugin_type, body))
    }

    pub fn active_path_nodes(&self) -> Vec<&SessionNodeRecord> {
        self.cache()
            .active_path_indices
            .iter()
            .map(|idx| &self.nodes[*idx])
            .collect()
    }

    pub(crate) fn read_model(&self) -> SessionReadModel {
        let cache = self.cache();
        SessionReadModel {
            active_events: Arc::clone(&cache.active_events),
            messages: Arc::clone(&cache.active_messages),
            prompt_render_cache: Arc::clone(&cache.prompt_render_cache),
        }
    }

    pub(crate) fn read_model_for_agent_frame(
        &self,
        frame_id: &str,
        include_unscoped: bool,
    ) -> SessionReadModel {
        if frame_id.is_empty() {
            return self.read_model();
        }
        self.cache()
            .read_model_for_agent_frame(self, frame_id, include_unscoped)
    }

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
        self.append_node_drafts_scoped_at(None, drafts, crate::SystemClock.timestamp_rfc3339())
    }

    pub(crate) fn append_node_drafts_for_agent_frame_at<I>(
        &mut self,
        agent_frame_id: &str,
        drafts: I,
        timestamp: String,
    ) -> Vec<String>
    where
        I: IntoIterator<Item = SessionNodeDraft>,
    {
        self.append_node_drafts_scoped_at(Some(agent_frame_id), drafts, timestamp)
    }

    fn append_node_drafts_scoped_at<I>(
        &mut self,
        agent_frame_id: Option<&str>,
        drafts: I,
        timestamp: String,
    ) -> Vec<String>
    where
        I: IntoIterator<Item = SessionNodeDraft>,
    {
        let mut builder = self.append_builder();
        if let Some(agent_frame_id) = agent_frame_id {
            builder = builder.with_agent_frame_id(agent_frame_id.to_string());
        }
        let nodes = builder.append_drafts_at(drafts, timestamp);
        let node_ids = nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<Vec<_>>();
        self.append_prebuilt_nodes(nodes);
        node_ids
    }

    pub fn user_message_count(&self) -> usize {
        self.nodes
            .iter()
            .filter_map(SessionNodeRecord::message)
            .filter(|message| matches!(message.role, MessageRole::User))
            .count()
    }

    pub fn first_user_message(&self) -> String {
        self.nodes
            .iter()
            .filter_map(SessionNodeRecord::message)
            .find(|message| matches!(message.role, MessageRole::User))
            .map(|message| first_message_search_text(&message))
            .unwrap_or_default()
    }

    pub fn branch_to(&mut self, node_id: Option<String>) {
        self.data_mut().leaf_node_id = node_id;
    }

    pub fn set_leaf_node_id(&mut self, node_id: Option<String>) {
        self.data_mut().leaf_node_id = node_id;
    }

    pub fn push_node_record(&mut self, node: SessionNodeRecord) {
        self.data_mut().nodes.push(node);
    }

    pub fn extend_node_records<I>(&mut self, nodes: I)
    where
        I: IntoIterator<Item = SessionNodeRecord>,
    {
        self.data_mut().nodes.extend(nodes);
    }

    /// Append nodes that extend the current active path, advancing the
    /// leaf to the last node and updating the cache incrementally
    /// instead of invalidating it. Use this when the appended nodes are
    /// genuinely new descendants of the current leaf — e.g. the
    /// turn-driver merging turn-local graph editor deltas into the base graph.
    /// Use `extend_node_records` + `set_leaf_node_id` for store-side
    /// replay paths that don't follow the active-path append shape.
    pub fn extend_active_path(&mut self, nodes: Vec<SessionNodeRecord>) {
        self.append_prebuilt_nodes(nodes);
    }

    pub fn active_path_contains(&self, node_id: &str) -> bool {
        self.active_path_nodes()
            .into_iter()
            .any(|node| node.node_id == node_id)
    }

    /// If `leaf_node_id` points to a node that no longer exists in
    /// `self.nodes` (e.g. after compaction rewrote the graph, or a
    /// stored session referenced a node that was later purged), fall
    /// back to the most recent message node. Returns `true` if the
    /// leaf was repaired. Call this on load paths where an orphan
    /// leaf would project to an empty transcript and silently drop
    /// the user's history.
    pub fn heal_orphaned_leaf(&mut self) -> bool {
        if let Some(leaf) = self.leaf_node_id.as_ref()
            && self.find_node(leaf).is_none()
        {
            let fallback = self
                .nodes
                .iter()
                .rev()
                .find(|node| node.message().is_some())
                .map(|node| node.node_id.clone());
            self.data_mut().leaf_node_id = fallback;
            return true;
        }
        false
    }

    pub fn fork_current_path(&self) -> SessionGraph {
        let path = self.active_path_nodes();
        SessionGraph::from_nodes(
            path.into_iter().cloned().collect(),
            self.leaf_node_id.clone(),
        )
    }

    pub fn find_node(&self, node_id: &str) -> Option<&SessionNodeRecord> {
        self.cache()
            .by_id
            .get(node_id)
            .and_then(|idx| self.nodes.get(*idx))
    }

    pub fn node_index(&self, node_id: &str) -> Option<usize> {
        self.cache().by_id.get(node_id).copied()
    }

    pub fn replace_active_read_state(&mut self, messages: &[Message]) {
        self.replace_active_read_state_scoped(None, messages);
    }

    pub fn replace_active_read_state_for_agent_frame(
        &mut self,
        agent_frame_id: &str,
        messages: &[Message],
    ) {
        self.replace_active_read_state_scoped(Some(agent_frame_id), messages);
    }

    fn replace_active_read_state_scoped(
        &mut self,
        agent_frame_id: Option<&str>,
        messages: &[Message],
    ) {
        let current_nodes = self.active_path_nodes();
        let existing_ids = self
            .nodes
            .iter()
            .map(|node| node.node_id.clone())
            .collect::<HashSet<_>>();
        let replacement = build_active_read_replacement(
            current_nodes,
            &existing_ids,
            agent_frame_id,
            messages,
            crate::SystemClock.timestamp_rfc3339(),
        );
        let data = self.data_mut();
        data.leaf_node_id = replacement.leaf_node_id;
        data.nodes.extend(replacement.new_tail_nodes);
    }

    pub fn from_active_read_state(messages: &[Message]) -> Self {
        let mut graph = Self::default();
        graph.replace_active_read_state(messages);
        graph
    }

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
        let by_id = self
            .nodes
            .iter()
            .map(|node| (node.node_id.as_str(), node))
            .collect::<HashMap<_, _>>();
        let mut current = node_id.and_then(|id| by_id.get(id).copied());
        while let Some(node) = current {
            if node.message().is_some() {
                return Some(node.node_id.clone());
            }
            current = node
                .parent_node_id
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied());
        }
        None
    }
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

fn node_belongs_to_agent_frame(
    node: &SessionNodeRecord,
    frame_id: &str,
    include_unscoped: bool,
) -> bool {
    match node.agent_frame_id.as_deref() {
        Some(node_frame_id) => node_frame_id == frame_id,
        None => include_unscoped,
    }
}

pub(crate) fn build_active_read_replacement<'a>(
    current_nodes: impl IntoIterator<Item = &'a SessionNodeRecord>,
    existing_node_ids: &HashSet<String>,
    agent_frame_id: Option<&str>,
    messages: &[Message],
    timestamp: String,
) -> ActiveReadReplacement {
    let target = messages
        .iter()
        .filter(|message| !message.is_transient())
        .collect::<Vec<_>>();

    let mut active_events = Vec::new();
    let mut active_messages = Vec::new();
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
            push_active_read_node(node, &mut active_events, &mut active_messages);
            leaf_node_id = Some(node.node_id.clone());
            target_idx += 1;
        } else {
            push_active_read_node(node, &mut active_events, &mut active_messages);
            leaf_node_id = Some(node.node_id.clone());
        }
    }

    let mut new_node_ids = HashSet::new();
    let mut new_tail_nodes = Vec::new();

    for message in target.into_iter().skip(target_idx) {
        let parent_node_id = leaf_node_id.clone();
        let node_id = next_replacement_draft_node_id(existing_node_ids, &new_node_ids);
        let node = SessionNodeRecord {
            node_id,
            parent_node_id,
            caused_by: causal_ref_from_message_origin(&message.origin),
            agent_frame_id: agent_frame_id.map(ToOwned::to_owned),
            timestamp: timestamp.clone(),
            payload: SessionNodePayload::Event {
                event: SessionHistoryRecord::Conversation(ConversationRecord::from_message(
                    message.clone(),
                )),
            },
        };
        new_node_ids.insert(node.node_id.clone());
        leaf_node_id = Some(node.node_id.clone());
        push_active_read_node(&node, &mut active_events, &mut active_messages);
        new_tail_nodes.push(node);
    }

    ActiveReadReplacement {
        leaf_node_id,
        new_tail_nodes,
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
) -> String {
    for ordinal in 0_u64.. {
        let candidate = format!("draft-node/v1/{ordinal}");
        if !existing_ids.contains(&candidate) && !new_ids.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("draft node id space exhausted")
}

fn causal_ref_from_message_origin(
    origin: &Option<crate::MessageOrigin>,
) -> Option<crate::CausalRef> {
    let Some(crate::MessageOrigin::Process {
        process_id,
        sequence,
        ..
    }) = origin
    else {
        return None;
    };
    Some(crate::CausalRef::ProcessEvent {
        process_id: process_id.clone(),
        sequence: *sequence,
    })
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
mod tests {
    use super::*;
    use crate::{Part, PartKind, PruneState, shared_parts};

    fn text_message(id: &str, role: MessageRole, content: &str) -> Message {
        Message {
            id: id.to_string(),
            role,
            parts: shared_parts(vec![Part {
                id: format!("{id}.p0"),
                kind: PartKind::Text,
                content: content.to_string(),
                attachment: None,
                tool_call_id: None,
                tool_name: None,
                tool_replay: None,
                prune_state: PruneState::Intact,
                reasoning_meta: None,
                response_meta: None,
            }]),
            origin: None,
        }
    }

    fn protocol_event() -> ProtocolEvent {
        ProtocolEvent::typed("test_protocol", serde_json::json!({"step": "started"}))
            .expect("protocol event serializes")
    }

    #[test]
    fn draft_node_ids_are_opaque_distinct_and_ignore_message_ids() {
        let mut graph = SessionGraph::default();

        let message_id = graph.append_message(text_message("m1", MessageRole::User, "hello"));
        let protocol_id = graph.append_protocol_event(protocol_event());
        let plugin_id = graph.append_plugin("example", serde_json::json!({"ok": true}));

        assert_ne!(message_id, "m1");
        assert!(message_id.starts_with("draft-node/v1/"));
        assert!(protocol_id.starts_with("draft-node/v1/"));
        assert!(plugin_id.starts_with("draft-node/v1/"));
        assert_ne!(message_id, protocol_id);
        assert_ne!(protocol_id, plugin_id);
    }

    #[test]
    fn read_model_preserves_distinct_nodes_with_identical_messages() {
        let mut graph = SessionGraph::default();
        let message = text_message("same-message-id", MessageRole::User, "same content");

        let first = graph.append_message(message.clone());
        let second = graph.append_message(message);

        assert_ne!(first, second);
        let read = graph.read_model();
        assert_eq!(read.messages.len(), 2);
        assert_eq!(read.messages[0].id, "same-message-id");
        assert_eq!(read.messages[1].id, "same-message-id");
    }

    #[test]
    fn message_tree_marks_active_nodes_without_using_message_identity() {
        let mut graph = SessionGraph::default();
        let message = text_message("same-message-id", MessageRole::User, "same content");
        let root = graph.append_message(message.clone());
        let inactive = graph.append_message(message.clone());
        graph.set_leaf_node_id(Some(root));
        let active = graph.append_message(message);

        let tree = graph.message_tree();
        assert_eq!(tree.len(), 1);
        assert!(tree[0].active);
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].node_id, inactive);
        assert!(!tree[0].children[0].active);
        assert_eq!(tree[0].children[1].node_id, active);
        assert!(tree[0].children[1].active);
    }

    #[test]
    fn active_read_replacement_persists_messages_only() {
        let message = text_message("m1", MessageRole::User, "hello");
        let graph = SessionGraph::from_active_read_state(&[message]);

        assert_eq!(graph.nodes.len(), 1);
        assert!(matches!(
            graph.nodes[0].event(),
            Some(SessionHistoryRecord::Conversation(_))
        ));
    }

    #[test]
    fn graph_writers_keep_payload_kind_out_of_draft_identity() {
        let mut graph = SessionGraph::default();
        graph.append_message(text_message("m1", MessageRole::User, "hello"));
        graph.append_protocol_event(protocol_event());
        graph.append_plugin("example", serde_json::json!({"ok": true}));

        for node in &graph.nodes {
            assert!(node.node_id.starts_with("draft-node/v1/"), "{:?}", node);
        }
    }
}
