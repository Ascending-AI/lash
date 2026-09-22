use crate::SessionId;
use crate::TurnId;
use serde::{Deserialize, Serialize};

use super::*;
use crate::SessionAppendNode;
use crate::facade_support::ToolStateFacadeOps;

#[async_trait::async_trait]
pub trait SessionStateService: Send + Sync {
    async fn turn_scope(
        &self,
        _session_id: &SessionId,
        _turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError> {
        Err(PluginError::Session(
            "session turn scopes are unavailable in this runtime".to_string(),
        ))
    }

    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Err(PluginError::Session(
            "session snapshots are unavailable in this runtime".to_string(),
        ))
    }

    async fn snapshot_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionSnapshot, PluginError> {
        Err(PluginError::Session(
            "session lookup is unavailable in this runtime".to_string(),
        ))
    }

    async fn tool_catalog(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<serde_json::Value>, PluginError> {
        Err(PluginError::Session(
            "tool catalogs are unavailable in this runtime".to_string(),
        ))
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &SessionId,
    ) -> Result<std::sync::Arc<Vec<serde_json::Value>>, PluginError> {
        Ok(std::sync::Arc::new(self.tool_catalog(session_id).await?))
    }

    /// Capture the spawn-time [`SessionPluginInit`] payload a
    /// `ParentFork` creation request must carry. The capture reads the named
    /// resident session exactly once; the request then travels durably and
    /// materialization never reads the live session again.
    async fn session_plugin_init(
        &self,
        _session_id: &SessionId,
    ) -> Result<SessionPluginInit, PluginError> {
        Err(PluginError::Session(
            "session plugin init capture is unavailable in this runtime".to_string(),
        ))
    }

    async fn tool_state(&self, _session_id: &SessionId) -> Result<crate::ToolState, PluginError> {
        Err(PluginError::Session(
            "tool state is unavailable in this session".to_string(),
        ))
    }

    async fn apply_tool_state(
        &self,
        _session_id: &SessionId,
        _snapshot: crate::ToolState,
    ) -> Result<u64, PluginError> {
        Err(PluginError::Session(
            "tool state mutation is unavailable in this session".to_string(),
        ))
    }

    /// Toggle Tool Catalog membership for several tools at once. `present` adds
    /// the tools as members; `!present` removes them (non-membership) while
    /// keeping their state for later re-add.
    async fn set_tool_membership(
        &self,
        session_id: &SessionId,
        tool_names: &[String],
        present: bool,
    ) -> Result<u64, PluginError> {
        let mut snapshot = self.tool_state(session_id).await?;
        for name in tool_names {
            let id = snapshot
                .iter()
                .find(|(_, entry)| entry.manifest().name == *name)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| PluginError::Session(format!("unknown tool `{name}`")))?;
            snapshot
                .set_membership(&id, present)
                .map_err(|err| PluginError::Session(err.to_string()))?;
        }
        self.apply_tool_state(session_id, snapshot).await
    }
}

/// Session initialisation service (ADR 0089).
///
/// `create_session` is the one lifecycle verb: it durably commits a new
/// ordinary session's initial head and returns its handle. There is no close
/// verb — lash never deletes sessions — and no turn verb: a session runs by
/// opening it through the ordinary open path, and a process's
/// `SessionTurn` input is initialized and driven inside the process run.
#[async_trait::async_trait]
pub trait SessionLifecycleService: Send + Sync {
    async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError> {
        Err(PluginError::Session(
            "session creation is unavailable in this runtime".to_string(),
        ))
    }
}

