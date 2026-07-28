//! Runtime session state and persistence helpers.
//!
//! `RuntimeSessionState` is the runtime-private mutable state shape. Public
//! host/plugin reads use `SessionSnapshot` from the plugin API instead.

use lash_sansio::PromptUsage;

use crate::session_model::{Message, SessionPolicy, TokenUsage, plugin_message_to_message};
use crate::{PersistedTurnState, SessionSnapshot};

use super::usage::TokenLedgerEntry;

/// The runtime's view of a session: the persistable snapshot fields
/// **plus** scratch fields the runtime tracks but never persists
/// (head-revision CAS guard, pending dirty-write buffers, graph-flush
/// flag). Public serialization goes through [`RuntimeSessionState::to_snapshot`],
/// which drops runtime-only fields by construction.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSessionState {
    pub session_id: String,
    /// Lifetime identity for this runtime state.
    ///
    /// Defaulted and snapshot-derived states are explicitly ephemeral. A
    /// persistent runtime replaces this value with the identity realized and
    /// read back by its store before any frame or effect scope is opened.
    #[serde(skip)]
    pub session_lifetime: crate::SessionLifetime,
    #[serde(default)]
    pub policy: SessionPolicy,
    /// Derived cache of FrameOpen nodes; never serialized or persisted.
    #[serde(skip)]
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
    #[serde(default)]
    pub session_graph: crate::SessionGraph,
    #[serde(default)]
    pub turn_index: usize,
    #[serde(default)]
    pub token_usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt_usage: Option<PromptUsage>,
    #[serde(default)]
    pub protocol_turn_options: crate::ProtocolTurnOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_state_snapshot: Option<crate::ToolState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_snapshot: Option<crate::PluginSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_state_ref: Option<crate::store::BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_state_snapshot: Option<Vec<u8>>,
    /// Cost-accounting ledger. Every LLM call (parent turns, subagent
    /// children, compaction, observers, background helpers) contributes an
    /// entry keyed by `(source, model)`. Separate from `token_usage`
    /// which tracks context-window accounting only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<TokenLedgerEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<crate::store::BlobRef>,
    /// Store head revision observed by the runtime. Revision zero is the
    /// create/fork baseline; `checkpoint_ref` distinguishes an unpersisted
    /// empty runtime from a durable revision-zero fork.
    #[serde(skip)]
    pub head_revision: u64,
    /// Node ids known to exist durably. This is deliberately independent of
    /// the resident graph: partial residency omits durable off-path nodes,
    /// while host-side edits can add resident nodes before they commit.
    #[serde(skip)]
    #[doc(hidden)]
    pub persisted_node_ids: std::collections::HashSet<String>,
}

