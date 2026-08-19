use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::*;
use crate::SessionAppendNode;
use crate::facade_support::SessionGraphFacadeOps;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHandle {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub policy: SessionPolicy,
    /// Per-id outcome for observer edges requested at session creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_processes: Vec<SessionObservedProcessResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObservedProcessResult {
    pub process_id: crate::ProcessId,
    pub outcome: SessionObservedProcessOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionObservedProcessOutcome {
    Observed,
    NotFound,
    NoLongerRetained {
        terminal_label: String,
        pruned_at_ms: u64,
    },
    Unavailable {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: String,
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
    /// Read-only projection of the hydrated tool-state reference. Applying a
    /// snapshot does not write this field; the resident component set wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_generation: Option<u64>,
    /// Read-only projection of the hydrated plugin-snapshot reference. Applying
    /// a snapshot does not write this field; the resident component set wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_revision: Option<u64>,
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
            session_id: String::new(),
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
            plugin_snapshot_ref: None,
            plugin_snapshot_revision: None,
            execution_state_ref: None,
            token_ledger: Vec::new(),
            checkpoint_ref: None,
        }
    }
}

impl SessionSnapshot {
    pub(crate) fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.current_frame_node_id.as_deref().map_or_else(
            || self.session_graph.read_model(),
            |frame_node_id| self.session_graph.read_model_for_frame(frame_node_id),
        )
    }

    /// Exposes read view to store and durable-substrate implementors while snapshotting or
    /// restoring durable session state.
    pub fn read_view(&self) -> crate::SessionReadView {
        crate::SessionReadView::from_snapshot(self)
    }

    /// Replaces the active frame's readable message tail for store implementors restoring a
    /// snapshot; transient messages are not inserted into the graph.
    pub fn replace_active_read_state(&mut self, messages: &[crate::Message]) {
        if let Some(frame_node_id) = self.current_frame_node_id.as_deref() {
            self.session_graph
                .rewrite_active_read_tail_for_frame(frame_node_id, messages);
        } else {
            self.session_graph.rewrite_active_read_tail(messages);
        }
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(str::to_string);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    /// Appends non-transient messages after the active leaf for store implementors applying a
    /// snapshot delta in source order.
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
    pub(crate) const INITIAL: &'static str = "initial";
    pub(crate) const CONTINUE_AS: &'static str = "continue_as";
    pub(crate) const COMPACTION: &'static str = "compaction";
    pub(crate) fn new(label: impl Into<String>) -> Self {
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

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`AgentFrameReason`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    pub trait AgentFrameReasonFacadeOps {
        fn continue_as() -> Self;
    }

    impl AgentFrameReasonFacadeOps for AgentFrameReason {
        fn continue_as() -> Self {
            Self::new(Self::CONTINUE_AS)
        }
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_at(
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRelation {
    #[default]
    Root,
    Child {
        parent_session_id: String,
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
        source_session_id: String,
        /// Host-declared source node, persisted alongside
        /// [`Self::Fork::source_session_id`] and equally unvalidated.
        source_node_id: String,
        #[serde(default)]
        observer_inheritance: crate::ObserverInheritance,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending_observer_process_ids: Vec<crate::ProcessId>,
    },
    /// Durable session-create intent for observer edges that are published
    /// after the session's initial state commit.
    ObserverIntent {
        relation: Box<SessionRelation>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending_observer_process_ids: Vec<crate::ProcessId>,
    },
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
            Self::ObserverIntent { relation, .. } => relation.parent_session_id(),
        }
    }

    pub(crate) fn with_observer_intent(
        self,
        pending_observer_process_ids: Vec<crate::ProcessId>,
    ) -> Self {
        if pending_observer_process_ids.is_empty() {
            self
        } else {
            match self {
                Self::ObserverIntent {
                    relation,
                    pending_observer_process_ids: mut pending,
                } => {
                    for process_id in pending_observer_process_ids {
                        if !pending.contains(&process_id) {
                            pending.push(process_id);
                        }
                    }
                    Self::ObserverIntent {
                        relation,
                        pending_observer_process_ids: pending,
                    }
                }
                relation => Self::ObserverIntent {
                    relation: Box::new(relation),
                    pending_observer_process_ids,
                },
            }
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
    /// Host-selected process observer edges to apply after session creation.
    ///
    /// Edge application is idempotent, durably recoverable when the session
    /// has a store, and reported per process on the returned [`SessionHandle`].
    /// Unknown or pruned ids do not fail session creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_processes: Vec<crate::ProcessId>,
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
    /// Builds a root-session request with a fresh UUID for protocol implementors materializing a
    /// new independent session.
    pub fn root(start: SessionStartPoint, plugin_options: PluginOptions) -> Self {
        Self {
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            relation: SessionRelation::Root,
            start,
            policy: None,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        }
    }

    /// Builds a child-session request with a fresh UUID and inherited policy selection for protocol
    /// implementors materializing nested work.
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
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        }
    }

    /// Builds a child-session request with an explicit policy and usage-ledger source for protocol
    /// and process-engine implementors materializing nested work.
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
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: Some(usage_source.into()),
        }
    }

    /// Sets the plugin source carried by a `SessionCreateRequest` for protocol and process-engine
    /// implementors while preparing or executing plugin and tool work.
    pub fn with_plugin_source(mut self, plugin_source: SessionPluginSource) -> Self {
        self.plugin_source = plugin_source;
        self
    }

    /// Sets the session id carried by a `SessionCreateRequest` for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the initial nodes carried by a `SessionCreateRequest` for store, effect-host, and
    /// protocol implementors while materializing, executing, or persisting a session turn.
    pub fn with_initial_nodes(mut self, initial_nodes: Vec<SessionAppendNode>) -> Self {
        self.initial_nodes = initial_nodes;
        self
    }

    /// Sets the observed processes carried by a `SessionCreateRequest` for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_observed_processes(
        mut self,
        process_ids: impl IntoIterator<Item = impl Into<crate::ProcessId>>,
    ) -> Self {
        self.observed_processes = process_ids.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the tool access carried by a `SessionCreateRequest` for protocol and process-engine
    /// implementors while preparing or executing plugin and tool work.
    pub fn with_tool_access(mut self, tool_access: SessionToolAccess) -> Self {
        self.tool_access = tool_access;
        self
    }

    /// Sets the subagent context carried by a `SessionCreateRequest` for store, effect-host, and
    /// protocol implementors while materializing, executing, or persisting a session turn.
    pub fn with_subagent_context(mut self, subagent: SubagentSessionContext) -> Self {
        self.subagent = Some(subagent);
        self
    }

    /// Records causal provenance for protocol and process-engine implementors when the request is a
    /// child; root requests remain unchanged.
    pub fn with_caused_by(mut self, caused_by: crate::CausalRef) -> Self {
        if let SessionRelation::Child {
            caused_by: cause, ..
        } = &mut self.relation
        {
            *cause = Some(caused_by);
        }
        self
    }

    /// Sets the context overlay carried by a `SessionCreateRequest` for store, effect-host, and
    /// protocol implementors while materializing, executing, or persisting a session turn.
    pub fn with_context_overlay(mut self, context_overlay: SessionContextOverlay) -> Self {
        self.context_overlay = context_overlay;
        self
    }

    /// Labels child-session token cost for protocol and administration embedders so committed usage
    /// is attributed to the correct parent-ledger source.
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
    /// Lets protocol implementors apply the session's persisted tool-hiding policy by exact name.
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
