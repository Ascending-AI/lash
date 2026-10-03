// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::SessionId;
use crate::plugin::PluginSessionRequest;
use crate::plugin::{PluginSession, StaticPluginFactory};
use crate::runtime::ScopedEffectController;
use crate::support::prelude::*;
use crate::tool_dispatch::*;
use crate::{
    ToolCall, ToolCallOutcome, ToolOutcome, ToolProvider, ToolRetryPolicy, ToolRetryStatus,
    coordinate_prepared_tool_call_launch_with_execution_context,
    dispatch_tool_call_with_execution_context,
};
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;

use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

mod attachment_normalization;
mod composition_laws;
mod host_effect_ledger;
mod protocol_version_refusal;
mod retry_effect_controllers;
mod retry_laws;
mod retry_turn_cancel_gate;

use retry_effect_controllers::{FailingSleepEffectController, SleepRecordingEffectController};

type AttemptObservation = (u32, u32, String);
type SharedAttemptObservations = Arc<std::sync::Mutex<Vec<AttemptObservation>>>;

fn test_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
}

fn beta_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:beta",
        "beta",
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

fn contract_from(
    definitions: Vec<crate::ToolDefinition>,
    name: &str,
) -> Option<Arc<crate::ToolContract>> {
    definitions
        .into_iter()
        .find(|tool| tool.name() == name)
        .map(|tool| Arc::new(tool.contract()))
}

struct MockTools;

#[derive(Clone)]
struct AttemptIntentTools {
    definition: crate::ToolDefinition,
    calls: Arc<AtomicUsize>,
    target: Arc<std::sync::OnceLock<crate::ProcessId>>,
}

#[derive(Clone)]
struct RetryingIntentTools {
    definition: crate::ToolDefinition,
    calls: Arc<AtomicUsize>,
    target: crate::ProcessId,
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

