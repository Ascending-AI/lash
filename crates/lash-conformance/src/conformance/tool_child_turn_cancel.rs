//! Turn-cancel laws for tool calls that run as effect-group children.
//!
//! Since FIG-3397 a turn's tool calls run as `ToolInvocation` children of a
//! durable effect group: on the in-process tiers in host-owned child tasks, on
//! Restate in the endpoint's dispatch invocations. These laws drive a real
//! turn on the tier (through its [`ConformanceTurnRunner`](crate::ConformanceTurnRunner))
//! and pin what turn control means for a child: an after-step stop does not
//! cut a child's retry sleep short, a child spawned under a follow-on agent
//! frame waits under that frame's physical turn-cancel gate, and an immediate
//! cancel closes the turn's group under `Cancel`, dropping a child that
//! ignores its cancellation token.

use crate::admit;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::{
    AwaitEventKey, AwaitEventWaitIdentity, EffectHost, ExecutionScope, Resolution, ResolveOutcome,
    TurnAddress, TurnCancelMode, TurnCancelOutcome, TurnCancelRequest, TurnOutcome, TurnStop,
    TurnWorkDriver,
};

/// Long enough that a cancel request issued after the first attempt lands
/// while the retry sleep is still parked on every tier, live Restate included.
const RETRY_AFTER_MS: u64 = 3_000;

/// The session store the turn-work driver and the runtime share: the store
/// set's own, created for `session_id`, whose turn control the tier's host
/// owns.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn session_store(
    stores: &Arc<dyn crate::StoreSet>,
    session_id: &SessionId,
) -> Arc<dyn crate::RuntimePersistence> {
    stores
        .session_store_factory()
        .create_store(&crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
        .await
        .expect("create the tool-child turn-cancel session store")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(
    host: &Arc<dyn EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    store: Arc<dyn crate::RuntimePersistence>,
    session_id: &SessionId,
    plugin: Arc<dyn crate::facade_support::PluginFactory>,
    model: crate::testing::TestProvider,
) -> crate::LashRuntime {
    let mut config = crate::conformance::store_set_host_config(
        stores.as_ref(),
        Arc::clone(host),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    config.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    Box::pin(
        crate::LashRuntime::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            crate::testing::runtime_lease_owner(),
        )
        .with_session_id(session_id)
        .with_policy(policy)
        .with_initial_state(state)
        .with_runtime_host(config)
        .with_plugin_factories(
            crate::testing::test_standard_protocol_factories()
                .into_iter()
                .chain([plugin])
                .collect(),
        )
        .with_store(store)
        .with_process_work(crate::testing::process_work_wiring_for_registry(
            stores.process_registry(),
        ))
        .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
        .build(),
    )
    .await
    .expect("build tool-child turn-cancel conformance runtime")
}

/// Runs `runtime`'s turn `turn_id` on the tier's runner in its own task and
/// hands back the join handle; the assembled turn is the task's output.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a runner that drops the turn is a fixture defect"
)]
fn spawn_turn(
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    mut runtime: crate::LashRuntime,
    session_id: &SessionId,
    turn_id: &TurnId,
    text: &str,
) -> tokio::task::JoinHandle<Result<crate::AssembledTurn, crate::RuntimeError>> {
    let admitted = admit(ExecutionScope::turn(session_id, turn_id));
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.clone());
    crate::task::spawn(async move {
        let (turn_tx, turn_rx) = tokio::sync::oneshot::channel();
        runner
            .run_turn(
                admitted,
                Box::new(move |scope| {
                    Box::pin(async move {
                        let turn = runtime
                            .stream_turn(
                                input,
                                crate::TurnOptions::new(
                                    tokio_util::sync::CancellationToken::new(),
                                    scope,
                                ),
                            )
                            .await;
                        let _ = turn_tx.send(turn);
                    })
                }),
            )
            .await;
        turn_rx
            .await
            .expect("the tier's turn runner ran the conformance turn")
    })
}

fn scripted_model(
    responses: Vec<crate::LlmResponse>,
) -> (crate::testing::TestProvider, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let calls = Arc::clone(&calls);
                let responses = Arc::clone(&responses);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let next = responses
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop_front();
                    Ok(next.unwrap_or_else(|| panic!("unexpected extra model call")))
                }
            }
        })
        .build();
    (model, calls)
}

fn tool_call(call_id: &str, tool_name: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        ..crate::LlmResponse::default()
    }
}

fn text(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        ..crate::LlmResponse::default()
    }
}

fn empty_object_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

#[derive(Clone)]
struct RetryOnceTool {
    attempts: Arc<AtomicUsize>,
}

