use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;
use tokio::sync::mpsc;

use crate::plugin::{
    PluginSession, SessionGraphService, SessionLifecycleService, SessionStateService,
};
use crate::{
    PreparedToolCall, SessionStreamEvent, ToolCallRecord, ToolCatalog, ToolFailure,
    ToolFailureClass, ToolProvider, ToolResult,
};

#[derive(Clone, Default)]
pub(crate) struct CheckpointMessageBuffer {
    queue: Arc<Mutex<Vec<crate::PluginMessage>>>,
}

impl CheckpointMessageBuffer {
    pub(crate) fn enqueue(&self, messages: Vec<crate::PluginMessage>) {
        let mut queue = self.queue.lock_recover();
        queue.extend(messages);
    }

    pub(crate) fn drain(&self) -> Vec<crate::PluginMessage> {
        let mut queue = self.queue.lock_recover();
        queue.drain(..).collect()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolTriggerEffectOutcome {
    pub source_type: String,
    pub source_key: String,
    pub occurrence_id: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    pub deliveries: Vec<crate::TriggerDeliveryEmitReport>,
}

#[derive(Clone, Default)]
pub(crate) struct ToolTriggerOutcomeBuffer {
    queue: Arc<Mutex<Vec<ToolTriggerEffectOutcome>>>,
}

impl ToolTriggerOutcomeBuffer {
    pub(crate) fn enqueue(&self, outcome: ToolTriggerEffectOutcome) {
        let mut queue = self.queue.lock_recover();
        queue.push(outcome);
    }

    pub(crate) fn drain(&self) -> Vec<ToolTriggerEffectOutcome> {
        let mut queue = self.queue.lock_recover();
        queue.drain(..).collect()
    }
}

#[derive(Clone)]
pub struct ToolDispatchContext<'run> {
    pub plugins: Arc<PluginSession>,
    pub tools: Arc<dyn ToolProvider>,
    pub tool_catalog: Arc<ToolCatalog>,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
    pub trigger_router: Option<crate::TriggerRouter>,
    pub(crate) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub(crate) direct_completions: crate::DirectCompletionClient<'run>,
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub session_id: String,
    pub agent_frame_id: crate::AgentFrameId,
    pub event_tx: mpsc::Sender<SessionStreamEvent>,
    pub(crate) checkpoint_messages: CheckpointMessageBuffer,
    pub(crate) trigger_outcomes: ToolTriggerOutcomeBuffer,
    pub attachment_store: Arc<crate::SessionAttachmentStore>,
    pub attachment_source_policy: Arc<dyn crate::AttachmentSourcePolicy>,
    pub turn_context: crate::TurnContext,
    pub clock: Arc<dyn crate::Clock>,
}

impl<'run> ToolDispatchContext<'run> {
    pub fn process_scope(&self) -> crate::ProcessOpScope<'_> {
        crate::ProcessOpScope::new(self.effect_controller.scoped())
            .with_parent_invocation(self.parent_invocation.clone())
            .with_agent_frame_id(Some(self.agent_frame_id.clone()))
    }

    pub(crate) fn to_static(&self) -> Option<ToolDispatchContext<'static>> {
        Some(ToolDispatchContext {
            plugins: Arc::clone(&self.plugins),
            tools: Arc::clone(&self.tools),
            tool_catalog: Arc::clone(&self.tool_catalog),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            session_graph: Arc::clone(&self.session_graph),
            processes: Arc::clone(&self.processes),
            trigger_router: self.trigger_router.clone(),
            effect_controller: self.effect_controller.to_static()?,
            direct_completions: self.direct_completions.to_static()?,
            parent_invocation: self.parent_invocation.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            event_tx: self.event_tx.clone(),
            checkpoint_messages: self.checkpoint_messages.clone(),
            trigger_outcomes: self.trigger_outcomes.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            attachment_source_policy: Arc::clone(&self.attachment_source_policy),
            turn_context: self.turn_context.clone(),
            clock: Arc::clone(&self.clock),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToolDispatchOutcome {
    pub record: ToolCallRecord,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<lash_trace::TraceRetryAttempt>,
    #[serde(default, skip_serializing_if = "crate::ToolIntents::is_empty")]
    pub intents: crate::ToolIntents,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingToolDispatchOutcome {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub key: crate::AwaitEventKey,
    pub pending: crate::PendingCompletion,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<lash_trace::TraceRetryAttempt>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum ToolCallLaunch {
    Done(Box<ToolDispatchOutcome>),
    Pending(Box<PendingToolDispatchOutcome>),
}

pub(crate) enum ToolPreparationOutcome {
    Prepared(PreparedToolCall),
    Completed(Box<ToolDispatchOutcome>),
}

pub(super) fn completed_preparation(outcome: ToolDispatchOutcome) -> ToolPreparationOutcome {
    ToolPreparationOutcome::Completed(Box::new(outcome))
}
pub(super) fn outcome(
    tool_name: String,
    args: serde_json::Value,
    result: ToolResult,
    duration_ms: u64,
) -> ToolDispatchOutcome {
    let record = ToolCallRecord {
        call_id: None,
        tool: tool_name,
        args,
        output: result.into_done_output().unwrap_or_else(|_| {
            crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "pending_tool_not_finalized",
                "pending tool result reached a completed-output projection path",
            ))
        }),
        duration_ms,
    };
    ToolDispatchOutcome {
        record,
        attempts: Vec::new(),
        intents: crate::ToolIntents::default(),
        intent_outcomes: Vec::new(),
    }
}

pub(super) fn launch_done(outcome: ToolDispatchOutcome) -> ToolCallLaunch {
    ToolCallLaunch::Done(Box::new(outcome))
}

pub(super) fn runtime_failure(
    class: ToolFailureClass,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ToolResult {
    ToolResult::failure(ToolFailure::runtime(class, code, message))
}
