//! The server double's own semantics, on small handlers: the Restate
//! behaviours lash builds on, each checked in streaming and in always-replay
//! mode, under concurrent and serial scheduling.

#![expect(
    clippy::unwrap_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use lash_http_transport::{HttpMethod, HttpRequest, read_http_body_bytes};
use lash_restate_test::{
    CrashPoint, CrashRule, RestateTestServer, Scheduling, ServerConfig, TimeMode,
};
use restate_sdk::prelude::*;

// ---------------------------------------------------------------------------
// Handlers under test
// ---------------------------------------------------------------------------

fn counter(name: &str) -> &'static AtomicUsize {
    static COUNTERS: OnceLock<Mutex<HashMap<String, &'static AtomicUsize>>> = OnceLock::new();
    let mut counters = COUNTERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    counters
        .entry(name.to_owned())
        .or_insert_with(|| Box::leak(Box::new(AtomicUsize::new(0))))
}

struct Counter;

#[restate_sdk::object]
impl Counter {
    #[handler]
    async fn add(&self, ctx: ObjectContext<'_>, Json(n): Json<i64>) -> HandlerResult<Json<i64>> {
        let current = ctx.get::<i64>("count").await?.unwrap_or(0);
        ctx.set("count", current + n);
        Ok(Json(current + n))
    }

    #[handler]
    async fn read(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<i64>> {
        Ok(Json(ctx.get::<i64>("count").await?.unwrap_or(0)))
    }
}

struct Flow;

#[restate_sdk::workflow]
impl Flow {
    /// Runs a side effect once, sleeps a virtual minute, and waits for its
    /// `approval` promise.
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(tag): Json<String>,
    ) -> HandlerResult<Json<String>> {
        let executed = ctx
            .run(|| async move { Ok(counter(&tag).fetch_add(1, Ordering::SeqCst) as u64 + 1) })
            .name("effect")
            .await?;
        ctx.sleep(Duration::from_secs(60)).await?;
        let approval = ctx.promise::<String>("approval").await?;
        Ok(Json(format!("{executed}:{approval}")))
    }

    #[handler]
    async fn approve(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(value): Json<String>,
    ) -> HandlerResult<()> {
        ctx.resolve_promise::<String>("approval", value);
        Ok(())
    }

    #[handler]
    async fn peek(&self, ctx: SharedWorkflowContext<'_>) -> HandlerResult<Json<Option<String>>> {
        Ok(Json(ctx.peek_promise::<String>("approval").await?))
    }
}

struct Caller;

#[restate_sdk::service]
impl Caller {
    /// Calls `Counter/{key}/add` twice and returns the second result.
    #[handler]
    async fn twice(&self, ctx: Context<'_>, Json(key): Json<String>) -> HandlerResult<Json<i64>> {
        ctx.object_client::<CounterClient>(key.clone())
            .add(Json(1))
            .call()
            .await?;
        let Json(second) = ctx
            .object_client::<CounterClient>(key)
            .add(Json(1))
            .call()
            .await?;
        Ok(Json(second))
    }

    /// Creates an awakeable, hands its id to `Resolver`, and awaits it.
    #[handler]
    async fn await_awakeable(&self, ctx: Context<'_>) -> HandlerResult<Json<String>> {
        let (id, awakeable) = ctx.awakeable::<String>();
        ctx.service_client::<ResolverClient>()
            .resolve(Json(id))
            .send();
        Ok(Json(awakeable.await?))
    }
}

struct Resolver;

#[restate_sdk::service]
impl Resolver {
    #[handler]
    async fn resolve(&self, ctx: Context<'_>, Json(id): Json<String>) -> HandlerResult<()> {
        ctx.resolve_awakeable(&id, "resolved".to_owned());
        Ok(())
    }
}

struct Flaky;

#[restate_sdk::service]
impl Flaky {
    /// Fails retryably forever; the invoker's policy pauses it after three
    /// attempts.
    #[handler(invocation_retry_policy(
        initial_interval = "1s",
        factor = 2.0,
        max_attempts = 3,
        on_max_attempts = "pause",
    ))]
    async fn fail(&self, _ctx: Context<'_>, Json(tag): Json<String>) -> HandlerResult<()> {
        counter(&tag).fetch_add(1, Ordering::SeqCst);
        Err(HandlerError::from(std::io::Error::other("transient")))
    }
}