    fn release(&self) {
        self.pause_release.notify_one();
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

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("IntentReplayController"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("IntentReplayController"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("IntentReplayController"))
    }

    async fn commit_group_child_final(
        &self,
        _commit: crate::runtime::effect::GroupChildFinalCommit,
    ) -> Result<
        crate::runtime::effect::EffectGroupChildCommitOutcome,
        crate::RuntimeEffectControllerError,
    > {
        Ok(crate::runtime::effect::EffectGroupChildCommitOutcome::Ungrouped)
    }
}

#[async_trait::async_trait]
impl ToolProvider for RetryingIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let intents = crate::ToolIntents::v3(vec![crate::ToolIntent::EmitProcessEvent(
            crate::EmitProcessEventIntent {
                owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                process_id: self.target.clone(),
                event_type: "attempt.retry.final".to_string(),
                payload: json!({"attempt": call.context.attempt_number()}),
            },
        )]);
        if attempt == 1 {
            crate::ToolAttemptOutcome::done(
                crate::ToolOutcomeDone::failure(crate::ToolFailure::safe_retry(
                    crate::ToolFailureClass::External,
                    "retry_once",
                    "literal first attempt failure",
                    Some(0),
                )),
                intents,
            )
        } else {
            crate::ToolAttemptOutcome::done(crate::ToolOutcomeDone::ok(json!("done")), intents)
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for AttemptIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            call.context
                .session_id()
                .expect("the call runs in a session")
                .as_str(),
            "session"
        );
        assert_eq!(
            call.context.call_id(),
            &crate::ToolCallId::fixture("attempt-intents-call")
        );
        assert_eq!(call.context.attempt_number(), 1);
        assert_eq!(call.context.max_attempts(), 1);
        assert!(call.context.cancellation_token().is_some());
        assert_eq!(call.context.prepared_payload(), &serde_json::Value::Null);
        assert_eq!(
            call.context.tool_execution_binding(),
            &serde_json::Value::Null
        );
        let _phase = call.context.named_phase("attempt-context-law");
        call.context
            .sessions()
            .snapshot_current()
            .await
            .expect("attempt session snapshot read");
        call.context
            .sessions()
            .model()
            .await
            .expect("attempt session model read");
        assert_eq!(
            call.context
                .sessions()
                .tool_catalog()
                .await
                .expect("attempt catalog read"),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(
            call.context
                .processes()
                .list_handles_filtered(&crate::ProcessListFilter::default())
                .await
                .expect("controller-free attempt process read")
                .len(),
            1
        );
        call.context
            .attachments()
            .put(
                vec![1, 2, 3],
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("application/octet-stream")
                        .expect("literal media type"),
                    None,
                    Some("attempt.bin".to_string()),
                ),
            )
            .await
            .expect("content-addressed attempt attachment write");
        assert_eq!(
            call.context
                .direct_completions()
                .complete(
                    crate::DirectRequest::text("attempt prompt"),
                    "attempt-context-law",
                )
                .await
                .expect("attempt-local direct completion")
                .text,
            "attempt direct ok"
        );
        assert_eq!(
            call.context
                .completion_key()
                .expect_err("non-deferable provider receives no completion key")
                .code,
            // The tool never declared `may_defer`, and the refusal says so
            // rather than blaming the host's effect controller.
            crate::RuntimeErrorCode::ToolDeferralNotDeclared
        );
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(json!({"provider": "done"})),
            crate::ToolIntents::v3(vec![
                crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
                    owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                    declaration: crate::ProcessStartDeclaration::external(
                        crate::ProcessOriginator::host_scoped("attempt-intents-test"),
                        json!({"source": "recorded-attempt"}),
                        crate::Lifetime::Detached,
                    ),
                })),
                crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
                    owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                    process_id: self.target.get().expect("the target is registered").clone(),
                    signal_name: "resume".to_string(),
                    payload: json!({"ordinal": 1}),
                }),
                crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                    owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                    process_id: self.target.get().expect("the target is registered").clone(),
                    event_type: "attempt.intent.note".to_string(),
                    payload: json!({"ordinal": 2}),
                }),
                crate::ToolIntent::EmitTrigger(crate::EmitTriggerIntent {
                    owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                    request: crate::TriggerOccurrenceRequest::new(
                        "attempt.intent.trigger",
                        "attempt-intents-source",
                        json!({"ordinal": 3}),
                        "attempt-intents-occurrence",
                    ),
                }),
                crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
                    owner: crate::RuntimeOwner::Session(SessionId::from("session")),
                    process_id: self.target.get().expect("the target is registered").clone(),
                }),
            ]),
        )
    }
}

#[async_trait::async_trait]
impl ToolProvider for MockTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![test_tool("alpha"), beta_tool()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        contract_from(vec![test_tool("alpha"), beta_tool()], name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        (match call.name() {
            "alpha" => ToolOutcome::ok(json!("alpha")),
            "beta" => {
                if call.args.get("value").and_then(|value| value.as_str()) == Some("fail") {
                    ToolOutcome::err_fmt("beta failed")
                } else {
                    ToolOutcome::ok(json!(
                        call.args.get("value").cloned().unwrap_or(json!(null))
                    ))
                }
            }
            other => ToolOutcome::err_fmt(format!("Unknown tool: {other}")),
        })
        .into()
    }
}

#[derive(Clone, Copy)]
enum PendingProbeMode {
    MissingKey,
    PendingWithKey,
    FailureThenPending,
    /// Declares a park announcement the runtime cannot append, because this
    /// dispatch context is not inside a durable process.
    AnnouncingWithoutProcess,
}

#[derive(Clone)]
struct PendingProbeTools {
    definition: crate::ToolDefinition,
    attempts: Arc<AtomicUsize>,
    mode: PendingProbeMode,
}