#[async_trait::async_trait]
pub trait SessionGraphService: Send + Sync {
    async fn append_session_nodes(
        &self,
        _session_id: &SessionId,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesOutcome, PluginError> {
        Err(PluginError::Session(
            "session graph mutation is unavailable in this session".to_string(),
        ))
    }

    async fn emit_trace_event(
        &self,
        _context: lash_trace::TraceContext,
        _event: lash_trace::TraceEvent,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    /// Plugin-visible agent-frame switch (FIG-3107).
    ///
    /// Same durable semantics as the in-turn `SwitchAgentFrame` control: the
    /// named frame key, its naming material and reason are journaled with the
    /// running turn, the switch materializes at that turn's final commit
    /// (fresh frame with `initial_nodes`, protocol execution cleared), it is
    /// replay-deterministic because it derives from the requested frame
    /// material, and a switch naming the already-current frame reports
    /// `opened = false` instead of failing.
    ///
    /// A turn materializes at most one agent-frame switch, and this call and
    /// the turn's own `AgentFrameSwitch` outcome are two authors of that one
    /// switch with no precedence order between them (FIG-3303). A second
    /// author naming a different frame key, or the same key with different
    /// `initial_nodes`, is refused; an author repeating the recorded switch is
    /// answered its first outcome, so redrive is idempotent.
    ///
    /// Reachable only under the running session's turn scope: the switch is a
    /// turn-owned graph operation. A switch for a different session or from a
    /// lane-less host service is refused; hosts open frames through
    /// [`crate::SessionRuntime::open_agent_frame`] instead.
    async fn switch_agent_frame(
        &self,
        _session_id: &SessionId,
        _request: SwitchAgentFrameRequest,
    ) -> Result<crate::OpenAgentFrameResult, PluginError> {
        Err(PluginError::Session(
            "agent-frame switches are unavailable in this session".to_string(),
        ))
    }
}

/// Post-turn plugin-requested agent-frame switch (FIG-3107).
///
/// The frame key names the target frame and carries the turn's conflict rule:
/// a turn holds one switch, so a second author naming a different key, or the
/// same key with different `initial_nodes`, is a typed conflict, while
/// re-deriving the recorded switch collapses onto its first outcome. The
/// operation id is the switch's stable identity in that record; `initial_nodes`
/// seed the fresh frame's history; `task` records the switch's task label
/// exactly as the in-turn control does.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SwitchAgentFrameRequest {
    /// Stable idempotency identity of this switch.
    pub operation_id: String,
    pub frame_key: crate::FrameKey,
    /// Reasoning material naming the switch, same authority class as the
    /// in-turn control's task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Frame-open reason the switch journals at materialization.
    pub reason: crate::AgentFrameReason,
    /// Nodes the fresh frame starts with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_nodes: Vec<crate::SessionAppendNode>,
}

impl SwitchAgentFrameRequest {
    pub fn new(
        operation_id: impl Into<String>,
        frame_key: crate::FrameKey,
        reason: crate::AgentFrameReason,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            frame_key,
            task: None,
            reason,
            initial_nodes: Vec::new(),
        }
    }

    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    pub fn with_initial_nodes(mut self, initial_nodes: Vec<crate::SessionAppendNode>) -> Self {
        self.initial_nodes = initial_nodes;
        self
    }
}

/// Result of a single-shot direct LLM call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectCompletion {
    pub text: String,
    pub usage: crate::TokenUsage,
    pub llm_call: crate::LlmCallRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectLlmCompletion {
    pub response: crate::LlmResponse,
    pub usage: crate::TokenUsage,
    pub llm_call: crate::LlmCallRecord,
}

