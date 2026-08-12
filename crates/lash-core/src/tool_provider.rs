use std::sync::{Arc, Mutex};

use crate::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::llm::types::ProviderReplayMeta;
use lash_sansio::sync::MutexExt;
use serde::{Deserialize, Serialize};

use crate::plugin::{
    PluginError, SessionGraphService, SessionLifecycleService, SessionSnapshot, SessionStateService,
};
use crate::{ToolContract, ToolDefinition, ToolId, ToolManifest, ToolResult};

mod attachments;
mod direct_completion;
mod dispatch;
mod process;
pub(crate) mod process_events;
mod session;
mod triggers;

pub use attachments::ToolAttachmentClient;
pub use direct_completion::ToolDirectCompletionClient;
pub use dispatch::ToolDispatchClient;
pub use process::ToolSessionProcessAdmin;
pub use process_events::ToolProcessEventClient;
pub use session::ToolSessionAdmin;
pub use triggers::ToolTriggerClient;

/// Read-only session capability available inside a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptSessionReads {
    session_id: String,
    sessions: Arc<dyn SessionStateService>,
}

impl AttemptSessionReads {
    pub async fn model(&self) -> Result<session::ToolSessionModel, PluginError> {
        let snapshot = self.snapshot_current().await?;
        let generation = snapshot
            .policy
            .model
            .clamped_generation(&snapshot.policy.generation);
        Ok(session::ToolSessionModel {
            model: snapshot.policy.model.id,
            model_variant: snapshot.policy.model.variant,
            model_capability: snapshot.policy.model.capability,
            generation,
        })
    }

    pub async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(&self.session_id).await
    }

    pub async fn snapshot(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(session_id.as_ref()).await
    }

    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        self.sessions.tool_catalog(&self.session_id).await
    }

    pub async fn shared_tool_catalog(&self) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        self.sessions.shared_tool_catalog(&self.session_id).await
    }
}

/// Controller-free process read capability available inside a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptProcessReads {
    session_id: String,
    processes: Arc<dyn crate::ProcessService>,
}

impl AttemptProcessReads {
    pub async fn list_handles_filtered(
        &self,
        filter: &crate::ProcessListFilter,
    ) -> Result<Vec<crate::ProcessHandleSummary>, PluginError> {
        Ok(self
            .processes
            .list_visible_for_attempt(&self.session_id, filter.list_mode())
            .await?
            .into_iter()
            .filter(|record| filter.matches_record(record))
            .map(crate::ProcessHandleSummary::from_record)
            .collect())
    }
}

/// Sealed, controller-free environment for a recorded leaf attempt.
#[derive(Clone)]
pub struct AttemptContext<'run> {
    session_id: String,
    turn_id: String,
    agent_frame_id: crate::AgentFrameId,
    sessions: AttemptSessionReads,
    processes: AttemptProcessReads,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    async_process_id: Option<String>,
    runtime_process_id: Option<String>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    direct_completions: crate::DirectCompletionClient<'run>,
    provider: Option<crate::ProviderHandle>,
    prepared_payload: serde_json::Value,
    tool_execution_binding: serde_json::Value,
    tool_call_id: Option<String>,
    attempt_number: u32,
    max_attempts: u32,
    replay_key: Option<String>,
    completion_key: Option<crate::AwaitEventKey>,
    completion_supported: bool,
    phase_probe: Option<Arc<dyn crate::runtime::RuntimeTurnPhaseProbe>>,
}

impl<'run> AttemptContext<'run> {
    pub(crate) fn from_tool_context(
        context: &ToolContext<'run>,
        turn_id: String,
        completion_key: Option<crate::AwaitEventKey>,
        completion_supported: bool,
    ) -> Self {
        let phase_probe = context
            .runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::attempt_phase_probe);
        let provider = context
            .runtime_dispatch
            .as_ref()
            .and_then(|dispatch| dispatch.turn_context.provider().cloned());
        Self {
            session_id: context.session_id.clone(),
            turn_id,
            agent_frame_id: context.agent_frame_id.clone(),
            sessions: AttemptSessionReads {
                session_id: context.session_id.clone(),
                sessions: Arc::clone(&context.sessions),
            },
            processes: AttemptProcessReads {
                session_id: context.session_id.clone(),
                processes: Arc::clone(&context.processes),
            },
            cancellation_token: context.cancellation_token.clone(),
            async_process_id: context.async_process_id.clone(),
            runtime_process_id: context.runtime_process_id.clone(),
            attachment_store: Arc::clone(&context.attachment_store),
            direct_completions: context.direct_completions.clone(),
            provider,
            prepared_payload: context.prepared_payload.clone(),
            tool_execution_binding: context.tool_execution_binding.clone(),
            tool_call_id: context.tool_call_id.clone(),
            attempt_number: context.attempt_number,
            max_attempts: context.max_attempts,
            replay_key: context.replay_key.clone(),
            completion_key,
            completion_supported,
            phase_probe,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }
    pub fn agent_frame_id(&self) -> &str {
        &self.agent_frame_id
    }
    pub fn sessions(&self) -> AttemptSessionReads {
        self.sessions.clone()
    }
    pub fn processes(&self) -> AttemptProcessReads {
        self.processes.clone()
    }
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }
    pub fn async_process_id(&self) -> Option<&str> {
        self.async_process_id.as_deref()
    }
    pub fn runtime_process_id(&self) -> Option<&str> {
        self.runtime_process_id.as_deref()
    }
    pub fn attachments(&self) -> ToolAttachmentClient {
        ToolAttachmentClient {
            store: Arc::clone(&self.attachment_store),
        }
    }
    pub fn direct_completions(&self) -> ToolDirectCompletionClient<'run> {
        ToolDirectCompletionClient {
            session_id: self.session_id.clone(),
            tool_call_id: self.tool_call_id.clone(),
            direct_completions: self.direct_completions.clone(),
            parent_invocation: None,
        }
    }
    pub fn provider(&self) -> Option<&crate::ProviderHandle> {
        self.provider.as_ref()
    }
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.prepared_payload
    }
    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }
    pub fn replay_key(&self) -> Option<&str> {
        self.replay_key.as_deref()
    }
    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.prepared_payload.clone())
    }
    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        crate::runtime::RuntimeNamedPhase::begin(self.phase_probe.clone(), phase)
    }
    pub fn completion_key(&self) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        if !self.completion_supported {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
                "completion keys require an effect controller with process-loss-safe await-event routing",
            ));
        }
        self.completion_key.clone().ok_or_else(|| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyMissingCallId,
                "completion keys require a prepared tool call id",
            )
        })
    }
    pub fn intent_identity(
        &self,
        intent_index: usize,
    ) -> Result<crate::ToolIntentIdentity, crate::ToolIntentRefusalReason> {
        crate::derive_tool_intent_identity(
            &self.session_id,
            &self.turn_id,
            self.tool_call_id.as_deref(),
            intent_index,
        )
    }
}

