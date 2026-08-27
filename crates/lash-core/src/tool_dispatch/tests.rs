use super::*;
use crate::plugin::{PluginHost, PluginSession, StaticPluginFactory};
use crate::runtime::RuntimeEffectControllerHandle;
use crate::{
    ProcessRegistry as _, ToolCall, ToolCallOutcome, ToolContext, ToolOutcome, ToolProvider,
    ToolRetryPolicy, ToolRetryStatus,
};
use lash_sansio::core_support::*;
use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Barrier, mpsc, oneshot};
use tokio::time::{Duration, timeout};

mod directives;
mod intent_drain;
mod internal_activation;
mod settlement_order;

type AttemptObservation = (u32, u32, Option<String>);
type SharedAttemptObservations = Arc<std::sync::Mutex<Vec<AttemptObservation>>>;

fn test_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        crate::ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    )
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

struct InternalProbeTools {
    executed: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for InternalProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![
            test_tool("internal_probe").with_activation(crate::ToolActivation::Internal),
        ])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "internal_probe").then(|| {
            Arc::new(
                test_tool("internal_probe")
                    .with_activation(crate::ToolActivation::Internal)
                    .contract(),
            )
        })
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("internal body ran"))
    }
}

#[derive(Clone)]
struct AttemptIntentTools {
    definition: crate::ToolDefinition,
    calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct RetryingIntentTools {
    definition: crate::ToolDefinition,
    calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct FixedAttemptIntentTools {
    definition: crate::ToolDefinition,
    intents: crate::ToolIntents,
    calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct OrderedBatchIntentTools {
    definitions: Vec<crate::ToolDefinition>,
    second_attempt_finished: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl ToolProvider for OrderedBatchIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(self.definitions.clone())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        contract_from(self.definitions.clone(), name)
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        panic!("ordered batch intent law uses AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.name == "intent_batch_first" {
            self.second_attempt_finished.notified().await;
        } else {
            assert_eq!(call.name, "intent_batch_second");
            self.second_attempt_finished.notify_one();
        }
        let call_id = call
            .context
            .tool_call_id()
            .expect("ordered batch calls carry ids");
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(json!({"completed": call.name})),
            crate::ToolIntents::v1(
                [0, 1]
                    .into_iter()
                    .map(|intent_index| {
                        let event_type = format!("{call_id}.intent.{intent_index}");
                        crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                            session_id: "session".to_string(),
                            process_id: "intent-law-target".to_string(),
                            event_type,
                            payload: json!({"call_id": call_id, "intent_index": intent_index}),
                        })
                    })
                    .collect(),
            ),
        )
    }
}