/// How many `Gauge/work` handlers are between their two journaled steps
/// right now, and the most there have ever been at once.
static GAUGE: Mutex<(usize, usize)> = Mutex::new((0, 0));

struct Gauge;

#[restate_sdk::service]
impl Gauge {
    /// Journals a step, does a little wall-clock work outside the journal
    /// while counted in [`GAUGE`], and journals a second step.
    #[handler]
    async fn work(&self, ctx: Context<'_>, Json(tag): Json<String>) -> HandlerResult<Json<String>> {
        let first = ctx
            .run(|| async move { Ok(format!("{tag}:in")) })
            .name("in")
            .await?;
        {
            let mut gauge = GAUGE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            gauge.0 += 1;
            gauge.1 = gauge.1.max(gauge.0);
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
        GAUGE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0 -= 1;
        let second = ctx
            .run(|| async move { Ok(format!("{first}:out")) })
            .name("out")
            .await?;
        Ok(Json(second))
    }
}

/// The server's ingress transport, for a handler that calls the ingress
/// itself — as lash's handlers do when they resolve a durable wait.
static INGRESS: Mutex<Option<RestateTestServer>> = Mutex::new(None);

struct Relay;

#[restate_sdk::service]
impl Relay {
    /// From inside a `ctx.run`, calls `Counter/{key}/add` through the
    /// server's ingress and returns its answer.
    #[handler]
    async fn ask(&self, ctx: Context<'_>, Json(key): Json<String>) -> HandlerResult<Json<String>> {
        let server = INGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        let answer = ctx
            .run(|| async move { Ok(post(&server, &format!("Counter/{key}/add"), "1").await.1) })
            .name("ask")
            .await?;
        Ok(Json(answer))
    }
}

fn endpoint() -> Endpoint {
    Endpoint::builder()
        .bind(Counter)
        .bind(Flow)
        .bind(Caller)
        .bind(Resolver)
        .bind(Flaky)
        .bind(Gauge)
        .bind(Relay)
        .build()
}

// ---------------------------------------------------------------------------
// Ingress helpers
// ---------------------------------------------------------------------------

async fn post(server: &RestateTestServer, path: &str, body: &str) -> (u16, String) {
    let request = HttpRequest::new(
        HttpMethod::Post,
        format!("{}/{path}", server.ingress_url()),
        body.to_owned(),
    )
    .with_header("content-type", "application/json");
    let response = server.transport().send(request, None).await.unwrap();
    let status = response.status;
    let body = read_http_body_bytes(response.body, None, "body")
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn server(config: ServerConfig) -> RestateTestServer {
    RestateTestServer::start(endpoint(), config).await.unwrap()
}

fn modes() -> [ServerConfig; 6] {
    [
        ServerConfig::default(),
        ServerConfig::default().always_replay(true),
        ServerConfig::default().protocol(lash_restate_test::ProtocolVersion::V7),
        ServerConfig::default()
            .protocol(lash_restate_test::ProtocolVersion::V7)
            .always_replay(true),
        ServerConfig::default().scheduling(Scheduling::Serial),
        ServerConfig::default()
            .protocol(lash_restate_test::ProtocolVersion::V7)
            .always_replay(true)
            .scheduling(Scheduling::Serial),
    ]
}

// ---------------------------------------------------------------------------
// Laws
// ---------------------------------------------------------------------------

#[tokio::test]
async fn object_state_is_serialized_per_key_and_calls_return_results() {
    for config in modes() {
        let server = server(config).await;
        assert_eq!(
            post(&server, "Caller/twice", "\"a\"").await,
            (200, "2".into())
        );
        assert_eq!(post(&server, "Counter/a/add", "5").await, (200, "7".into()));
        assert_eq!(post(&server, "Counter/a/read", "").await, (200, "7".into()));
        assert_eq!(post(&server, "Counter/b/read", "").await, (200, "0".into()));
    }
}

#[tokio::test]
async fn a_workflow_runs_once_sleeps_on_virtual_time_and_waits_for_its_promise() {
    for (index, config) in modes().into_iter().enumerate() {
        let tag = format!("workflow-{index}");
        let server = server(config.time(TimeMode::Manual)).await;
        let (status, body) = post(
            &server,
            &format!("Flow/{tag}/run/send"),
            &format!("\"{tag}\""),
        )
        .await;
        assert_eq!(status, 202, "{body}");
        assert!(body.contains("\"Accepted\""), "{body}");
        let (status, body) = post(
            &server,
            &format!("Flow/{tag}/run/send"),
            &format!("\"{tag}\""),
        )
        .await;
        assert_eq!(status, 202);
        assert!(body.contains("PreviouslyAccepted"), "{body}");

        server.settle().await;
        let timers = server.timers();
        assert_eq!(timers.len(), 1, "{timers:?}");
        let start = server.now_ms();
        assert_eq!(timers[0].fire_at_ms, start + 60_000);
        assert_eq!(
            post(&server, &format!("Flow/{tag}/peek"), "").await,
            (200, "null".into())
        );

        server.advance(Duration::from_secs(60));
        server.settle().await;
        assert_eq!(
            post(&server, &format!("Flow/{tag}/approve"), "\"yes\"")
                .await
                .0,
            200
        );
        let (status, body) =
            post_get_attach(&server, &format!("restate/workflow/Flow/{tag}/attach")).await;
        assert_eq!((status, body.as_str()), (200, "\"1:yes\""));
        assert_eq!(
            counter(&tag).load(Ordering::SeqCst),
            1,
            "the side effect ran once"
        );
        assert_eq!(
            post(&server, &format!("Flow/{tag}/run"), &format!("\"{tag}\""))
                .await
                .0,
            409,
            "a second run of one workflow key is refused"
        );
    }
}

async fn post_get_attach(server: &RestateTestServer, path: &str) -> (u16, String) {
    let request = HttpRequest::new(
        HttpMethod::Get,
        format!("{}/{path}", server.ingress_url()),
        "",
    );
    let response = server.transport().send(request, None).await.unwrap();
    let status = response.status;
    let body = read_http_body_bytes(response.body, None, "body")
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_crash_before_a_run_result_is_stored_replays_and_reruns_the_effect() {
    let tag = "crash-before-result";
    let server = server(ServerConfig::default()).await;
    server.crash_on(
        CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some("effect".into()),
        })
        .service("Flow"),
    );
    post(
        &server,
        &format!("Flow/{tag}/run/send"),
        &format!("\"{tag}\""),
    )
    .await;
    post(&server, &format!("Flow/{tag}/approve"), "\"ok\"").await;
    // The minute-long sleep is past the auto-advance horizon: move time.
    server.settle().await;
    server.advance(Duration::from_secs(60));
    let (status, body) =
        post_get_attach(&server, &format!("restate/workflow/Flow/{tag}/attach")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "\"2:ok\"", "the effect re-ran once after the crash");
    assert_eq!(server.stats().crashes, 1);
}

#[tokio::test]
async fn awakeables_route_their_completion_to_the_owning_invocation() {
    for config in modes() {
        let server = server(config).await;
        assert_eq!(
            post(&server, "Caller/await_awakeable", "").await,
            (200, "\"resolved\"".into())
        );
    }
}

#[tokio::test]
async fn exhausting_the_handler_retry_policy_pauses_the_invocation_until_resumed() {
    let tag = "flaky";
    let server = server(ServerConfig::default().time(TimeMode::Manual)).await;
    let (_, body) = post(&server, "Flaky/fail/send", &format!("\"{tag}\"")).await;
    let id: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = id["invocationId"].as_str().unwrap().to_owned();
    server.settle().await;
    assert_eq!(counter(tag).load(Ordering::SeqCst), 1);
    // Backoff 1s then 2s on virtual time; the third failure pauses.
    assert!(server.fire_next_timer().is_some());
    server.settle().await;
    assert!(server.fire_next_timer().is_some());
    server.settle().await;
    assert_eq!(counter(tag).load(Ordering::SeqCst), 3);
    let view = server
        .invocations()
        .into_iter()
        .find(|view| view.id == id)
        .unwrap();
    assert_eq!(view.status, "paused");
    assert!(server.timers().is_empty());
    assert_eq!(server.resume(&id), Some(true));
    server.settle().await;
    assert_eq!(counter(tag).load(Ordering::SeqCst), 4);
    assert_eq!(server.kill(&id), Some(true));
    assert_eq!(server.outcome(&id), Some(Err((409, "killed".into()))));
}

#[tokio::test]
async fn a_purged_workflow_run_starts_over_under_its_key_with_an_empty_journal() {
    let tag = "purge-me";
    let server = server(ServerConfig::default().time(TimeMode::Manual)).await;
    let (_, body) = post(
        &server,
        &format!("Flow/{tag}/run/send"),
        &format!("\"{tag}\""),
    )
    .await;
    let first: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = first["invocationId"].as_str().unwrap().to_owned();
    server.settle().await;
    assert_eq!(counter(tag).load(Ordering::SeqCst), 1);
    assert_eq!(
        server.purge(&id),
        Some(false),
        "a running invocation is not purged"
    );
    assert_eq!(server.kill(&id), Some(true));
    assert_eq!(server.purge(&id), Some(true));
    assert!(
        server.invocations().iter().all(|view| view.id != id),
        "a purged invocation is gone"
    );
    assert_eq!(server.purge(&id), None, "a purged id names nothing");

    let (status, body) = post(
        &server,
        &format!("Flow/{tag}/run/send"),
        &format!("\"{tag}\""),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let second: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        body.contains("\"Accepted\""),
        "the key's run was forgotten: {body}"
    );
    assert_eq!(
        second["invocationId"].as_str(),
        Some(id.as_str()),
        "a workflow's id derives from its key"
    );
    server.settle().await;
    assert_eq!(
        counter(tag).load(Ordering::SeqCst),
        2,
        "the fresh run replays nothing: its effect runs again"
    );
}

#[tokio::test]
async fn cancelling_a_suspended_workflow_ends_it_as_cancelled() {
    let tag = "cancel-me";
    let server = server(ServerConfig::default().time(TimeMode::Manual)).await;
    let (_, body) = post(
        &server,
        &format!("Flow/{tag}/run/send"),
        &format!("\"{tag}\""),
    )
    .await;
    let id: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = id["invocationId"].as_str().unwrap().to_owned();
    server.settle().await;
    assert_eq!(server.cancel(&id), Some(true));
    server.settle().await;
    assert_eq!(server.outcome(&id), Some(Err((409, "cancelled".into()))));
}

#[tokio::test]
async fn one_seed_reproduces_the_same_journals() {
    let mut digests = Vec::new();
    for _ in 0..3 {
        let server = server(ServerConfig::default().with_seed(42)).await;
        post(&server, "Caller/twice", "\"d\"").await;
        post(&server, "Caller/await_awakeable", "").await;
        server.settle().await;
        digests.push(server.journal_digest());
    }
    assert!(
        digests.windows(2).all(|pair| pair[0] == pair[1]),
        "{digests:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serial_scheduling_runs_one_attempt_at_a_time_in_one_seeded_order() {
    let mut traces = Vec::new();
    for _ in 0..3 {
        *GAUGE.lock().unwrap() = (0, 0);
        let server = server(
            ServerConfig::default()
                .with_seed(11)
                .scheduling(Scheduling::Serial),
        )
        .await;
        for index in 0..6 {
            assert_eq!(
                post(&server, "Gauge/work/send", &format!("\"{index}\""))
                    .await
                    .0,
                202
            );
        }
        server.settle().await;
        let completed = server
            .invocations()
            .iter()
            .filter(|view| view.status == "completed")
            .count();
        assert_eq!(
            completed,
            6,
            "{:#?} {:?}",
            server.invocations(),
            server.schedule_trace()
        );
        assert_eq!(GAUGE.lock().unwrap().1, 1, "one handler ran at a time");
        assert_eq!(server.stats().stall_preemptions, 0);
        traces.push(server.schedule_trace());
    }
    assert!(traces[0].len() >= 6, "{:?}", traces[0]);
    assert!(
        traces.windows(2).all(|pair| pair[0] == pair[1]),
        "one seed grants the turn in one order: {traces:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handler_waiting_on_its_own_ingress_request_yields_the_serial_turn() {
    for config in [
        ServerConfig::default().scheduling(Scheduling::Serial),
        ServerConfig::default()
            .always_replay(true)
            .scheduling(Scheduling::Serial),
    ] {
        let server = server(config).await;
        *INGRESS.lock().unwrap() = Some(server.clone());
        assert_eq!(
            post(&server, "Relay/ask", "\"relay\"").await,
            (200, "\"1\"".into())
        );
        // The relay's request is attributed to its attempt, so the turn
        // moves to the counter at once instead of after a stall.
        assert_eq!(server.stats().stall_preemptions, 0);
        *INGRESS.lock().unwrap() = None;
    }
}