/// A plugin-authored append onto a session's history graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppendSessionNodesRequest {
    /// Caller-stable identity for this logical append. While its durable receipt
    /// is retained, a retry that reproduces the same ancestor, ordered nodes,
    /// and semantic field values under the same identity-encoding version
    /// returns the first append result even after the session head advances.
    /// Changing any of those request fields while reusing the operation id is a
    /// typed caller conflict. A retry must therefore preserve the original
    /// request, rather than rebuilding it from the session's new head.
    pub operation_id: String,
    pub nodes: Vec<SessionAppendNode>,
    /// Branch-liveness precondition: refuse the append unless this node is
    /// still somewhere on the session's active path. `None` skips the check.
    ///
    /// This serves the derive-then-append pattern — read history up to some
    /// node, spend seconds deriving something from it (a model summary, a
    /// memory observation, an embedding, an index entry), then append the
    /// result. Such a caller has to tell two kinds of staleness apart. *More
    /// content arrived* is harmless: the derivation is still true of the prefix
    /// it read, and discarding expensive work over it would be wrong. *The
    /// history it read was rewritten* is fatal: the base the derivation
    /// describes is no longer part of what this session executes. This field
    /// catches the second and deliberately tolerates the first.
    ///
    /// # Accepted
    ///
    /// The runtime reloads the durable head first, then accepts the append if
    /// the named node is anywhere on the resulting active path — the leaf or
    /// any ancestor of it. An append whose base was overtaken while it was
    /// being derived is therefore accepted.
    ///
    /// # Where the nodes land
    ///
    /// **Not at the named node.** The append is always built from the *current*
    /// leaf: the first new node's parent is the leaf as of that reload, and
    /// history stays linear. A caller that named an ancestor gets its nodes
    /// appended *after* content it never read. Nothing forks, and nothing
    /// already committed is lost or reordered — but the position of the
    /// appended nodes carries no claim about what preceded them. A reader that
    /// needs to know what a node was derived from must find that in the node's
    /// own payload (an observed message id, a revision) instead of inferring it
    /// from graph position.
    ///
    /// # Refused
    ///
    /// [`AppendSessionNodesOutcome::StaleBranch`], with nothing written, when the
    /// named node has left the active path: the session forked or was rewound
    /// onto another line of history, or the id was never durable here at all.
    /// This applies only to a fresh operation id. A retry whose durable receipt
    /// proves the append already committed returns that first result even when
    /// the ancestor has since left the active path.
    ///
    /// # What this does *not* guarantee
    ///
    /// This is **not** a compare-and-swap on the session head, and it does
    /// **not** detect concurrent appends. Any number of other writers may have
    /// committed between the caller's read and this append, and this field will
    /// still accept it. If a derivation is only valid when nothing at all was
    /// added since it read — for instance it *replaces* rather than accumulates
    /// some derived state — this field will not protect it: carry the observed
    /// base in the payload and let the reader adjudicate, or make the derived
    /// state idempotent under late arrival. The store underneath does enforce a
    /// strict head fence, but the runtime satisfies it by construction by
    /// re-parenting onto the leaf it just read, so it never surfaces here as a
    /// conflict a caller could use as a concurrency signal.
    #[serde(default)]
    pub requires_ancestor_node_id: Option<crate::NodeId>,
}

/// Outcome of [`SessionGraphService::append_session_nodes`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AppendSessionNodesOutcome {
    /// The nodes are durable. `node_ids` are their store-assigned ids in
    /// request order. On a fresh append, `leaf_node_id` is the selected leaf
    /// after that commit. On receipt replay both fields are the stored
    /// first-attempt result; later commits may have moved the current session
    /// leaf elsewhere.
    ///
    /// The first appended node parents on whatever the leaf was when the
    /// runtime reloaded the head, which is not necessarily
    /// [`AppendSessionNodesRequest::requires_ancestor_node_id`]; see that field.
    Appended {
        node_ids: Vec<crate::NodeId>,
        leaf_node_id: crate::NodeId,
    },
    /// Nothing was written: the branch the caller read from has been abandoned.
    /// [`AppendSessionNodesRequest::requires_ancestor_node_id`] named a node
    /// that is no longer on this session's active path, so the base the
    /// derivation describes is gone from this session's line of execution.
    ///
    /// This is not a failed compare-and-swap. There is no expected-versus-actual
    /// head to reconcile and no value to retry *against*: resubmitting the same
    /// append cannot make the base come back, so the derivation has to be redone
    /// against a fresh read or dropped. A head that merely advanced never
    /// produces this outcome.
    StaleBranch {
        /// Echo of the request's `requires_ancestor_node_id`, so a caller with
        /// several derivations in flight can tell which one lost its base.
        required_node_id: crate::NodeId,
    },
}