#[derive(Clone)]
struct BlockingAttemptIntentTools {
    definition: crate::ToolDefinition,
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl ToolProvider for BlockingAttemptIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        panic!("pre-result cancellation law uses AttemptContext")
    }

    async fn execute_attempt(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl ToolProvider for FixedAttemptIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        panic!("fixed intent law uses AttemptContext")
    }

    async fn execute_attempt(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
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
    inline: crate::InlineRuntimeEffectController,
    recorded: std::sync::Mutex<
        BTreeMap<String, Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError>>,
    >,
    frame_sightings: std::sync::Mutex<BTreeMap<String, Vec<String>>>,
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

    fn timestamp_ms(&self) -> u64 {
        1_700_000_000_000
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(self.timestamp_ms() as i64)
            .expect("fixed intent-law timestamp")
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

impl IntentReplayController {
    fn new(pause: Option<IntentPausePoint>) -> Self {
        Self {
            inline: crate::InlineRuntimeEffectController::default(),
            recorded: std::sync::Mutex::new(BTreeMap::new()),
            frame_sightings: std::sync::Mutex::new(BTreeMap::new()),
            process_commands: AtomicUsize::new(0),
            pause: std::sync::Mutex::new(pause),
            pause_entered: tokio::sync::Notify::new(),
            pause_release: tokio::sync::Notify::new(),
        }
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
    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inline.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inline.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inline.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inline.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        self.inline
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        self.inline
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
        let replay_key = envelope
            .invocation
            .replay_key()
            .expect("law effects carry replay keys")
            .to_string();
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
        let process_ordinal = (kind == crate::RuntimeEffectKind::Process)
            .then(|| self.process_commands.fetch_add(1, Ordering::SeqCst) + 1);
        if let Some(ordinal) = process_ordinal {
            self.pause_if(IntentPausePoint::BeforeProcessCommand(ordinal))
                .await;
        }
        let result = match envelope.command {
            crate::RuntimeEffectCommand::Process { command } => local_executor
                .into_process()?
                .execute(*command)
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

#[async_trait::async_trait]
impl ToolProvider for RetryingIntentTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        panic!("retry intent law uses AttemptContext")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let intents = crate::ToolIntents::v1(vec![crate::ToolIntent::EmitProcessEvent(
            crate::EmitProcessEventIntent {
                session_id: "session".to_string(),
                process_id: "retry-intent-target".to_string(),
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        panic!("the legacy ToolContext entrypoint must not run for an AttemptContext provider")
    }

    async fn execute_attempt(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(call.context.session_id(), "session");
        assert_eq!(call.context.tool_call_id(), Some("attempt-intents-call"));
        assert_eq!(call.context.attempt_number(), 1);
        assert_eq!(call.context.max_attempts(), 1);
        assert!(call.context.replay_key().is_some());
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
                    crate::DirectRequest::text("attempt-model", "attempt prompt"),
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
            // The provider never declared `attempt_may_defer`, and the refusal
            // says so rather than blaming the host's effect controller.
            crate::RuntimeErrorCode::ToolDeferralNotDeclared
        );
        crate::ToolAttemptOutcome::done(
            crate::ToolOutcomeDone::ok(json!({"provider": "done"})),
            crate::ToolIntents::v1(vec![
                crate::ToolIntent::StartProcess(Box::new(crate::StartProcessIntent {
                    session_id: "session".to_string(),
                    request: crate::ProcessStartRequest::external(
                        "provider-supplied-id-is-replaced",
                        crate::ProcessOriginator::host_scoped("attempt-intents-test"),
                        json!({"source": "recorded-attempt"}),
                    ),
                    on_parent_end: crate::ProcessParentEndPolicy::Abandon,
                })),
                crate::ToolIntent::SignalProcess(crate::SignalProcessIntent {
                    session_id: "session".to_string(),
                    process_id: "attempt-intents-target".to_string(),
                    signal_name: "resume".to_string(),
                    payload: json!({"ordinal": 1}),
                }),
                crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                    session_id: "session".to_string(),
                    process_id: "attempt-intents-target".to_string(),
                    event_type: "attempt.intent.note".to_string(),
                    payload: json!({"ordinal": 2}),
                }),
                crate::ToolIntent::EmitTrigger(crate::EmitTriggerIntent {
                    session_id: "session".to_string(),
                    request: crate::TriggerOccurrenceRequest::new(
                        "attempt.intent.trigger",
                        "attempt-intents-source",
                        json!({"ordinal": 3}),
                        "attempt-intents-occurrence",
                    ),
                }),
                crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
                    session_id: "session".to_string(),
                    process_id: "attempt-intents-target".to_string(),
                    reason: Some("literal final intent".to_string()),
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        match call.name {
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
        }
    }
}

struct ParallelProbeTools {
    barrier: Arc<Barrier>,
    started: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum PendingProbeMode {
    MissingKey,
    PendingWithKey,
    FailureThenPending,
    /// Declares a park announcement the runtime cannot append, because this
    /// dispatch context is not inside a durable process.
    AnnouncingWithoutProcess,
    Done,
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

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == self.definition.id()
            && matches!(
                self.mode,
                PendingProbeMode::PendingWithKey
                    | PendingProbeMode::FailureThenPending
                    | PendingProbeMode::AnnouncingWithoutProcess
            )
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        match self.mode {
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
            PendingProbeMode::Done => ToolOutcome::ok(json!({ "done": true })),
        }
    }
}

#[async_trait::async_trait]
impl ToolProvider for ParallelProbeTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![test_tool("probe_a"), test_tool("probe_b")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        contract_from(vec![test_tool("probe_a"), test_tool("probe_b")], name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.started.fetch_add(1, Ordering::SeqCst);
        let waited = timeout(Duration::from_millis(100), self.barrier.wait()).await;
        match waited {
            Ok(_) => ToolOutcome::ok(json!(call.name)),
            Err(_) => ToolOutcome::err_fmt(format!("{} did not overlap with peer", call.name)),
        }
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!({ "executed": true }))
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(json!("ok"))
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
    .with_argument_projection(
        crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
    )
}

fn strict_mcp_dispatch_context(executed: Arc<AtomicUsize>) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let plugins = test_plugins(Arc::new(StrictMcpTools { executed }));
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

fn test_plugins(provider: Arc<dyn ToolProvider>) -> Arc<PluginSession> {
    PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
    ))])
    .build_session("root")
    .expect("plugin session")
}

/// Runs a registered orchestrating tool the way session dispatch does.
///
/// The test `batch` tool lives in the orchestration lane — a recorded leaf
/// attempt receives an `AttemptContext` and cannot fan out — so these laws
/// enter through the same seam the runtime uses instead of the leaf route.
async fn dispatch_orchestrating_tool_call(
    context: &ToolDispatchContext<'_>,
    tool_name: &str,
    args: serde_json::Value,
) -> ToolDispatchOutcome {
    // The orchestration lane resolves its registration through the tool
    // registry, which the plain leaf-dispatch fixture leaves unset.
    let mut context = context.clone();
    context.tool_registry = Some(context.plugins.tool_registry());
    let context = &context;
    let manifest = super::resolve_callable_manifest(context, tool_name)
        .unwrap_or_else(|| panic!("orchestrating tool `{tool_name}` must be registered"));
    let prepared = crate::PreparedToolCall::identity(
        manifest.id,
        crate::sansio::PendingToolCall {
            call_id: format!("orchestrating:{tool_name}"),
            tool_name: tool_name.to_string(),
            args,
            replay: None,
        },
    );
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .build();
    crate::tool_dispatch::execute_orchestrating_tool(context, prepared, tool_context).await
}

use crate::testing::MockSessionManager;

fn dispatch_context() -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let plugins = test_plugins(Arc::new(MockTools));
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

fn projection_policy_dispatch_context(
    captured: Arc<std::sync::Mutex<Option<crate::ToolArgumentProjectionPolicy>>>,
) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let provider: Arc<dyn ToolProvider> = Arc::new(ProjectionPolicyTools);
    let hook_captured = Arc::clone(&captured);
    let hook: crate::plugin::BeforeToolCallHook = Arc::new(move |ctx| {
        let hook_captured = Arc::clone(&hook_captured);
        Box::pin(async move {
            *hook_captured.lock_recover() = Some(ctx.argument_projection.clone());
            Ok(Vec::new())
        })
    });
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "projection_policy_tools",
        crate::PluginSpec::new()
            .with_tool_provider(Arc::clone(&provider))
            .with_before_tool_call(hook),
    ))])
    .build_session("root")
    .expect("plugin session");
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