#[derive(Clone, Default)]
pub(crate) struct ToolCompletionState {
    key: Arc<Mutex<Option<crate::AwaitEventKey>>>,
}

impl ToolCompletionState {
    pub(crate) fn store(
        &self,
        key: crate::AwaitEventKey,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        let mut guard = self.key.lock_recover();
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        *guard = Some(key.clone());
        Ok(key)
    }

    pub(crate) fn take(&self) -> Option<crate::AwaitEventKey> {
        self.key.lock_recover().take()
    }

    pub(crate) fn load(&self) -> Option<crate::AwaitEventKey> {
        self.key.lock_recover().clone()
    }
}

/// Per-call environment for [`ToolProvider::execute`]. Fields are sealed so
/// the runtime can add capabilities without breaking tool authors.
#[derive(Clone)]
pub struct ToolContext<'run> {
    pub(crate) session_id: String,
    pub(crate) agent_frame_id: crate::AgentFrameId,
    pub(crate) sessions: Arc<dyn SessionStateService>,
    pub(crate) session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub(crate) processes: Arc<dyn crate::ProcessService>,
    pub(crate) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    pub(crate) runtime_dispatch: Option<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>>,
    pub(crate) runtime_execution_context: Option<crate::RuntimeExecutionContext<'run>>,
    pub(crate) cancellation_token: Option<tokio_util::sync::CancellationToken>,
    pub(crate) async_process_id: Option<String>,
    pub(crate) runtime_process_id: Option<String>,
    pub(crate) process_events: Option<ToolProcessEventContext>,
    pub(crate) attachment_store: Arc<crate::SessionAttachmentStore>,
    pub(crate) direct_completions: crate::DirectCompletionClient<'run>,
    pub(crate) prepared_payload: serde_json::Value,
    pub(crate) tool_execution_binding: serde_json::Value,
    /// The id of the in-flight tool call that is invoking this tool.
    pub(crate) tool_call_id: Option<String>,
    pub(crate) attempt_number: u32,
    pub(crate) max_attempts: u32,
    pub(crate) replay_key: Option<String>,
    pub(crate) completion: ToolCompletionState,
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) execution_env_spec: crate::ProcessExecutionEnvSpec,
    pub(crate) child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
}

#[derive(Clone)]
pub struct ToolChildProcessStarted {
    pub process_id: String,
    pub child_entry_name: Option<String>,
}

#[derive(Clone)]
pub struct ToolChildExecutionTraceHook {
    on_child_process_started: Arc<dyn Fn(ToolChildProcessStarted) + Send + Sync>,
}

impl ToolChildExecutionTraceHook {
    pub fn new(
        on_child_process_started: impl Fn(ToolChildProcessStarted) + Send + Sync + 'static,
    ) -> Self {
        Self {
            on_child_process_started: Arc::new(on_child_process_started),
        }
    }

    pub fn child_process_started(&self, event: ToolChildProcessStarted) {
        (self.on_child_process_started)(event);
    }
}

#[derive(Clone)]
pub(crate) struct ToolProcessEventContext {
    process_id: String,
    execution_write_authority: crate::ProcessExecutionWriteAuthority,
    registry: Arc<dyn crate::ProcessRegistry>,
    awaiter: crate::ProcessAwaiter,
    store: Option<Arc<dyn crate::RuntimePersistence>>,
    session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
    session_graph: Arc<dyn SessionGraphService>,
    queued_work_driver: Option<crate::QueuedWorkDriver>,
    process_wake_delivery_policy: crate::DeliveryPolicy,
    clock: Arc<dyn crate::Clock>,
}

pub(crate) struct ToolContextBuilder<'run> {
    session_id: String,
    agent_frame_id: crate::AgentFrameId,
    sessions: Arc<dyn SessionStateService>,
    session_lifecycle: Arc<dyn SessionLifecycleService>,
    session_graph: Arc<dyn SessionGraphService>,
    processes: Arc<dyn crate::ProcessService>,
    effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
    runtime_dispatch: Option<Arc<crate::tool_dispatch::ToolDispatchContext<'run>>>,
    runtime_execution_context: Option<crate::RuntimeExecutionContext<'run>>,
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
    async_process_id: Option<String>,
    runtime_process_id: Option<String>,
    process_events: Option<ToolProcessEventContext>,
    attachment_store: Arc<crate::SessionAttachmentStore>,
    direct_completions: crate::DirectCompletionClient<'run>,
    prepared_payload: serde_json::Value,
    tool_execution_binding: serde_json::Value,
    tool_call_id: Option<String>,
    completion: ToolCompletionState,
    parent_invocation: Option<crate::RuntimeInvocation>,
    execution_env_spec: crate::ProcessExecutionEnvSpec,
    child_execution_trace_hook: Option<ToolChildExecutionTraceHook>,
}

