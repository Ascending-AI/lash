//! The server double's own semantics, on small handlers: the Restate
//! behaviours lash builds on, each checked in streaming and in always-replay
//! mode.

#![expect(
    clippy::unwrap_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use lash_http_transport::{HttpMethod, HttpRequest, read_http_body_bytes};
use lash_restate_test::{CrashPoint, CrashRule, RestateTestServer, ServerConfig, TimeMode};
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

fn endpoint() -> Endpoint {
    Endpoint::builder()
        .bind(Counter)
        .bind(Flow)
        .bind(Caller)
        .bind(Resolver)
        .bind(Flaky)
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

fn modes() -> [ServerConfig; 4] {
    [
        ServerConfig::default(),
        ServerConfig::default().always_replay(true),
        ServerConfig::default().protocol(lash_restate_test::ProtocolVersion::V7),
        ServerConfig::default()
            .protocol(lash_restate_test::ProtocolVersion::V7)
            .always_replay(true),
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