struct CountingContractTools {
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
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
impl ToolProvider for CountingContractTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![beta_tool()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        self.contracts_resolved.fetch_add(1, Ordering::SeqCst);
        (name == "beta").then(|| Arc::new(beta_tool().contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("ok"))
    }
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        if let Some(bindings) = &self.observed_execution_bindings {
            bindings
                .lock_recover()
                .push(call.context.tool_execution_binding().clone());
        }
        ToolOutcome::ok(json!("host"))
    }

    async fn prepare_granted_tool_call(
        &self,
        _grant: &crate::ToolExecutionGrant,
        call: crate::ToolPrepareCall<'_>,
    ) -> Result<crate::PreparedToolCall, ToolOutcome> {
        Ok(crate::PreparedToolCall::identity(
            call.tool_id,
            call.pending,
        ))
    }

    async fn execute_granted(
        &self,
        grant: &crate::ToolExecutionGrant,
        args: &serde_json::Value,
        context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        self.execute_by_id(&grant.manifest().id, args, context)
            .await
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

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!("hidden"))
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.observed_attempts.lock_recover().push((
            call.context.attempt_number(),
            call.context.max_attempts(),
            call.context.replay_key().map(str::to_string),
        ));
        let attempt_index = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if self.cancel_on_first {
            return ToolOutcome::cancelled("cancelled");
        }
        if attempt_index >= self.successes_after {
            return ToolOutcome::ok(json!({ "attempt": attempt_index }));
        }
        ToolOutcome::retryable_failure(
            crate::ToolFailureClass::External,
            "transient",
            "transient failure",
            self.retry_after_ms,
        )
    }
}

fn lazy_contract_dispatch_context(
    contracts_resolved: Arc<AtomicUsize>,
    executed: Arc<AtomicUsize>,
) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let provider: Arc<dyn ToolProvider> = Arc::new(CountingContractTools {
        contracts_resolved,
        executed,
    });
    let tools = Arc::clone(&provider);
    let tool_catalog = Arc::new(crate::ToolCatalog::from_tools(
        provider.tool_manifests(),
        BTreeMap::new(),
    ));
    ToolDispatchContext {
        plugins: test_plugins(provider),
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

/// Build a dispatch context where the provider's tool is authority-hidden,
/// so it is removed from the Tool Catalog (non-membership) and rejected before
/// contract resolution.
fn hidden_member_dispatch_context(provider: Arc<dyn ToolProvider>) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut tool_access = crate::SessionToolAccess::default();
    tool_access.hidden_tools.insert("hidden".to_string());
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "test_tools",
        crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider)),
    ))])
    .build_session_with_parent(
        "root",
        None,
        crate::plugin::SessionCreationConfig {
            authority: crate::plugin::SessionAuthorityContext {
                tool_access,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect("plugin session");
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

fn exact_dispatch_context(provider: Arc<dyn ToolProvider>) -> ToolDispatchContext<'static> {
    exact_dispatch_context_with_plugins(test_plugins(provider))
}

fn exact_dispatch_context_with_plugins(
    plugins: Arc<PluginSession>,
) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

fn retry_tool(name: &str, retry_policy: ToolRetryPolicy) -> crate::ToolDefinition {
    named_beta_tool(name).with_retry_policy(retry_policy)
}

fn retry_dispatch_context(
    retry_policy: ToolRetryPolicy,
    attempts: Arc<AtomicUsize>,
    successes_after: usize,
    cancel_on_first: bool,
    observed_attempts: SharedAttemptObservations,
) -> ToolDispatchContext<'static> {
    exact_dispatch_context(Arc::new(RetryProbeTools {
        definition: retry_tool("retry_probe", retry_policy),
        attempts,
        successes_after,
        cancel_on_first,
        observed_attempts,
        retry_after_ms: Some(0),
    }))
}

fn retry_dispatch_context_with_after_observations(
    attempts: Arc<AtomicUsize>,
    observed_attempts: SharedAttemptObservations,
    observed_retries: Arc<std::sync::Mutex<Vec<ToolRetryStatus>>>,
) -> ToolDispatchContext<'static> {
    let provider: Arc<dyn ToolProvider> = Arc::new(RetryProbeTools {
        definition: retry_tool("retry_probe", ToolRetryPolicy::safe(2, 0, 0)),
        attempts,
        successes_after: usize::MAX,
        cancel_on_first: false,
        observed_attempts,
        retry_after_ms: Some(0),
    });
    let hook: crate::plugin::AfterToolCallHook = Arc::new(move |ctx| {
        let observed_retries = Arc::clone(&observed_retries);
        Box::pin(async move {
            if let Some(output) = ctx.result.as_done_output()
                && let ToolCallOutcome::Failure(failure) = &output.outcome
            {
                observed_retries.lock_recover().push(failure.retry.clone());
            }
            Ok(Vec::new())
        })
    });
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "retry_probe_tools",
        crate::PluginSpec::new()
            .with_tool_provider(provider)
            .with_after_tool_call(hook),
    ))])
    .build_session("root")
    .expect("plugin session");
    exact_dispatch_context_with_plugins(plugins)
}

