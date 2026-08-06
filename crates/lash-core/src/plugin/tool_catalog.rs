use lash_sansio::ToolCallOutput;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::*;

#[derive(Clone)]
pub struct ToolCatalogContext {
    pub session_id: String,
    pub tools: Vec<ToolManifest>,
    pub resolve_contract: Option<lash_sansio::ToolContractResolver>,
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    pub extensions: PluginExtensions,
}

#[derive(Clone, Debug)]
pub struct PluginAbort {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct TurnPreparation {
    pub messages: crate::MessageSequence,
    pub events: Vec<crate::SessionStreamEvent>,
    pub abort: Option<PluginAbort>,
}

#[derive(Clone)]
pub struct PrepareTurnRequest {
    pub session_id: String,
    pub state: SessionReadView,
    pub messages: crate::MessageSequence,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub turn_context: crate::TurnContext,
}

#[derive(Clone, Debug, Default)]
pub struct CheckpointApplication {
    pub messages: Vec<PluginMessage>,
    pub events: Vec<crate::SessionStreamEvent>,
    pub abort: Option<PluginAbort>,
}

#[derive(Clone, Debug)]
pub struct TurnFinalization {
    pub turn: AssembledTurn,
    pub events: Vec<crate::SessionStreamEvent>,
}

pub(crate) async fn emit_plugin_runtime_events(
    event_tx: &mpsc::Sender<crate::SessionStreamEvent>,
    plugin_id: &str,
    events: Vec<PluginRuntimeEvent>,
) {
    for event in plugin_runtime_session_events(plugin_id, events) {
        crate::session_model::send_event(event_tx, event).await;
    }
}

pub(crate) fn plugin_runtime_session_events(
    plugin_id: &str,
    events: Vec<PluginRuntimeEvent>,
) -> Vec<crate::SessionStreamEvent> {
    events
        .into_iter()
        .map(|event| crate::SessionStreamEvent::PluginEvent {
            plugin_id: plugin_id.to_string(),
            event,
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// justification: directives are transient public plugin values and the common short-circuit output avoids another allocation.
#[allow(clippy::large_enum_variant)]
pub enum PluginDirective {
    AbortTurn {
        code: String,
        message: String,
    },
    EnqueueMessages {
        messages: Vec<PluginMessage>,
    },
    CreateSession {
        request: Box<SessionCreateRequest>,
    },
    ReplaceToolArgs {
        args: serde_json::Value,
    },
    ShortCircuitTool {
        output: ToolCallOutput,
    },
    EmitRuntimeEvents {
        events: Vec<PluginRuntimeEvent>,
    },
    EmitTrace {
        name: String,
        #[serde(default)]
        payload: serde_json::Value,
        #[serde(default)]
        context: Box<lash_trace::TraceContext>,
    },
}

impl PluginDirective {
    pub fn short_circuit(result: ToolResult) -> Self {
        Self::ShortCircuitTool {
            output: result.into_done_output().unwrap_or_else(|_| {
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "pending_tool_short_circuit",
                    "plugin short-circuit directives require completed tool output",
                ))
            }),
        }
    }

    pub fn emit_runtime_events(events: Vec<PluginRuntimeEvent>) -> Self {
        Self::EmitRuntimeEvents { events }
    }

    pub fn emit_trace(name: impl Into<String>, payload: serde_json::Value) -> Self {
        Self::EmitTrace {
            name: name.into(),
            payload,
            context: Box::new(lash_trace::TraceContext::default()),
        }
    }
}
