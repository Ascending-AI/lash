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
/// (head-revision CAS guard, pending dirty-write buffers, replace-graph
/// flag). Public serialization goes through [`RuntimeSessionState::to_snapshot`],
/// which drops runtime-only fields by construction.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeSessionState {
    pub session_id: String,
    #[serde(default)]
    pub policy: SessionPolicy,
    #[serde(default)]
    pub agent_frames: Vec<crate::AgentFrameRecord>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub current_agent_frame_id: crate::AgentFrameId,
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
    /// Store head revision observed by the runtime. Lease-fenced commits use it
    /// as the stale-writer CAS backstop; `None` means the runtime is creating
    /// the first persisted head.
    #[serde(skip)]
    pub head_revision: Option<u64>,
    /// Signals that the next commit must write the full graph (for example,
    /// `heal_orphaned_leaf` repaired an invalid leaf). Cleared after the
    /// next commit.
    #[serde(skip)]
    pub graph_replace_required: bool,
}

impl RuntimeSessionState {
    pub fn from_snapshot(snapshot: SessionSnapshot) -> Self {
        let mut state = Self {
            session_id: snapshot.session_id,
            policy: snapshot.policy,
            agent_frames: snapshot.agent_frames,
            current_agent_frame_id: snapshot.current_agent_frame_id,
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
            head_revision: None,
            graph_replace_required: false,
        };
        for frame in &mut state.agent_frames {
            frame.execution_state_snapshot = None;
        }
        state.ensure_agent_frame_initialized();
        state
    }

