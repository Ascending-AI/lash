//! The turn crash matrix on Restate, keyed to journal points.
//!
//! A real lash turn — one LLM call that asks for a tool, the tool, a second
//! LLM call that answers — runs in a handler on the server double. A clean
//! run fixes the reference: its journal and its answer. Then, for every
//! journal point of the turn's handler and of the tool child's dispatch
//! handler, a fresh backend under the same seed drops the handler just before
//! the server stores that frame and replays the invocation. Every crash must
//! reach the reference answer, and each effect runs exactly once unless its
//! result was the frame the crash lost — then at least once, never more than
//! twice.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use serde_json::json;

const TURN_HOST: &str = "LashTestHandlerHost";
const DISPATCH: &str = "EffectGroupDispatch";
const TOOL: &str = "count_call";

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        response_metadata: Default::default(),
        ..Default::default()
    }
}

/// A stateless scripted model: the reply depends only on the request, so a
/// re-executed call (its result lost to a crash) answers the same again.
fn model_reply(request: &LlmRequest) -> LlmResponse {
    let saw_tool_result = serde_json::to_string(&request.messages)
        .unwrap_or_default()
        .contains("counted");
    if saw_tool_result {
        response(vec![LlmOutputPart::Text {
            text: "done".into(),
            response_meta: None,
        }])
    } else {
        response(vec![LlmOutputPart::ToolCall {
            call_id: "call-1".into(),
            tool_name: TOOL.into(),
            input_json: "{}".into(),
            replay: None,
        }])
    }
}

struct CountingTool {
    executions: Arc<AtomicUsize>,
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Count this call.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
    }
}

/// `(id, service, entries in journal order)` of one invocation.
type InvocationJournal = (String, String, Vec<(MessageType, Option<String>)>);

/// What one run of the turn observed.
#[derive(Debug)]
struct Run {
    answer: String,
    llm_calls: usize,
    tool_executions: usize,
    crashes: u64,
    /// Every invocation's journal, by id.
    journals: Vec<InvocationJournal>,
}

fn owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-restate-test", "turn-crash-replay")
}

