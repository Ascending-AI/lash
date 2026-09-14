//! Session identity and agent-frame vocabulary.
//!
//! The durable identifiers and records the session graph and the store are
//! written in terms of. The plugin-facing session handle, its create request
//! and the lifecycle services stay in `lash-core`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

#[serde(rename_all = "snake_case")]
pub enum SessionObserverIntentAttribution {
    HostRequested,
    ForkInherited,
}
/// One durable, not-yet-settled process-observer edge.
///
/// `process_incarnation` is reserved for the structural process incarnation
/// identity. Until that identity lands, `None` means the host-facing process
/// name must be resolved by the settlement boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionObserverIntent {
    pub process_id: crate::ProcessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_incarnation: Option<u64>,
    pub attribution: SessionObserverIntentAttribution,
}
impl SessionObserverIntent {
    pub fn host_requested(process_id: impl Into<crate::ProcessId>) -> Self {
        Self {
            process_id: process_id.into(),
            process_incarnation: None,
            attribution: SessionObserverIntentAttribution::HostRequested,
        }
    }

    pub fn fork_inherited(process_id: impl Into<crate::ProcessId>) -> Self {
        Self {
            process_id: process_id.into(),
            process_incarnation: None,
            attribution: SessionObserverIntentAttribution::ForkInherited,
        }
    }
}
/// Durable identity of a frame-open node in the session graph.
///
/// This is distinct from [`crate::FrameKey`], the checked key used to derive
/// this value. An absent [`FrameNodeId`] is represented by `Option::None`; the
/// empty string is never a frame identity.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct FrameNodeId(String);
/// Rejection produced when constructing a [`FrameNodeId`] from invalid text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrameNodeIdError {
    /// The empty string is the removed legacy sentinel for an unscoped read.
    #[error("frame node id must not be empty; use explicit absence for an unscoped operation")]
    Empty,
}
impl FrameNodeId {
    /// Validates and wraps a durable frame-node identity.
    pub fn new(value: impl Into<String>) -> Result<Self, FrameNodeIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(FrameNodeIdError::Empty);
        }
        Ok(Self(value))
    }

    /// Borrows the durable frame-node identity as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned durable frame-node identity.
    pub fn into_inner(self) -> String {
        self.0
    }
}
impl<'de> Deserialize<'de> for FrameNodeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}
impl std::ops::Deref for FrameNodeId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}
impl AsRef<str> for FrameNodeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl std::borrow::Borrow<str> for FrameNodeId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Display for FrameNodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
impl From<FrameNodeId> for String {
    fn from(value: FrameNodeId) -> Self {
        value.into_inner()
    }
}
impl From<&FrameNodeId> for String {
    fn from(value: &FrameNodeId) -> Self {
        value.to_string()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentFrameReason(String);
impl AgentFrameReason {
    pub(crate) const INITIAL: &'static str = "initial";
    pub(crate) const CONTINUE_AS: &'static str = "continue_as";
    pub(crate) const COMPACTION: &'static str = "compaction";
    pub fn new(label: impl Into<String>) -> Self {
        Self(label.into())
    }

    /// Constructs the canonical initial-frame reason for store and protocol implementors restoring
    /// a session that has not yet opened another frame.
    pub fn initial() -> Self {
        Self::new(Self::INITIAL)
    }

    pub(crate) fn compaction() -> Self {
        Self::new(Self::COMPACTION)
    }

    /// Exposes the stable reason label to store and protocol implementors for persistence and
    /// diagnostics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
    impl AgentFrameReasonFacadeOps for AgentFrameReason {
        fn continue_as() -> Self {
            Self::new(Self::CONTINUE_AS)
        }
    }
impl Default for AgentFrameReason {
    fn default() -> Self {
        Self::initial()
    }
}
impl From<&str> for AgentFrameReason {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}
impl From<String> for AgentFrameReason {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}
impl std::fmt::Display for AgentFrameReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentFrameAssignment {
    pub policy: SessionPolicy,
    #[serde(default)]
    pub plugin_options: PluginOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_source: Option<String>,
}
impl AgentFrameAssignment {
    pub(crate) fn from_session_request(
        request: &SessionCreateRequest,
        policy: SessionPolicy,
    ) -> Self {
        Self {
            policy,
            plugin_options: request.plugin_options.clone(),
            usage_source: request.usage_source.clone(),
        }
    }

    /// Builds a `AgentFrameAssignment` from policy data for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn from_policy(policy: SessionPolicy) -> Self {
        Self {
            policy,
            plugin_options: PluginOptions::default(),
            usage_source: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentFrameRecord {
    pub frame_node_id: FrameNodeId,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_frame_node_id: Option<FrameNodeId>,
    #[serde(default)]
    pub reason: AgentFrameReason,
    pub created_at: String,
    pub assignment: AgentFrameAssignment,
    #[serde(default)]
    pub protocol_turn_options: ProtocolTurnOptions,
}
impl AgentFrameRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_at(
        frame_node_id: FrameNodeId,
        session_id: impl Into<SessionId>,
        previous_frame_node_id: Option<FrameNodeId>,
        reason: AgentFrameReason,
        assignment: AgentFrameAssignment,
        protocol_turn_options: ProtocolTurnOptions,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            frame_node_id,
            session_id: session_id.into(),
            previous_frame_node_id,
            reason,
            created_at: created_at.into(),
            assignment,
            protocol_turn_options,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenAgentFrameRequest {
    pub frame_key: crate::FrameKey,
    pub reason: AgentFrameReason,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_nodes: Vec<SessionAppendNode>,
}
impl OpenAgentFrameRequest {
    pub fn new(frame_key: crate::FrameKey, reason: AgentFrameReason) -> Self {
        Self {
            frame_key,
            reason,
            initial_nodes: Vec::new(),
        }
    }

    pub fn with_initial_nodes(mut self, initial_nodes: Vec<SessionAppendNode>) -> Self {
        self.initial_nodes = initial_nodes;
        self
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenAgentFrameResult {
    pub frame_node_id: String,
    pub opened: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_node_ids: Vec<crate::NodeId>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRelation {
    #[default]
    Root,
    Child {
        parent_session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caused_by: Option<crate::CausalRef>,
    },
    Fork {
        /// Host-declared lineage: the session this fork branched from as the
        /// host understands it. Stores persist it as durable fork lineage
        /// and never validate it against the fork point's anchor provenance —
        /// repeated rewinds legitimately name superseded intermediate
        /// sessions, while [`crate::ForkSessionReceipt::source_session_id`]
        /// always reports the original writer.
        source_session_id: SessionId,
        /// Host-declared source node, persisted alongside
        /// [`Self::Fork::source_session_id`] and equally unvalidated.
        source_node_id: crate::NodeId,
        #[serde(default)]
        observer_inheritance: crate::ObserverInheritance,
    },
}
/// Durable lineage identity of a [`SessionRelation`].
///
/// This is the part of a relation that session admission compares on a rebind:
/// the relation kind plus the session ids it names. Causal provenance
/// (`caused_by`) and observer inheritance are deliberately excluded — they
/// record *why* and *how* a session was created, not what it descends from,
/// and a legitimate reopen of an existing child carries neither.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionLineage {
    #[default]
    Root,
    Child {
        parent_session_id: SessionId,
    },
    Fork {
        source_session_id: SessionId,
        source_node_id: String,
    },
}
impl SessionLineage {
    /// The lineage a relation declares.
    pub fn of(relation: &SessionRelation) -> Self {
        match relation {
            SessionRelation::Root => Self::Root,
            SessionRelation::Child {
                parent_session_id, ..
            } => Self::Child {
                parent_session_id: parent_session_id.clone(),
            },
            SessionRelation::Fork {
                source_session_id,
                source_node_id,
                ..
            } => Self::Fork {
                source_session_id: source_session_id.clone(),
                source_node_id: source_node_id.to_string(),
            },
        }
    }

    /// Human-readable description used in admission refusals.
    pub fn label(&self) -> String {
        match self {
            Self::Root => "a root session".to_string(),
            Self::Child { parent_session_id } => {
                format!("a child of session `{parent_session_id}`")
            }
            Self::Fork {
                source_session_id,
                source_node_id,
            } => format!("a fork of session `{source_session_id}` at node `{source_node_id}`"),
        }
    }

    /// Whether a rebind declaring `requested` conflicts with `self` as recorded.
    ///
    /// [`SessionLineage::Root`] is the default a binding carries when the caller
    /// declares no lineage — every resume, park and plain reopen path admits
    /// with it — so it is read as "no claim" and never conflicts. Any other
    /// declared lineage must equal the recorded one: a rebind that renames a
    /// parent, or claims a parent for a session recorded as a root, is refused
    /// rather than silently absorbed.
    pub fn rebind_conflicts_with_recorded(&self, requested: &Self) -> bool {
        match requested {
            Self::Root => false,
            declared => declared != self,
        }
    }
}
impl SessionRelation {
    /// Exposes the parent session ID to store implementors for child and fork relations, returning
    /// `None` for a root session.
    pub fn parent_session_id(&self) -> Option<&str> {
        match self {
            Self::Root => None,
            Self::Child {
                parent_session_id, ..
            } => Some(parent_session_id),
            Self::Fork { .. } => None,
        }
    }
}
/// Explicit resident-tool authority for one session.
///
/// Ambient access uses the host registry's captured resident definitions.
/// Restricted access uses only the complete definitions carried here; an empty
/// restricted set therefore means no resident tools. Exact-name hiding is a
/// separate projection policy in both modes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionToolAccess {
    resident: SessionResidentToolAccess,
    hidden_tools: BTreeSet<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionResidentToolAccess {
    Ambient,
    Restricted(Vec<ToolDefinition>),
}
/// Refusal from checked [`SessionToolAccess`] construction or decoding.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionToolAccessError {
    #[error("restricted tool definition at index {index} has an empty name")]
    EmptyToolName { index: usize },
    #[error("restricted tool name `{name}` appears more than once")]
    DuplicateToolName { name: String },
    #[error("restricted tool id `{tool_id}` appears more than once")]
    DuplicateToolId { tool_id: crate::ToolId },
    #[error("hidden tool name at index {index} is empty")]
    EmptyHiddenToolName { index: usize },
    #[error("hidden tool name `{name}` appears more than once")]
    DuplicateHiddenToolName { name: String },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum SessionToolAccessWire {
    Ambient {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hidden_tools: Vec<String>,
    },
    Restricted {
        tools: Vec<ToolDefinition>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hidden_tools: Vec<String>,
    },
}
impl Default for SessionToolAccess {
    fn default() -> Self {
        Self::ambient()
    }
}
impl SessionToolAccess {
    /// Selects the host registry's captured resident tool definitions.
    pub fn ambient() -> Self {
        Self {
            resident: SessionResidentToolAccess::Ambient,
            hidden_tools: BTreeSet::new(),
        }
    }

    /// Selects exactly the supplied complete resident definitions.
    pub fn restricted(
        tools: impl IntoIterator<Item = ToolDefinition>,
    ) -> Result<Self, SessionToolAccessError> {
        let tools = tools.into_iter().collect::<Vec<_>>();
        Self::validate_restricted_tools(&tools)?;
        Ok(Self {
            resident: SessionResidentToolAccess::Restricted(tools),
            hidden_tools: BTreeSet::new(),
        })
    }

    /// Applies exact-name hiding independently from resident membership.
    pub fn with_hidden_tools(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, SessionToolAccessError> {
        for (index, name) in names.into_iter().enumerate() {
            self.insert_hidden_tool(name.into(), index)?;
        }
        Ok(self)
    }

    /// Adds one exact hidden name while preserving checked authority invariants.
    pub fn hide_tool(&mut self, name: impl Into<String>) -> Result<(), SessionToolAccessError> {
        self.insert_hidden_tool(name.into(), self.hidden_tools.len())
    }

    /// Returns the explicit restricted definitions, or `None` for ambient access.
    pub fn restricted_tools(&self) -> Option<&[ToolDefinition]> {
        match &self.resident {
            SessionResidentToolAccess::Ambient => None,
            SessionResidentToolAccess::Restricted(tools) => Some(tools),
        }
    }

    /// Returns the exact names hidden after resident membership is selected.
    pub fn hidden_tools(&self) -> &BTreeSet<String> {
        &self.hidden_tools
    }

    /// Lets protocol implementors apply the session's persisted tool-hiding policy by exact name.
    pub fn hides(&self, name: &str) -> bool {
        self.hidden_tools.contains(name)
    }

    fn validate_restricted_tools(tools: &[ToolDefinition]) -> Result<(), SessionToolAccessError> {
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for (index, tool) in tools.iter().enumerate() {
            if tool.manifest.name.trim().is_empty() {
                return Err(SessionToolAccessError::EmptyToolName { index });
            }
            if !names.insert(tool.manifest.name.clone()) {
                return Err(SessionToolAccessError::DuplicateToolName {
                    name: tool.manifest.name.clone(),
                });
            }
            if !ids.insert(tool.manifest.id.clone()) {
                return Err(SessionToolAccessError::DuplicateToolId {
                    tool_id: tool.manifest.id.clone(),
                });
            }
        }
        Ok(())
    }

    fn insert_hidden_tool(
        &mut self,
        name: String,
        index: usize,
    ) -> Result<(), SessionToolAccessError> {
        if name.trim().is_empty() {
            return Err(SessionToolAccessError::EmptyHiddenToolName { index });
        }
        if !self.hidden_tools.insert(name.clone()) {
            return Err(SessionToolAccessError::DuplicateHiddenToolName { name });
        }
        Ok(())
    }
}
impl Serialize for SessionToolAccess {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let hidden_tools = self.hidden_tools.iter().cloned().collect::<Vec<_>>();
        match &self.resident {
            SessionResidentToolAccess::Ambient => SessionToolAccessWire::Ambient { hidden_tools },
            SessionResidentToolAccess::Restricted(tools) => SessionToolAccessWire::Restricted {
                tools: tools.clone(),
                hidden_tools,
            },
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for SessionToolAccess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SessionToolAccessWire::deserialize(deserializer)?;
        let (access, hidden_tools) = match wire {
            SessionToolAccessWire::Ambient { hidden_tools } => (Self::ambient(), hidden_tools),
            SessionToolAccessWire::Restricted {
                tools,
                hidden_tools,
            } => (
                Self::restricted(tools).map_err(serde::de::Error::custom)?,
                hidden_tools,
            ),
        };
        access
            .with_hidden_tools(hidden_tools)
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSessionContext {
    pub parent_session_id: SessionId,
    pub capability: String,
    pub depth: u8,
    pub max_depth: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub policy: SessionPolicy,
    /// Derived convenience view of `session_graph` FrameOpen nodes.
    #[serde(skip)]
    pub agent_frames: Vec<AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<FrameNodeId>,
    #[serde(default)]
    pub session_graph: crate::SessionGraph,
    #[serde(default)]
    pub turn_index: usize,
    #[serde(default)]
    pub token_usage: crate::TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_usage: Option<crate::PromptUsage>,
    #[serde(default)]
    pub protocol_turn_options: ProtocolTurnOptions,
    /// Read-only projection of the hydrated tool-state reference. Applying a
    /// snapshot does not write this field; the resident component set wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_generation: Option<u64>,
    /// Read-only projection of the hydrated plugin-snapshot reference. Applying
    /// a snapshot does not write this field; the resident component set wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_state_ref: Option<crate::store::BlobRef>,
    /// Host-owned mediated generations from the resident plugin-state component.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub plugin_state_generations: std::collections::BTreeMap<String, u64>,
    /// Read-only projection of the hydrated execution-state reference. Applying
    /// a snapshot does not write this field; the resident component set wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<crate::store::BlobRef>,
}
impl SessionSnapshot {
    /// Construct an empty snapshot with an explicitly chosen session policy.
    pub fn new(policy: SessionPolicy) -> Self {
        Self {
            session_id: SessionId::new(String::new()),
            policy,
            agent_frames: Vec::new(),
            current_frame_node_id: None,
            session_graph: crate::SessionGraph::default(),
            turn_index: 0,
            token_usage: crate::TokenUsage::default(),
            last_prompt_usage: None,
            protocol_turn_options: ProtocolTurnOptions::default(),
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
impl SessionSnapshot {
    pub fn read_model(
        &self,
    ) -> Result<crate::session_graph::SessionReadModel, crate::SessionGraphScopeError> {
        self.session_graph
            .read_model(self.current_frame_node_id.as_ref())
    }

    /// Exposes read view to store and durable-substrate implementors while snapshotting or
    /// restoring durable session state.
    pub fn read_view(&self) -> Result<crate::SessionReadView, crate::SessionGraphScopeError> {
        crate::SessionReadView::from_snapshot(self)
    }

    /// Replaces the active frame's readable message tail for store implementors restoring a
    /// snapshot; transient messages are not inserted into the graph.
    pub fn replace_active_read_state(
        &mut self,
        messages: &[crate::Message],
    ) -> Result<(), crate::SessionGraphScopeError> {
        self.session_graph
            .rewrite_active_read_tail(self.current_frame_node_id.as_ref(), messages)?;
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(|frame_node_id| {
                FrameNodeId::new(frame_node_id.as_str())
                    .expect("a graph node identity selected as a frame is non-empty")
            });
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        Ok(())
    }

    /// Appends non-transient messages after the active leaf for store implementors applying a
    /// snapshot delta in source order.
    pub fn append_active_read_delta(&mut self, messages: &[crate::Message]) {
        self.session_graph.append_active_read_delta(messages);
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(|frame_node_id| {
                FrameNodeId::new(frame_node_id.as_str())
                    .expect("a graph node identity selected as a frame is non-empty")
            });
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionStartPoint {
    Empty,
    CurrentSession,
    ExistingSession { session_id: SessionId },
    Snapshot { snapshot: Box<SessionSnapshot> },
}
#[derive(Clone)]
pub struct SessionContextOverlay {
    pub include_base_tools: bool,
    pub tool_providers: Vec<Arc<dyn ToolProvider>>,
    pub prompt_contributions: Vec<PromptContribution>,
}
impl Default for SessionContextOverlay {
    fn default() -> Self {
        Self {
            include_base_tools: true,
            tool_providers: Vec::new(),
            prompt_contributions: Vec::new(),
        }
    }
}
impl std::fmt::Debug for SessionContextOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContextOverlay")
            .field("include_base_tools", &self.include_base_tools)
            .field("tool_provider_count", &self.tool_providers.len())
            .field(
                "prompt_contribution_count",
                &self.prompt_contributions.len(),
            )
            .finish()
    }
}
