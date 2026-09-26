//! A frame switch's follow-on under crashes on Restate (ADR 0101 §3,
//! FIG-3542).
//!
//! A real lash turn runs in a handler on the server double: its model calls a
//! tool that switches agent frame, the switch commit leaves the follow-on owed
//! on the session head, and the follow-on answers in the switched frame. A
//! clean run fixes the reference. Then, for every journal point of the turn's
//! handler — among them the gap between the switch commit and the follow-on —
//! a fresh backend under the same seed drops the handler just before the
//! server stores that frame and replays the invocation. The drive owns its
//! chain, so the replay continues it in order: every crash reaches the
//! reference answer, the follow-on commits exactly once, and the head owes
//! nothing afterwards.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use serde_json::json;

const TURN_HOST: &str = "LashTestHandlerHost";
const SESSION: &str = "follow-on-crash";
const TURN: &str = "turn-1";
const FOLLOW_ON: &str = "turn-1:agent-frame:1";
const TOOL: &str = "switch_frame";
const TASK: &str = "answer from the switched frame";

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..Default::default()
    }
}

/// A stateless scripted model: the switched frame's context carries the task,
/// the first frame's does not, so a re-executed call answers the same again.
fn model_reply(request: &LlmRequest) -> LlmResponse {
    if serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains(TASK)
    {
        response(vec![LlmOutputPart::Text {
            text: "follow-on done".into(),
            response_meta: None,
        }])
    } else {
        response(vec![LlmOutputPart::ToolCall {
            call_id: "call-switch".into(),
            tool_name: TOOL.into(),
            input_json: "{}".into(),
            replay: None,
        }])
    }
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Hand the task to another agent frame.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    )
}

struct SwitchFrameTool;

#[async_trait::async_trait]
impl lash_core::ToolProvider for SwitchFrameTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(json!({"switched": true}))
            .with_control(lash_core::ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("follow-on-frame")
                    .expect("non-empty caller material"),
                initial_nodes: Vec::new(),
                task: Some(TASK.to_string()),
            })
            .into()
    }
}

/// What one run of the turn observed.
#[derive(Debug)]
struct Run {
    answer: String,
    llm_calls: usize,
    follow_on_calls: usize,
    crashes: u64,
    follow_on_committed: bool,
    owed_after: Option<lash_core::store::PendingFollowOn>,
    journal: Vec<(MessageType, Option<String>)>,
}

fn owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-restate-test", "follow-on-crash-replay")
}

async fn run_turn(seed: u64, crash: Option<CrashRule>) -> Run {
    let backend: RestateTestBackend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    if let Some(rule) = crash {
        backend.server().crash_on(rule);
    }
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let follow_on_calls = Arc::new(AtomicUsize::new(0));
    let provider = {
        let llm_calls = Arc::clone(&llm_calls);
        let follow_on_calls = Arc::clone(&follow_on_calls);
        lash_core::testing::TestProvider::builder()
            .kind("follow-on-crash-replay")
            .complete(move |request: LlmRequest| {
                llm_calls.fetch_add(1, Ordering::SeqCst);
                if serde_json::to_string(&request.messages)
                    .unwrap_or_default()
                    .contains(TASK)
                {
                    follow_on_calls.fetch_add(1, Ordering::SeqCst);
                }
                async move { Ok::<_, LlmTransportError>(model_reply(&request)) }
            })
            .build()
            .into_handle()
    };
    let core =
        lash::LashCore::standard_builder(backend.lash_backend(), lash::TurnBudget::Unbounded)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .provider(provider)
            .model(
                lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(200_000)
                    .build()
                    .expect("model spec"),
            )
            .tools(Arc::new(SwitchFrameTool) as Arc<dyn lash_core::ToolProvider>)
            .build(owner())
            .expect("build the lash core");
    let session = core
        .session(SESSION)
        .open()
        .await
        .expect("open the session");
    let turn_id = lash::TurnId::from(TURN);
    let admitted = lash_core::AdmittedScope::new(session.turn_scope(turn_id.clone()));
    let answer = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let answer = Arc::clone(&answer);
        Arc::new(move |scoped| {
            let session = session.clone();
            let turn_id = turn_id.clone();
            let answer = Arc::clone(&answer);
            Box::pin(async move {
                let output = session
                    .turn(lash::TurnInput::text("hand this off"))
                    .turn_id(turn_id)
                    .advanced()
                    .run_with_scope(scoped)
                    .await;
                *answer.lock().unwrap() = Some(match output {
                    Ok(output) => match output.result.assistant_message() {
                        Some(message) => message.to_owned(),
                        None => format!("no message: {:?}", output.result.outcome),
                    },
                    Err(error) => format!("error: {error}"),
                });
            })
        })
    };
    let server = backend.server();
    let completed = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        backend.run_in_handler(admitted, attempt),
    )
    .await;
    match completed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => *answer.lock().unwrap() = Some(format!("stuck: {error}")),
        Err(_) => *answer.lock().unwrap() = Some("stuck: timed out".to_string()),
    }
    server.settle().await;
    let journal = server
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with(TURN_HOST))
        .map(|view| {
            server
                .journal(&view.id)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.ty, entry.name))
                .collect()
        })
        .unwrap_or_default();
    let store = lash_core::SessionStoreFactory::open_existing_store_by_id(
        backend.stores().session_store_factory().as_ref(),
        &lash_core::SessionId::from(SESSION),
    )
    .await
    .expect("open the session store")
    .expect("the session exists");
    let follow_on_committed = lash_core::store::SessionCommitStore::committed_turn_exists(
        store.as_ref(),
        &lash::TurnId::from(FOLLOW_ON),
    )
    .await
    .expect("read the follow-on's receipt");
    let owed_after = lash_core::store::SessionCommitStore::load_session_head_meta(store.as_ref())
        .await
        .expect("load the head")
        .and_then(|head| head.pending_follow_on);
    let answer = answer
        .lock()
        .unwrap()
        .clone()
        .expect("the turn recorded an answer");
    Run {
        answer,
        llm_calls: llm_calls.load(Ordering::SeqCst),
        follow_on_calls: follow_on_calls.load(Ordering::SeqCst),
        crashes: server.stats().crashes,
        follow_on_committed,
        owed_after,
        journal,
    }
}