fn pending_probe_tool(retry_policy: ToolRetryPolicy) -> crate::ToolDefinition {
    named_beta_tool("pending_probe").with_retry_policy(retry_policy)
}

fn pending_dispatch_context(
    mode: PendingProbeMode,
    attempts: Arc<AtomicUsize>,
    after_calls: Option<Arc<AtomicUsize>>,
    retry_policy: ToolRetryPolicy,
) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let provider: Arc<dyn ToolProvider> = Arc::new(PendingProbeTools {
        definition: pending_probe_tool(retry_policy),
        attempts,
        mode,
    });
    let mut spec = crate::PluginSpec::new().with_tool_provider(Arc::clone(&provider));
    if let Some(after_calls) = after_calls {
        let hook: crate::plugin::AfterToolCallHook = Arc::new(move |_ctx| {
            let after_calls = Arc::clone(&after_calls);
            Box::pin(async move {
                after_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
        });
        spec = spec.with_after_tool_call(hook);
    }
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "pending_probe_tools",
        spec,
    ))])
    .build_session("root")
    .expect("plugin session");
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

fn pending_prepared_call() -> crate::PreparedToolCall {
    crate::PreparedToolCall::from_parts(
        "pending-call",
        "tool:pending_probe",
        "pending_probe",
        json!({ "value": "runtime perf benchmark ok" }),
        None,
        serde_json::Value::Null,
    )
}

fn tool_context_for_prepared<'run>(
    context: &ToolDispatchContext<'run>,
    prepared: &crate::PreparedToolCall,
) -> ToolContext<'run> {
    ToolContext::from_dispatch(Arc::new(context.clone()))
        .prepared_call(prepared)
        .build()
}

fn parallel_dispatch_context(
    barrier: Arc<Barrier>,
    started: Arc<AtomicUsize>,
) -> ToolDispatchContext<'static> {
    let (event_tx, _event_rx) = mpsc::channel(8);
    let plugins = test_plugins(Arc::new(ParallelProbeTools { barrier, started }));
    let tools = plugins.tools();
    let tool_catalog = plugins
        .resolved_tool_catalog("session")
        .expect("tool catalog");
    ToolDispatchContext {
        plugins,
        tools,
        tool_registry: None,
        tool_catalog,
        sessions: Arc::new(MockSessionManager::default()),
        session_lifecycle: Arc::new(MockSessionManager::default()),
        session_graph: Arc::new(MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        effect_controller: RuntimeEffectControllerHandle::shared(Arc::new(
            crate::InlineRuntimeEffectController::default()
                .allow_process_lifetime_completion_keys(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: "session".to_string(),
        agent_frame_id: crate::FrameNodeId::default(),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        recorded_intent_outcomes: crate::tool_dispatch::RecordedToolIntentOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: std::sync::Arc::new(crate::SystemClock),
    }
}

#[tokio::test]
async fn dispatch_rejects_invalid_args_before_provider_execution() {
    let outcome = dispatch_tool_call(&dispatch_context(), "beta".to_string(), json!({})).await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        outcome.record.output.value_for_projection()["message"],
        json!("\"value\" is a required property")
    );
}

#[tokio::test]
async fn dispatch_resolves_contract_only_for_called_tool_before_execution() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let outcome = dispatch_tool_call(
        &lazy_contract_dispatch_context(Arc::clone(&contracts_resolved), Arc::clone(&executed)),
        "beta".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(contracts_resolved.load(Ordering::SeqCst), 1);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_tool_without_completion_key_is_runtime_failure() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let context = pending_dispatch_context(
        PendingProbeMode::MissingKey,
        Arc::clone(&attempts),
        None,
        ToolRetryPolicy::Never,
    );
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
        panic!("missing completion key must fail launch synchronously");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let ToolCallOutcome::Failure(failure) = &outcome.record.output.outcome else {
        panic!("expected failure output");
    };
    assert_eq!(failure.code, "pending_tool_missing_completion_key");
}

#[tokio::test]
async fn retry_policy_stops_after_pending_launch() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let context = pending_dispatch_context(
        PendingProbeMode::PendingWithKey,
        Arc::clone(&attempts),
        None,
        ToolRetryPolicy::safe(5, 0, 0),
    );
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
        panic!("tool should launch pending");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(pending.tool_name, "pending_probe");
    assert_eq!(
        pending.key.wait,
        crate::AwaitEventWaitIdentity::tool_completion("pending-call")
    );
}

#[tokio::test]
async fn retry_ladder_survives_a_later_pending_completion() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let context = pending_dispatch_context(
        PendingProbeMode::FailureThenPending,
        Arc::clone(&attempts),
        None,
        ToolRetryPolicy::safe(3, 0, 0),
    );
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
    assert_eq!(pending.attempts[0].outcome, "failed");

    let attachment_store = Arc::clone(&context.attachment_store);
    let execution = crate::RuntimeExecutionContext::new(
        "session".to_string(),
        Arc::new(context),
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    );
    let completed = execution
        .pending_completion_dispatch_outcome(
            pending.tool_name,
            pending.args,
            crate::Resolution::Ok(serde_json::json!({ "done": true })),
            pending.duration_ms,
            pending.attempts,
        )
        .await;
    assert_eq!(completed.attempts.len(), 2);
    assert_eq!(completed.attempts[1].ordinal, 2);
    assert_eq!(completed.attempts[1].outcome, "completed");
}

