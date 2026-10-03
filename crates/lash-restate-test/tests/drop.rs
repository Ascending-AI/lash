//! A dropped server double frees everything it held (FIG-3721): its state,
//! its tasks, and the ingress callers still waiting on it. Long test binaries
//! build a server per run, so a server that outlived its handles would grow
//! them without bound.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]
// FIG-2971: test code; the RSS measurement reads /proc and its run count from
// the environment.
#![allow(clippy::disallowed_methods)]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_http_transport::{HttpMethod, HttpRequest};
use lash_restate_test::{RestateTestServer, ServerConfig};
use restate_sdk::prelude::*;
use serde_json::json;
use tokio::sync::oneshot;

// `#[restate_sdk::*]` expansions name `::restate_sdk` absolute paths; the SDK
// reaches this crate through lash's re-export, so the crate answers to that
// name and generated code resolves the modules below at the crate root.
extern crate self as restate_sdk;
#[allow(unused_imports)]
use lash_restate::restate_sdk::{
    context, discovery, endpoint, errors, handler, object, prelude, service, workflow,
};

/// A workflow that parks on a promise nobody resolves and a sleep nobody
/// fires: whatever a dropped server leaves behind, this leaves it.
struct Parked;

#[restate_sdk::workflow]
impl Parked {
    #[handler]
    async fn run(&self, ctx: WorkflowContext<'_>) -> HandlerResult<Json<String>> {
        ctx.sleep(Duration::from_secs(3600)).await?;
        Ok(Json(ctx.promise::<String>("never").await?))
    }

    #[handler]
    async fn wait(&self, ctx: SharedWorkflowContext<'_>) -> HandlerResult<Json<String>> {
        Ok(Json(ctx.promise::<String>("never").await?))
    }
}

async fn post(transport: &Arc<dyn lash_http_transport::HttpTransport>, url: String) -> u16 {
    let request = HttpRequest::new(HttpMethod::Post, url, "null".to_owned())
        .with_header("content-type", "application/json");
    transport.send(request, None).await.unwrap().status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_server_frees_its_state_its_tasks_and_its_waiting_callers() {
    let server = RestateTestServer::start(
        Endpoint::builder().bind(Parked).build(),
        ServerConfig::default(),
    )
    .await
    .unwrap();
    let transport = server.transport();
    let base = server.ingress_url().to_owned();
    assert_eq!(
        post(&transport, format!("{base}/Parked/k/run/send")).await,
        202
    );
    // Callers that wait on outcomes the dropped server never produces.
    let (callers, started): (Vec<_>, Vec<_>) = ["run/attach", "wait"]
        .into_iter()
        .map(|path| {
            let transport = Arc::clone(&transport);
            let url = if path == "run/attach" {
                format!("{base}/restate/workflow/Parked/k/attach")
            } else {
                format!("{base}/Parked/k/{path}")
            };
            let (started_tx, started_rx) = oneshot::channel();
            let caller = tokio::spawn(async move {
                let mut request = Box::pin(post(&transport, url));
                let mut started_tx = Some(started_tx);
                std::future::poll_fn(|cx| {
                    let result = request.as_mut().poll(cx);
                    if let Some(started_tx) = started_tx.take() {
                        assert!(result.is_pending(), "caller must be waiting");
                        let _ = started_tx.send(());
                    }
                    result
                })
                .await
            });
            (caller, started_rx)
        })
        .unzip();
    for started_rx in started {
        started_rx.await.expect("caller started waiting");
    }
    server.settle().await;
    assert!(
        !server.timers().is_empty(),
        "the workflow sleeps on a timer"
    );
    let watch = server.drop_watch();
    assert!(!watch.is_freed());
    drop(server);
    assert!(
        watch.freed_within(Duration::from_secs(5)).await,
        "the dropped server is freed; {} task(s) left",
        watch.live_tasks()
    );
    for caller in callers {
        let status = tokio::time::timeout(Duration::from_secs(5), caller)
            .await
            .expect("a waiting caller is answered once the server is gone")
            .unwrap();
        assert_eq!(status, 503);
    }
    // The transport outlives the server and answers for it.
    assert_eq!(
        post(&transport, format!("{base}/Parked/j/run/send")).await,
        503
    );
}

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        ..Default::default()
    }
}

struct CountingTool;

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:count_call",
        "count_call",
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
        (name == "count_call").then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(json!({"result": "counted"})).into()
    }
}