impl<'run> ToolContextBuilder<'run> {
    pub(crate) fn from_dispatch(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    ) -> Self {
        Self {
            session_id: dispatch.session_id.clone(),
            agent_frame_id: dispatch.agent_frame_id.clone(),
            sessions: Arc::clone(&dispatch.sessions),
            session_lifecycle: Arc::clone(&dispatch.session_lifecycle),
            session_graph: Arc::clone(&dispatch.session_graph),
            processes: Arc::clone(&dispatch.processes),
            effect_controller: dispatch.effect_controller.clone(),
            runtime_dispatch: Some(Arc::clone(&dispatch)),
            runtime_execution_context: None,
            cancellation_token: None,
            async_process_id: None,
            runtime_process_id: None,
            process_events: None,
            attachment_store: Arc::clone(&dispatch.attachment_store),
            direct_completions: dispatch.direct_completions.clone(),
            prepared_payload: serde_json::Value::Null,
            tool_execution_binding: serde_json::Value::Null,
            tool_call_id: None,
            completion: ToolCompletionState::default(),
            parent_invocation: dispatch.parent_invocation.clone(),
            execution_env_spec: dispatch.execution_env_spec.clone(),
            child_execution_trace_hook: None,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn tool_call_id(mut self, tool_call_id: impl Into<Option<String>>) -> Self {
        self.tool_call_id = tool_call_id.into();
        self
    }

    pub(crate) fn prepared_call(mut self, call: &PreparedToolCall) -> Self {
        self.tool_call_id = Some(call.call_id.clone());
        self.prepared_payload = call.prepared_payload.clone();
        self
    }

    #[cfg(test)]
    pub(crate) fn tool_execution_binding(mut self, binding: serde_json::Value) -> Self {
        self.tool_execution_binding = binding;
        self
    }

    pub(crate) fn cancellation_token(
        mut self,
        cancellation_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Self {
        self.cancellation_token = cancellation_token;
        self
    }

    pub(crate) fn runtime_execution_context(
        mut self,
        context: crate::RuntimeExecutionContext<'run>,
    ) -> Self {
        self.runtime_execution_context = Some(context);
        self
    }

    pub(crate) fn runtime_process_id(mut self, process_id: Option<String>) -> Self {
        self.runtime_process_id = process_id;
        self
    }

    pub(crate) fn async_process(
        mut self,
        process_id: impl Into<String>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        self.async_process_id = Some(process_id.into());
        self.cancellation_token = Some(cancellation_token);
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process_events(
        mut self,
        process_id: impl Into<String>,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        registry: Arc<dyn crate::ProcessRegistry>,
        awaiter: crate::ProcessAwaiter,
        store: Option<Arc<dyn crate::RuntimePersistence>>,
        session_store_factory: Option<Arc<dyn crate::SessionStoreFactory>>,
        queued_work_driver: Option<crate::QueuedWorkDriver>,
        process_wake_delivery_policy: crate::DeliveryPolicy,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        self.process_events = Some(ToolProcessEventContext {
            process_id: process_id.into(),
            execution_write_authority,
            registry,
            awaiter,
            store,
            session_store_factory,
            session_graph: Arc::clone(&self.session_graph),
            queued_work_driver,
            process_wake_delivery_policy,
            clock,
        });
        self
    }

    pub(crate) fn parent_invocation(mut self, metadata: Option<crate::RuntimeInvocation>) -> Self {
        self.parent_invocation = metadata;
        self
    }

    pub(crate) fn child_execution_trace_hook(
        mut self,
        hook: Option<ToolChildExecutionTraceHook>,
    ) -> Self {
        self.child_execution_trace_hook = hook;
        self
    }

    pub(crate) fn build(self) -> ToolContext<'run> {
        ToolContext {
            session_id: self.session_id,
            agent_frame_id: self.agent_frame_id,
            sessions: self.sessions,
            session_lifecycle: self.session_lifecycle,
            processes: self.processes,
            effect_controller: self.effect_controller,
            runtime_dispatch: self.runtime_dispatch,
            runtime_execution_context: self.runtime_execution_context,
            cancellation_token: self.cancellation_token,
            async_process_id: self.async_process_id,
            runtime_process_id: self.runtime_process_id,
            process_events: self.process_events,
            attachment_store: self.attachment_store,
            direct_completions: self.direct_completions,
            prepared_payload: self.prepared_payload,
            tool_execution_binding: self.tool_execution_binding,
            tool_call_id: self.tool_call_id,
            attempt_number: 1,
            max_attempts: 1,
            replay_key: None,
            completion: self.completion,
            parent_invocation: self.parent_invocation,
            execution_env_spec: self.execution_env_spec,
            child_execution_trace_hook: self.child_execution_trace_hook,
        }
    }
}

impl<'run> ToolContext<'run> {
    pub(crate) fn install_prederived_completion_key(&self, key: Option<crate::AwaitEventKey>) {
        if let Some(key) = key {
            let _ = self.completion.store(key);
        }
    }
    pub(crate) fn replay_validation_trace(&self) -> Option<crate::RuntimeEffectReplayTrace> {
        self.runtime_execution_context
            .as_ref()
            .and_then(crate::RuntimeExecutionContext::replay_validation_trace)
    }

    pub(crate) fn to_static(&self) -> Option<ToolContext<'static>> {
        Some(ToolContext {
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            processes: Arc::clone(&self.processes),
            effect_controller: self.effect_controller.to_static()?,
            runtime_dispatch: match self.runtime_dispatch.as_ref() {
                Some(dispatch) => Some(Arc::new(dispatch.to_static()?)),
                None => None,
            },
            runtime_execution_context: match self.runtime_execution_context.as_ref() {
                Some(context) => Some(context.to_static()?),
                None => None,
            },
            cancellation_token: self.cancellation_token.clone(),
            async_process_id: self.async_process_id.clone(),
            runtime_process_id: self.runtime_process_id.clone(),
            process_events: self.process_events.clone(),
            attachment_store: Arc::clone(&self.attachment_store),
            direct_completions: self.direct_completions.to_static()?,
            prepared_payload: self.prepared_payload.clone(),
            tool_execution_binding: self.tool_execution_binding.clone(),
            tool_call_id: self.tool_call_id.clone(),
            attempt_number: self.attempt_number,
            max_attempts: self.max_attempts,
            replay_key: self.replay_key.clone(),
            completion: self.completion.clone(),
            parent_invocation: self.parent_invocation.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
            child_execution_trace_hook: self.child_execution_trace_hook.clone(),
        })
    }

    #[cfg(any(test, feature = "testing"))]
    #[expect(
        clippy::too_many_arguments,
        reason = "testing constructor mirrors the sealed runtime tool context dependencies"
    )]
    pub(crate) fn builder(
        session_id: String,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        effect_controller: crate::runtime::RuntimeEffectControllerHandle<'run>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        direct_completions: crate::DirectCompletionClient<'run>,
    ) -> ToolContextBuilder<'run> {
        ToolContextBuilder {
            session_id,
            agent_frame_id: String::new(),
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            effect_controller,
            runtime_dispatch: None,
            runtime_execution_context: None,
            cancellation_token: None,
            async_process_id: None,
            runtime_process_id: None,
            process_events: None,
            attachment_store,
            direct_completions,
            prepared_payload: serde_json::Value::Null,
            tool_execution_binding: serde_json::Value::Null,
            tool_call_id: None,
            completion: ToolCompletionState::default(),
            parent_invocation: None,
            execution_env_spec: crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            ),
            child_execution_trace_hook: None,
        }
    }

    pub(crate) fn from_dispatch(
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
    ) -> ToolContextBuilder<'run> {
        ToolContextBuilder::from_dispatch(dispatch)
    }

    /// Exposes session id to protocol and process-engine implementors while preparing or executing
    /// an authorized tool call.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Exposes agent frame id to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn agent_frame_id(&self) -> &str {
        &self.agent_frame_id
    }

    /// Overrides the current frame lineage in an isolated tool-provider test.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn with_agent_frame_id_for_testing(
        mut self,
        agent_frame_id: impl Into<crate::AgentFrameId>,
    ) -> Self {
        self.agent_frame_id = agent_frame_id.into();
        self
    }

    /// Exposes sessions to protocol and process-engine implementors while preparing or executing an
    /// authorized tool call.
    pub fn sessions(&self) -> ToolSessionAdmin<'run> {
        ToolSessionAdmin {
            session_id: self.session_id.clone(),
            sessions: Arc::clone(&self.sessions),
            session_lifecycle: Arc::clone(&self.session_lifecycle),
            effect_controller: self.effect_controller.clone(),
            parent_invocation: self.parent_invocation.clone(),
        }
    }

    /// Exposes dispatch to protocol and process-engine implementors while preparing or executing an
    /// authorized tool call.
    pub fn dispatch(&self) -> ToolDispatchClient<'run> {
        ToolDispatchClient {
            context: self.clone(),
        }
    }

    /// Exposes triggers to protocol and process-engine implementors while preparing or executing an
    /// authorized tool call.
    pub fn triggers(&self) -> ToolTriggerClient<'run> {
        ToolTriggerClient {
            context: self.clone(),
        }
    }

    /// Provides process lifecycle operations to tool implementors under the call's session and
    /// execution scopes.
    pub fn processes(&self) -> ToolSessionProcessAdmin<'run> {
        ToolSessionProcessAdmin {
            session_id: self.session_id.clone(),
            agent_frame_id: self.agent_frame_id.clone(),
            processes: Arc::clone(&self.processes),
            effect_controller: self.effect_controller.clone(),
            parent_invocation: self.parent_invocation.clone(),
            tool_call_id: self.tool_call_id.clone(),
            execution_env_spec: self.execution_env_spec.clone(),
        }
    }

    /// Exposes emit child process started to protocol and process-engine implementors while
    /// preparing or executing an authorized tool call.
    pub fn emit_child_process_started(
        &self,
        process_id: impl Into<String>,
        child_entry_name: Option<String>,
    ) {
        let Some(hook) = &self.child_execution_trace_hook else {
            return;
        };
        hook.child_process_started(ToolChildProcessStarted {
            process_id: process_id.into(),
            child_entry_name,
        });
    }

    /// Exposes direct completions to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn direct_completions(&self) -> ToolDirectCompletionClient<'run> {
        ToolDirectCompletionClient {
            session_id: self.session_id.clone(),
            tool_call_id: self.tool_call_id.clone(),
            direct_completions: self.direct_completions.clone(),
            parent_invocation: self.parent_invocation.clone(),
        }
    }

    /// Provides session-scoped attachment operations to tool implementors so tool-produced blobs
    /// participate in durable intent and retention tracking.
    pub fn attachments(&self) -> ToolAttachmentClient {
        ToolAttachmentClient {
            store: Arc::clone(&self.attachment_store),
        }
    }

    /// Exposes process events to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn process_events(&self) -> ToolProcessEventClient {
        ToolProcessEventClient {
            context: self.process_events.clone(),
        }
    }

    /// Exposes cooperative cancellation to tool implementors, returning `None` when the execution
    /// boundary supplied no cancellation scope.
    pub fn cancellation_token(&self) -> Option<&tokio_util::sync::CancellationToken> {
        self.cancellation_token.as_ref()
    }

    #[doc(hidden)]
    pub fn named_phase(&self, phase: &'static str) -> crate::runtime::RuntimeNamedPhase {
        match self.runtime_execution_context.as_ref() {
            Some(context) => context.named_phase(phase),
            None => crate::runtime::RuntimeNamedPhase::begin(None, phase),
        }
    }

    /// Exposes async process id to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call. Returns `None` when no async process id is present.
    pub fn async_process_id(&self) -> Option<&str> {
        self.async_process_id.as_deref()
    }

    /// Exposes runtime process id to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call. Returns `None` when no runtime process id is present.
    pub fn runtime_process_id(&self) -> Option<&str> {
        self.async_process_id
            .as_deref()
            .or(self.runtime_process_id.as_deref())
            .or_else(|| {
                self.process_events
                    .as_ref()
                    .map(|context| context.process_id.as_str())
            })
    }

    /// Exposes tool call id to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call. Returns `None` when no tool call id is present.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    /// Exposes prepared payload to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.prepared_payload
    }

    /// Exposes tool execution binding to protocol and process-engine implementors while preparing
    /// or executing an authorized tool call.
    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }

    /// Deserializes the frozen prepared payload for tool implementors without consulting mutable
    /// plugin or provider state.
    pub fn decode_prepared_payload<T>(&self) -> Result<T, serde_json::Error>
    where
        T: serde::de::DeserializeOwned,
    {
        serde_json::from_value(self.prepared_payload.clone())
    }

    /// Current one-based attempt number for tool implementors handling this call.
    pub fn attempt_number(&self) -> u32 {
        self.attempt_number
    }

    /// Exposes max attempts to protocol and process-engine implementors while preparing or
    /// executing an authorized tool call.
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// Exposes the durable replay key to tool implementors, returning `None` for calls that are not
    /// replay-scoped.
    pub fn replay_key(&self) -> Option<&str> {
        self.replay_key.as_deref()
    }

    /// Obtain the durable completion key for this call, required before returning
    /// [`ToolResult::Pending`](crate::ToolResult::Pending).
    ///
    /// A tool that defers its outcome (waiting on a webhook, human approval, or another
    /// service) calls this, hands the returned [`AwaitEventKey`](crate::AwaitEventKey)
    /// to whatever will complete the work out-of-band, and then returns
    /// `ToolResult::Pending(..)`. The key names the durable wait the runtime parks the
    /// call on; the external resolver delivers the result against it later.
    ///
    /// The key is stored on the context and consumed by the dispatcher when the tool
    /// returns `Pending`. Returning `Pending` without first calling this fails the call
    /// with `pending_tool_missing_completion_key`. Calls made outside a prepared tool
    /// invocation (no tool call id) fail with `tool_completion_key_missing_call_id`.
    pub async fn completion_key(&self) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        let tool_call_id = self.tool_call_id.clone().ok_or_else(|| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyMissingCallId,
                "completion keys require a prepared tool call id",
            )
        })?;
        let scoped = self.effect_controller.scoped();
        if !scoped
            .controller()
            .allows_process_lifetime_completion_keys()
        {
            return Err(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
                "completion keys require an effect controller with process-loss-safe await-event routing; single-process deployments may explicitly opt in with InlineEffectHost::allow_process_lifetime_completion_keys()",
            ));
        }
        let key = scoped
            .controller()
            .await_event_key(
                scoped.execution_scope(),
                crate::AwaitEventWaitIdentity::tool_completion(tool_call_id),
            )
            .await?;
        self.completion.store(key)
    }

    pub(crate) fn take_completion_key(&self) -> Option<crate::AwaitEventKey> {
        self.completion.take()
    }

    /// Sets the async process carried by a `ToolContext` for protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    pub fn with_async_process(
        mut self,
        process_id: impl Into<String>,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Self {
        self.async_process_id = Some(process_id.into());
        self.runtime_process_id = self.async_process_id.clone();
        self.cancellation_token = Some(cancellation_token);
        self
    }

    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn with_process_events_for_testing(
        mut self,
        process_id: impl Into<String>,
        registry: Arc<dyn crate::ProcessRegistry>,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
    ) -> Self {
        let process_id = process_id.into();
        let awaiter = crate::ProcessAwaiter::polling(Arc::clone(&registry));
        self.process_events = Some(ToolProcessEventContext {
            execution_write_authority,
            process_id,
            registry,
            awaiter,
            store: None,
            session_store_factory: None,
            session_graph: Arc::new(crate::plugin::NoopSessionManager),
            queued_work_driver: None,
            process_wake_delivery_policy: crate::DeliveryPolicy::EarliestSafeBoundary,
            clock: Arc::new(crate::SystemClock),
        });
        self
    }

    pub(crate) fn with_retry_context(
        mut self,
        tool_name: &str,
        attempt_number: u32,
        max_attempts: u32,
    ) -> Self {
        self.attempt_number = attempt_number.max(1);
        self.max_attempts = max_attempts.max(1);
        self.replay_key = self
            .tool_call_id
            .as_ref()
            .map(|call_id| format!("lash-tool:{}:{call_id}:{tool_name}", self.session_id));
        self
    }

    pub(crate) fn with_prepared_payload(mut self, payload: serde_json::Value) -> Self {
        self.prepared_payload = payload;
        self
    }

    pub(crate) fn with_tool_execution_binding(mut self, binding: serde_json::Value) -> Self {
        self.tool_execution_binding = binding;
        self
    }

    pub(crate) fn with_attempt_dispatch(
        mut self,
        dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'run>>,
        parent_invocation: crate::RuntimeInvocation,
    ) -> Self {
        self.effect_controller = dispatch.effect_controller.clone();
        self.runtime_dispatch = Some(dispatch);
        self.parent_invocation = Some(parent_invocation);
        self
    }

    /// Constructor reserved for `lash_core::testing` helpers. Do not call directly;
    /// use [`lash_core::testing::mock_tool_context`] instead.
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    #[expect(
        clippy::too_many_arguments,
        reason = "test-only constructor mirrors the sealed runtime tool context"
    )]
    pub fn __for_testing(
        session_id: String,
        sessions: Arc<dyn SessionStateService>,
        session_lifecycle: Arc<dyn SessionLifecycleService>,
        session_graph: Arc<dyn SessionGraphService>,
        processes: Arc<dyn crate::ProcessService>,
        attachment_store: Arc<crate::SessionAttachmentStore>,
        direct_completions: crate::DirectCompletionClient<'static>,
        tool_call_id: Option<String>,
    ) -> ToolContext<'static> {
        ToolContext::builder(
            session_id,
            sessions,
            session_lifecycle,
            session_graph,
            processes,
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default()
                    .allow_process_lifetime_completion_keys(),
            )),
            attachment_store,
            direct_completions,
        )
        .tool_call_id(tool_call_id)
        .build()
    }
}