#[tokio::test]
async fn after_tool_hook_runs_only_for_completed_tool_results() {
    let after_calls = Arc::new(AtomicUsize::new(0));
    let pending_attempts = Arc::new(AtomicUsize::new(0));
    let pending_context = pending_dispatch_context(
        PendingProbeMode::PendingWithKey,
        pending_attempts,
        Some(Arc::clone(&after_calls)),
        ToolRetryPolicy::Never,
    );
    let prepared = pending_prepared_call();
    let tool_context = tool_context_for_prepared(&pending_context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &pending_context,
        prepared,
        None,
        tool_context,
    )
    .await;

    assert!(matches!(launch, ToolCallLaunch::Pending(_)));
    assert_eq!(
        after_calls.load(Ordering::SeqCst),
        0,
        "launch-time Pending is not a completed tool result"
    );

    let done_attempts = Arc::new(AtomicUsize::new(0));
    let done_context = pending_dispatch_context(
        PendingProbeMode::Done,
        done_attempts,
        Some(Arc::clone(&after_calls)),
        ToolRetryPolicy::Never,
    );
    let prepared = pending_prepared_call();
    let tool_context = tool_context_for_prepared(&done_context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &done_context,
        prepared,
        None,
        tool_context,
    )
    .await;

    assert!(matches!(launch, ToolCallLaunch::Done(_)));
    assert_eq!(after_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn before_tool_hook_receives_resolved_argument_projection_policy() {
    let captured = Arc::new(std::sync::Mutex::new(None));
    let outcome = dispatch_tool_call(
        &projection_policy_dispatch_context(Arc::clone(&captured)),
        "seedy".to_string(),
        json!({}),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(
        captured.lock_recover().clone(),
        Some(crate::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"))
    );
}

#[tokio::test]
async fn dispatch_rejects_non_catalog_tool_before_provider_resolution() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: true,
        observed_execution_bindings: None,
    });
    let outcome = dispatch_tool_call(
        &exact_dispatch_context(provider),
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
}

#[tokio::test]
async fn non_catalog_tool_is_rejected_before_contract_resolution() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: false,
        observed_execution_bindings: None,
    });
    let outcome = dispatch_tool_call(
        &exact_dispatch_context(provider),
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
}

#[tokio::test]
async fn explicit_execution_grant_runs_non_catalog_tool_with_binding() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(ExactDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
        contract_available: false,
        observed_execution_bindings: Some(Arc::clone(&observed_execution_bindings)),
    });
    let context = exact_dispatch_context(provider);
    let grant = crate::ToolExecutionGrant::from_definition(named_beta_tool("host_only"))
        .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID)
        .with_execution_binding(json!({ "kind": "test", "route": "deferred" }));
    let pending = crate::sansio::PendingToolCall {
        call_id: "grant-call".to_string(),
        tool_name: "host_only".to_string(),
        args: json!({ "value": "ok" }),
        replay: None,
    };
    let prepared = match prepare_granted_tool_call_with_context(
        &context,
        &grant,
        pending,
        Some("grant-call".to_string()),
    )
    .await
    {
        ToolPreparationOutcome::Prepared(prepared) => *prepared,
        ToolPreparationOutcome::Completed(outcome) => {
            panic!("grant should prepare, got {:?}", outcome.record.output)
        }
    };
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .tool_execution_binding(grant.execution_binding.clone())
        .build();
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
}