#[async_trait::async_trait]
impl ToolProvider for PendingProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        (match self.mode {
            PendingProbeMode::MissingKey => ToolOutcome::pending(crate::PendingCompletion::new()),
            PendingProbeMode::PendingWithKey => {
                call.context.completion_key().expect("completion key");
                ToolOutcome::pending(crate::PendingCompletion::new())
            }
            PendingProbeMode::FailureThenPending if attempt == 1 => ToolOutcome::retryable_failure(
                crate::ToolFailureClass::External,
                "transient",
                "transient before pending",
                Some(0),
            ),
            PendingProbeMode::FailureThenPending => {
                call.context.completion_key().expect("completion key");
                ToolOutcome::pending(crate::PendingCompletion::new())
            }
            PendingProbeMode::AnnouncingWithoutProcess => {
                call.context.completion_key().expect("completion key");
                ToolOutcome::pending(crate::PendingCompletion::new().announcing(
                    crate::PendingAnnouncement::new(
                        "process.yield",
                        json!({ "type": "work.input_request.opened" }),
                        "pending-probe:announcement",
                    ),
                ))
            }
        })
        .into()
    }
}

struct StrictMcpTools {
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for StrictMcpTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![strict_mcp_tool_definition()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "mcp__appworld__venmo_show_transactions")
            .then(|| Arc::new(strict_mcp_tool_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!({ "executed": true })).into()
    }
}

fn strict_mcp_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:mcp__appworld__venmo_show_transactions",
        "mcp__appworld__venmo_show_transactions",
        "Show Venmo transactions",
        json!({
            "type": "object",
            "properties": {
                "min_created_at": { "type": "string" },
                "max_created_at": { "type": "string" },
                "limit": { "type": "integer", "maximum": 100 }
            },
            "required": ["limit"]
        }),
        json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
}

struct ProjectionPolicyTools;

#[async_trait::async_trait]
impl ToolProvider for ProjectionPolicyTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![projection_policy_tool_definition()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "seedy").then(|| Arc::new(projection_policy_tool_definition().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!("ok")).into()
    }
}

fn projection_policy_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:seedy",
        "seedy",
        "Seed-aware",
        crate::ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_argument_projection(
        crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
    )
}

async fn strict_mcp_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    executed: Arc<AtomicUsize>,
) -> ToolDispatchContext<'h> {
    let plugins = test_plugins(Arc::new(StrictMcpTools { executed }));
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

fn test_plugins(provider: Arc<dyn ToolProvider>) -> Arc<PluginSession> {
    crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("test_tools"),
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
    ))])
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session")
}

use crate::testing::MockSessionManager;

async fn dispatch_context<'h>(ports: crate::support::DispatchPorts<'h>) -> ToolDispatchContext<'h> {
    let plugins = test_plugins(Arc::new(MockTools));
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

async fn projection_policy_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    captured: Arc<std::sync::Mutex<Option<crate::ToolArgumentProjectionPolicy>>>,
) -> ToolDispatchContext<'h> {
    let provider: Arc<dyn ToolProvider> = Arc::new(ProjectionPolicyTools);
    let hook_captured = Arc::clone(&captured);
    let hook: crate::plugin::ToolArgsTransformHook = Arc::new(move |input| {
        let hook_captured = Arc::clone(&hook_captured);
        Box::pin(async move {
            *hook_captured.lock_recover() = Some(input.context.argument_projection.clone());
            Ok(input.current)
        })
    });
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("projection_policy_tools"),
        crate::PluginSpec::new()
            .with_tool_provider(Arc::clone(&provider))
            .with_tool_args_transform(lash_core_execution::hook_key!("capture"), hook),
    ))])
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
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

struct ExactDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
    contract_available: bool,
    observed_execution_bindings: Option<Arc<std::sync::Mutex<Vec<serde_json::Value>>>>,
}

struct HiddenDispatchTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
}

struct RetryProbeTools {
    definition: crate::ToolDefinition,
    attempts: Arc<AtomicUsize>,
    successes_after: usize,
    cancel_on_first: bool,
    observed_attempts: SharedAttemptObservations,
    retry_after_ms: Option<u64>,
}