/// Runtime-prepared executable tool call.
///
/// The raw model/provider identity remains visible, but any argument rewrites
/// and provider-owned context projections are frozen before the call crosses a
/// runtime effect or process boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedToolCall {
    pub call_id: String,
    pub tool_id: ToolId,
    pub tool_name: String,
    pub args: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ProviderReplayMeta>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub prepared_payload: serde_json::Value,
}

impl PreparedToolCall {
    /// Freezes an identity preparation for protocol and process-engine implementors without
    /// rewriting the model-supplied call arguments or provider metadata.
    pub fn identity(tool_id: ToolId, call: crate::sansio::PendingToolCall) -> Self {
        Self {
            call_id: call.call_id,
            tool_id,
            tool_name: call.tool_name,
            args: call.args,
            replay: call.replay,
            prepared_payload: serde_json::Value::Null,
        }
    }

    /// Reconstructs a fully prepared call for protocol and process-engine implementors crossing an
    /// effect or process boundary, preserving the supplied replay metadata and prepared payload.
    pub fn from_parts(
        call_id: impl Into<String>,
        tool_id: impl Into<ToolId>,
        tool_name: impl Into<String>,
        args: serde_json::Value,
        replay: Option<ProviderReplayMeta>,
        prepared_payload: serde_json::Value,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool_id: tool_id.into(),
            tool_name: tool_name.into(),
            args,
            replay,
            prepared_payload,
        }
    }
}