#[tokio::test]
async fn dispatch_rejects_hidden_tool_before_contract_resolution() {
    let contracts_resolved = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(HiddenDispatchTools {
        contracts_resolved: Arc::clone(&contracts_resolved),
        executed: Arc::clone(&executed),
    });
    let outcome = dispatch_tool_call(
        &hidden_member_dispatch_context(provider),
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
}

#[tokio::test]
async fn dispatch_allows_unknown_mcp_args_when_schema_does_not_forbid_them() {
    let executed = Arc::new(AtomicUsize::new(0));
    let outcome = dispatch_tool_call(
        &strict_mcp_dispatch_context(Arc::clone(&executed)),
        "mcp__appworld__venmo_show_transactions".to_string(),
        json!({
            "min_datetime": "2024-01-01T00:00:00Z",
            "limit": 20
        }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn default_retry_policy_never_retries_safe_failures() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            ToolRetryPolicy::Never,
            Arc::clone(&attempts),
            usize::MAX,
            false,
            Arc::clone(&observed),
        ),
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(observed.lock_recover()[0].0, 1);
}

#[tokio::test]
async fn safe_retry_policy_retries_safe_failure_and_stops_on_success() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            ToolRetryPolicy::safe(3, 0, 0),
            Arc::clone(&attempts),
            2,
            false,
            Arc::clone(&observed),
        ),
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(outcome.attempts.len(), 2);
    assert_eq!(outcome.attempts[0].ordinal, 1);
    assert_eq!(outcome.attempts[0].outcome, "failed");
    assert!(
        outcome.attempts[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("transient"))
    );
    assert_eq!(outcome.attempts[0].delay_ms, Some(0));
    assert_eq!(outcome.attempts[1].ordinal, 2);
    assert_eq!(outcome.attempts[1].outcome, "completed");
    assert_eq!(outcome.attempts[1].delay_ms, None);
    let directory = tempfile::tempdir().expect("trace tempdir");
    let path = directory.path().join("tool-retry.trace.jsonl");
    let sink: Arc<dyn lash_trace::TraceSink> = Arc::new(lash_trace::JsonlTraceSink::new(&path));
    let tracing = crate::RuntimeExecutionTracing::new(
        sink,
        lash_trace::TraceContext::default(),
        lash_trace::TraceContext::default().for_session("tool-retry-session"),
    );
    tracing.emit_tool_call_completed(
        &outcome.record,
        &outcome.attempts,
        &crate::facade_support::SystemClock,
    );
    let emitted: lash_trace::TraceRecord = serde_json::from_str(
        std::fs::read_to_string(path)
            .expect("read tool trace")
            .trim(),
    )
    .expect("parse emitted tool trace");
    let lash_trace::TraceEvent::ToolCallCompleted { attempts, .. } = emitted.event else {
        panic!("expected emitted tool completion");
    };
    assert_eq!(attempts.expect("emitted attempt ladder").len(), 2);
    assert_eq!(
        observed
            .lock_recover()
            .iter()
            .map(|(attempt, max, _)| (*attempt, *max))
            .collect::<Vec<_>>(),
        vec![(1, 3), (2, 3)]
    );
}

#[tokio::test]
async fn scalar_after_tool_hook_runs_once_per_retry_attempt_before_exhaustion() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_retries = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context_with_after_observations(
            Arc::clone(&attempts),
            observed_attempts,
            Arc::clone(&observed_retries),
        ),
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        *observed_retries.lock_recover(),
        vec![
            ToolRetryStatus::Safe { after_ms: Some(0) },
            ToolRetryStatus::Safe { after_ms: Some(0) },
        ],
        "the after hook runs for each finalized attempt, before exhaustion is marked"
    );
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected exhausted failure");
    };
    assert_eq!(failure.retry, ToolRetryStatus::Exhausted { attempts: 2 });
}

#[derive(Default)]
struct SleepRecordingEffectController {
    sleeps: Arc<std::sync::Mutex<Vec<crate::RuntimeInvocation>>>,
}

impl crate::AwaitEventResolver for SleepRecordingEffectController {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for SleepRecordingEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
            self.sleeps.lock_recover().push(envelope.invocation);
            Ok(crate::RuntimeEffectOutcome::Sleep)
        } else {
            local_executor.execute(envelope).await
        }
    }
}

struct FailingSleepEffectController;

impl crate::AwaitEventResolver for FailingSleepEffectController {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for FailingSleepEffectController {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(&envelope.command, crate::RuntimeEffectCommand::Sleep { .. }) {
            Err(crate::RuntimeEffectControllerError::foreign(
                "test_sleep_rejected",
                format!("rejected {}", envelope.command.kind().as_str()),
            ))
        } else {
            local_executor.execute(envelope).await
        }
    }
}