/// Every crash point the turn handler's journal offers: each command it
/// stored and each `ctx.run` result, named.
fn crash_points(reference: &Run) -> Vec<(CrashRule, Option<String>)> {
    let mut points = Vec::new();
    let commands = reference.journal.iter().filter(|(ty, _)| ty.is_command());
    for (index, (ty, name)) in commands.enumerate().skip(1) {
        // The SDK starts a run's closure as it writes the RunCommand, so a
        // crash before the server stores that command may already have run
        // the effect: it too is a lost run.
        let lost_on_command = (*ty == MessageType::RunCommand)
            .then(|| name.clone())
            .flatten();
        points.push((
            CrashRule::new(CrashPoint::BeforeCommand { index })
                .service(TURN_HOST)
                .within_attempts(1),
            lost_on_command,
        ));
        if *ty == MessageType::RunCommand {
            points.push((
                CrashRule::new(CrashPoint::BeforeRunResult { name: name.clone() })
                    .service(TURN_HOST)
                    .within_attempts(1),
                name.clone(),
            ));
        }
    }
    points
}

/// Crash points where lash itself does not recover yet, shared with the tool
/// turn's crash matrix (FIG-3678): a re-executed turn-input claim run can find
/// its lost attempt's claim still holding the input, on some runs.
const KNOWN_DIVERGENCES: &[&str] = &[
    "BeforeCommand { index: 2 }",
    "BeforeRunResult { name: Some(\"lash:follow-on-crash:turn-1:accept_turn_input:claim_accepted_turn_input\") }",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_anywhere_in_a_switching_turn_runs_its_follow_on_exactly_once() {
    let seed = 0x3542;
    let reference = run_turn(seed, None).await;
    assert_eq!(reference.answer, "follow-on done");
    assert_eq!(reference.crashes, 0);
    assert_eq!((reference.llm_calls, reference.follow_on_calls), (2, 1));
    assert!(reference.follow_on_committed);
    assert_eq!(reference.owed_after, None);
    let points = crash_points(&reference);
    assert!(!points.is_empty(), "the turn handler has journal points");
    let mut violations = Vec::new();
    for (rule, lost_run) in points {
        let label = format!("{:?}", rule.point);
        let run = run_turn(seed, Some(rule)).await;
        // A lost run re-executes once at most; only the follow-on's own call
        // may be that run for the follow-on count.
        let lost = lost_run.is_some();
        let expected_calls = if lost { 2..=3 } else { 2..=2 };
        let expected_follow_on_calls = if lost { 1..=2 } else { 1..=1 };
        if run.crashes != 1
            || run.answer != reference.answer
            || !expected_calls.contains(&run.llm_calls)
            || !expected_follow_on_calls.contains(&run.follow_on_calls)
            || !run.follow_on_committed
            || run.owed_after.is_some()
        {
            violations.push(format!(
                "{label} ({lost_run:?}): crashes={} answer={:?} llm_calls={} follow_on_calls={} committed={} owed_after={:?}",
                run.crashes,
                run.answer,
                run.llm_calls,
                run.follow_on_calls,
                run.follow_on_committed,
                run.owed_after
            ));
        }
    }
    let unexplained: Vec<_> = violations
        .iter()
        .filter(|violation| {
            !KNOWN_DIVERGENCES
                .iter()
                .any(|known| violation.starts_with(known))
        })
        .collect();
    assert!(
        unexplained.is_empty(),
        "crash points that did not run the follow-on exactly once:\n{unexplained:#?}"
    );
}