impl RuntimeSessionState {
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        let agent_frames = snapshot
            .session_graph
            .agent_frame_records(&snapshot.session_id);
        let mut state = Self {
            session_id: snapshot.session_id,
            session_lifetime: crate::SessionLifetime::default(),
            policy: snapshot.policy,
            agent_frames,
            current_frame_node_id: snapshot.current_frame_node_id,
            session_graph: snapshot.session_graph,
            turn_index: snapshot.turn_index,
            token_usage: snapshot.token_usage,
            last_prompt_usage: snapshot.last_prompt_usage,
            protocol_turn_options: snapshot.protocol_turn_options,
            tool_state_ref: snapshot.tool_state_ref,
            tool_state_generation: snapshot.tool_state_generation,
            tool_state_snapshot: None,
            plugin_snapshot_ref: snapshot.plugin_snapshot_ref,
            plugin_snapshot_revision: snapshot.plugin_snapshot_revision,
            plugin_snapshot: None,
            execution_state_ref: snapshot.execution_state_ref,
            execution_state_snapshot: None,
            token_ledger: snapshot.token_ledger,
            checkpoint_ref: snapshot.checkpoint_ref,
            head_revision: 0,
            persisted_node_ids: std::collections::HashSet::new(),
        };
        state.ensure_agent_frame_initialized();
        state
    }

    pub fn to_snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            session_id: self.session_id.clone(),
            policy: self.policy.clone(),
            agent_frames: self.session_graph.agent_frame_records(&self.session_id),
            current_frame_node_id: self.current_frame_node_id.clone(),
            session_graph: self.session_graph.clone(),
            turn_index: self.turn_index,
            token_usage: self.token_usage.clone(),
            last_prompt_usage: self.last_prompt_usage.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
            tool_state_ref: self.tool_state_ref.clone(),
            tool_state_generation: self.tool_state_generation,
            plugin_snapshot_ref: self.plugin_snapshot_ref.clone(),
            plugin_snapshot_revision: self.plugin_snapshot_revision,
            execution_state_ref: self.execution_state_ref.clone(),
            token_ledger: self.token_ledger.clone(),
            checkpoint_ref: self.checkpoint_ref.clone(),
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: &SessionSnapshot) {
        self.session_id = snapshot.session_id.clone();
        self.policy = snapshot.policy.clone();
        self.session_graph = snapshot.session_graph.clone();
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        self.current_frame_node_id = snapshot.current_frame_node_id.clone();
        self.ensure_agent_frame_initialized();
        self.turn_index = snapshot.turn_index;
        self.token_usage = snapshot.token_usage.clone();
        self.last_prompt_usage = snapshot.last_prompt_usage.clone();
        self.protocol_turn_options = snapshot.protocol_turn_options.clone();
        self.tool_state_ref = snapshot.tool_state_ref.clone();
        self.tool_state_generation = snapshot.tool_state_generation;
        self.plugin_snapshot_ref = snapshot.plugin_snapshot_ref.clone();
        self.plugin_snapshot_revision = snapshot.plugin_snapshot_revision;
        self.execution_state_ref = snapshot.execution_state_ref.clone();
        self.token_ledger = snapshot.token_ledger.clone();
        self.checkpoint_ref = snapshot.checkpoint_ref.clone();
    }

    pub fn stamp_runtime_state(
        &mut self,
        tool_state: Option<&crate::ToolState>,
        plugin_snapshot: Option<&crate::PluginSessionSnapshot>,
    ) {
        self.tool_state_snapshot = tool_state.cloned();
        self.tool_state_generation = tool_state.map(|snapshot| snapshot.generation());
        self.plugin_snapshot = plugin_snapshot.cloned();
    }

    pub fn usage_report(&self) -> super::usage::SessionUsageReport {
        super::usage::SessionUsageReport::from_entries(&self.token_ledger)
    }

    pub(crate) fn read_model(&self) -> crate::session_graph::SessionReadModel {
        self.current_frame_node_id.as_deref().map_or_else(
            || self.session_graph.read_model(),
            |frame_node_id| self.session_graph.read_model_for_frame(frame_node_id),
        )
    }

    pub fn replace_active_read_state(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        if let Some(frame_node_id) = self.current_frame_node_id.as_deref() {
            self.session_graph
                .replace_active_read_state_for_frame(frame_node_id, messages);
        } else {
            self.session_graph.replace_active_read_state(messages);
        }
        self.refresh_current_frame_projection();
    }

    pub fn append_active_read_delta(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_active_read_delta(messages);
        self.refresh_current_frame_projection();
    }

    pub fn append_active_conversation_messages(&mut self, messages: &[Message]) {
        self.ensure_agent_frame_initialized();
        self.session_graph.append_active_read_delta(messages);
        self.refresh_current_frame_projection();
    }

    pub(crate) fn append_active_conversation_messages_with_clock(
        &mut self,
        messages: &[Message],
        clock: &dyn crate::Clock,
    ) {
        self.ensure_agent_frame_initialized_with_clock(clock);
        self.session_graph
            .append_active_conversation_messages_at(messages, clock.timestamp_rfc3339());
        self.refresh_current_frame_projection();
    }

    pub fn read_view(&self) -> crate::SessionReadView {
        crate::SessionReadView::from_persisted_state(self)
    }

    pub fn session_graph(&self) -> &crate::SessionGraph {
        &self.session_graph
    }

    pub fn policy(&self) -> &SessionPolicy {
        self.effective_policy()
    }

    pub fn turn_state(&self) -> PersistedTurnState {
        PersistedTurnState {
            turn_index: self.turn_index,
            token_usage: self.token_usage.clone(),
            last_prompt_usage: self.last_prompt_usage.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
        }
    }

    pub fn token_ledger(&self) -> &[TokenLedgerEntry] {
        &self.token_ledger
    }

    pub fn apply_persisted_commit_result(&mut self, result: crate::store::RuntimeCommitResult) {
        self.head_revision = result.head_revision;
        self.checkpoint_ref = Some(result.checkpoint_ref);
        self.session_graph
            .apply_realized_node_timestamps(&result.realized_node_timestamps);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        self.tool_state_ref = result.manifest.tool_state_ref;
        if let Some(snapshot) = self.tool_state_snapshot.as_ref() {
            self.tool_state_generation = Some(snapshot.generation());
        } else if self.tool_state_ref.is_none() {
            self.tool_state_generation = None;
        }
        self.plugin_snapshot_ref = result.manifest.plugin_snapshot_ref;
        self.plugin_snapshot_revision = result.manifest.plugin_snapshot_revision;
        self.execution_state_ref = result.manifest.execution_state_ref;
        self.tool_state_snapshot = None;
        self.plugin_snapshot = None;
        self.execution_state_snapshot = None;
    }

    pub(crate) fn pending_graph_commit(&self) -> crate::GraphAppend {
        let nodes = self
            .session_graph
            .nodes
            .iter()
            .filter(|node| !self.persisted_node_ids.contains(&node.node_id))
            .cloned()
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            crate::GraphAppend {
                nodes: Vec::new(),
                leaf_node_id: self.session_graph.leaf_node_id.clone(),
            }
        } else {
            crate::GraphAppend {
                nodes,
                leaf_node_id: self.session_graph.leaf_node_id.clone(),
            }
        }
    }

    pub(crate) fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.persisted_node_ids.extend(node_ids);
    }

    pub fn discard_runtime_snapshots(&mut self) {
        self.tool_state_snapshot = None;
        self.plugin_snapshot = None;
        self.execution_state_snapshot = None;
    }

    pub fn set_execution_state_snapshot(&mut self, execution_state_snapshot: Option<Vec<u8>>) {
        if execution_state_snapshot.is_none() {
            self.execution_state_ref = None;
        }
        self.execution_state_snapshot = execution_state_snapshot;
    }

    pub fn execution_state_snapshot(&self) -> Option<&[u8]> {
        self.execution_state_snapshot.as_deref()
    }

    pub fn refresh_plugin_snapshots(&mut self, plugins: &crate::PluginSession) {
        let tool_registry = plugins.tool_registry();
        let generation = tool_registry.generation();
        if self.tool_state_ref.is_none() || self.tool_state_generation != Some(generation) {
            let snapshot = tool_registry.export_state();
            self.tool_state_generation = Some(snapshot.generation());
            self.tool_state_snapshot = Some(snapshot);
        }

        let revision = plugins.snapshot_revision_fingerprint();
        if self.plugin_snapshot_ref.is_none() || self.plugin_snapshot_revision != Some(revision) {
            store_plugin_snapshot(&mut self.plugin_snapshot, plugins.snapshot());
        }
        self.plugin_snapshot_revision = Some(revision);
    }
}