#[tokio::test]
async fn retry_delay_crosses_effect_controller_as_sleep_effect() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::new(SleepRecordingEffectController::default());
    let mut context = exact_dispatch_context(Arc::new(RetryProbeTools {
        definition: retry_tool("retry_probe", ToolRetryPolicy::safe(3, 25, 25)),
        attempts: Arc::clone(&attempts),
        successes_after: 2,
        cancel_on_first: false,
        observed_attempts: Arc::clone(&observed),
        retry_after_ms: Some(25),
    }));
    context.effect_controller = RuntimeEffectControllerHandle::shared(recorder.clone());
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id("call-1".to_string())
        .build();

    let outcome = dispatch_tool_call_with_execution_context(
        &context,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
        tool_context,
    )
    .await;

    assert!(outcome.record.output.is_success());
    let sleeps = recorder.sleeps.lock_recover();
    assert_eq!(sleeps.len(), 1);
    assert_eq!(
        sleeps[0].effect_kind(),
        Some(crate::RuntimeEffectKind::Sleep)
    );
    assert_eq!(
        sleeps[0].replay_key(),
        Some("lash-tool:session:call-1:retry_probe:attempt:1:sleep")
    );
}

#[tokio::test]
async fn retry_sleep_controller_rejection_returns_explicit_tool_failure() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut context = exact_dispatch_context(Arc::new(RetryProbeTools {
        definition: retry_tool("retry_probe", ToolRetryPolicy::safe(3, 25, 25)),
        attempts: Arc::clone(&attempts),
        successes_after: 2,
        cancel_on_first: false,
        observed_attempts: Arc::clone(&observed),
        retry_after_ms: Some(25),
    }));
    context.effect_controller =
        RuntimeEffectControllerHandle::shared(Arc::new(FailingSleepEffectController));
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id("call-1".to_string())
        .build();

    let outcome = dispatch_tool_call_with_execution_context(
        &context,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
        tool_context,
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected failure");
    };
    assert_eq!(failure.code, "tool_retry_sleep_failed");
}

#[tokio::test]
async fn safe_retry_policy_marks_exhausted_after_final_attempt() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            ToolRetryPolicy::safe(2, 0, 0),
            Arc::clone(&attempts),
            usize::MAX,
            false,
            Arc::clone(&observed),
        ),
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let ToolCallOutcome::Failure(failure) = outcome.record.output.outcome else {
        panic!("expected failure");
    };
    assert_eq!(failure.retry, ToolRetryStatus::Exhausted { attempts: 2 });
}

#[tokio::test]
async fn cancellation_stops_retry_immediately() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            ToolRetryPolicy::safe(3, 0, 0),
            Arc::clone(&attempts),
            usize::MAX,
            true,
            Arc::clone(&observed),
        ),
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(matches!(
        outcome.record.output.outcome,
        ToolCallOutcome::Cancelled(_)
    ));
}

#[tokio::test]
async fn retry_context_has_stable_replay_key_across_attempts() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = retry_dispatch_context(
        ToolRetryPolicy::safe(3, 0, 0),
        Arc::clone(&attempts),
        3,
        false,
        Arc::clone(&observed),
    );
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id("call-1".to_string())
        .build();
    let outcome = dispatch_tool_call_with_execution_context(
        &context,
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
        tool_context,
    )
    .await;

    assert!(outcome.record.output.is_success());
    let observed = observed.lock_recover();
    assert_eq!(observed.len(), 3);
    assert_eq!(
        observed
            .iter()
            .map(|(attempt, max, _)| (*attempt, *max))
            .collect::<Vec<_>>(),
        vec![(1, 3), (2, 3), (3, 3)]
    );
    let keys = observed
        .iter()
        .map(|(_, _, key)| key.clone())
        .collect::<Vec<_>>();
    assert!(keys.iter().all(|key| key == &keys[0]));
    assert_eq!(
        keys[0].as_deref(),
        Some("lash-tool:session:call-1:retry_probe")
    );
}