#[async_trait::async_trait]
impl ToolProvider for ExactDispatchTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest(&self, name: &str) -> Option<crate::ToolManifest> {
        (name == "host_only").then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<crate::ToolManifest> {
        (id == &crate::ToolId::from("tool:host_only"))
            .then(|| named_beta_tool("host_only").manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (self.contract_available && name == "host_only")
            .then(|| Arc::new(named_beta_tool("host_only").contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        if let Some(bindings) = &self.observed_execution_bindings {
            bindings
                .lock_recover()
                .push(call.context.tool_execution_binding().clone());
        }
        ToolOutcome::ok(json!("host")).into()
    }
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

#[async_trait::async_trait]
impl ToolProvider for RetryProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.observed_attempts.lock_recover().push((
            call.context.attempt_number(),
            call.context.max_attempts(),
            call.context.call_id().to_string(),
        ));
        let attempt_index = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.cancel_on_first {
            return ToolOutcome::cancelled("cancelled").into();
        }
        if attempt_index >= self.successes_after {
            return ToolOutcome::ok(json!({ "attempt": attempt_index })).into();
        }
        ToolOutcome::retryable_failure(
            crate::ToolFailureClass::External,
            "transient",
            "transient failure",
            self.retry_after_ms,
        )
        .into()
    }
}

/// Build a dispatch context where the provider's tool is authority-hidden,
/// so it is removed from the Tool Catalog (non-membership) and rejected before
/// contract resolution.
async fn authority_hidden_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    provider: Arc<dyn ToolProvider>,
) -> ToolDispatchContext<'h> {
    let tool_access = crate::SessionToolAccess::ambient()
        .with_hidden_tools(["hidden"])
        .expect("valid hidden name");
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("test_tools"),
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
    ))])
    .build_session(PluginSessionRequest::creation(
        "root",
        crate::plugin::SessionAuthorityContext {
            tool_access,
            ..Default::default()
        },
    ))
    .expect("plugin session");
    assert!(
        plugins
            .tool_registry()
            .export_state()
            .iter()
            .find(|(_, entry)| entry.manifest().name == "hidden")
            .is_some_and(|(_, entry)| entry.is_member()),
        "authority hiding must not rewrite the registry's curation bit"
    );
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

fn retry_tool(name: &str, retry_policy: ToolRetryPolicy) -> crate::ToolDefinition {
    named_beta_tool(name).with_retry_policy(retry_policy)
}

async fn retry_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    retry_policy: ToolRetryPolicy,
    attempts: Arc<AtomicUsize>,
    successes_after: usize,
    cancel_on_first: bool,
    observed_attempts: SharedAttemptObservations,
) -> ToolDispatchContext<'h> {
    exact_dispatch_context(
        ports,
        Arc::new(RetryProbeTools {
            definition: retry_tool("retry_probe", retry_policy),
            attempts,
            successes_after,
            cancel_on_first,
            observed_attempts,
            retry_after_ms: Some(0),
        }),
    )
    .await
}

async fn retry_dispatch_context_with_after_observations<'h>(
    ports: crate::support::DispatchPorts<'h>,
    attempts: Arc<AtomicUsize>,
    observed_attempts: SharedAttemptObservations,
    observed_retries: Arc<std::sync::Mutex<Vec<ToolRetryStatus>>>,
) -> ToolDispatchContext<'h> {
    let provider: Arc<dyn ToolProvider> = Arc::new(RetryProbeTools {
        definition: retry_tool("retry_probe", ToolRetryPolicy::safe(2, 0, 0)),
        attempts,
        successes_after: usize::MAX,
        cancel_on_first: false,
        observed_attempts,
        retry_after_ms: Some(0),
    });
    let hook: crate::plugin::ToolResultCheckHook = Arc::new(move |input| {
        let observed_retries = Arc::clone(&observed_retries);
        Box::pin(async move {
            if let ToolCallOutcome::Failure(failure) = &input.final_result.outcome {
                observed_retries.lock_recover().push(failure.retry.clone());
            }
            Ok(crate::plugin::AfterToolContributions::default())
        })
    });
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("retry_probe_tools"),
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_tool_result_check(lash_core_execution::hook_key!("observe-retry"), hook),
    ))])
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
    exact_dispatch_context_with_plugins(ports, plugins).await
}

