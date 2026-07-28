use serde::{Deserialize, Serialize};

use super::*;
use crate::SessionAppendNode;

#[async_trait::async_trait]
pub trait SessionStateService: Send + Sync {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Err(PluginError::Session(
            "session snapshots are unavailable in this runtime".to_string(),
        ))
    }

    async fn snapshot_session(&self, _session_id: &str) -> Result<SessionSnapshot, PluginError> {
        Err(PluginError::Session(
            "session lookup is unavailable in this runtime".to_string(),
        ))
    }

    async fn tool_catalog(&self, _session_id: &str) -> Result<Vec<serde_json::Value>, PluginError> {
        Err(PluginError::Session(
            "tool catalogs are unavailable in this runtime".to_string(),
        ))
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<std::sync::Arc<Vec<serde_json::Value>>, PluginError> {
        Ok(std::sync::Arc::new(self.tool_catalog(session_id).await?))
    }

    async fn tool_state(&self, _session_id: &str) -> Result<crate::ToolState, PluginError> {
        Err(PluginError::Session(
            "tool state is unavailable in this session".to_string(),
        ))
    }

    async fn apply_tool_state(
        &self,
        _session_id: &str,
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
        session_id: &str,
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

    async fn close_session(&self, _session_id: &str) -> Result<(), PluginError> {
        Err(PluginError::Session(
            "session closing is unavailable in this runtime".to_string(),
        ))
    }

    async fn start_turn(
        &self,
        _request: SessionTurnRequest<'_>,
    ) -> Result<AssembledTurn, PluginError> {
        Err(PluginError::Session(
            "session execution is unavailable in this runtime".to_string(),
        ))
    }
}

#[async_trait::async_trait]
pub trait SessionGraphService: Send + Sync {
    async fn append_session_nodes(
        &self,
        _session_id: &str,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesResult, PluginError> {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionTurnInput {
    pub session_id: String,
    pub turn_id: String,
    pub input: TurnInput,
}

pub struct SessionTurnRequest<'run> {
    turn: SessionTurnInput,
    scoped_effect_controller: crate::ScopedEffectController<'run>,
}

impl<'run> SessionTurnRequest<'run> {
    pub fn new(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        mut input: TurnInput,
        scoped_effect_controller: crate::ScopedEffectController<'run>,
    ) -> Result<Self, PluginError> {
        let session_id = session_id.into();
        let turn_id = turn_id.into();
        if turn_id.trim().is_empty() {
            return Err(PluginError::Session(
                "session turns require a non-empty stable turn id".to_string(),
            ));
        }
        if scoped_effect_controller.turn_id() != Some(turn_id.as_str()) {
            return Err(PluginError::Session(format!(
                "session turn `{turn_id}` requires an effect turn scope with the same id"
            )));
        }
        if scoped_effect_controller.execution_scope().session_id() != Some(session_id.as_str()) {
            return Err(PluginError::Session(format!(
                "session turn `{turn_id}` requires an execution scope for session `{session_id}`"
            )));
        }
        if let Some(input_turn_id) = input.trace_turn_id.as_deref()
            && input_turn_id != turn_id
        {
            return Err(PluginError::Session(format!(
                "input trace_turn_id `{input_turn_id}` does not match turn id `{turn_id}`"
            )));
        }
        input.trace_turn_id = Some(turn_id.clone());
        Ok(Self {
            turn: SessionTurnInput {
                session_id,
                turn_id,
                input,
            },
            scoped_effect_controller,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.turn.session_id
    }

    pub fn turn_id(&self) -> &str {
        &self.turn.turn_id
    }

    pub fn input(&self) -> &TurnInput {
        &self.turn.input
    }

    pub fn scoped_effect_controller(&self) -> &crate::ScopedEffectController<'run> {
        &self.scoped_effect_controller
    }

    pub fn into_parts(self) -> (SessionTurnInput, crate::ScopedEffectController<'run>) {
        (self.turn, self.scoped_effect_controller)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppendSessionNodesRequest {
    /// Caller-stable identity for this logical append. Retries must reuse it
    /// even when the session head has advanced.
    pub operation_id: String,
    pub nodes: Vec<SessionAppendNode>,
    #[serde(default)]
    pub requires_ancestor_node_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AppendSessionNodesResult {
    Appended {
        node_ids: Vec<String>,
        leaf_node_id: String,
    },
    StaleBranch {
        current_leaf_node_id: Option<String>,
    },
}