/// One ordered child inside a runtime-prepared tool batch.
///
/// The call itself carries the executable provider payload. `replay_suffix`
/// is the deterministic suffix used for child effects such as retry sleeps or
/// pending completion awaits when the batch is the durable parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedToolBatchCall {
    pub call: PreparedToolCall,
    pub replay_suffix: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_grant: Option<Box<ToolExecutionGrant>>,
}

/// Runtime-prepared executable tool batch.
///
/// The vector order is source order. Calls run concurrently, but launches and
/// pending completion consumption are projected back through this order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedToolBatch {
    pub batch_id: String,
    pub calls: Vec<PreparedToolBatchCall>,
}

impl PreparedToolBatch {
    /// Freezes source-order prepared calls for protocol and process-engine implementors; execution
    /// may be concurrent, but launch and completion projection retain this order.
    pub fn new(batch_id: impl Into<String>, calls: Vec<PreparedToolCall>) -> Self {
        let batch_id = batch_id.into();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| PreparedToolBatchCall {
                replay_suffix: format!("child:{index}:{}", call.call_id),
                call,
                execution_grant: None,
            })
            .collect();
        Self { batch_id, calls }
    }

    pub(crate) fn new_with_grants(
        batch_id: impl Into<String>,
        calls: Vec<(PreparedToolCall, Option<ToolExecutionGrant>)>,
    ) -> Self {
        let batch_id = batch_id.into();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, (call, execution_grant))| PreparedToolBatchCall {
                replay_suffix: format!("child:{index}:{}", call.call_id),
                call,
                execution_grant: execution_grant.map(Box::new),
            })
            .collect();
        Self { batch_id, calls }
    }
}

