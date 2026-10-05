//! The turn crash matrix on Restate, keyed to journal points (FIG-3678).
//!
//! A real lash turn — one LLM call that asks for a tool, the tool, a second
//! LLM call that answers — is accepted through the session's durable ingress
//! and executed by the engine: the session's `LashSession` shift admits it and
//! executes its run in a `LashTurn` workflow. A clean run fixes the reference:
//! its journals and its answer. Then, for every journal point of the shift,
//! and of the run's workflow, which records the tool Run, a fresh
//! backend under the same seed drops the handler just before the server
//! stores that frame on the first attempt that reaches it, including after
//! suspension, and replays the invocation. Every crash must reach the
//! reference answer, and each effect runs exactly once unless its result was
//! the frame the crash lost — then at least once, never more than twice.
//! There are no known divergences: a crash point that does not recover fails
//! the matrix.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::RunOutcome;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::store::RunStore as _;
use lash_restate_test::protocol::MessageType;
use lash_restate_test::protocol::generated::CallCommandMessage;
use lash_restate_test::{
    CrashPoint, CrashRule, RestateTestBackend, ServerConfig, TURN_DRIVER_SERVICE,
};
use prost::Message as _;
use serde_json::json;

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
    gate: Arc<lash_core::testing::Gate>,
}

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "Count this call.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        json!({"type": "object"}),
    )
    .expect("valid declared tool schemas")
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
        self.gate.pass().await;
        self.executions.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
    }
}

/// `(id, service, key, handler, entries in journal order)` of one
/// invocation. The key is the object or workflow key when the target has one.
type InvocationJournal = (
    String,
    String,
    Option<String>,
    String,
    Vec<(MessageType, Option<String>)>,
);

/// What one run of the turn observed.
#[derive(Debug)]
struct Run {
    answer: String,
    llm_calls: usize,
    tool_executions: usize,
    crashes: u64,
    turn_attempts: u32,
    /// Every invocation's journal, by id.
    journals: Vec<InvocationJournal>,
}

fn owner() -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque("lash-restate-test", "turn-crash-replay")
}

async fn run_turn(seed: u64, crash: Option<CrashRule>, config: ServerConfig) -> Run {
    let backend: RestateTestBackend = lash_restate_test::backend(seed, config)
        .await
        .expect("build the Restate test backend");
    let server = backend.server();
    // A completed tool skips wait-registration commands. Hold its body
    // until the opener has registered its wait, so all sweep cells take the
    // same journal path regardless of how quickly the tool would finish.
    let tool_gate = Arc::new(lash_core::testing::Gate::new("turn crash matrix tool"));
    let crash = crash.inspect(|rule| backend.server().crash_on(rule.clone()));
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
    let core = lash::LashCore::standard_builder(backend.lash_backend())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .expect("model spec"),
        )
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(&tool_executions),
            gate: Arc::clone(&tool_gate),
        }) as Arc<dyn lash_core::ToolProvider>)
        .build(owner())
        .expect("build the lash core");
    let session = created_session(&core, "turn-crash-replay")
        .await
        .open()
        .await
        .expect("open the session");
    let session_id = lash_core::SessionId::from("turn-crash-replay");
    let receipt = session
        .send(lash::TurnInput::text("count once"))
        .id("turn-1")
        .await
        .expect("accept the turn input")
        .receipt()
        .clone();
    // The acceptance scheduled the shift under the input's own request; the
    // attach names the same request, so it waits on that one shift.
    let request = lash_core::shift::ingress_shift_request(
        receipt.input_id.as_str(),
        lash_core::shift::FIRST_INGRESS_ATTEMPT,
    );
    tool_gate.reached(1).await;
    let run = server
        .invocations()
        .into_iter()
        .find(|view| {
            view.target.starts_with(&format!("{TURN_DRIVER_SERVICE}/"))
                && view.target.ends_with("/run")
        })
        .expect("the held tool has a run invocation");
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        while !server
            .journal(&run.id)
            .unwrap_or_default()
            .into_iter()
            .any(|entry| {
                entry.ty == MessageType::CallCommand
                    && CallCommandMessage::decode(entry.payload).is_ok_and(|call| {
                        call.service_name == "LashDurableWaitIndex"
                            && call.handler_name == "register_awakeable"
                    })
            })
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the opener registers its wait while the tool is held");
    assert_eq!(tool_executions.load(Ordering::SeqCst), 0);
    tool_gate.open_all();
    let shift = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        backend.attach_shift(&session_id, request),
    )
    .await;
    let answer = match shift {
        Ok(Ok(outcome)) => match outcome.ran.as_slice() {
            [RunOutcome::Committed { run, .. }] => match backend
                .stores()
                .session_store_factory()
                .run_terminal(&session_id, run)
                .await
                .expect("read terminal")
                .expect("committed terminal")
                .cause
            {
                lash_core::store::RunTerminalCause::Committed { outcome, .. } => {
                    match lash_core::facade_support::TurnOutcome::from(outcome) {
                        lash_core::facade_support::TurnOutcome::Finished(
                            lash_core::facade_support::TurnFinish::AssistantMessage { text },
                        ) => text.clone(),
                        other => format!("no message: {other:?}"),
                    }
                }
                other => format!("run terminal: {other:?}"),
            },
            other => format!("shift ran {other:?}, stopped {:?}", outcome.stop),
        },
        Ok(Err(error)) => format!("stuck: {error}"),
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
            format!("stuck: {stuck:?}")
        }
    };
    server.settle().await;
    let mut views = server.invocations();
    views.sort_by(|left, right| left.id.cmp(&right.id));
    let turn_attempts = views
        .iter()
        .filter(|view| {
            view.target.starts_with(&format!("{TURN_DRIVER_SERVICE}/"))
                && view.target.ends_with("/run")
        })
        .map(|view| view.attempts)
        .max()
        .unwrap_or_default();
    if crash.is_none() || server.stats().crashes == 0 {
        for view in &views {
            println!(
                "reference invocation {} attempts={} last_failure={:?}",
                view.target, view.attempts, view.last_failure
            );
            if crash.is_some() {
                let journal: Vec<_> = server
                    .journal(&view.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|entry| (entry.ty, entry.name))
                    .collect();
                println!(
                    "missed crash {crash:?}: {} suspensions={} journal={journal:?}",
                    view.target, view.suspensions
                );
            }
        }
    }
    let journals = views
        .into_iter()
        .map(|view| {
            // A target is `service/key/handler` for keyed services and
            // `service/handler` otherwise; the key itself holds no `/`.
            let service = view.target.split('/').next().unwrap_or_default().to_owned();
            let handler = view
                .target
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            let key = view
                .target
                .strip_prefix(&format!("{service}/"))
                .and_then(|rest| rest.strip_suffix(&format!("/{handler}")))
                .filter(|key| !key.is_empty())
                .map(str::to_owned);
            let entries = server
                .journal(&view.id)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| (entry.ty, entry.name))
                .collect();
            (view.id, service, key, handler, entries)
        })
        .collect();
    Run {
        answer,
        llm_calls: llm_calls.load(Ordering::SeqCst),
        tool_executions: tool_executions.load(Ordering::SeqCst),
        crashes: server.stats().crashes,
        turn_attempts,
        journals,
    }
}