    pub fn to_snapshot(&self) -> SessionSnapshot {
        let mut agent_frames = self.agent_frames.clone();
        for frame in &mut agent_frames {
            frame.execution_state_snapshot = None;
        }
        SessionSnapshot {
            session_id: self.session_id.clone(),
            policy: self.policy.clone(),
            agent_frames,
            current_agent_frame_id: self.current_agent_frame_id.clone(),
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
        self.agent_frames = snapshot.agent_frames.clone();
        self.current_agent_frame_id = snapshot.current_agent_frame_id.clone();
        self.ensure_agent_frame_initialized();
        self.session_graph = snapshot.session_graph.clone();
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
        self.session_graph.read_model_for_agent_frame(
            &self.current_agent_frame_id,
            self.current_agent_frame_is_initial(),
        )
    }

    pub fn replace_active_read_state(&mut self, messages: &[Message]) {
        self.session_graph
            .replace_active_read_state_for_agent_frame(&self.current_agent_frame_id, messages);
        self.graph_replace_required = false;
    }

    pub fn append_active_read_delta(&mut self, messages: &[Message]) {
        self.session_graph
            .append_active_read_delta_for_agent_frame(&self.current_agent_frame_id, messages);
    }

    pub fn append_active_conversation_messages(&mut self, messages: &[Message]) {
        self.session_graph
            .append_active_conversation_messages_for_agent_frame(
                &self.current_agent_frame_id,
                messages,
            );
    }

    pub(crate) fn append_active_conversation_messages_with_clock(
        &mut self,
        messages: &[Message],
        clock: &dyn crate::Clock,
    ) {
        self.session_graph
            .append_active_conversation_messages_for_agent_frame_at(
                &self.current_agent_frame_id,
                messages,
                clock.timestamp_rfc3339(),
            );
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
        self.head_revision = Some(result.head_revision);
        self.checkpoint_ref = Some(result.checkpoint_ref);
        self.agent_frames = result.agent_frames;
        self.current_agent_frame_id = result.current_agent_frame_id;
        self.tool_state_ref = result.manifest.tool_state_ref;
        if let Some(snapshot) = self.tool_state_snapshot.as_ref() {
            self.tool_state_generation = Some(snapshot.generation());
        } else if self.tool_state_ref.is_none() {
            self.tool_state_generation = None;
        }
        self.plugin_snapshot_ref = result.manifest.plugin_snapshot_ref;
        self.plugin_snapshot_revision = result.manifest.plugin_snapshot_revision;
        self.execution_state_ref = result.manifest.execution_state_ref;
        let execution_state_ref = self.execution_state_ref.clone();
        if let Some(frame) = self.current_agent_frame_mut() {
            frame.execution_state_ref = execution_state_ref;
            frame.execution_state_snapshot = None;
        }
        self.graph_replace_required = false;
        self.tool_state_snapshot = None;
        self.plugin_snapshot = None;
        self.execution_state_snapshot = None;
        if let Some(frame) = self.current_agent_frame_mut() {
            frame.execution_state_snapshot = None;
        }
    }

    pub fn discard_runtime_snapshots(&mut self) {
        self.tool_state_snapshot = None;
        self.plugin_snapshot = None;
        self.execution_state_snapshot = None;
        if let Some(frame) = self.current_agent_frame_mut() {
            frame.execution_state_snapshot = None;
        }
    }

    pub fn set_execution_state_snapshot(&mut self, execution_state_snapshot: Option<Vec<u8>>) {
        if execution_state_snapshot.is_none() {
            self.execution_state_ref = None;
        }
        self.execution_state_snapshot = execution_state_snapshot.clone();
        if let Some(frame) = self.current_agent_frame_mut() {
            if execution_state_snapshot.is_none() {
                frame.execution_state_ref = None;
            }
            frame.execution_state_snapshot = execution_state_snapshot;
        }
    }

    pub fn execution_state_snapshot(&self) -> Option<&[u8]> {
        self.current_agent_frame()
            .and_then(|frame| frame.execution_state_snapshot.as_deref())
            .or(self.execution_state_snapshot.as_deref())
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
    pub fn current_agent_frame(&self) -> Option<&crate::AgentFrameRecord> {
        self.agent_frames
            .iter()
            .find(|frame| frame.frame_id == self.current_agent_frame_id)
    }

    pub fn current_agent_frame_mut(&mut self) -> Option<&mut crate::AgentFrameRecord> {
        let current_agent_frame_id = self.current_agent_frame_id.clone();
        self.agent_frames
            .iter_mut()
            .find(|frame| frame.frame_id == current_agent_frame_id)
    }

    pub fn effective_policy(&self) -> &SessionPolicy {
        self.current_agent_frame()
            .map(|frame| &frame.assignment.policy)
            .unwrap_or(&self.policy)
    }

    pub fn process_execution_env_spec(
        &self,
        fallback_policy: &SessionPolicy,
    ) -> crate::ProcessExecutionEnvSpec {
        self.current_agent_frame()
            .map(|frame| {
                crate::ProcessExecutionEnvSpec::new(
                    frame.assignment.plugin_options.clone(),
                    frame.assignment.policy.clone(),
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
        self.current_agent_frame()
            .map(|frame| &frame.protocol_turn_options)
            .unwrap_or(&self.protocol_turn_options)
    }

    pub fn ensure_agent_frame_initialized(&mut self) {
        self.ensure_agent_frame_initialized_with_clock(&crate::SystemClock);
    }

    pub fn ensure_agent_frame_initialized_with_clock(&mut self, clock: &dyn crate::Clock) {
        if self.current_agent_frame_id.is_empty() {
            self.current_agent_frame_id = default_agent_frame_id(&self.session_id);
        }
        if self
            .agent_frames
            .iter()
            .any(|frame| frame.frame_id == self.current_agent_frame_id)
        {
            return;
        }
        let mut frame = default_agent_frame_with_clock(&self.session_id, &self.policy, clock);
        frame.frame_id = self.current_agent_frame_id.clone();
        frame.protocol_turn_options = self.protocol_turn_options.clone();
        frame.execution_state_ref = self.execution_state_ref.clone();
        frame.execution_state_snapshot = self.execution_state_snapshot.clone();
        self.agent_frames.push(frame);
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
        let frame_id = default_agent_frame_id(&self.session_id);
        self.policy = assignment.policy.clone();
        self.protocol_turn_options = protocol_turn_options.clone();
        self.current_agent_frame_id = frame_id.clone();
        self.agent_frames = vec![crate::AgentFrameRecord::new_at(
            frame_id,
            self.session_id.clone(),
            None,
            crate::AgentFrameReason::initial(),
            None,
            assignment,
            protocol_turn_options,
            clock.timestamp_rfc3339(),
        )];
    }

    pub fn append_agent_frame(&mut self, mut frame: crate::AgentFrameRecord) {
        let previous_frame_id = self.current_agent_frame_id.clone();
        for existing in &mut self.agent_frames {
            if existing.frame_id == previous_frame_id {
                existing.status = crate::AgentFrameStatus::Superseded;
            }
        }
        if frame.previous_frame_id.is_none() && !previous_frame_id.is_empty() {
            frame.previous_frame_id = Some(previous_frame_id);
        }
        frame.status = crate::AgentFrameStatus::Active;
        self.policy = frame.assignment.policy.clone();
        self.protocol_turn_options = frame.protocol_turn_options.clone();
        self.current_agent_frame_id = frame.frame_id.clone();
        self.execution_state_ref = frame.execution_state_ref.clone();
        self.execution_state_snapshot = frame.execution_state_snapshot.clone();
        self.agent_frames.push(frame);
    }

    fn current_agent_frame_is_initial(&self) -> bool {
        self.current_agent_frame()
            .map(|frame| frame.previous_frame_id.is_none())
            .unwrap_or(true)
    }
}

impl Default for RuntimeSessionState {
    fn default() -> Self {
        Self {
            session_id: "root".to_string(),
            policy: SessionPolicy::default(),
            agent_frames: default_agent_frames("root", &SessionPolicy::default()),
            current_agent_frame_id: default_agent_frame_id("root"),
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
            head_revision: None,
            graph_replace_required: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            head_revision: Some(42),
            graph_replace_required: true,
            ..RuntimeSessionState::default()
        };
        state.ensure_agent_frame_initialized();
        if let Some(frame) = state.current_agent_frame_mut() {
            frame.execution_state_snapshot = Some(vec![4, 5, 6]);
        }

        let value = serde_json::to_value(state.to_snapshot()).expect("serialize snapshot");

        for runtime_key in [
            "head_revision",
            "graph_replace_required",
            "tool_state_snapshot",
            "plugin_snapshot",
            "execution_state_snapshot",
        ] {
            assert!(
                value.get(runtime_key).is_none(),
                "snapshot unexpectedly exposed {runtime_key}"
            );
        }
        assert!(
            value["agent_frames"]
                .as_array()
                .expect("agent frames")
                .iter()
                .all(|frame| frame.get("execution_state_snapshot").is_none())
        );

        let snapshot: SessionSnapshot = serde_json::from_value(value).expect("round-trip snapshot");
        let hydrated = RuntimeSessionState::from_snapshot(snapshot);

        assert_eq!(hydrated.session_id, "snapshot-test");
        assert_eq!(hydrated.policy.recorded_provider_id(), "mock");
        assert!(hydrated.head_revision.is_none());
        assert!(!hydrated.graph_replace_required);
        assert!(hydrated.tool_state_snapshot.is_none());
        assert!(hydrated.plugin_snapshot.is_none());
        assert!(hydrated.execution_state_snapshot.is_none());
        assert!(
            hydrated
                .agent_frames
                .iter()
                .all(|frame| frame.execution_state_snapshot.is_none())
        );
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
    state.execution_state_snapshot = None;
    state.ensure_agent_frame_initialized();
    if let Some(frame) = state.current_agent_frame_mut() {
        frame.execution_state_ref = checkpoint.execution_state_ref.clone();
        frame.execution_state_snapshot = checkpoint.execution_state;
    }
}

pub(super) fn apply_session_head(
    state: &mut RuntimeSessionState,
    head: &crate::store::SessionHead,
) {
    state.session_graph = head.graph.clone();
    state.agent_frames = head.agent_frames.clone();
    state.current_agent_frame_id = head.current_agent_frame_id.clone();
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
    state.head_revision = Some(head.head_revision);
    state.graph_replace_required = false;
    apply_persisted_session_config(&mut state.policy, &head.config);
}

pub(super) fn append_session_nodes_to_state_with_clock(
    state: &mut RuntimeSessionState,
    nodes: &[crate::SessionAppendNode],
    clock: &dyn crate::Clock,
) -> Vec<String> {
    let drafts = nodes
        .iter()
        .map(session_append_node_draft)
        .collect::<Vec<_>>();
    state.ensure_agent_frame_initialized_with_clock(clock);
    let node_ids = state.session_graph.append_node_drafts_for_agent_frame_at(
        &state.current_agent_frame_id,
        drafts,
        clock.timestamp_rfc3339(),
    );
    normalize_session_graph(state);
    node_ids
}

pub(super) fn revision_operation(
    state: &RuntimeSessionState,
    key: impl Into<String>,
) -> crate::OperationId {
    crate::OperationId::new(
        crate::ExecutionScope::runtime_operation(format!(
            "session:{}:head:{}",
            state.session_id,
            state.head_revision.unwrap_or(0)
        )),
        key,
    )
}

pub(super) fn derive_graph_commit_node_ids(
    state: &mut RuntimeSessionState,
    graph: &mut crate::GraphCommitDelta,
    operation: &crate::OperationId,
) -> Result<Vec<String>, crate::StoreError> {
    let mapping = graph.derive_node_ids(&state.session_id, operation)?;
    state.session_graph.remap_node_ids(&mapping);
    Ok(mapping.into_iter().map(|(_, derived)| derived).collect())
}

pub(super) fn open_agent_frame_in_state_with_clock(
    state: &mut RuntimeSessionState,
    request: crate::OpenAgentFrameRequest,
    clock: &dyn crate::Clock,
) -> crate::OpenAgentFrameResult {
    state.ensure_agent_frame_initialized_with_clock(clock);
    if request.frame_id.trim().is_empty() || state.current_agent_frame_id == request.frame_id {
        return crate::OpenAgentFrameResult {
            frame_id: state.current_agent_frame_id.clone(),
            opened: false,
            initial_node_ids: Vec::new(),
        };
    }

    let previous = state.current_agent_frame().cloned();
    let assignment = previous
        .as_ref()
        .map(|frame| frame.assignment.clone())
        .unwrap_or_else(|| crate::AgentFrameAssignment::from_policy(state.policy.clone()));
    let protocol_turn_options = previous
        .as_ref()
        .map(|frame| frame.protocol_turn_options.clone())
        .unwrap_or_else(|| state.protocol_turn_options.clone());
    let previous_frame_id = previous.map(|frame| frame.frame_id);
    state.append_agent_frame(crate::AgentFrameRecord::new_at(
        request.frame_id.clone(),
        state.session_id.clone(),
        previous_frame_id,
        request.reason,
        request.caused_by,
        assignment,
        protocol_turn_options,
        clock.timestamp_rfc3339(),
    ));

    let initial_node_ids =
        append_session_nodes_to_state_with_clock(state, &request.initial_nodes, clock);
    if !initial_node_ids.is_empty() {
        state.graph_replace_required = true;
    }
    crate::OpenAgentFrameResult {
        frame_id: state.current_agent_frame_id.clone(),
        opened: true,
        initial_node_ids,
    }
}

fn session_append_node_draft(
    node: &crate::SessionAppendNode,
) -> crate::session_graph::SessionNodeDraft {
    match node {
        crate::SessionAppendNode::Message { message, caused_by } => {
            crate::session_graph::SessionNodeDraft::message(plugin_message_to_message(message))
                .with_caused_by(caused_by.clone())
        }
        crate::SessionAppendNode::ProtocolEvent { event, caused_by } => {
            crate::session_graph::SessionNodeDraft::protocol_event(event.clone())
                .with_caused_by(caused_by.clone())
        }
        crate::SessionAppendNode::Plugin {
            plugin_type,
            body,
            caused_by,
        } => crate::session_graph::SessionNodeDraft::plugin(plugin_type.clone(), body.clone())
            .with_caused_by(caused_by.clone()),
    }
}

fn default_agent_frame_id(session_id: &str) -> crate::AgentFrameId {
    format!("{session_id}:frame:initial")
}

fn default_agent_frames(session_id: &str, policy: &SessionPolicy) -> Vec<crate::AgentFrameRecord> {
    vec![default_agent_frame(session_id, policy)]
}

fn default_agent_frame(session_id: &str, policy: &SessionPolicy) -> crate::AgentFrameRecord {
    default_agent_frame_with_clock(session_id, policy, &crate::SystemClock)
}

fn default_agent_frame_with_clock(
    session_id: &str,
    policy: &SessionPolicy,
    clock: &dyn crate::Clock,
) -> crate::AgentFrameRecord {
    crate::AgentFrameRecord::new_at(
        default_agent_frame_id(session_id),
        session_id.to_string(),
        None,
        crate::AgentFrameReason::initial(),
        None,
        crate::AgentFrameAssignment::from_policy(policy.clone()),
        crate::ProtocolTurnOptions::default(),
        clock.timestamp_rfc3339(),
    )
}

/// Heal any graph corruption (orphaned leaf) on load.
///
/// Must run BEFORE any residency-based trim (phase-9 feature) because
/// healing's fallback search relies on having the full node set in RAM.
/// Under `Residency::ActivePathOnly`, the runtime loads only the active
/// path; if the leaf doesn't resolve against that reduced set, the
/// caller falls back to a full `load_session_graph()` + `normalize` +
/// trim.
pub(super) fn normalize_session_graph(state: &mut RuntimeSessionState) {
    if state.session_graph.heal_orphaned_leaf() {
        state.graph_replace_required = true;
    }
}

/// Trim the resident node set according to `Residency`. Called AFTER
/// `normalize_session_graph` during `from_environment` load. Under
/// `KeepAll` this is a no-op; under `ActivePathOnly` it replaces the
/// resident graph with just the active path. Orphans remain on disk —
/// the host decides whether/when to tombstone + vacuum them via
/// `LashRuntime::orphaned_node_ids` + the store primitives.
///
pub(super) fn apply_residency_on_load(
    state: &mut RuntimeSessionState,
    residency: crate::Residency,
) {
    match residency {
        crate::Residency::KeepAll => {}
        crate::Residency::ActivePathOnly => {
            state.session_graph = state.session_graph.fork_current_path();
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

#[cfg(test)]
mod residency_tests {
    use super::apply_residency_on_load;
    use crate::{
        Message, MessageRole, Part, PartKind, PruneState, Residency, RuntimeSessionState,
        shared_parts,
    };

    fn text_message(id: &str, content: &str) -> Message {
        Message {
            id: id.to_string(),
            role: MessageRole::User,
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

    /// Root, an inactive branch off the root, then an active branch off the root.
    /// Returns the state plus the inactive and active branch node ids.
    fn branching_state() -> (RuntimeSessionState, String, String) {
        let mut state = RuntimeSessionState::default();
        state.append_active_conversation_messages(&[text_message("root", "root")]);
        let root = state.session_graph.leaf_node_id.clone();
        state.append_active_conversation_messages(&[text_message("inactive", "inactive branch")]);
        let inactive_node = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("inactive node");
        state.session_graph.branch_to(root);
        state.append_active_conversation_messages(&[text_message("active", "active branch")]);
        let active_node = state
            .session_graph
            .leaf_node_id
            .clone()
            .expect("active node");
        (state, inactive_node, active_node)
    }

    #[test]
    fn active_path_only_trims_orphan_branches_on_load() {
        // The durable worker rebuild (and session resume) call this to match the
        // live runtime's residency. ActivePathOnly drops nodes off the active
        // path so a rebuilt session does not silently retain the full graph.
        let (mut state, inactive_node, active_node) = branching_state();
        assert!(
            state.session_graph.find_node(&inactive_node).is_some(),
            "the inactive branch is resident before trimming"
        );
        apply_residency_on_load(&mut state, Residency::ActivePathOnly);
        assert!(
            state.session_graph.find_node(&inactive_node).is_none(),
            "ActivePathOnly must drop the orphaned inactive branch on rebuild"
        );
        assert!(
            state.session_graph.find_node(&active_node).is_some(),
            "the active path must be retained"
        );
    }

    #[test]
    fn keep_all_retains_orphan_branches_on_load() {
        let (mut state, inactive_node, _active_node) = branching_state();
        apply_residency_on_load(&mut state, Residency::KeepAll);
        assert!(
            state.session_graph.find_node(&inactive_node).is_some(),
            "KeepAll must retain the full resident graph"
        );
    }
}