/// Explicit authority to execute a tool outside Tool Catalog membership.
///
/// Normal tool calls are authorized by catalog membership. A grant is a
/// separate, caller-provided capability used by deferred resolution flows: it
/// carries the manifest/contract to validate the call plus an opaque host
/// execution binding that providers can inspect from the prepare and execute
/// contexts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolExecutionGrant {
    /// Tool identity and model-facing metadata authorized by the grant.
    pub manifest: ToolManifest,
    /// Contract used to validate granted call arguments without consulting the
    /// current Tool Catalog.
    pub contract: Box<ToolContract>,
    /// Explicit registry source route for registry-backed execution. Direct
    /// non-registry providers may ignore this; [`ToolRegistry`](crate::ToolRegistry)
    /// requires it.
    pub source_id: Option<String>,
    /// Opaque host routing payload passed to prepare and execute contexts.
    pub execution_binding: serde_json::Value,
}

impl ToolExecutionGrant {
    /// Constructs explicit out-of-catalog execution authority for protocol and process-engine
    /// implementors handling deferred-resolution flows.
    pub fn new(manifest: ToolManifest, contract: ToolContract) -> Self {
        Self {
            manifest,
            contract: Box::new(contract),
            source_id: None,
            execution_binding: serde_json::Value::Null,
        }
    }