fn retry_once_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:conformance_retry_once",
        "conformance_retry_once",
        "Fails once with a safe retry, then succeeds.",
        empty_object_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_retry_policy(crate::ToolRetryPolicy::safe(
        2,
        RETRY_AFTER_MS,
        RETRY_AFTER_MS,
    ))
}

#[async_trait::async_trait]
impl crate::ToolProvider for RetryOnceTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![retry_once_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "conformance_retry_once").then(|| Arc::new(retry_once_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return crate::ToolOutcome::retryable_failure(
                crate::ToolFailureClass::External,
                "transient",
                "transient failure",
                Some(RETRY_AFTER_MS),
            )
            .into();
        }
        crate::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

/// An after-step stop requested while a group child sleeps before its retry
/// does not wake the sleep: the retry runs after the full delay, the
/// iteration that issued the tool call finishes, and the turn stops at that
/// boundary with the request's after-step evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_after_step_stop_during_a_child_retry_sleep_finishes_the_iteration(
    prefix: &str,
    host: Arc<dyn EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    // These laws start no process; the substrate is part of the shared
    // turn-runner fixture.
    _process_work: Arc<dyn crate::ProcessWorkSubstrate>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-retry-sleep-session"));
    let turn_id = TurnId::from(format!("{prefix}-retry-sleep-turn"));
    let tool = RetryOnceTool {
        attempts: Arc::new(AtomicUsize::new(0)),
    };
    let attempts = Arc::clone(&tool.attempts);
    let plugin: Arc<dyn crate::facade_support::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-retry-once",
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(tool)),
        ));
    let (model, model_calls) = scripted_model(vec![
        tool_call("retry-call-1", "conformance_retry_once"),
        text("finished after the retry"),
    ]);
    let store = session_store(&stores, &session_id).await;
    let runtime = build_runtime(
        &host,
        &stores,
        Arc::clone(&store),
        &session_id,
        plugin,
        model,
    )
    .await;
    let turn = spawn_turn(runner, runtime, &session_id, &turn_id, "retry once");

    wait_until("the first attempt fails retryably", || {
        attempts.load(Ordering::SeqCst) == 1
    })
    .await;
    let driver = TurnWorkDriver::for_session(Arc::clone(&host), session_id.clone(), store);
    let receipt = driver
        .request_cancel(
            TurnCancelRequest::new(
                TurnAddress::new(session_id.clone(), turn_id.clone()),
                "stop-in-retry",
                None,
            )
            .mode(TurnCancelMode::AfterStep),
        )
        .await
        .expect("request an after-step stop during the retry sleep");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !turn.is_finished(),
        "the turn stays parked in its retry sleep"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "an after-step stop does not wake the retry sleep early"
    );

    let turn = tokio::time::timeout(Duration::from_secs(60), turn)
        .await
        .expect("the turn stops after the retried step")
        .expect("join the turn task")
        .expect("the turn assembles");
    let TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) = &turn.outcome else {
        panic!("turn did not stop on cancellation: {:?}", turn.outcome);
    };
    assert_eq!(evidence.request_id, "stop-in-retry");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(evidence.honoured_after_step, Some(0));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "the retry runs after wake; the iteration finishes before the stop lands"
    );
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
}

struct FollowOnPendingTools {
    completion_key_tx: Mutex<Option<tokio::sync::oneshot::Sender<AwaitEventKey>>>,
    executions: Arc<AtomicUsize>,
    pending_attempts: AtomicUsize,
}

fn follow_on_switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:conformance_follow_on_switch",
        "conformance_follow_on_switch",
        "Switch to a follow-on physical frame.",
        empty_object_schema(),
        serde_json::json!({ "type": "object" }),
    )
}

fn follow_on_pending_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:conformance_follow_on_pending",
        "conformance_follow_on_pending",
        "Wait for an externally completed value in the follow-on frame.",
        empty_object_schema(),
        serde_json::json!({
            "type": "object",
            "properties": { "answer": {} },
            "required": ["answer"],
            "additionalProperties": true
        }),
    )
    .with_retry_policy(crate::ToolRetryPolicy::safe(2, 1, 1))
}