/// One lash turn — a model call that asks for a tool, the tool as an effect
/// group child, a model call that answers — executed by the engine on a fresh
/// backend, which is dropped when the turn is done. Returns the watch on its
/// server.
async fn one_turn_run(seed: u64, worker: bool) -> lash_restate_test::DropWatch {
    let backend = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let watch = backend.server().drop_watch();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("drop")
        .complete(move |request: LlmRequest| async move {
            let answered = serde_json::to_string(&request.messages)
                .unwrap_or_default()
                .contains("counted");
            Ok::<_, LlmTransportError>(response(vec![if answered {
                LlmOutputPart::Text {
                    text: "done".into(),
                    response_meta: None,
                }
            } else {
                LlmOutputPart::ToolCall {
                    call_id: "call-1".into(),
                    tool_name: "count_call".into(),
                    input_json: "{}".into(),
                    replay: None,
                }
            }]))
        })
        .build()
        .into_handle();
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
        .tools(Arc::new(CountingTool) as Arc<dyn lash_core::ToolProvider>)
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "drop",
        ))
        .expect("build the lash core");
    if worker {
        // A deployment serves its process segments with the core's durable
        // worker, which holds the core's configuration.
        backend.install_process_worker(
            lash::durability::DurableProcessWorker::new(
                core.durable_process_worker_config()
                    .expect("the core's process worker configuration"),
            )
            .expect("build the process worker"),
        );
    }
    let session = created_session(&core, "drop")
        .await
        .open()
        .await
        .expect("open the session");
    let receipt = session
        .send(lash::TurnInput::text("count once"))
        .await
        .expect("accept the turn input")
        .receipt()
        .clone();
    // The engine executes the accepted input: its LashSession shift admits it
    // and its run executes in a LashTurn workflow.
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        backend.attach_shift(
            &lash_core::SessionId::from("drop"),
            lash_core::shift::ingress_shift_request(
                receipt.input_id.as_str(),
                lash_core::shift::FIRST_INGRESS_ATTEMPT,
            ),
        ),
    )
    .await
    .expect("the turn finishes")
    .expect("the shift answers");
    let [lash_core::engine::RunOutcome::Committed { run, .. }] = outcome.ran.as_slice() else {
        panic!("the shift runs the one run: {outcome:?}");
    };
    let terminal = lash_core::StoreSet::session_store_factory(backend.stores().as_ref())
        .run_terminal(&lash_core::SessionId::from("drop"), run)
        .await
        .expect("read terminal")
        .expect("committed terminal");
    assert!(matches!(terminal.cause,
        lash_core::store::RunTerminalCause::Committed {
            outcome: lash_core::store::RunCommittedOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage { text }), ..
        } if text == "done"));
    watch
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_backend_with_a_process_worker_frees_its_server() {
    let watch = one_turn_run(8, true).await;
    assert!(
        watch.freed_within(Duration::from_secs(5)).await,
        "the backend's server is freed with the core's process worker installed on it; \
         {} task(s) left",
        watch.live_tasks()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_backend_frees_its_server_after_a_lash_turn() {
    let watch = one_turn_run(7, false).await;
    assert!(
        watch.freed_within(Duration::from_secs(5)).await,
        "the backend's server is freed once the turn's core, session and backend are dropped; \
         {} task(s) left",
        watch.live_tasks()
    );
}

/// Resident set size of this process, in KiB.
fn rss_kib() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse().ok())
        .unwrap_or(0);
    pages * 4
}

/// Measurement, run by hand: resident memory over sequential backend turns.
/// Prints the RSS after every tenth run; flat once warm means runs free
/// what they built.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement: run by hand, it prints resident memory per run"]
async fn resident_memory_stays_flat_over_sequential_backend_runs() {
    let runs: u64 = std::env::var("LASH_RESTATE_TEST_RSS_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(100);
    let mut samples = Vec::new();
    for run in 0..runs {
        let watch = one_turn_run(run, true).await;
        assert!(
            watch.freed_within(Duration::from_secs(5)).await,
            "run {run}"
        );
        if run % 10 == 9 {
            samples.push((run + 1, rss_kib()));
            println!("runs={} rss_kib={}", run + 1, rss_kib());
        }
    }
    if let (Some((first_runs, first)), Some((last_runs, last))) =
        (samples.get(1).copied(), samples.last().copied())
    {
        println!(
            "growth after warm-up: {:.1} KiB/run over runs {first_runs}..{last_runs}",
            (last as f64 - first as f64) / (last_runs - first_runs).max(1) as f64
        );
    }
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