    /// Constructs out-of-catalog execution authority from one tool definition for protocol and
    /// process-engine implementors handling deferred-resolution flows.
    pub fn from_definition(definition: ToolDefinition) -> Self {
        Self::new(definition.manifest(), definition.contract())
    }

    /// Sets the source id carried by a `ToolExecutionGrant` for protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    /// Sets the execution binding carried by a `ToolExecutionGrant` for protocol and process-engine
    /// implementors while preparing or executing an authorized tool call.
    pub fn with_execution_binding(mut self, execution_binding: serde_json::Value) -> Self {
        self.execution_binding = execution_binding;
        self
    }
}

#[derive(Clone)]
pub struct ToolPrepareContext {
    session_id: String,
    sessions: Arc<dyn SessionStateService>,
    turn_context: crate::TurnContext,
    tool_call_id: Option<String>,
    tool_execution_binding: serde_json::Value,
}

impl ToolPrepareContext {
    pub(crate) fn with_execution_binding(
        session_id: String,
        sessions: Arc<dyn SessionStateService>,
        turn_context: crate::TurnContext,
        tool_call_id: Option<String>,
        tool_execution_binding: serde_json::Value,
    ) -> Self {
        Self {
            session_id,
            sessions,
            turn_context,
            tool_call_id,
            tool_execution_binding,
        }
    }

    /// Exposes session id to protocol and process-engine implementors while preparing or executing
    /// plugin and tool work.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Exposes tool call id to protocol and process-engine implementors while preparing or
    /// executing plugin and tool work. Returns `None` when no tool call id is present.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    /// Exposes tool execution binding to protocol and process-engine implementors while preparing
    /// or executing plugin and tool work.
    pub fn tool_execution_binding(&self) -> &serde_json::Value {
        &self.tool_execution_binding
    }

    /// Exposes turn context to protocol and process-engine implementors while preparing or
    /// executing plugin and tool work.
    pub fn turn_context(&self) -> &crate::TurnContext {
        &self.turn_context
    }

    /// Exposes plugin input to protocol and process-engine implementors while preparing or
    /// executing plugin and tool work. Returns `None` when no plugin input is present.
    pub fn plugin_input<T>(&self, plugin_id: &'static str) -> Option<&T>
    where
        T: 'static,
    {
        self.turn_context.plugin_input::<T>(plugin_id)
    }

    /// Snapshots the current session for protocol and tool implementors preparing an authorized
    /// call; failures preserve the plugin error contract.
    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions.snapshot_session(&self.session_id).await
    }

    /// Exposes tool catalog to protocol and process-engine implementors while preparing or
    /// executing plugin and tool work.
    pub async fn tool_catalog(&self) -> Result<Vec<serde_json::Value>, PluginError> {
        self.sessions.tool_catalog(&self.session_id).await
    }

    /// Returns the shared canonical catalog snapshot for protocol and tool implementors that
    /// prepare multiple calls without rebuilding the projection.
    pub async fn shared_tool_catalog(
        &self,
    ) -> Result<std::sync::Arc<Vec<serde_json::Value>>, PluginError> {
        self.sessions.shared_tool_catalog(&self.session_id).await
    }
}

/// Inputs handed to [`ToolProvider::prepare_tool_call`].
pub struct ToolPrepareCall<'a> {
    pub tool_id: ToolId,
    pub pending: crate::sansio::PendingToolCall,
    pub context: &'a ToolPrepareContext,
}

/// Per-call inputs handed to [`ToolProvider::execute`].
///
/// Fields are `pub` because `ToolCall` is a transient borrow; consumers
/// typically destructure (`let ToolCall { name, args, .. } = call`). The
/// stable surface lives on [`ToolContext`] (sealed) and the runtime's
/// dispatcher, which constructs `ToolCall` values.
pub struct ToolCall<'a> {
    pub name: &'a str,
    pub args: &'a serde_json::Value,
    pub context: &'a ToolContext<'a>,
}

/// Per-call inputs handed to a leaf provider that opts into the controller-free attempt shape.
pub struct AttemptToolCall<'a> {
    pub name: &'a str,
    pub args: &'a serde_json::Value,
    pub context: &'a AttemptContext<'a>,
}

