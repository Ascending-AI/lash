// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::SessionId;
use crate::plugin::PluginSessionRequest;
use crate::plugin::{PluginSession, StaticPluginFactory};
use crate::support::prelude::*;
use crate::tool_dispatch::*;
use crate::{
    ToolCall, ToolOutcome, ToolProvider,
    coordinate_prepared_tool_call_launch_with_execution_context,
};

use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod composition_laws;

fn named_beta_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
}

fn manifests(definitions: Vec<crate::ToolDefinition>) -> Vec<crate::ToolManifest> {
    definitions
        .into_iter()
        .map(|tool| tool.manifest())
        .collect()
}

#[derive(Clone)]
struct FixedAttemptIntentTools {
    definition: crate::ToolDefinition,
    intents: crate::ToolIntents,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for FixedAttemptIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(json!({"provider": "recorded"})),
            self.intents.clone(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntentPausePoint {
    AfterToolAttemptCommit,
    BeforeProcessCommand(usize),
    AfterProcessCommandCommit(usize),
}

struct IntentReplayController {
    /// The backend controller the fake's await events live in.
    native: Arc<dyn crate::RuntimeEffectController>,
    recorded: std::sync::Mutex<
        BTreeMap<String, Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError>>,
    >,
    frame_sightings: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
    process_abort: Option<crate::RuntimeEffectControllerError>,
    process_commands: AtomicUsize,
    pause: std::sync::Mutex<Option<IntentPausePoint>>,
    pause_entered: tokio::sync::Notify,
    pause_release: tokio::sync::Notify,
}

#[derive(Debug)]
struct FrozenIntentLawClock {
    now: std::time::Instant,
}

impl FrozenIntentLawClock {
    fn new() -> Self {
        Self {
            now: std::time::Instant::now(),
        }
    }
}

#[async_trait::async_trait]
impl crate::Clock for FrozenIntentLawClock {
    fn now(&self) -> std::time::Instant {
        self.now
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = 1_700_000_000_000;
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

impl IntentReplayController {
    async fn new(pause: Option<IntentPausePoint>) -> Self {
        Self {
            native: crate::support::runtime_operation_controller().await,
            recorded: std::sync::Mutex::new(BTreeMap::new()),
            frame_sightings: std::sync::Mutex::new(BTreeMap::new()),
            process_abort: None,
            process_commands: AtomicUsize::new(0),
            pause: std::sync::Mutex::new(pause),
            pause_entered: tokio::sync::Notify::new(),
            pause_release: tokio::sync::Notify::new(),
        }
    }

    fn with_process_abort(mut self, error: crate::RuntimeEffectControllerError) -> Self {
        self.process_abort = Some(error);
        self
    }

    fn take_pause(&self, expected: IntentPausePoint) -> bool {
        let mut pause = self.pause.lock_recover();
        if pause.as_ref() == Some(&expected) {
            pause.take();
            true
        } else {
            false
        }
    }

    async fn pause_if(&self, expected: IntentPausePoint) {
        if self.take_pause(expected) {
            self.pause_entered.notify_one();
            self.pause_release.notified().await;
        }
    }

    async fn wait_until_paused(&self) {
        self.pause_entered.notified().await;
    }

    fn frame_sightings(&self) -> BTreeMap<String, Vec<String>> {
        self.frame_sightings.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for IntentReplayController {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.native.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.native.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.native.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.native.await_await_event(key, cancel).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.native
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.native
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for IntentReplayController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let replay_key = envelope.invocation.effect_replay_key().to_string();
        let frame = serde_json::to_string(&envelope).expect("serialize law effect frame");
        self.frame_sightings
            .lock_recover()
            .entry(replay_key.clone())
            .or_default()
            .push(frame);
        if let Some(result) = self.recorded.lock_recover().get(&replay_key).cloned() {
            return result;
        }

        let kind = envelope.command.kind();
        if kind == crate::RuntimeEffectKind::Process
            && let Some(error) = self.process_abort.as_ref()
        {
            return Err(error.clone());
        }
        let process_ordinal = (kind == crate::RuntimeEffectKind::Process)
            .then(|| self.process_commands.fetch_add(1, Ordering::SeqCst) + 1);
        if let Some(ordinal) = process_ordinal {
            self.pause_if(IntentPausePoint::BeforeProcessCommand(ordinal))
                .await;
        }
        let result = match envelope.command {
            crate::RuntimeEffectCommand::Process { command } => local_executor
                .into_process()?
                .execute(envelope.invocation.execution_scope(), *command)
                .await
                .map(|result| crate::RuntimeEffectOutcome::Process { result }),
            command => {
                local_executor
                    .execute(crate::RuntimeEffectEnvelope::new(
                        envelope.invocation,
                        command,
                    ))
                    .await
            }
        };
        self.recorded
            .lock_recover()
            .insert(replay_key, result.clone());
        if kind == crate::RuntimeEffectKind::ToolAttempt {
            self.pause_if(IntentPausePoint::AfterToolAttemptCommit)
                .await;
        }
        if let Some(ordinal) = process_ordinal {
            self.pause_if(IntentPausePoint::AfterProcessCommandCommit(ordinal))
                .await;
        }
        result
    }
}

fn test_plugins(provider: Arc<dyn ToolProvider>) -> Arc<PluginSession> {
    crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("test_tools"),
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
    ))])
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session")
}

use crate::testing::MockSessionManager;

struct HiddenDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for HiddenDispatchTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![named_beta_tool("hidden")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (name == "hidden").then(|| Arc::new(named_beta_tool("hidden").contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("hidden")).into()
    }
}

async fn exact_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    provider: Arc<dyn ToolProvider>,
) -> ToolDispatchContext<'h> {
    exact_dispatch_context_with_plugins(ports, test_plugins(provider)).await
}

async fn exact_dispatch_context_with_plugins<'h>(
    ports: crate::support::DispatchPorts<'h>,
    plugins: Arc<PluginSession>,
) -> ToolDispatchContext<'h> {
    let tools = plugins.tools();
    let tool_catalog = plugins.resolved_tool_catalog().expect("tool catalog");
    ToolDispatchContext {
        tool_receipts: None,
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_engines: Default::default(),
        effect_controller: ports.controller,
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        observation_call_key: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
        owner: crate::ExecutionOwner::SessionFrame {
            session_id: SessionId::from("session"),
            agent_frame_id: crate::FrameNodeId::new("test-frame").unwrap(),
        },
        observer: crate::engine::NullObservationSink::arc(),
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: ports.attachment_store,
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
        process_lineage: None,
        process_originator: None,
    }
}

fn tool_context_for_prepared<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &crate::PreparedToolCall,
) -> crate::testing::ToolCallFixture<'run> {
    crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .prepared_call(prepared)
}

mod single_gate;

#[cfg(test)]
mod intent_laws;
