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
//! parked. The same work declared isolated is a lash process whose
//! invocation suspends while its OS worker runs and is never aborted
//! (FIG-5152, the long-process laws of `isolated_tool_route`).

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