/// Trait for providing tools to the sandbox. Implement this per-project.
///
/// Implementations supply cheap [`ToolManifest`]s, lazily resolved
/// [`ToolContract`]s, and a single
/// [`execute`](Self::execute) method that handles every call. Tools that
/// need session state read it from `call.context`.
///
/// Lash contains an `execute` panic as a typed call failure. Containment does
/// not establish that the host object's own interior-mutability state still
/// satisfies its invariants; hosts own replacement or repair before reuse.
#[async_trait::async_trait]
pub trait ToolProvider: Send + Sync + 'static {
    fn tool_manifests(&self) -> Vec<ToolManifest>;
    fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
        self.tool_manifests()
            .into_iter()
            .find(|manifest| manifest.name == name)
    }
    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.tool_manifests()
            .into_iter()
            .find(|manifest| manifest.id == *id)
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>>;
    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        let manifest = self.resolve_manifest_by_id(id)?;
        self.resolve_contract(&manifest.name)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }
    async fn prepare_granted_tool_call(
        &self,
        grant: &ToolExecutionGrant,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolResult> {
        let _ = call;
        Err(ToolResult::err_fmt(format_args!(
            "Granted execution is unsupported for tool id `{}`",
            grant.manifest.id
        )))
    }
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult;
    /// Whether this tool id uses the leaf attempt signature. The legacy
    /// `ToolContext` path remains only while FIG-1293/1294 migrate and delete
    /// the remaining orchestrating surfaces in the aggregate cutover.
    fn supports_attempt_context(&self, _tool_id: &ToolId) -> bool {
        false
    }
    async fn execute_attempt(&self, call: AttemptToolCall<'_>) -> crate::ToolAttemptResult {
        crate::ToolAttemptResult::without_intents(ToolResult::failure(crate::ToolFailure {
            class: crate::ToolFailureClass::Unavailable,
            code: "tool_attempt_context_not_implemented".to_string(),
            message: format!(
                "tool `{}` has not implemented the leaf AttemptContext signature",
                call.name
            ),
            source: crate::ToolFailureSource::Runtime,
            retry: crate::ToolRetryDisposition::Never,
            raw: None,
        }))
    }
    async fn execute_attempt_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &AttemptContext<'_>,
    ) -> crate::ToolAttemptResult {
        let Some(manifest) = self.resolve_manifest_by_id(tool_id) else {
            return crate::ToolAttemptResult::without_intents(ToolResult::err_fmt(format!(
                "Unknown tool id: {tool_id}"
            )));
        };
        self.execute_attempt(AttemptToolCall {
            name: &manifest.name,
            args,
            context,
        })
        .await
    }
    async fn execute_granted(
        &self,
        grant: &ToolExecutionGrant,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let _ = (args, context);
        ToolResult::err_fmt(format_args!(
            "Granted execution is unsupported for tool id `{}`",
            grant.manifest.id
        ))
    }
    async fn execute_granted_attempt(
        &self,
        grant: &ToolExecutionGrant,
        _args: &serde_json::Value,
        _context: &AttemptContext<'_>,
    ) -> crate::ToolAttemptResult {
        crate::ToolAttemptResult::without_intents(ToolResult::err_fmt(format_args!(
            "Granted leaf execution is unsupported for tool id `{}`",
            grant.manifest.id
        )))
    }
    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> ToolResult {
        let Some(manifest) = self.resolve_manifest_by_id(tool_id) else {
            return ToolResult::err_fmt(format!("Unknown tool id: {tool_id}"));
        };
        self.execute(ToolCall {
            name: &manifest.name,
            args,
            context,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ControllerOwnedWithoutCompletionKeyCapability;

    impl crate::AwaitEventResolver for ControllerOwnedWithoutCompletionKeyCapability {
        fn replay_ownership(&self) -> crate::EffectReplayOwnership {
            crate::EffectReplayOwnership::Controller
        }
    }

    #[async_trait::async_trait]
    impl crate::RuntimeEffectController for ControllerOwnedWithoutCompletionKeyCapability {
        async fn execute_effect(
            &self,
            _envelope: crate::RuntimeEffectEnvelope,
            _local_executor: crate::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
            unreachable!("completion-key capability is rejected before effect execution")
        }
    }

    #[test]
    fn tool_context_builder_carries_call_payload_and_cancellation_state() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let prepared = PreparedToolCall::from_parts(
            "call-1",
            "tool:demo_tool",
            "demo_tool",
            serde_json::json!({ "input": true }),
            None,
            serde_json::json!({ "prepared": true }),
        );

        let context = ToolContext::builder(
            "session-1".to_string(),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .prepared_call(&prepared)
        .cancellation_token(Some(cancellation.clone()))
        .async_process("process-1", cancellation.clone())
        .build();

        assert_eq!(context.session_id(), "session-1");
        assert_eq!(context.tool_call_id(), Some("call-1"));
        assert_eq!(
            context.prepared_payload(),
            &serde_json::json!({ "prepared": true })
        );
        assert_eq!(context.async_process_id(), Some("process-1"));
        assert!(context.cancellation_token().is_some());
    }

    #[tokio::test]
    async fn inline_completion_key_requires_process_lifetime_opt_in() {
        let prepared = PreparedToolCall::from_parts(
            "call-inline-risk",
            "tool:demo_tool",
            "demo_tool",
            serde_json::json!({}),
            None,
            serde_json::json!({}),
        );
        let context = ToolContext::builder(
            "session-inline-risk".to_string(),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                crate::InlineRuntimeEffectController::default(),
            )),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .prepared_call(&prepared)
        .build();

        let error = context
            .completion_key()
            .await
            .expect_err("Inline completion keys must refuse by default");
        assert_eq!(error.code.as_str(), "tool_completion_key_process_lifetime");
        assert!(error.message.contains("process-loss-safe"));
        assert!(
            error
                .message
                .contains("allow_process_lifetime_completion_keys")
        );
    }

    #[tokio::test]
    async fn controller_replay_ownership_does_not_bypass_completion_key_capability() {
        let prepared = PreparedToolCall::from_parts(
            "call-controller-risk",
            "tool:demo_tool",
            "demo_tool",
            serde_json::json!({}),
            None,
            serde_json::json!({}),
        );
        let context = ToolContext::builder(
            "session-controller-risk".to_string(),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
                ControllerOwnedWithoutCompletionKeyCapability,
            )),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .prepared_call(&prepared)
        .build();

        let error = context
            .completion_key()
            .await
            .expect_err("controller ownership alone must not permit completion keys");
        assert_eq!(error.code.as_str(), "tool_completion_key_process_lifetime");
    }
}