#[async_trait::async_trait]
impl crate::ToolProvider for FollowOnPendingTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![
            follow_on_switch_tool().manifest(),
            follow_on_pending_tool().manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        match name {
            "conformance_follow_on_switch" => Some(Arc::new(follow_on_switch_tool().contract())),
            "conformance_follow_on_pending" => Some(Arc::new(follow_on_pending_tool().contract())),
            _ => None,
        }
    }

    fn attempt_may_defer(&self, tool_id: &crate::ToolId) -> bool {
        tool_id == follow_on_pending_tool().id()
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the frame key material is a non-empty literal"
    )]
    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        match call.name() {
            "conformance_follow_on_switch" => crate::ToolOutcome::ok(serde_json::json!({
                "switched": true
            }))
            .with_control(crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("conformance-follow-on")
                    .expect("non-empty frame key material"),
                initial_nodes: Vec::new(),
                task: Some("complete the pending tool".to_string()),
            })
            .into(),
            "conformance_follow_on_pending" => {
                if self.pending_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return crate::ToolOutcome::retryable_failure(
                        crate::ToolFailureClass::External,
                        "transient",
                        "retry the follow-on tool once",
                        Some(1),
                    )
                    .into();
                }
                let key = match call.context.completion_key() {
                    Ok(key) => key,
                    Err(error) => return crate::ToolOutcome::err_fmt(error).into(),
                };
                if let Some(tx) = self
                    .completion_key_tx
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = tx.send(key);
                }
                crate::ToolOutcome::pending(crate::PendingCompletion::new()).into()
            }
            other => crate::ToolOutcome::err_fmt(format!("unknown tool `{other}`")).into(),
        }
    }
}

