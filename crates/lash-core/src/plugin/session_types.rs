use crate::SessionId;
use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::*;
use crate::SessionAppendNode;
use crate::facade_support::SessionGraphFacadeOps;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHandle {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    pub policy: SessionPolicy,
    /// Per-id outcome for observer edges requested at session creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_processes: Vec<SessionObservedProcessReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionObservedProcessReceipt {
    pub process_id: crate::ProcessId,
    pub attribution: SessionObserverIntentAttribution,
    pub outcome: SessionObservedProcessOutcome,
}

/// Why a session still owes one process-observer edge.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionObservedProcessOutcome {
    Observed {
        incarnation: crate::ProcessIncarnation,
    },
    IncarnationSuperseded {
        requested_incarnation: crate::ProcessIncarnation,
        current_incarnation: crate::ProcessIncarnation,
    },
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
    pub(crate) fn read_model(
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
                FrameNodeId::new(frame_node_id)
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
                FrameNodeId::new(frame_node_id)
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

#[cfg(test)]
mod frame_node_id_tests {
    use super::{FrameNodeId, FrameNodeIdError};

    #[test]
    fn frame_node_id_round_trips_non_empty_identity() {
        let encoded = r#""frame-node/v3/derived""#;
        let frame_node_id: FrameNodeId =
            serde_json::from_str(encoded).expect("deserialize frame node id");

        assert_eq!(
            serde_json::to_string(&frame_node_id).expect("serialize frame node id"),
            encoded
        );
    }

    #[test]
    fn frame_node_id_rejects_empty_api_and_serialized_values() {
        assert_eq!(FrameNodeId::new(""), Err(FrameNodeIdError::Empty));
        assert!(
            serde_json::from_str::<FrameNodeId>(r#""""#)
                .expect_err("empty serialized frame node id must be rejected")
                .to_string()
                .contains("frame node id must not be empty")
        );
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
        source_node_id: String,
        #[serde(default)]
        observer_inheritance: crate::ObserverInheritance,
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
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
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
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
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
        parent_session_id: impl Into<SessionId>,
        start: SessionStartPoint,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
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
        parent_session_id: impl Into<SessionId>,
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
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
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
    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
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

#[cfg(test)]
mod session_tool_access_tests {
    use super::{SessionToolAccess, SessionToolAccessError};

    fn tool(id: &str, name: &str) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            id,
            name,
            format!("{name} description"),
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
    }

    #[test]
    fn ambient_and_restricted_empty_have_distinct_canonical_encodings() {
        assert_eq!(
            serde_json::to_value(SessionToolAccess::ambient()).expect("serialize ambient"),
            serde_json::json!({ "mode": "ambient" })
        );
        let restricted = SessionToolAccess::restricted([]).expect("empty restriction is valid");
        assert_eq!(
            serde_json::to_value(&restricted).expect("serialize restricted empty"),
            serde_json::json!({ "mode": "restricted", "tools": [] })
        );
        assert_eq!(
            restricted
                .restricted_tools()
                .expect("explicit restricted mode")
                .len(),
            0
        );
    }

    #[test]
    fn checked_construction_rejects_invalid_restricted_definitions() {
        let empty_name = SessionToolAccess::restricted([tool("tool:empty", " ")])
            .expect_err("blank name must refuse");
        assert_eq!(
            empty_name,
            SessionToolAccessError::EmptyToolName { index: 0 }
        );

        let duplicate_name = SessionToolAccess::restricted([
            tool("tool:first", "duplicate"),
            tool("tool:second", "duplicate"),
        ])
        .expect_err("duplicate name must refuse");
        assert_eq!(
            duplicate_name,
            SessionToolAccessError::DuplicateToolName {
                name: "duplicate".to_string()
            }
        );

        let duplicate_id = SessionToolAccess::restricted([
            tool("tool:duplicate", "first"),
            tool("tool:duplicate", "second"),
        ])
        .expect_err("duplicate id must refuse");
        assert!(matches!(
            duplicate_id,
            SessionToolAccessError::DuplicateToolId { tool_id }
                if tool_id.as_str() == "tool:duplicate"
        ));

        assert_eq!(
            SessionToolAccess::ambient()
                .with_hidden_tools([" "])
                .expect_err("blank hidden name must refuse"),
            SessionToolAccessError::EmptyHiddenToolName { index: 0 }
        );
        assert_eq!(
            SessionToolAccess::ambient()
                .with_hidden_tools(["duplicate", "duplicate"])
                .expect_err("duplicate hidden name must refuse"),
            SessionToolAccessError::DuplicateHiddenToolName {
                name: "duplicate".to_string()
            }
        );
    }

    #[test]
    fn decoded_authority_rejects_missing_unknown_and_malformed_modes() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "mode": "unknown" }),
            serde_json::json!({ "mode": "restricted" }),
            serde_json::json!({ "mode": "ambient", "tools": [] }),
            serde_json::json!({ "mode": "restricted", "tools": "all" }),
        ] {
            assert!(
                serde_json::from_value::<SessionToolAccess>(value.clone()).is_err(),
                "malformed access unexpectedly decoded: {value}"
            );
        }
    }

    #[test]
    fn decoded_authority_uses_the_same_name_and_id_checks() {
        let duplicate_names = serde_json::json!({
            "mode": "restricted",
            "tools": [tool("tool:first", "same"), tool("tool:second", "same")]
        });
        let error = serde_json::from_value::<SessionToolAccess>(duplicate_names)
            .expect_err("duplicate names must refuse on decode");
        assert!(
            error
                .to_string()
                .contains("name `same` appears more than once")
        );

        let duplicate_ids = serde_json::json!({
            "mode": "restricted",
            "tools": [tool("tool:same", "first"), tool("tool:same", "second")]
        });
        let error = serde_json::from_value::<SessionToolAccess>(duplicate_ids)
            .expect_err("duplicate ids must refuse on decode");
        assert!(
            error
                .to_string()
                .contains("id `tool:same` appears more than once")
        );

        let duplicate_hidden = serde_json::json!({
            "mode": "ambient",
            "hidden_tools": ["hidden", "hidden"]
        });
        let error = serde_json::from_value::<SessionToolAccess>(duplicate_hidden)
            .expect_err("duplicate hidden names must refuse on decode");
        assert!(
            error
                .to_string()
                .contains("hidden tool name `hidden` appears more than once")
        );
    }

    #[test]
    fn restricted_roundtrip_preserves_complete_definition_and_opaque_null_binding() {
        let mut definition = tool("tool:restricted", "restricted");
        definition
            .manifest
            .bindings
            .insert("opaque".to_string(), serde_json::Value::Null);
        let access = SessionToolAccess::restricted([definition.clone()])
            .expect("valid restricted definition")
            .with_hidden_tools(["restricted"])
            .expect("valid hidden name");

        let encoded = serde_json::to_value(&access).expect("serialize restricted access");
        let decoded: SessionToolAccess =
            serde_json::from_value(encoded).expect("decode restricted access");
        let [decoded_definition] = decoded
            .restricted_tools()
            .expect("restricted definition survives")
        else {
            panic!("expected exactly one restricted definition")
        };
        assert_eq!(decoded_definition.manifest, definition.manifest);
        assert_eq!(
            decoded_definition.contract().input_schema,
            definition.contract().input_schema
        );
        assert_eq!(
            decoded_definition.contract().output_schema,
            definition.contract().output_schema
        );
        assert_eq!(
            decoded_definition.manifest.bindings.get("opaque"),
            Some(&serde_json::Value::Null)
        );
        assert!(decoded.hides("restricted"));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSessionContext {
    pub parent_session_id: SessionId,
    pub capability: String,
    pub depth: u8,
    pub max_depth: u8,
}

#[cfg(test)]
mod observer_intent_relation_cutover_tests {
    use super::SessionRelation;

    #[test]
    fn create_relation_rejects_empty_observer_intent_wrapper() {
        let error = serde_json::from_value::<SessionRelation>(serde_json::json!({
            "kind": "observer_intent",
            "relation": { "kind": "root" }
        }))
        .expect_err("an empty wrapper over root is no longer a relation");
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(
            error
                .to_string()
                .contains("unknown variant `observer_intent`")
        );
    }

    #[test]
    fn create_relation_rejects_nested_observer_intent_wrappers() {
        let error = serde_json::from_value::<SessionRelation>(serde_json::json!({
            "kind": "observer_intent",
            "relation": {
                "kind": "observer_intent",
                "relation": { "kind": "root" },
                "pending_observer_process_ids": ["inner-process"]
            },
            "pending_observer_process_ids": ["outer-process"]
        }))
        .expect_err("nested observer-intent wrappers are no longer relations");
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(
            error
                .to_string()
                .contains("unknown variant `observer_intent`")
        );
    }
}
