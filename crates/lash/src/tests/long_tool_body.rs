//! Work that outlives Restate's invoker timeouts (FIG-5149), on a live
//! `restate-server` (the `long-tool-body` suite of
//! `scripts/restate-suites.toml`) whose inactivity timeout is 1 s and abort
//! timeout 2 s.
//!
//! A tool body runs as a `ctx.run` inside its Run's invocation. While it runs
//! the invocation cannot suspend, so once the inactivity timeout asks it to
//! and the abort timeout passes, the server aborts the invocation and retries
//! it from its journal, where the body was never recorded: every attempt runs
//! the body again. Lash's handler attempt bound
//! ([`TURN_HANDLER_MAX_ATTEMPTS`](lash_restate::TURN_HANDLER_MAX_ATTEMPTS))
//! is what ends the loop: the invocation pauses and the host sees the turn
//! parked. An isolated call is a lash process whose OS worker outlives the
//! process invocation's aborted attempts: each redelivery adopts the live
//! worker and never launches another.

use super::*;

/// How long the in-invocation body runs: past the server's 1 s inactivity
/// plus 2 s abort window on every attempt.
const BODY: std::time::Duration = std::time::Duration::from_secs(6);
/// How long a law waits for what it observes; each aborted attempt of the
/// turn takes about ten seconds.
const WAIT: std::time::Duration = std::time::Duration::from_secs(240);
const SLOW: &str = "slow_lookup";

/// A tool whose body takes [`BODY`] and counts every run of it.
#[derive(Default)]
struct SlowTools {
    bodies: AtomicUsize,
}

fn slow_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:slow_lookup",
        SLOW,
        "A lookup that takes six seconds.",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    )
    .expect("valid declared tool schemas")
}

#[async_trait]
impl ToolProvider for SlowTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![slow_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == SLOW).then(|| Arc::new(slow_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.bodies.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(BODY).await;
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

/// A model that calls [`SLOW`] once and answers once its result is shown.
fn slow_caller() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("long-tool-body")
        .complete(|request| async move {
            let answered = request.messages.iter().any(|message| {
                message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
            });
            if answered {
                return Ok(text_response("answered"));
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "slow-0".into(),
                    tool_name: SLOW.into(),
                    input_json: "{}".into(),
                    replay: None,
                }],
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

/// Serve a deployment on the suite's server, named apart from every other
/// run's: the server outlives one law.
#[allow(
    clippy::disallowed_methods,
    reason = "the live law reads the suite's server and endpoint addresses"
)]
async fn serve(tag: &str) -> (lash_restate_test::live::LiveRestateBackend, String) {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment sets {name}"))
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after the epoch")
        .as_nanos();
    let prefix = format!("long-tool-body-{tag}-{nonce}");
    let live =
        lash_restate_test::live::LiveRestateBackend::start(lash_restate_test::live::LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("LTB_BIND").parse().expect("a socket address"),
            endpoint_url: env("LTB_URL"),
            run_tag: prefix.clone(),
            namespace: lash_restate::RestateNamespace::default(),
        })
        .await
        .expect("serve the live deployment");
    (live, prefix)
}

/// What the server reported for a service's `run` invocations: the most
/// attempts one reached, and whether one failed on the abort timeout.
#[derive(Clone, Copy, Debug, Default)]
struct Aborts {
    attempts: u64,
    aborted: bool,
}