/// Persist a freshly captured plugin snapshot, logging and **retaining the prior
/// snapshot** when the capture fails.
///
/// A failed capture (`Err`) previously collapsed to `None` via `.ok()`, erasing
/// the last good snapshot — so the next cold rebuild would restore an empty
/// plugin surface even though a valid snapshot had been captured earlier. Keep
/// the prior value and surface the error instead.
pub(crate) fn store_plugin_snapshot(
    target: &mut Option<crate::PluginSessionSnapshot>,
    captured: Result<crate::PluginSessionSnapshot, crate::PluginError>,
) {
    match captured {
        Ok(snapshot) => *target = Some(snapshot),
        Err(err) => tracing::warn!(
            error = %err,
            "failed to capture plugin snapshot; retaining the prior snapshot",
        ),
    }
}

impl RuntimeSessionState {
    pub fn bind_durable_incarnation(&mut self, incarnation_id: crate::IncarnationId) {
        let frame_mapping = self
            .session_graph
            .nodes
            .iter()
            .filter(|node| !self.persisted_node_ids.contains(&node.node_id))
            .filter_map(|node| {
                let crate::SessionNodePayload::FrameOpen { frame_key, .. } = &node.payload else {
                    return None;
                };
                let durable_node_id =
                    crate::frame_node_id(&self.session_id, &incarnation_id, frame_key);
                (durable_node_id != node.node_id).then(|| (node.node_id.clone(), durable_node_id))
            })
            .collect::<Vec<_>>();
        self.session_lifetime = crate::SessionLifetime::durable(incarnation_id);
        self.session_graph
            .remap_node_ids(&self.session_id, &frame_mapping);
        if let Some(current) = self.current_frame_node_id.as_mut()
            && let Some((_, durable)) = frame_mapping
                .iter()
                .find(|(provisional, _)| provisional == current)
        {
            *current = durable.clone();
        }
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    pub fn durable_incarnation_id(
        &self,
        boundary: &'static str,
    ) -> Result<&crate::IncarnationId, crate::StoreError> {
        self.session_lifetime.as_durable().ok_or_else(|| {
            crate::StoreError::EphemeralSessionAtDurableBoundary {
                session_id: self.session_id.clone(),
                boundary,
            }
        })
    }

    pub fn turn_scope(&self, turn_id: impl Into<String>) -> crate::ExecutionScope {
        let turn_id = turn_id.into();
        match self.session_lifetime.as_durable() {
            Some(incarnation_id) => crate::ExecutionScope::turn_incarnation(
                &self.session_id,
                incarnation_id.clone(),
                turn_id,
            ),
            None => crate::ExecutionScope::turn(&self.session_id, turn_id),
        }
    }

    pub fn queue_drain_scope(&self, drain_id: impl Into<String>) -> crate::ExecutionScope {
        let drain_id = drain_id.into();
        match self.session_lifetime.as_durable() {
            Some(incarnation_id) => crate::ExecutionScope::queue_drain_incarnation(
                &self.session_id,
                incarnation_id.clone(),
                drain_id,
            ),
            None => crate::ExecutionScope::queue_drain(&self.session_id, drain_id),
        }
    }

    pub(crate) fn refresh_current_frame_projection(&mut self) {
        self.current_frame_node_id = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
            .map(str::to_string);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    pub fn current_agent_frame(&self) -> Option<&crate::AgentFrameRecord> {
        self.agent_frames.iter().find(|frame| {
            Some(frame.frame_node_id.as_str()) == self.current_frame_node_id.as_deref()
        })
    }

    pub fn effective_policy(&self) -> &SessionPolicy {
        &self.policy
    }

    pub fn process_execution_env_spec(
        &self,
        fallback_policy: &SessionPolicy,
    ) -> crate::ProcessExecutionEnvSpec {
        self.current_agent_frame()
            .map(|frame| {
                crate::ProcessExecutionEnvSpec::new(
                    frame.assignment.plugin_options.clone(),
                    self.policy.clone(),
                )
            })
            .unwrap_or_else(|| {
                crate::ProcessExecutionEnvSpec::new(
                    crate::PluginOptions::default(),
                    fallback_policy.clone(),
                )
            })
    }

    pub fn effective_protocol_turn_options(&self) -> &crate::ProtocolTurnOptions {
        &self.protocol_turn_options
    }

    pub fn ensure_agent_frame_initialized(&mut self) {
        self.ensure_agent_frame_initialized_with_clock(&crate::SystemClock);
    }

    pub fn ensure_agent_frame_initialized_with_clock(&mut self, clock: &dyn crate::Clock) {
        if let Some(frame_node_id) = self
            .session_graph
            .nearest_frame_node_id(self.session_graph.leaf_node_id.as_deref())
        {
            self.current_frame_node_id = Some(frame_node_id.to_string());
            self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
            return;
        }
        if self.session_graph.leaf_node_id.is_some() {
            self.current_frame_node_id = None;
            self.agent_frames.clear();
            return;
        }
        let assignment = crate::AgentFrameAssignment::from_policy(self.policy.clone());
        let frame_key = "initial-frame";
        let frame_node_id = crate::session_graph::frame_node_id_for_lifetime(
            &self.session_id,
            &self.session_lifetime,
            frame_key,
        );
        self.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key.to_string(),
            crate::AgentFrameReason::initial(),
            assignment,
            self.protocol_turn_options.clone(),
            clock.timestamp_rfc3339(),
        );
        self.current_frame_node_id = Some(frame_node_id);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    pub fn reset_initial_agent_frame(
        &mut self,
        assignment: crate::AgentFrameAssignment,
        protocol_turn_options: crate::ProtocolTurnOptions,
    ) {
        self.reset_initial_agent_frame_with_clock(
            assignment,
            protocol_turn_options,
            &crate::SystemClock,
        );
    }

    pub fn reset_initial_agent_frame_with_clock(
        &mut self,
        assignment: crate::AgentFrameAssignment,
        protocol_turn_options: crate::ProtocolTurnOptions,
        clock: &dyn crate::Clock,
    ) {
        self.policy = assignment.policy.clone();
        self.protocol_turn_options = protocol_turn_options.clone();
        let frame_key = "initial-frame";
        let frame_node_id = crate::session_graph::frame_node_id_for_lifetime(
            &self.session_id,
            &self.session_lifetime,
            frame_key,
        );
        self.session_graph.append_frame_open_with_id_at(
            frame_node_id.clone(),
            frame_key.to_string(),
            crate::AgentFrameReason::initial(),
            assignment,
            protocol_turn_options,
            clock.timestamp_rfc3339(),
        );
        self.current_frame_node_id = Some(frame_node_id);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }
}

impl Default for RuntimeSessionState {
    fn default() -> Self {
        Self {
            session_id: "root".to_string(),
            session_lifetime: crate::SessionLifetime::default(),
            policy: SessionPolicy::default(),
            agent_frames: Vec::new(),
            current_frame_node_id: None,
            session_graph: crate::SessionGraph::default(),
            turn_index: 0,
            token_usage: TokenUsage::default(),
            last_prompt_usage: None,
            protocol_turn_options: crate::ProtocolTurnOptions::default(),
            tool_state_ref: None,
            tool_state_generation: None,
            tool_state_snapshot: None,
            plugin_snapshot_ref: None,
            plugin_snapshot_revision: None,
            plugin_snapshot: None,
            execution_state_ref: None,
            execution_state_snapshot: None,
            token_ledger: Vec::new(),
            checkpoint_ref: None,
            head_revision: 0,
            persisted_node_ids: std::collections::HashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_operation_identity_depends_on_caller_boundary_not_head_revision() {
        let first = boundary_operation("session", "request-42", "append-session-nodes");
        let retry = boundary_operation("session", "request-42", "append-session-nodes");
        let next = boundary_operation("session", "request-43", "append-session-nodes");

        assert_eq!(first, retry);
        assert_ne!(first, next);
    }
    use std::sync::{Arc, Mutex};

    struct DynamicSnapshotTools {
        names: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::ToolProvider for DynamicSnapshotTools {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            self.names
                .lock()
                .expect("dynamic snapshot names")
                .iter()
                .map(|name| {
                    crate::ToolDefinition::raw(
                        format!("tool:{name}"),
                        name,
                        "dynamic snapshot tool",
                        crate::ToolDefinition::default_input_schema(),
                        serde_json::json!({}),
                    )
                    .manifest()
                })
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
            self.names
                .lock()
                .expect("dynamic snapshot names")
                .iter()
                .any(|candidate| candidate == name)
                .then(|| {
                    Arc::new(
                        crate::ToolDefinition::raw(
                            format!("tool:{name}"),
                            name,
                            "dynamic snapshot tool",
                            crate::ToolDefinition::default_input_schema(),
                            serde_json::json!({}),
                        )
                        .contract(),
                    )
                })
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
            crate::ToolResult::ok(serde_json::json!("ok"))
        }
    }

    #[test]
    fn session_snapshot_serialization_excludes_runtime_only_fields_and_round_trips() {
        let mut state = RuntimeSessionState {
            session_id: "snapshot-test".to_string(),
            policy: SessionPolicy {
                provider_id: "mock".to_string(),
                ..SessionPolicy::default()
            },
            tool_state_snapshot: Some(crate::ToolState::default()),
            plugin_snapshot: Some(crate::PluginSessionSnapshot::default()),
            execution_state_snapshot: Some(vec![1, 2, 3]),
            head_revision: 42,
            ..RuntimeSessionState::default()
        };
        state.ensure_agent_frame_initialized();

        let value = serde_json::to_value(state.to_snapshot()).expect("serialize snapshot");

        for runtime_key in [
            "head_revision",
            "persisted_node_ids",
            "tool_state_snapshot",
            "plugin_snapshot",
            "execution_state_snapshot",
        ] {
            assert!(
                value.get(runtime_key).is_none(),
                "snapshot unexpectedly exposed {runtime_key}"
            );
        }
        assert!(value.get("agent_frames").is_none());

        let snapshot: SessionSnapshot = serde_json::from_value(value).expect("round-trip snapshot");
        let hydrated = RuntimeSessionState::from_snapshot(snapshot);

        assert_eq!(hydrated.session_id, "snapshot-test");
        assert_eq!(hydrated.policy.recorded_provider_id(), "mock");
        assert_eq!(hydrated.head_revision, 0);
        assert!(hydrated.tool_state_snapshot.is_none());
        assert!(hydrated.plugin_snapshot.is_none());
        assert!(hydrated.execution_state_snapshot.is_none());
        assert!(!hydrated.agent_frames.is_empty());
    }

    #[test]
    fn reconciled_generation_forces_next_plugin_snapshot_export() {
        let names = Arc::new(Mutex::new(vec!["dynamic_one".to_string()]));
        let tools: Arc<dyn crate::ToolProvider> = Arc::new(DynamicSnapshotTools {
            names: Arc::clone(&names),
        });
        let plugins = crate::runtime::tests::helpers::plugin_session_with_tools("root", tools);
        let snapshot = plugins.tool_registry().export_state();
        let persisted_generation = snapshot.generation();
        let mut state = RuntimeSessionState {
            tool_state_ref: Some("persisted-tool-state".to_string().into()),
            tool_state_generation: Some(persisted_generation),
            ..RuntimeSessionState::default()
        };

        names
            .lock()
            .expect("dynamic snapshot names")
            .push("dynamic_two".to_string());
        let report = plugins
            .tool_registry()
            .restore_state(snapshot)
            .expect("live surface restore");
        assert_eq!(report.generation, persisted_generation + 1);

        state.refresh_plugin_snapshots(&plugins);
        let refreshed = state
            .tool_state_snapshot
            .as_ref()
            .expect("generation change re-exports the tool snapshot");
        assert_eq!(refreshed.generation(), report.generation);
        assert!(refreshed.contains(&crate::ToolId::from("tool:dynamic_two")));
    }
}

pub(super) fn apply_persisted_session_config(
    policy: &mut SessionPolicy,
    config: &crate::PersistedSessionConfig,
) {
    policy.model = config.model.clone();
    policy.provider_id = config.provider_id.clone();
}

pub(super) fn apply_session_checkpoint(
    state: &mut RuntimeSessionState,
    checkpoint: Option<crate::store::HydratedSessionCheckpoint>,
) {
    let Some(checkpoint) = checkpoint else {
        state.tool_state_ref = None;
        state.tool_state_generation = None;
        state.tool_state_snapshot = None;
        state.plugin_snapshot_ref = None;
        state.plugin_snapshot_revision = None;
        state.plugin_snapshot = None;
        state.execution_state_ref = None;
        state.execution_state_snapshot = None;
        state.ensure_agent_frame_initialized();
        return;
    };
    state.turn_index = checkpoint.turn_state.turn_index;
    state.token_usage = checkpoint.turn_state.token_usage;
    state.last_prompt_usage = checkpoint.turn_state.last_prompt_usage;
    state.protocol_turn_options = checkpoint.turn_state.protocol_turn_options;
    state.tool_state_ref = checkpoint.tool_state_ref.clone();
    state.tool_state_generation = checkpoint
        .tool_state
        .as_ref()
        .map(|snapshot| snapshot.generation());
    state.tool_state_snapshot = checkpoint.tool_state;
    state.plugin_snapshot_ref = checkpoint.plugin_snapshot_ref.clone();
    state.plugin_snapshot_revision = checkpoint.plugin_snapshot_revision;
    state.plugin_snapshot = checkpoint.plugin_snapshot;
    state.execution_state_ref = checkpoint.execution_state_ref.clone();
    state.execution_state_snapshot = checkpoint.execution_state;
    state.ensure_agent_frame_initialized();
}

pub(super) fn apply_session_head(
    state: &mut RuntimeSessionState,
    head: &crate::store::SessionHead,
) {
    state.session_graph = head.graph.clone();
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    state.current_frame_node_id = head.current_frame_node_id.clone();
    state.checkpoint_ref = head.checkpoint_ref.clone();
    state.token_ledger = head.token_ledger.clone();
    state.tool_state_ref = None;
    state.tool_state_generation = None;
    state.tool_state_snapshot = None;
    state.plugin_snapshot_ref = None;
    state.plugin_snapshot_revision = None;
    state.plugin_snapshot = None;
    state.execution_state_ref = None;
    state.execution_state_snapshot = None;
    state.ensure_agent_frame_initialized();
    state.head_revision = head.head_revision;
    state.persisted_node_ids = head
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect();
    apply_persisted_session_config(&mut state.policy, &head.config);
}

pub(super) fn append_session_nodes_to_state_with_clock(
    state: &mut RuntimeSessionState,
    nodes: &[crate::SessionAppendNode],
    draft_namespace: &str,
    clock: &dyn crate::Clock,
) -> Vec<String> {
    let drafts = nodes
        .iter()
        .enumerate()
        .map(|(ordinal, node)| {
            let fallback_digest =
                crate::stable_hash::sha256_hex(format!("{draft_namespace}:{ordinal}").as_bytes());
            session_append_node_draft(node, &format!("m_append_{fallback_digest}"))
        })
        .collect::<Vec<_>>();
    state.ensure_agent_frame_initialized_with_clock(clock);
    state
        .session_graph
        .append_node_drafts_at(draft_namespace, drafts, clock.timestamp_rfc3339())
}

pub(super) fn boundary_operation(
    session_id: &str,
    boundary_id: &str,
    key: impl Into<String>,
) -> crate::OperationId {
    crate::OperationId::new(
        crate::ExecutionScope::runtime_operation(format!(
            "session:{session_id}:boundary:{boundary_id}"
        )),
        key,
    )
}

pub(super) fn derive_graph_commit_node_ids(
    state: &mut RuntimeSessionState,
    graph: &mut crate::GraphAppend,
    operation: &crate::OperationId,
) -> Result<Vec<String>, crate::StoreError> {
    let mapping = graph.derive_node_ids(
        &state.session_id,
        state.durable_incarnation_id("history node derivation")?,
        operation,
    )?;
    state
        .session_graph
        .remap_node_ids(&state.session_id, &mapping);
    if let Some(current) = state.current_frame_node_id.as_mut()
        && let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current)
    {
        *current = derived.clone();
    }
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    Ok(mapping.into_iter().map(|(_, derived)| derived).collect())
}

pub(super) fn open_agent_frame_in_state_with_clock(
    state: &mut RuntimeSessionState,
    request: crate::OpenAgentFrameRequest,
    clock: &dyn crate::Clock,
) -> crate::OpenAgentFrameResult {
    state.ensure_agent_frame_initialized_with_clock(clock);
    if request.frame_id.trim().is_empty() {
        return crate::OpenAgentFrameResult {
            frame_node_id: state.current_frame_node_id.clone().unwrap_or_default(),
            opened: false,
            initial_node_ids: Vec::new(),
        };
    }

    let previous = state.current_agent_frame().cloned();
    let mut assignment = previous
        .as_ref()
        .map(|frame| frame.assignment.clone())
        .unwrap_or_else(|| crate::AgentFrameAssignment::from_policy(state.policy.clone()));
    assignment.policy = state.policy.clone();
    let protocol_turn_options = state.protocol_turn_options.clone();
    let frame_node_id = crate::session_graph::frame_node_id_for_lifetime(
        &state.session_id,
        &state.session_lifetime,
        &request.frame_id,
    );
    let opened = state.session_graph.append_frame_open_with_id_at(
        frame_node_id.clone(),
        request.frame_id.clone(),
        request.reason,
        assignment,
        protocol_turn_options,
        clock.timestamp_rfc3339(),
    );
    if !opened {
        if state.current_frame_node_id.as_deref() == Some(frame_node_id.as_str()) {
            return crate::OpenAgentFrameResult {
                frame_node_id,
                opened: false,
                initial_node_ids: Vec::new(),
            };
        }
        state
            .session_graph
            .set_leaf_node_id(Some(frame_node_id.clone()));
    }
    state.current_frame_node_id = Some(frame_node_id);
    state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
    if let Some((policy, protocol_turn_options)) = state.current_agent_frame().map(|frame| {
        (
            frame.assignment.policy.clone(),
            frame.protocol_turn_options.clone(),
        )
    }) {
        state.policy = policy;
        state.protocol_turn_options = protocol_turn_options;
    }

    let initial_node_ids = append_session_nodes_to_state_with_clock(
        state,
        &request.initial_nodes,
        &request.frame_id,
        clock,
    );
    crate::OpenAgentFrameResult {
        frame_node_id: state.current_frame_node_id.clone().unwrap_or_default(),
        opened: true,
        initial_node_ids,
    }
}

fn session_append_node_draft(
    node: &crate::SessionAppendNode,
    fallback_message_id: &str,
) -> crate::session_graph::SessionNodeDraft {
    match node {
        crate::SessionAppendNode::Message { message } => {
            crate::session_graph::SessionNodeDraft::message(plugin_message_to_message(
                message,
                fallback_message_id,
            ))
        }
        crate::SessionAppendNode::ProtocolEvent { event } => {
            crate::session_graph::SessionNodeDraft::protocol_event(event.clone())
        }
        crate::SessionAppendNode::Plugin { plugin_type, body } => {
            crate::session_graph::SessionNodeDraft::plugin(plugin_type.clone(), body.clone())
        }
    }
}

#[cfg(test)]
mod plugin_snapshot_tests {
    use super::store_plugin_snapshot;
    use crate::{PluginError, PluginSessionSnapshot};

    #[test]
    fn ok_capture_overwrites_target() {
        let mut target = None;
        store_plugin_snapshot(&mut target, Ok(PluginSessionSnapshot::default()));
        assert!(target.is_some(), "a successful capture must be stored");
    }

    #[test]
    fn failed_capture_retains_prior_snapshot() {
        // The regression this guards: a failed snapshot capture used to collapse
        // to `None` via `.ok()`, erasing the last good snapshot so the next cold
        // rebuild would restore an empty plugin surface. A failure must leave the
        // prior snapshot intact.
        let prior = PluginSessionSnapshot::default();
        let mut target = Some(prior);
        store_plugin_snapshot(
            &mut target,
            Err(PluginError::Snapshot("capture failed".to_string())),
        );
        assert!(
            target.is_some(),
            "a failed capture must retain the prior snapshot, not erase it"
        );
    }
}