#[tokio::test]
async fn idempotent_retry_policy_uses_journaled_attempts_without_provider_replay_key() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = dispatch_tool_call(
        &retry_dispatch_context(
            ToolRetryPolicy::idempotent(3, 0, 0),
            Arc::clone(&attempts),
            usize::MAX,
            false,
            Arc::clone(&observed),
        ),
        "retry_probe".to_string(),
        json!({ "value": "ok" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    let observed = observed.lock_recover();
    assert!(
        observed
            .iter()
            .all(|(_, max_attempts, replay_key)| *max_attempts == 3 && replay_key.is_none())
    );
}

#[tokio::test]
async fn batch_returns_explicit_errors_without_runtime_execution_context() {
    let outcome = dispatch_orchestrating_tool_call(
        &dispatch_context(),
        "batch",
        json!({
            "tool_calls": [
                {"tool": "alpha", "parameters": {}},
                {"tool": "beta", "parameters": {"value": "ok"}},
                {"tool": "beta", "parameters": {"value": "fail"}}
            ]
        }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(outcome.record.tool, "batch");
    let value = outcome.record.output.value_for_projection();
    let results = value
        .get("results")
        .and_then(|value| value.as_array())
        .expect("results");
    assert_eq!(results.len(), 3);
    assert_eq!(
        results
            .iter()
            .filter(|item| item.get("success").and_then(|value| value.as_bool()) == Some(false))
            .count(),
        3
    );
    assert_eq!(results[0].get("tool"), Some(&json!("tool:alpha")));
    assert_eq!(
        results[0]
            .get("error")
            .and_then(|value| value.get("message"))
            .and_then(|value| value.as_str()),
        Some("tool batch orchestration is unavailable outside process replay")
    );
}

#[tokio::test]
async fn batch_rejects_nested_batch_as_partial_failure() {
    let outcome = dispatch_orchestrating_tool_call(
        &dispatch_context(),
        "batch",
        json!({
            "tool_calls": [
                {"tool": "batch", "parameters": {"tool_calls": []}}
            ]
        }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    let value = outcome.record.output.value_for_projection();
    let first = value
        .get("results")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .expect("first result");
    assert_eq!(
        first.get("error"),
        Some(&json!("Tool 'batch' is not allowed inside batch"))
    );
}

#[tokio::test]
async fn batch_marks_overflow_calls_as_failures() {
    let tool_calls = (0..26)
        .map(|_| json!({"tool": "alpha", "parameters": {}}))
        .collect::<Vec<_>>();

    let outcome = dispatch_tool_call(
        &dispatch_context(),
        "batch".to_string(),
        json!({ "tool_calls": tool_calls }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    let value = outcome.record.output.value_for_projection();
    let error = value
        .get("message")
        .and_then(|value| value.as_str())
        .expect("string error result");
    assert!(
        error.contains("tool_calls") && error.contains("has more than 25 items"),
        "{error}",
    );
}

#[tokio::test]
async fn batch_does_not_run_child_tools_without_runtime_execution_context() {
    let barrier = Arc::new(Barrier::new(2));
    let started = Arc::new(AtomicUsize::new(0));
    let outcome = dispatch_orchestrating_tool_call(
        &parallel_dispatch_context(Arc::clone(&barrier), Arc::clone(&started)),
        "batch",
        json!({
            "tool_calls": [
                {"tool": "probe_a", "parameters": {}},
                {"tool": "probe_b", "parameters": {}}
            ]
        }),
    )
    .await;

    assert!(outcome.record.output.is_success());
    assert_eq!(started.load(Ordering::SeqCst), 0);
    let value = outcome.record.output.value_for_projection();
    let results = value
        .get("results")
        .and_then(|value| value.as_array())
        .expect("results");
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|item| item.get("success").and_then(|value| value.as_bool()) == Some(false))
    );
}

/// The v1 provider seam law: an opted-in leaf is called through the public
/// coordinator path and every declared intent kind is realized after its final
/// attempt is committed, in declaration order.
#[tokio::test]
async fn attempt_context_provider_realizes_every_v1_intent_through_the_coordinator() {
    let definition = named_beta_tool("attempt_intents");
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(AttemptIntentTools {
        definition: definition.clone(),
        calls: Arc::clone(&calls),
    });
    let mut context = exact_dispatch_context(provider);
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
    let registry = Arc::new(crate::TestLocalProcessRegistry::default());
    let event_types = ["signal.resume", "attempt.intent.note"]
        .into_iter()
        .map(|name| crate::ProcessEventType {
            name: name.to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        })
        .collect::<Vec<_>>();
    registry
        .register_process_with_observers(
            crate::ProcessRegistration::new(
                "attempt-intents-target",
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::Rerunnable,
                crate::ProcessProvenance::host(),
            )
            .with_extra_event_types(event_types),
            &["session".to_string()],
        )
        .await
        .expect("register intent target");
    let registry = Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>;
    context.processes = crate::testing::effect_backed_process_service(Arc::clone(&registry));
    context.trigger_router = Some(crate::TriggerRouter::new(
        Arc::new(crate::facade_support::InMemoryTriggerStore::default())
            as Arc<dyn crate::TriggerStore>,
        crate::testing::process_work_wiring_for_registry(registry),
    ));

    let prepared = crate::PreparedToolCall::from_parts(
        "attempt-intents-call",
        definition.id().to_string(),
        "attempt_intents",
        json!({"value": "drive"}),
        None,
        serde_json::Value::Null,
    );
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .prepared_call(&prepared)
        .cancellation_token(Some(tokio_util::sync::CancellationToken::new()))
        .build();
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
            .all(|outcome| matches!(outcome, crate::ToolIntentExecutionOutcome::Executed { .. }))
    );
}

#[tokio::test]
async fn empty_batch_dispatches_v0_and_v2_to_a_typed_protocol_refusal() {
    let context = dispatch_context();
    for recorded in [0, 2] {
        let outcomes = execute_final_tool_intents(
            &context,
            Some("empty-version-call"),
            &crate::ToolIntents {
                protocol_version: recorded,
                intents: Vec::new(),
            },
            None,
        )
        .await;
        assert_eq!(
            outcomes,
            vec![crate::ToolIntentExecutionOutcome::ProtocolRefused {
                refusal: crate::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded },
            }]
        );
    }
}
include!("tests/granted_dispatch.rs");
include!("tests/intent_laws.rs");
include!("tests/pending_park_laws.rs");