/// Every crash point a service's journal offers: each command it stored and
/// each `ctx.run` result, named.
fn crash_points(reference: &Run, service: &str) -> Vec<(CrashRule, Option<String>)> {
    let mut points = Vec::new();
    for (_, journal_service, key, handler, entries) in &reference.journals {
        // A pinned service's journal may be recorded under its generation
        // lane `<service>_g<G>` (FIG-3795): the same service's points, and
        // the name the crash rule must target.
        let same_service = journal_service == service
            || journal_service.strip_prefix(service).is_some_and(|rest| {
                rest.len() == 14
                    && rest.starts_with("_g")
                    && rest[2..]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if !same_service {
            continue;
        }
        let commands = entries.iter().filter(|(ty, _)| ty.is_command());
        for (index, (ty, name)) in commands.enumerate().skip(1) {
            // The SDK starts a run's closure as it writes the RunCommand, so
            // a crash before the server stores that command may already
            // have run the effect: it too is a lost run.
            // The rule fires on the first frame any invocation of the
            // service sends at this command index, so it must name the
            // handler (and key, when the service is keyed) of the invocation
            // the point was enumerated from: another invocation's same-index
            // command is a different step.
            let rule = |point| {
                // The journal survives suspension and retry. A later point
                // may first be reached on a later attempt; the one-shot rule
                // stays armed until that frame arrives.
                let rule = CrashRule::new(point)
                    .service(journal_service.clone())
                    .handler(handler.clone());
                match key {
                    Some(key) => rule.key(key.clone()),
                    None => rule,
                }
            };
            let lost_on_command = (*ty == MessageType::RunCommand)
                .then(|| name.clone())
                .flatten();
            points.push((rule(CrashPoint::BeforeCommand { index }), lost_on_command));
            if *ty == MessageType::RunCommand {
                // By position: a shift's admission and seal names embed the
                // accepted input's id, which each execution mints afresh.
                points.push((rule(CrashPoint::BeforeRunResultAt { index }), name.clone()));
            }
        }
        // One invocation of the service is enough: its points repeat.
        break;
    }
    points
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_crash_point_stays_armed_across_suspension() {
    let seed = 0x3665;
    // Closing input after replay forces unresolved awaits to suspend, without
    // depending on wall time or the host's scheduling load.
    let config = ServerConfig::default().always_replay(true);
    let reference = run_turn(seed, None, config.clone()).await;
    assert_eq!(reference.answer, "done");
    assert_eq!(reference.crashes, 0);
    assert!(
        reference.turn_attempts > 1,
        "the turn resumed after suspension"
    );
    let (rule, _) = crash_points(&reference, TURN_DRIVER_SERVICE)
        .into_iter()
        .rev()
        .find(|(rule, _)| matches!(rule.point, CrashPoint::BeforeRunResultAt { .. }))
        .expect("the turn has a final run result to lose");
    let point = rule.point.clone();
    let run = run_turn(seed, Some(rule), config).await;
    assert_eq!(run.answer, "done");
    assert!(
        run.turn_attempts > 1,
        "the crash cell resumed after suspension"
    );
    assert_eq!(run.tool_executions, 1);
    assert!((2..=3).contains(&run.llm_calls));
    assert_eq!(
        run.crashes, 1,
        "the prearmed {point:?} must fire on the attempt that reaches it ({} attempts)",
        run.turn_attempts
    );
    println!(
        "turn crash after suspension: {point:?}, {} attempts, {} crashes",
        run.turn_attempts, run.crashes
    );
}

/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}
