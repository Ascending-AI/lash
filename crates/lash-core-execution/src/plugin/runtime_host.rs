pub use lash_core_store::session_append::{AppendSessionNodesOutcome, AppendSessionNodesRequest};

use lash_execution_contracts::session_types::SessionCreateRequest;
use lash_execution_contracts::session_types::SessionHandle;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use serde::{Deserialize, Serialize};

use super::*;

#[async_trait::async_trait]
pub trait SessionStateService: SessionReadService {
    async fn turn_scope(
        &self,
        _session_id: &SessionId,
        _turn_id: &TurnId,
    ) -> Result<crate::ExecutionScope, PluginError>;
}

/// Session initialisation service (ADR 0089).
///
/// `create_session` is the one lifecycle verb: it durably commits a new
/// ordinary session's initial head and returns its handle. There is no close
/// verb — lash never deletes sessions — and no turn verb: a session runs by
/// opening it through the ordinary open path, and a process's
/// `SessionTurn` input is initialized and executed inside the process run.
#[async_trait::async_trait]
pub trait SessionLifecycleService: crate::core_internal::RuntimeService + Send + Sync {
    async fn create_session(
        &self,
        _request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError>;
}

#[async_trait::async_trait]
pub trait SessionGraphService: crate::core_internal::RuntimeService + Send + Sync {
    async fn append_session_nodes(
        &self,
        _session_id: &SessionId,
        _request: AppendSessionNodesRequest,
    ) -> Result<AppendSessionNodesOutcome, PluginError>;

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
    pub usage: crate::LlmUsage,
    pub llm_call: crate::LlmCallRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectLlmCompletion {
    pub response: crate::LlmResponse,
    pub usage: crate::LlmUsage,
    pub llm_call: crate::LlmCallRecord,
}
