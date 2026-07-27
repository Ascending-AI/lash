use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::*;
use crate::SessionAppendNode;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHandle {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub policy: SessionPolicy,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    #[serde(default)]
    pub policy: SessionPolicy,
    /// Derived convenience view of `session_graph` FrameOpen nodes.
    #[serde(skip)]
    pub agent_frames: Vec<AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<crate::store::BlobRef>,
}

impl SessionSnapshot {
    pub(crate) fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.current_frame_node_id.as_deref().map_or_else(
            || self.session_graph.read_model(),
            |frame_node_id| self.session_graph.read_model_for_frame(frame_node_id),
        )
    }

    pub fn read_view(&self) -> crate::SessionReadView {
        crate::SessionReadView::from_snapshot(self)
    }

    pub fn replace_active_read_state(&mut self, messages: &[crate::Message]) {
        if let Some(frame_node_id) = self.current_frame_node_id.as_deref() {
            self.session_graph
                .replace_active_read_state_for_frame(frame_node_id, messages);
        } else {
            self.session_graph.replace_active_read_state(messages);
        }
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(str::to_string);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    pub fn append_active_read_delta(&mut self, messages: &[crate::Message]) {
        self.session_graph.append_active_read_delta(messages);
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(str::to_string);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionStartPoint {
    Empty,
    CurrentSession,
    ExistingSession { session_id: String },
    Snapshot { snapshot: Box<SessionSnapshot> },
}

#[derive(Clone, Debug)]
pub struct PluginOwned<T> {
    pub plugin_id: String,
    pub value: T,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPluginSource {
    CurrentHostFresh,
    #[default]
    CurrentSessionFork,
}

pub type AgentFrameId = String;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentFrameReason(String);

impl AgentFrameReason {
    pub const INITIAL: &'static str = "initial";
    pub const CONTINUE_AS: &'static str = "continue_as";
    pub const COMPACTION: &'static str = "compaction";

    pub fn new(label: impl Into<String>) -> Self {
        Self(label.into())
    }

    pub fn initial() -> Self {
        Self::new(Self::INITIAL)
    }

    pub fn continue_as() -> Self {
        Self::new(Self::CONTINUE_AS)
    }

    pub fn compaction() -> Self {
        Self::new(Self::COMPACTION)
    }

    pub fn as_str(&self) -> &str {
        &self.0
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

#[cfg(test)]
mod agent_frame_reason_tests {
    use super::AgentFrameReason;

    #[test]
    fn agent_frame_reason_round_trips_arbitrary_labels() {
        let reason: AgentFrameReason =
            serde_json::from_str("\"plan_mode\"").expect("deserialize reason");

        assert_eq!(reason.as_str(), "plan_mode");
        assert_eq!(
            serde_json::to_string(&reason).expect("serialize reason"),
            "\"plan_mode\""
        );
        assert_eq!(AgentFrameReason::compaction().as_str(), "compaction");
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentFrameAssignment {
    pub policy: SessionPolicy,
    #[serde(default)]
    pub plugin_options: PluginOptions,
    #[serde(default)]
    pub plugin_source: SessionPluginSource,
    #[serde(default)]
    pub tool_access: SessionToolAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentSessionContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_source: Option<String>,
}

impl AgentFrameAssignment {
    pub fn from_session_request(request: &SessionCreateRequest, policy: SessionPolicy) -> Self {
        Self {
            policy,
            plugin_options: request.plugin_options.clone(),
            plugin_source: request.plugin_source,
            tool_access: request.tool_access.clone(),
            subagent: request.subagent.clone(),
            usage_source: request.usage_source.clone(),
        }
    }

    pub fn from_policy(policy: SessionPolicy) -> Self {
        Self {
            policy,
            plugin_options: PluginOptions::default(),
            plugin_source: SessionPluginSource::CurrentHostFresh,
            tool_access: SessionToolAccess::default(),
            subagent: None,
            usage_source: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentFrameRecord {
    pub frame_node_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_frame_node_id: Option<String>,
    #[serde(default)]
    pub reason: AgentFrameReason,
    pub created_at: String,
    pub assignment: AgentFrameAssignment,
    #[serde(default)]
    pub protocol_turn_options: ProtocolTurnOptions,
}

impl AgentFrameRecord {
    pub fn new(
        frame_node_id: impl Into<String>,
        session_id: impl Into<String>,
        previous_frame_node_id: Option<String>,
        reason: AgentFrameReason,
        assignment: AgentFrameAssignment,
        protocol_turn_options: ProtocolTurnOptions,
    ) -> Self {
        Self::new_at(
            frame_node_id,
            session_id,
            previous_frame_node_id,
            reason,
            assignment,
            protocol_turn_options,
            <crate::SystemClock as crate::Clock>::timestamp_rfc3339(&crate::SystemClock),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_at(
        frame_node_id: impl Into<String>,
        session_id: impl Into<String>,
        previous_frame_node_id: Option<String>,
        reason: AgentFrameReason,
        assignment: AgentFrameAssignment,
        protocol_turn_options: ProtocolTurnOptions,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            frame_node_id: frame_node_id.into(),
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
    pub frame_id: AgentFrameId,
    pub reason: AgentFrameReason,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_nodes: Vec<SessionAppendNode>,
}

impl OpenAgentFrameRequest {
    pub fn new(frame_id: impl Into<AgentFrameId>, reason: AgentFrameReason) -> Self {
        Self {
            frame_id: frame_id.into(),
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
    pub initial_node_ids: Vec<String>,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRelation {
    #[default]
    Root,
    Child {
        parent_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caused_by: Option<crate::CausalRef>,
    },
    Fork {
        source_session_id: String,
        source_node_id: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        process_grants: Vec<crate::ProcessHandleGrant>,
    },
}

impl SessionRelation {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub relation: SessionRelation,
    pub start: SessionStartPoint,
    #[serde(default)]
    pub policy: Option<SessionPolicy>,
    #[serde(default)]
    pub plugin_source: SessionPluginSource,
    #[serde(default)]
    pub initial_nodes: Vec<SessionAppendNode>,
    #[serde(default)]
    pub tool_access: SessionToolAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentSessionContext>,
    #[serde(skip)]
    pub context_overlay: SessionContextOverlay,
    /// Plugin-owned options that configure plugin behavior at session
    /// creation time. Each plugin decodes only the entry keyed by its id.
    #[serde(default)]
    pub plugin_options: PluginOptions,
    /// Label for the token-cost ledger. When this session's turns
    /// complete, their token usage is accumulated under this label on
    /// the parent session's `token_ledger`. Examples: `"subagent"`,
    /// `"compaction"`. Defaults to `"child"` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_source: Option<String>,
}

impl SessionCreateRequest {
    pub fn root(start: SessionStartPoint, plugin_options: PluginOptions) -> Self {
        Self {
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            relation: SessionRelation::Root,
            start,
            policy: None,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        }
    }

    pub fn root_with_policy(
        start: SessionStartPoint,
        policy: SessionPolicy,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            policy: Some(policy),
            ..Self::root(start, plugin_options)
        }
    }

    pub fn child_session(
        parent_session_id: impl Into<String>,
        start: SessionStartPoint,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            relation: SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            policy: None,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        }
    }

    pub fn child_session_with_policy(
        parent_session_id: impl Into<String>,
        start: SessionStartPoint,
        policy: SessionPolicy,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            policy: Some(policy),
            ..Self::child_session(parent_session_id, start, plugin_options)
        }
    }

    pub fn child(
        parent_session_id: impl Into<String>,
        start: SessionStartPoint,
        policy: SessionPolicy,
        plugin_options: PluginOptions,
        usage_source: impl Into<String>,
    ) -> Self {
        Self::related(
            SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            Some(policy),
            plugin_options,
            usage_source,
        )
    }

    pub fn child_inheriting_policy(
        parent_session_id: impl Into<String>,
        start: SessionStartPoint,
        plugin_options: PluginOptions,
        usage_source: impl Into<String>,
    ) -> Self {
        Self::related(
            SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            None,
            plugin_options,
            usage_source,
        )
    }

    fn related(
        relation: SessionRelation,
        start: SessionStartPoint,
        policy: Option<SessionPolicy>,
        plugin_options: PluginOptions,
        usage_source: impl Into<String>,
    ) -> Self {
        Self {
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            relation,
            start,
            policy,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: Some(usage_source.into()),
        }
    }

    pub fn with_plugin_source(mut self, plugin_source: SessionPluginSource) -> Self {
        self.plugin_source = plugin_source;
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_initial_nodes(mut self, initial_nodes: Vec<SessionAppendNode>) -> Self {
        self.initial_nodes = initial_nodes;
        self
    }

    pub fn with_tool_access(mut self, tool_access: SessionToolAccess) -> Self {
        self.tool_access = tool_access;
        self
    }

    pub fn with_subagent_context(mut self, subagent: SubagentSessionContext) -> Self {
        self.subagent = Some(subagent);
        self
    }

    pub fn with_caused_by(mut self, caused_by: crate::CausalRef) -> Self {
        if let SessionRelation::Child {
            caused_by: cause, ..
        } = &mut self.relation
        {
            *cause = Some(caused_by);
        }
        self
    }

    pub fn with_context_overlay(mut self, context_overlay: SessionContextOverlay) -> Self {
        self.context_overlay = context_overlay;
        self
    }

    pub fn with_usage_source(mut self, usage_source: impl Into<String>) -> Self {
        self.usage_source = Some(usage_source.into());
        self
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionToolAccess {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub hidden_tools: BTreeSet<String>,
}

impl SessionToolAccess {
    pub fn hides(&self, name: &str) -> bool {
        self.hidden_tools.contains(name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentSessionContext {
    pub parent_session_id: String,
    pub capability: String,
    pub depth: u8,
    pub max_depth: u8,
}