async fn run_turn(seed: u64, crash: Option<CrashRule>) -> Run {
    let backend: RestateTestBackend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    if let Some(rule) = crash {
        backend.server().crash_on(rule);
    }
    let llm_calls = Arc::new(AtomicUsize::new(0));
    let tool_executions = Arc::new(AtomicUsize::new(0));
    let provider = {
        let llm_calls = Arc::clone(&llm_calls);
        lash_core::testing::TestProvider::builder()
            .kind("turn-crash-replay")
            .complete(move |request: LlmRequest| {
                llm_calls.fetch_add(1, Ordering::SeqCst);
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
            .tools(Arc::new(CountingTool {
                executions: Arc::clone(&tool_executions),
            }) as Arc<dyn lash_core::ToolProvider>)
            .build(owner())
            .expect("build the lash core");
    let session = core
        .session("turn-crash-replay")
        .open()
        .await
        .expect("open the session");
    let turn_id = lash::TurnId::from("turn-1");
    let admitted = lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
        .expect("admit the turn scope");
    let answer = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let answer = Arc::clone(&answer);
        Arc::new(move |scoped| {
            let session = session.clone();
            let turn_id = turn_id.clone();
            let answer = Arc::clone(&answer);
            Box::pin(async move {
                let output = session
                    .turn(lash::TurnInput::text("count once"))
                    .turn_id(turn_id)
                    .advanced()
                    .run_with_scope(scoped)
                    .await;
                *answer.lock().unwrap() = Some(match output {
                    Ok(output) => match output.result.assistant_message() {
                        Some(message) => message.to_owned(),
                        None => format!(
                            "no message: {:?} activities={:?}",
                            output.result.outcome, output.activities
                        ),
                    },
                    Err(error) => {
                        let error = error.to_string();
                        format!("error: {}", error.split(':').next().unwrap_or_default())
                    }
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
        Err(_) => {
            let stuck: Vec<_> = server
                .invocations()
                .into_iter()
                .filter(|view| view.status != "completed")
                .map(|view| {
                    let journal: Vec<_> = server
                        .journal(&view.id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|entry| format!("{:?}", entry.ty))
                        .collect();
                    format!(
                        "{} {} attempts={} last_failure={:?} journal={journal:?}",
                        view.target, view.status, view.attempts, view.last_failure
                    )
                })
                .collect();
            *answer.lock().unwrap() = Some(format!("stuck: {stuck:?}"));
        }
    }
    server.settle().await;
    let mut views = server.invocations();
    views.sort_by(|left, right| left.id.cmp(&right.id));
    let journals = views
        .into_iter()
        .map(|view| {
            let service = view.target.split('/').next().unwrap_or_default().to_owned();
            let entries = server
                .journal(&view.id)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.ty, entry.name))
                .collect();
            (view.id, service, entries)
        })
        .collect();
    let answer = answer
        .lock()
        .unwrap()
        .clone()
        .expect("the turn recorded an answer");
    Run {
        answer,
        llm_calls: llm_calls.load(Ordering::SeqCst),
        tool_executions: tool_executions.load(Ordering::SeqCst),
        crashes: server.stats().crashes,
        journals,
    }
}

/// Every crash point a service's journal offers: each command it stored and
/// each `ctx.run` result, named.
fn crash_points(reference: &Run, service: &str) -> Vec<(CrashRule, Option<String>)> {
    let mut points = Vec::new();
    for (_, journal_service, entries) in &reference.journals {
        if journal_service != service {
            continue;
        }
        let commands = entries.iter().filter(|(ty, _)| ty.is_command());
        for (index, (ty, name)) in commands.enumerate().skip(1) {
            // The SDK starts a run's closure as it writes the RunCommand, so
            // a crash before the server stores that command may already
            // have run the effect: it too is a lost run.
            let lost_on_command = (*ty == MessageType::RunCommand)
                .then(|| name.clone())
                .flatten();
            points.push((
                CrashRule::new(CrashPoint::BeforeCommand { index })
                    .service(service)
                    .within_attempts(1),
                lost_on_command,
            ));
            if *ty == MessageType::RunCommand {
                points.push((
                    CrashRule::new(CrashPoint::BeforeRunResult { name: name.clone() })
                        .service(service)
                        .within_attempts(1),
                    name.clone(),
                ));
            }
        }
        // One invocation of the service is enough: its points repeat.
        break;
    }
    points
}

/// Crash points where lash itself does not recover yet (FIG-3678), pinned so
/// a fix flips this test:
///
/// * when the turn-input claim's run (command 2) re-executes after a crash,
///   the claim the lost attempt already made can still hold the input, and
///   the turn stops with a runtime error — a race, on some runs.
const KNOWN_DIVERGENCES: &[(&str, Divergence)] = &[
    (
        "LashTestHandlerHost BeforeCommand { index: 2 }",
        Divergence::Sometimes,
    ),
    (
        "LashTestHandlerHost BeforeRunResult { name: Some(\"lash:turn-crash-replay:turn-1:accept_turn_input:claim_accepted_turn_input\") }",
        Divergence::Sometimes,
    ),
];

/// Whether a pinned divergence fails on every run or only on some.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Divergence {
    Always,
    Sometimes,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_journal_point_of_a_tool_turn_recovers_to_the_reference_answer() {
    let seed = 0x3665;
    let started = Instant::now();
    let reference = run_turn(seed, None).await;
    assert_eq!(reference.answer, "done");
    assert_eq!(reference.tool_executions, 1);
    assert_eq!(reference.crashes, 0);
    if let Some((_, _, entries)) = reference
        .journals
        .iter()
        .find(|(_, service, _)| service == TURN_HOST)
    {
        for (index, entry) in entries.iter().filter(|(ty, _)| ty.is_command()).enumerate() {
            println!("turn host command {index}: {entry:?}");
        }
    }
    let mut cases = 0;
    let mut violations = Vec::new();
    for service in [TURN_HOST, DISPATCH] {
        let points = crash_points(&reference, service);
        assert!(!points.is_empty(), "{service} has journal points");
        for (rule, lost_run) in points {
            let label = format!("{service} {:?}", rule.point);
            let run = run_turn(seed, Some(rule)).await;
            cases += 1;
            let lost_llm = lost_run.is_some() && service == TURN_HOST;
            let lost_tool = lost_run.is_some() && service == DISPATCH;
            let expected_llm = if lost_llm {
                reference.llm_calls..=reference.llm_calls + 1
            } else {
                reference.llm_calls..=reference.llm_calls
            };
            let expected_tool = if lost_tool { 1..=2 } else { 1..=1 };
            if run.crashes != 1
                || run.answer != reference.answer
                || !expected_llm.contains(&run.llm_calls)
                || !expected_tool.contains(&run.tool_executions)
            {
                violations.push(format!(
                    "{label} ({lost_run:?}): crashes={} answer={:?} llm_calls={} tool={}",
                    run.crashes, run.answer, run.llm_calls, run.tool_executions
                ));
            }
        }
    }
    println!(
        "turn crash matrix: {cases} journal points in {:?}, {} violations",
        started.elapsed(),
        violations.len()
    );
    for violation in &violations {
        println!("{violation}");
    }
    let unexplained: Vec<_> = violations
        .iter()
        .filter(|violation| {
            !KNOWN_DIVERGENCES
                .iter()
                .any(|(known, _)| violation.starts_with(known))
        })
        .collect();
    assert!(
        unexplained.is_empty(),
        "crash points that did not recover:\n{unexplained:#?}"
    );
    for (known, divergence) in KNOWN_DIVERGENCES {
        if *divergence == Divergence::Always {
            assert!(
                violations
                    .iter()
                    .any(|violation| violation.starts_with(known)),
                "`{known}` recovers now: drop it from KNOWN_DIVERGENCES (FIG-3678)"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_seed_reproduces_the_turn_journals_and_ids() {
    let first = run_turn(7, None).await;
    let second = run_turn(7, None).await;
    // One seed gives the turn — driven by one handler — the same id and the
    // same journal of commands and notifications, and the same answer.
    // Invocations that race each other stay as concurrent as on a real
    // server: a durable-wait index read may land before or after another
    // invocation's registration, and lash then takes a different path (an
    // extra index call, a different wait), so the full invocation set is not
    // a function of the seed. Payload bytes also differ where lash records a
    // fresh call id or a measured duration in a `ctx.run` result (FIG-3672).
    let turn_journal = |run: &Run| {
        run.journals
            .iter()
            .find(|(_, service, _)| service == TURN_HOST)
            .map(|(id, _, entries)| (id.clone(), entries.clone()))
    };
    assert_eq!(turn_journal(&first), turn_journal(&second));
    assert_eq!(first.answer, second.answer);
}