/// The probe declares it may defer: every mode but `MissingKey` parks under
/// its declaration.
fn pending_probe_tool(retry_policy: ToolRetryPolicy) -> crate::ToolDefinition {
    named_beta_tool("pending_probe")
        .with_retry_policy(retry_policy)
        .with_declaration(crate::ToolDeclaration::deferring())
}

async fn pending_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    mode: PendingProbeMode,
    attempts: Arc<AtomicUsize>,
    after_calls: Option<Arc<AtomicUsize>>,
    retry_policy: ToolRetryPolicy,
) -> ToolDispatchContext<'h> {
    let definition = match mode {
        // This mode parks under no deferral declaration.
        PendingProbeMode::MissingKey => {
            named_beta_tool("pending_probe").with_retry_policy(retry_policy)
        }
        PendingProbeMode::PendingWithKey
        | PendingProbeMode::FailureThenPending
        | PendingProbeMode::AnnouncingWithoutProcess => pending_probe_tool(retry_policy),
    };
    let provider: Arc<dyn ToolProvider> = Arc::new(PendingProbeTools {
        definition,
        attempts,
        mode,
    });
    let mut spec = crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider));
    if let Some(after_calls) = after_calls {
        let hook: crate::plugin::ToolResultCheckHook = Arc::new(move |_input| {
            let after_calls = Arc::clone(&after_calls);
            Box::pin(async move {
                after_calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::plugin::AfterToolContributions::default())
            })
        });
        spec = spec.with_tool_result_check(lash_core_execution::hook_key!("count"), hook);
    }
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("pending_probe_tools"),
        spec,
    ))])
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
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

use pending_park_laws::pending_prepared_call;

const SEED: u64 = 0x5_2d21;

fn tool_context_for_prepared<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &crate::PreparedToolCall,
) -> crate::testing::ToolCallFixture<'run> {
    crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .prepared_call(prepared)
}