/// Read `service`'s run invocations whose target holds `key` every quarter
/// second until the task is aborted.
fn watch_aborts(
    live: &lash_restate_test::live::LiveRestateBackend,
    service: &'static str,
    key: String,
) -> (Arc<StdMutex<Aborts>>, tokio::task::JoinHandle<()>) {
    let seen = Arc::new(StdMutex::new(Aborts::default()));
    let live = live.clone();
    let record = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        loop {
            for row in live.invocations().await.unwrap_or_default() {
                if !is_run_of(&row.target, service, &key) {
                    continue;
                }
                let mut seen = record.lock_recover();
                seen.attempts = seen.attempts.max(row.retry_count.unwrap_or(0));
                seen.aborted |= row
                    .last_failure
                    .as_deref()
                    .is_some_and(|failure| failure.contains("abort timeout"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
    (seen, task)
}

/// Whether `target` is the `run` handler of a `service` invocation whose key
/// holds `key`.
fn is_run_of(target: &str, service: &str, key: &str) -> bool {
    target
        .strip_prefix(service)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.strip_suffix("/run"))
        .is_some_and(|invocation_key| invocation_key.contains(key))
}

/// A tool body that outlives the abort window on every attempt reruns on
/// every attempt and never finishes the turn. Lash's handler attempt bound
/// ends the loop: the host is answered with the turn parked, its retries
/// exhausted, never with a hang or an unbounded rerun.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the long-tool-body suite"]
async fn live_a_tool_body_past_the_abort_window_reruns_per_attempt_until_the_turn_parks()
-> Result<()> {
    let (live, prefix) = serve("body").await;
    let tools = Arc::new(SlowTools::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(live.lash_backend()))
        .serve_test_llm_profile(slow_caller(), mock_llm_profile_spec())
        .tools(tools.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::fixture(format!("{prefix}-session")))
        .created()
        .await
        .open()
        .await?;
    let (aborts, watcher) = watch_aborts(&live, "LashTurn", prefix.clone());
    let handle = session.send(TurnInput::text("look it up slowly")).await?;
    let outcome = tokio::time::timeout(WAIT, handle.outcome())
        .await
        .expect("the host is answered once the attempts are spent")?;
    watcher.abort();
    let aborts = *aborts.lock_recover();
    let bodies = tools.bodies.load(Ordering::SeqCst);
    let crate::SendOutcome::Parked { parked, .. } = &outcome else {
        panic!("the turn parks instead of finishing: {outcome:?}");
    };
    assert_eq!(
        parked.reason.code(),
        lash_core::store::ParkReasonCode::EngineRetryExhausted,
        "{parked:?}"
    );
    assert!(
        aborts.aborted,
        "the server aborted the turn's attempts: {aborts:?}"
    );
    let bound = usize::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS).expect("a small bound");
    assert!(
        (2..=bound).contains(&bodies),
        "every aborted attempt reran the unrecorded body, and the attempt bound \
         stopped the reruns: {bodies} bodies, {aborts:?}"
    );
    live.kill_open("the parked turn's paused invocation keeps its journal for a resume")
        .await;
    Ok(())
}

/// The same long work declared isolated runs as a lash process: its OS
/// worker outlives the process invocation's aborted attempts, every
/// redelivery adopts the live worker, and the process settles with the
/// worker's one result.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the long-tool-body suite"]
#[allow(
    clippy::disallowed_methods,
    reason = "the law reads the PID marker its worker writes"
)]
async fn live_an_isolated_worker_past_the_abort_window_launches_once_and_settles() -> Result<()> {
    use super::isolated_tool_route::{IsolatedTools, KIND, WorkerEngineFactory};

    let (live, prefix) = serve("isolated").await;
    let marker = tempfile::tempdir().expect("a marker directory");
    let engine = Arc::new(lash_core::WorkerProcessEngine::new(
        KIND,
        lash_core::WorkerCommand {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "echo $$ >> \"$1\"; sleep 20; echo '{\"done\":true}'".into(),
                "sh".into(),
                marker.path().join("pids").into_os_string(),
            ],
        },
        marker.path().join("ownership"),
    ));
    let tools = Arc::new(IsolatedTools {
        bound: true,
        executions: AtomicUsize::new(0),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(rlm_core_builder_over(live.lash_backend()))
        .serve_test_llm_profile(
            crate::testing::TestProvider::builder()
                .kind("long-tool-body-isolated")
                .complete(move |_| {
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Ok(text_response(&typescript_block(if call == 0 {
                            "const started = await iso.run({label: \"fig5149\"});\nfinish(started);"
                        } else {
                            "finish(\"asked again\");"
                        })))
                    }
                })
                .build()
                .into_handle(),
            mock_llm_profile_spec(),
        )
        .tools(tools.clone())
        .plugin(Arc::new(WorkerEngineFactory(engine)))
        .build(crate::testing::runtime_lease_owner())?;
    live.install_process_worker(
        lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config()?)
            .expect("the core's process worker"),
    );
    let session = core
        .session(SessionId::fixture(format!("{prefix}-session")))
        .created()
        .await
        .open()
        .await?;
    let (aborts, watcher) = watch_aborts(&live, "LashProcessWorkflow", String::new());
    let output = tokio::time::timeout(
        WAIT,
        session.send(TurnInput::text("start the worker")).output(),
    )
    .await
    .expect("the turn answers with the process descriptor")?;
    assert_eq!(output.status(), crate::TurnStatus::Answered, "{output:?}");
    let registry = live.lash_backend().process_registry();
    let outcome = tokio::time::timeout(WAIT, async {
        loop {
            let settled = registry
                .list_processes(&lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..lash_core::ProcessListFilter::default()
                })
                .await
                .expect("list processes")
                .into_iter()
                .find_map(|record| record.outcome());
            if let Some(outcome) = settled {
                return outcome;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the process settles once its worker exits");
    watcher.abort();
    let aborts = *aborts.lock_recover();
    assert!(
        aborts.aborted && aborts.attempts >= 2,
        "the server aborted and redelivered the process invocation while its worker ran: {aborts:?}"
    );
    let launches = std::fs::read_to_string(marker.path().join("pids"))
        .expect("the worker wrote its PID")
        .lines()
        .count();
    assert_eq!(launches, 1, "every redelivery adopted the one live worker");
    assert_eq!(
        tools.executions.load(Ordering::SeqCst),
        0,
        "no ordinary body"
    );
    let lash_core::ProcessAwaitOutput::Settled { output } = &outcome else {
        panic!("the process settles: {outcome:?}");
    };
    assert!(
        matches!(output.outcome, lash_core::ToolCallOutcome::Success(_)),
        "{output:?}"
    );
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "done": true }),
        "the process settles with the worker's one result"
    );
    // The aborted process invocation itself converges: its redelivery
    // replays the settled terminal and completes, which releases every
    // attempt's cancel listener.
    let completed = tokio::time::timeout(WAIT, async {
        loop {
            let run = live
                .invocations()
                .await
                .expect("the server's invocations")
                .into_iter()
                .find(|row| is_run_of(&row.target, "LashProcessWorkflow", ""));
            if let Some(run) = run.filter(|run| run.status == "completed") {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the process invocation completes");
    assert_eq!(
        completed.completion_result.as_deref(),
        Some("success"),
        "{completed:?}"
    );
    live.finish().await;
    Ok(())
}