/// A deferred tool call issued under a follow-on agent frame parks under the
/// follow-on frame's *physical* turn-cancel gate, not the root turn's admitted
/// authority; resolving its completion finishes the turn, and the gate is
/// released with it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_follow_on_pending_child_waits_under_the_follow_on_turn_cancel_gate(
    prefix: &str,
    host: Arc<dyn EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    // These laws start no process; the substrate is part of the shared
    // turn-runner fixture.
    _process_work: Arc<dyn crate::ProcessWorkSubstrate>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-follow-on-session"));
    let root_turn_id = TurnId::from(format!("{prefix}-follow-on-root"));
    let follow_turn_id = TurnId::from(format!("{root_turn_id}:agent-frame:1"));
    let executions = Arc::new(AtomicUsize::new(0));
    let (completion_key_tx, completion_key_rx) = tokio::sync::oneshot::channel();
    let tools = Arc::new(FollowOnPendingTools {
        completion_key_tx: Mutex::new(Some(completion_key_tx)),
        executions: Arc::clone(&executions),
        pending_attempts: AtomicUsize::new(0),
    });
    let plugin: Arc<dyn crate::facade_support::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-follow-on-pending",
            crate::facade_support::PluginSpec::new().with_tool_provider(tools),
        ));
    let (model, model_calls) = scripted_model(vec![
        tool_call("switch-frame", "conformance_follow_on_switch"),
        tool_call("pending-in-follow-on", "conformance_follow_on_pending"),
        text("finished after the follow-on wait"),
    ]);
    let store = session_store(&stores, &session_id).await;
    let runtime = build_runtime(&host, &stores, store, &session_id, plugin, model).await;
    let mut turn = spawn_turn(
        runner,
        runtime,
        &session_id,
        &root_turn_id,
        "switch and wait",
    );

    let completion_key = tokio::select! {
        key = completion_key_rx => match key {
            Ok(key) => key,
            Err(_) => {
                let ended = (&mut turn).await;
                let summary = ended.as_ref().map(|turn| turn.as_ref().map(|turn| (
                    &turn.outcome,
                    &turn.errors,
                    turn.tool_calls
                        .iter()
                        .map(|call| (call.tool.clone(), call.output.value_for_projection()))
                        .collect::<Vec<_>>(),
                )));
                panic!(
                    "the pending tool dropped its completion key after {} executions; turn: {summary:?}",
                    executions.load(Ordering::SeqCst)
                )
            }
        },
        result = &mut turn => panic!("the turn ended before the pending launch: {result:?}"),
    };
    let follow_gate = host
        .await_event_key(
            &ExecutionScope::turn(&session_id, &follow_turn_id),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await
        .expect("follow-on turn-cancel gate key");
    let root_gate = host
        .await_event_key(
            &ExecutionScope::turn(&session_id, &root_turn_id),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await
        .expect("root turn-cancel gate key");
    let outstanding = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let outstanding = host
                .list_outstanding_await_event_keys(&session_id)
                .await
                .expect("list outstanding waits");
            if outstanding.contains(&follow_gate) || turn.is_finished() {
                break outstanding;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the pending child registers its turn-cancel gate");
    assert!(
        outstanding.contains(&follow_gate),
        "the durable cancel gate follows the follow-on physical turn: {outstanding:?}"
    );
    assert!(
        !outstanding.contains(&root_gate),
        "the pending child does not wait under the root turn's gate: {outstanding:?}"
    );

    assert_eq!(
        host.resolve_await_event(
            &completion_key,
            Resolution::Ok(serde_json::json!({ "answer": "complete" })),
        )
        .await
        .expect("resolve the follow-on completion"),
        ResolveOutcome::Accepted
    );
    let turn = tokio::time::timeout(Duration::from_secs(60), turn)
        .await
        .expect("the resolved follow-on turn finishes")
        .expect("join the turn task")
        .expect("the turn succeeds");
    assert!(
        matches!(turn.outcome, TurnOutcome::Finished(_)),
        "follow-on outcome: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(model_calls.load(Ordering::SeqCst), 3);
    assert_eq!(executions.load(Ordering::SeqCst), 3);
    let outstanding = host
        .list_outstanding_await_event_keys(&session_id)
        .await
        .expect("list outstanding waits after the turn");
    assert!(
        !outstanding.contains(&follow_gate),
        "the follow-on gate is released with the turn: {outstanding:?}"
    );
}

/// Flips its flag when the tool future that owns it is dropped.
struct DropWitness(Arc<std::sync::atomic::AtomicBool>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct IgnoresCancellationTool {
    started: Arc<AtomicUsize>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}

fn ignores_cancellation_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:conformance_ignores_cancellation",
        "conformance_ignores_cancellation",
        "Parks forever without watching its cancellation token.",
        empty_object_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for IgnoresCancellationTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![ignores_cancellation_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "conformance_ignores_cancellation")
            .then(|| Arc::new(ignores_cancellation_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let _witness = DropWitness(Arc::clone(&self.dropped));
        self.started.fetch_add(1, Ordering::SeqCst);
        // Deliberately never polls `call.context.cancellation_token()`: the
        // law pins that turn cancel does not depend on the tool cooperating.
        std::future::pending::<()>().await;
        unreachable!("the parked tool never completes")
    }
}

/// An immediate turn cancel closes the turn's tool-child group under
/// `Cancel`: the turn stops cancelled with the request's evidence, the call
/// settles cancelled, and a child that never watches its cancellation token
/// is dropped rather than left running under the turn's session.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_cancelled_turn_drops_a_tool_child_that_ignores_cancellation(
    prefix: &str,
    host: Arc<dyn EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    // These laws start no process; the substrate is part of the shared
    // turn-runner fixture.
    _process_work: Arc<dyn crate::ProcessWorkSubstrate>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-ignores-cancel-session"));
    let turn_id = TurnId::from(format!("{prefix}-ignores-cancel-turn"));
    let started = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tool = IgnoresCancellationTool {
        started: Arc::clone(&started),
        dropped: Arc::clone(&dropped),
    };
    let plugin: Arc<dyn crate::facade_support::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-ignores-cancellation",
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(tool)),
        ));
    // The native binding honours an immediate cancel found at the step
    // boundary by firing the cooperative token, so the next model call may
    // start before it observes the token; the law does not pin that call.
    let (model, _model_calls) = scripted_model(vec![
        tool_call("ignores-cancel-call", "conformance_ignores_cancellation"),
        text("unreachable after the cancel"),
    ]);
    let store = session_store(&stores, &session_id).await;
    let runtime = build_runtime(
        &host,
        &stores,
        Arc::clone(&store),
        &session_id,
        plugin,
        model,
    )
    .await;
    let turn = spawn_turn(runner, runtime, &session_id, &turn_id, "park and cancel");

    wait_until("the tool child starts", || {
        started.load(Ordering::SeqCst) == 1
    })
    .await;
    let driver = TurnWorkDriver::for_session(Arc::clone(&host), session_id.clone(), store);
    let receipt = driver
        .request_cancel(TurnCancelRequest::new(
            TurnAddress::new(session_id.clone(), turn_id.clone()),
            "cancel-ignoring-child",
            None,
        ))
        .await
        .expect("request an immediate turn cancel");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));

    let turn = tokio::time::timeout(Duration::from_secs(30), turn)
        .await
        .expect("the cancelled turn stops although its tool ignores cancellation")
        .expect("join the turn task")
        .expect("the turn assembles");
    let TurnOutcome::Stopped(TurnStop::Cancelled { evidence }) = &turn.outcome else {
        panic!("turn did not stop on cancellation: {:?}", turn.outcome);
    };
    assert_eq!(evidence.request_id, "cancel-ignoring-child");
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    wait_until("the cancel-ignoring tool child is dropped", || {
        dropped.load(Ordering::SeqCst)
    })
    .await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "the cancelled child is not re-run"
    );
}