/// A body that parks although its admitted declaration does not declare
/// `may_defer` is refused typed: no key was reserved, nothing parks, and the
/// cause names the declaration rather than the missing key.
#[tokio::test]
async fn an_undeclared_deferral_is_refused_typed_before_it_parks() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let context = pending_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        PendingProbeMode::MissingKey,
        Arc::clone(&attempts),
        None,
        ToolRetryPolicy::Never,
    )
    .await;
    let prepared = pending_prepared_call();
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        tool_context,
    )
    .await;

    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("an undeclared deferral must fail launch synchronously");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome else {
        panic!("expected failure output");
    };
    assert_eq!(failure.code, "tool_outcome_not_declared");
    assert_eq!(
        failure.cause.as_deref(),
        Some(&crate::ToolFailureCause::Declaration {
            refusal: crate::DeclarationRefusal::UndeclaredDeferral
        })
    );
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn retry_ladder_survives_a_later_pending_completion() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let context = pending_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        PendingProbeMode::FailureThenPending,
        Arc::clone(&attempts),
        None,
        ToolRetryPolicy::safe(3, 0, 0),
    )
    .await;
    let prepared = pending_prepared_call();
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        tool_context,
    )
    .await;

    let ToolCallLaunch::Pending(pending) = launch else {
        panic!("second attempt should park pending");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(pending.attempts.len(), 1);
    assert_eq!(pending.attempts[0].ordinal, 1);
    assert!(matches!(
        pending.attempts[0].detail,
        lash_trace::TraceRetryAttemptDetail::Tool {
            outcome: lash_trace::TraceToolAttemptOutcome::Failed { .. }
        }
    ));

    let attachment_store = Arc::clone(&context.attachment_store);
    let execution = crate::RuntimeExecutionContext::new(
        Arc::new(context),
        crate::support::sqlite_memory_store_set()
            .await
            .process_env_store(),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        crate::TurnContext::default(),
        crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
    );
    let completed = execution
        .pending_completion_dispatch_outcome(
            &crate::tool_dispatch::ToolCallIds {
                call_id: crate::ToolCallId::fixture("pending-call"),
                provider_call_id: None,
            },
            "test:pending-call",
            pending.tool_name,
            pending.args,
            crate::Resolution::Ok(serde_json::json!({ "done": true })),
            None,
            pending.attempts,
            pending.captures,
            pending.triggers,
        )
        .await;
    assert_eq!(completed.attempts.len(), 2);
    assert_eq!(completed.attempts[1].ordinal, 2);
    assert!(matches!(
        completed.attempts[1].detail,
        lash_trace::TraceRetryAttemptDetail::Tool {
            outcome: lash_trace::TraceToolAttemptOutcome::Completed
        }
    ));
    drop(execution);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn before_tool_hook_receives_resolved_argument_projection_policy() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let captured = Arc::new(std::sync::Mutex::new(None));
    let outcome = dispatch_tool_call(
        &projection_policy_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            Arc::clone(&captured),
        )
        .await,
        "seedy".to_string(),
        json!({}),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(
        captured.lock_recover().clone(),
        Some(crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"))
    );
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn dispatch_rejects_non_catalog_tool_before_provider_resolution() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: true,
        observed_execution_bindings: None,
    });
    let outcome = dispatch_tool_call(
        &exact_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            provider,
        )
        .await,
        "host_only".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        outcome.record.output.value_for_projection()["message"],
        json!("Tool is unavailable in this session")
    );
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn explicit_execution_grant_runs_non_catalog_tool_with_binding() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: false,
        observed_execution_bindings: Some(Arc::clone(&observed_execution_bindings)),
    });
    let context = exact_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        provider,
    )
    .await;
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::PluginRevision::new("mock", crate::plugin::BehaviorRevision::ONE),
        named_beta_tool("host_only"),
    )
    .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID)
    .with_execution_binding(json!({ "kind": "test", "route": "deferred" }));
    let pending = crate::sansio::PendingToolCall {
        call_id: lash_core_execution::ToolCallId::fixture("grant-call"),
        provider_call_id: None,
        tool_name: "host_only".to_string(),
        args: json!({ "value": "ok" }),
        replay: None,
    };
    let prepared = match prepare_granted_tool_call_with_context(&context, &grant, pending).await {
        ToolPreparationOutcome::Prepared(prepared) => *prepared,
        ToolPreparationOutcome::Completed(outcome) => {
            panic!("grant should prepare, got {:?}", outcome.record.output)
        }
    };
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .execution_binding(grant.execution_binding.clone());
    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        Some(Box::new(grant)),
        tool_context,
    )
    .await;
    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("grant call should complete");
    };

    assert!(outcome.record.output.is_success());
    assert_eq!(outcome.record.output.value_for_projection(), json!("host"));
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
    assert_eq!(
        *observed_execution_bindings.lock_recover(),
        vec![json!({ "kind": "test", "route": "deferred" })]
    );
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

mod single_gate;

