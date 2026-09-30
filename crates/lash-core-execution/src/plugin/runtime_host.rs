use crate::SessionId;
use crate::TurnId;
use serde::{Deserialize, Serialize};

use super::*;
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

pub use lash_core_store::session_append::{AppendSessionNodesOutcome, AppendSessionNodesRequest};