#[tokio::test]
async fn dispatch_rejects_hidden_tool_before_contract_resolution() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(HiddenDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
    });
    let outcome = dispatch_tool_call(
        &authority_hidden_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            provider,
        )
        .await,
        "hidden".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        outcome.record.output.value_for_projection()["message"],
        json!("Tool is unavailable in this session")
    );
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 0);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn dispatch_allows_unknown_mcp_args_when_schema_does_not_forbid_them() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let executed = Arc::new(AtomicUsize::new(0));
    let outcome = dispatch_tool_call(
        &strict_mcp_dispatch_context(
            crate::support::double_dispatch_ports(&double, &handler),
            Arc::clone(&executed),
        )
        .await,
        "mcp__appworld__venmo_show_transactions".to_string(),
        json!({
            "min_datetime": "2024-01-01T00:00:00Z",
            "limit": 20
        }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(executed.load(Ordering::SeqCst), 1);
    handler.close().await.expect("close the dispatch handler");
}

/// The v2 provider seam law: an opted-in leaf is called through the public
/// coordinator path and every declared intent kind is realized after its final
/// attempt is committed, in declaration order.
#[tokio::test]
async fn attempt_context_provider_realizes_every_v2_intent_through_the_coordinator() {
    // The process intents run on the engine's process workflow, which reaches
    // the deployment's process worker: install one over the double.
    let definition = named_beta_tool("attempt_intents").with_declaration(
        crate::ToolDeclaration::default().with_intents([
            crate::ToolIntentKind::StartProcess,
            crate::ToolIntentKind::SignalProcess,
            crate::ToolIntentKind::EmitProcessEvent,
            crate::ToolIntentKind::EmitTrigger,
            crate::ToolIntentKind::CancelProcess,
        ]),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let target = Arc::new(std::sync::OnceLock::new());
    let provider: Arc<dyn ToolProvider> = Arc::new(AttemptIntentTools {
        definition: definition.clone(),
        calls: Arc::clone(&calls),
        target: Arc::clone(&target),
    });
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    crate::support::install_process_worker(&double);
    let mut context = exact_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        provider,
    )
    .await;
    context.direct_completions = crate::DirectCompletionClient::from_fn(|_, _| {
        Ok(crate::plugin::DirectCompletion {
            text: "attempt direct ok".to_string(),
            usage: crate::TokenUsage::default(),
            llm_call: crate::LlmCallRecord {
                call_id: crate::LlmCallId("attempt-direct-call".to_string()),
                label: None,
                attempts: Vec::new(),
                replay_drops: Vec::new(),
            },
        })
    });
    let backend = double.stores();
    let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
    let event_types = ["signal.resume", "attempt.intent.note"]
        .into_iter()
        .map(|name| crate::ProcessEventType {
            name: name.to_string(),
            payload_schema: crate::JsonSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        })
        .collect::<Vec<_>>();
    let registered = registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::ProcessProvenance::host(),
                crate::Lifetime::Detached,
            )
            .with_extra_event_types(event_types),
            &[SessionId::from("session")],
        )
        .await
        .expect("register intent target");
    target
        .set(registered.id.clone())
        .expect("the target is registered once");
    context.processes = crate::testing::effect_backed_process_service(
        Arc::clone(&registry),
        backend.process_env_store(),
    );
    context.trigger_router = Some(crate::TriggerRouter::new(
        backend.trigger_store(),
        crate::testing::process_work_wiring_for_registry(registry),
    ));

    let prepared = crate::PreparedToolCall {
        call_id: crate::ToolCallId::fixture("attempt-intents-call"),
        provider_call_id: None,
        tool_id: definition.id().to_string().into(),
        tool_name: "attempt_intents".into(),
        args: json!({"value": "shift"}),
        replay: None,
        prepared_payload: serde_json::Value::Null,
    };
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .cancellation_token(Some(tokio_util::sync::CancellationToken::new()));
    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        tool_context,
    )
    .await;

    let ToolCallLaunch::Done(outcome) = launch else {
        panic!("the non-deferred provider must complete synchronously");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The attempt body runs under `catch_unwind`, so an assertion that panics
    // inside the provider comes back as a Done failure output. Pin the exact
    // success payload, or every in-provider law above this line is unenforced.
    let crate::ToolCallOutcome::Success(crate::ToolValue::UntrustedJson(value)) =
        &outcome.record.output.outcome
    else {
        panic!(
            "an in-provider assertion panic or a lossy JSON decode surfaces here: {:?}",
            outcome.record.output.outcome
        );
    };
    assert_eq!(value["provider"], json!("done"));
    assert_eq!(
        outcome
            .intent_outcomes
            .iter()
            .map(crate::ToolIntentExecutionOutcome::kind)
            .collect::<Vec<_>>(),
        vec![
            Some(crate::ToolIntentKind::StartProcess),
            Some(crate::ToolIntentKind::SignalProcess),
            Some(crate::ToolIntentKind::EmitProcessEvent),
            Some(crate::ToolIntentKind::EmitTrigger),
            Some(crate::ToolIntentKind::CancelProcess),
        ]
    );
    assert!(
        outcome
            .intent_outcomes
            .iter()
            .all(|outcome| matches!(outcome, crate::ToolIntentExecutionOutcome::Executed { .. })),
        "{:?}",
        outcome.intent_outcomes
    );
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[cfg(test)]
mod granted_dispatch;
#[cfg(test)]
mod intent_laws;
#[cfg(test)]
mod pending_park_laws;
