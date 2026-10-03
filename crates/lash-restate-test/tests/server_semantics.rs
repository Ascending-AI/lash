//! The server double's own semantics, on small handlers: the Restate
//! behaviours lash builds on, each checked in streaming and in always-replay
//! mode, under concurrent scheduling.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]
// FIG-2971: test code; the live-Restate leg reads the suite runner's env
// (RESTATE_INGRESS_URL, endpoint binds) — ambient env access is sanctioned
// in test targets.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lash_http_transport::{HttpMethod, HttpRequest, read_http_body_bytes};
use lash_restate_test::{
    AttemptDispatch, CrashPoint, CrashRule, DeploymentHooks, DeploymentId, Refusal,
    RemoveDeploymentError, RestateTestServer, ResumeDeployment, ResumeRefusal, ServerConfig,
    TimeMode,
};
use restate_sdk::prelude::*;

// `#[restate_sdk::*]` expansions name `::restate_sdk` absolute paths; the SDK
// reaches this crate through lash's re-export, so the crate answers to that
// name and generated code resolves the modules below at the crate root.
extern crate self as restate_sdk;
#[allow(unused_imports)]
use lash_restate::restate_sdk::{
    context, discovery, endpoint, errors, handler, http_server, ingress, object, prelude, service,
    workflow,
};

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

struct HeldTimerFrames;

#[restate_sdk::service]
impl HeldTimerFrames {
    #[handler]
    async fn sleep(&self, ctx: Context<'_>) -> HandlerResult<()> {
        let sleep = ctx.sleep(Duration::from_millis(400));
        // Keep the stamped command inside the endpoint's response poll.
        std::thread::sleep(Duration::from_millis(20));
        sleep.await?;
        Ok(())
    }

    #[handler]
    async fn send(&self, ctx: Context<'_>) -> HandlerResult<()> {
        ctx.object_client::<CounterClient>("held-send")
            .add(Json(1))
            .send_after(Duration::from_millis(400));
        std::thread::sleep(Duration::from_millis(20));
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

    /// Calls `Counter/{key}/add` through the server's ingress straight from
    /// the handler, outside any `ctx.run`, and returns its answer.
    #[handler]
    async fn ask_direct(
        &self,
        _ctx: Context<'_>,
        Json(key): Json<String>,
    ) -> HandlerResult<Json<String>> {
        let server = INGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        Ok(Json(
            post(&server, &format!("Counter/{key}/add"), "1").await.1,
        ))
    }
}

/// Ingress for a request spawned beside a handler.
static BESIDE_INGRESS: Mutex<Option<RestateTestServer>> = Mutex::new(None);

struct Beside;

#[restate_sdk::service]
impl Beside {
    /// Waits inside `ctx.run` for a spawned task that calls ingress.
    #[handler]
    async fn ask(&self, ctx: Context<'_>, Json(key): Json<String>) -> HandlerResult<Json<String>> {
        let server = BESIDE_INGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        let answer = ctx
            .run(|| async move {
                let task = tokio::spawn(async move {
                    post(&server, &format!("Counter/{key}/add"), "1").await.1
                });
                Ok(task.await.unwrap())
            })
            .name("ask-beside")
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
        .bind(Relay)
        .bind(Beside)
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
    let body = read_http_body_bytes(response.body, 16 * 1024 * 1024, None, "body")
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
async fn sdk_sleep_deadline_survives_a_held_response_frame() {
    for config in modes() {
        let server = RestateTestServer::start(
            Endpoint::builder().bind(HeldTimerFrames).build(),
            config.time(TimeMode::Manual),
        )
        .await
        .unwrap();
        let start = server.now_ms();
        assert_eq!(post(&server, "HeldTimerFrames/sleep/send", "").await.0, 202);
        server.settle().await;
        let invocation = server.invocations().into_iter().next().unwrap();
        let journal = server.journal(&invocation.id).unwrap();
        let stamped = journal
            .iter()
            .find(|entry| entry.ty == lash_restate_test::protocol::MessageType::SleepCommand)
            .unwrap();
        use prost::Message as _;
        let sleep = lash_restate_test::protocol::generated::SleepCommandMessage::decode(
            stamped.payload.clone(),
        )
        .unwrap();
        let timers = server.timers();
        assert_eq!(timers.len(), 1, "{timers:?}");
        assert_eq!(server.advance_to(timers[0].fire_at_ms - 1), 0);
        assert!(server.outcome(&invocation.id).is_none());
        let fire_at = server.fire_next_timer().unwrap();
        assert_eq!(fire_at, timers[0].fire_at_ms);
        eprintln!(
            "SDK sleep: stamp={}ms, response hold=20ms, virtual start={start}ms, deadline={}ms, fired={fire_at}ms, elapsed={}ms",
            sleep.wake_up_time,
            timers[0].fire_at_ms,
            fire_at - start,
        );
        assert!(
            fire_at >= start + 400,
            "the SDK's 400ms sleep fired after {}ms despite its 20ms response hold",
            fire_at - start,
        );
        server.settle().await;
        assert!(server.outcome(&invocation.id).unwrap().is_ok());
    }
}

#[tokio::test]
async fn sdk_delayed_send_deadline_survives_a_held_response_frame() {
    for config in modes() {
        let server = RestateTestServer::start(
            Endpoint::builder()
                .bind(HeldTimerFrames)
                .bind(Counter)
                .build(),
            config.time(TimeMode::Manual),
        )
        .await
        .unwrap();
        let start = server.now_ms();
        assert_eq!(post(&server, "HeldTimerFrames/send", "").await.0, 200);
        server.settle().await;
        let timers = server.timers();
        assert_eq!(timers.len(), 1, "{timers:?}");
        assert_eq!(server.advance_to(timers[0].fire_at_ms - 1), 0);
        assert_eq!(post(&server, "Counter/held-send/read", "").await.1, "0");
        let fire_at = server.fire_next_timer().unwrap();
        assert_eq!(fire_at, timers[0].fire_at_ms);
        assert!(
            fire_at >= start + 400,
            "the SDK's 400ms delayed send fired after {}ms despite its 20ms response hold",
            fire_at - start,
        );
        server.settle().await;
        assert_eq!(post(&server, "Counter/held-send/read", "").await.1, "1");
    }
}

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
        assert!(timers[0].fire_at_ms >= start + 60_000);
        assert_eq!(
            post(&server, &format!("Flow/{tag}/peek"), "").await,
            (200, "null".into())
        );

        server.advance_to(timers[0].fire_at_ms);
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
    let body = read_http_body_bytes(response.body, 16 * 1024 * 1024, None, "body")
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
    assert!(server.fire_next_timer().is_some());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handler_waiting_on_its_own_ingress_request_completes() {
    for config in [
        ServerConfig::default(),
        ServerConfig::default().always_replay(true),
    ] {
        let server = server(config).await;
        *INGRESS.lock().unwrap() = Some(server.clone());
        assert_eq!(
            post(&server, "Relay/ask", "\"relay\"").await,
            (200, "\"1\"".into())
        );
        assert_eq!(
            post(&server, "Relay/ask_direct", "\"direct\"").await,
            (200, "\"1\"".into())
        );
        *INGRESS.lock().unwrap() = None;
    }
}

/// A handler inside `ctx.run` waits on a spawned task whose ingress request
/// must complete before the handler can finish.
#[tokio::test]
async fn a_spawned_request_completes_while_its_handler_waits() {
    for config in [
        ServerConfig::default(),
        ServerConfig::default().always_replay(true),
    ] {
        let server = server(config).await;
        *BESIDE_INGRESS.lock().unwrap() = Some(server.clone());
        let answer = tokio::time::timeout(
            Duration::from_secs(20),
            post(&server, "Beside/ask", "\"beside\""),
        )
        .await
        .expect("the spawned request completes");
        assert_eq!(answer, (200, "\"1\"".into()));
        *BESIDE_INGRESS.lock().unwrap() = None;
    }
}

// ---------------------------------------------------------------------------
// Several deployments on one server (FIG-3795 part B)
// ---------------------------------------------------------------------------

/// What a deployment's `served` hook records per dispatch: the deployment
/// id, the invocation id, the target's key and the attempt number.
type ServedLog = Arc<Mutex<Vec<(String, String, Option<String>, u32)>>>;

fn served_log() -> ServedLog {
    Arc::new(Mutex::new(Vec::new()))
}

/// A `served` hook recording every dispatch the deployment sees into `log`.
fn served_hook(log: &ServedLog) -> DeploymentHooks {
    let log = Arc::clone(log);
    DeploymentHooks {
        served: Some(Arc::new(move |dispatch: &AttemptDispatch| {
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((
                    dispatch.deployment.to_string(),
                    dispatch.invocation_id.clone(),
                    dispatch.key.clone(),
                    dispatch.attempt,
                ));
        })),
        refuse: None,
    }
}

/// The dispatch records `log` holds for invocation `id`: `(deployment,
/// attempt)` in dispatch order.
fn dispatches_of(log: &ServedLog, id: &str) -> Vec<(String, u32)> {
    log.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(_, invocation, _, _)| invocation == id)
        .map(|(deployment, _, _, attempt)| (deployment.clone(), *attempt))
        .collect()
}

/// The invocation id a `…/send` submission returned.
async fn send_invocation(server: &RestateTestServer, path: &str, body: &str) -> String {
    let (status, body) = post(server, &format!("{path}/send"), body).await;
    assert_eq!(status, 202, "{path}: {body}");
    let response: serde_json::Value = serde_json::from_str(&body).unwrap();
    response["invocationId"].as_str().unwrap().to_owned()
}

/// A bodyless admin request (DELETE, PATCH), as an operator sends it.
async fn request(server: &RestateTestServer, method: HttpMethod, path: &str) -> (u16, String) {
    let request = HttpRequest::new(method, format!("{}/{path}", server.ingress_url()), "");
    let response = server.transport().send(request, None).await.unwrap();
    let status = response.status;
    let body = read_http_body_bytes(response.body, 16 * 1024 * 1024, None, "body")
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Shift `id` to paused under manual time and return its view: the handler
/// retry policy pauses it after three failed attempts.
async fn driven_to_paused(server: &RestateTestServer, id: &str) {
    server.settle().await;
    loop {
        let view = server
            .invocations()
            .into_iter()
            .find(|view| view.id == id)
            .unwrap();
        match view.status {
            "paused" => return,
            "backing-off" => {
                assert!(
                    server.fire_next_timer().is_some(),
                    "{id} is backing off without a retry timer"
                );
                server.settle().await;
            }
            status => panic!("{id} should pause, it is {status}"),
        }
    }
}

#[tokio::test]
async fn every_registration_adds_a_deployment_with_a_fresh_id() {
    let server = RestateTestServer::new(ServerConfig::default()).unwrap();
    assert!(server.deployments().is_empty());
    let first = server.register(endpoint()).await.unwrap();
    let second = server.register(endpoint()).await.unwrap();
    assert_ne!(first, second);
    for id in [&first, &second] {
        assert!(id.as_str().starts_with("dp_"), "{id}");
    }
    assert_eq!(server.deployments(), vec![first, second]);
}

#[tokio::test]
async fn a_new_invocation_routes_to_the_newest_deployment_serving_the_service() {
    let server = RestateTestServer::new(ServerConfig::default()).unwrap();
    let first = server.register(endpoint()).await.unwrap();
    // A newer deployment serving only Counter takes Counter's new
    // invocations and leaves every other name's routing alone.
    let second = server
        .register(Endpoint::builder().bind(Counter).build())
        .await
        .unwrap();
    assert_eq!(server.route_to("Counter"), Some(second.clone()));
    assert_eq!(server.route_to("Flow"), Some(first.clone()));
    assert_eq!(server.route_to("NoSuchService"), None);

    let counter_invocation = send_invocation(&server, "Counter/route/add", "1").await;
    let flow_invocation = send_invocation(&server, "Flow/routes/run", "\"routes\"").await;
    assert_eq!(
        server.pinned_deployment(&counter_invocation),
        Some(second),
        "Counter's newest deployment takes the new invocation"
    );
    assert_eq!(
        server.pinned_deployment(&flow_invocation),
        Some(first),
        "a deployment that never served Flow takes none of its invocations"
    );
}

#[tokio::test]
async fn retries_and_suspension_resumes_stay_pinned_to_the_starting_deployment() {
    let served = served_log();
    let server = RestateTestServer::new(
        ServerConfig::default()
            .always_replay(true)
            .time(TimeMode::Manual),
    )
    .unwrap();
    let first = server
        .register_with(endpoint(), "n", served_hook(&served))
        .await
        .unwrap();
    // One invocation that retries and one that suspends and resumes, both
    // started while the first deployment was the newest.
    let retrying = send_invocation(&server, "Flaky/fail", "\"pinned-retry\"").await;
    let suspending = send_invocation(&server, "Flow/pinned/run", "\"pinned\"").await;
    server.settle().await;
    // Register the newer build while both are in flight.
    let second = server
        .register_with(endpoint(), "n+1", served_hook(&served))
        .await
        .unwrap();
    assert_eq!(server.route_to("Flaky"), Some(second.clone()));

    // The retry fires on the deployment the invocation started on.
    assert!(server.fire_next_timer().is_some());
    server.settle().await;
    // So does the suspended invocation's resume — its sleep fired — and the
    // resume its approval executes.
    server.advance(Duration::from_secs(60));
    server.settle().await;
    assert_eq!(post(&server, "Flow/pinned/approve", "\"yes\"").await.0, 200);
    server.settle().await;

    // The third attempt — the Flaky retry policy's last before the
    // invocation pauses — fired when `advance` carried virtual time past its
    // backoff alongside the pinned run's sleep.
    let retrying_dispatches = dispatches_of(&served, &retrying);
    assert_eq!(
        retrying_dispatches,
        vec![
            (first.as_str().to_owned(), 1),
            (first.as_str().to_owned(), 2),
            (first.as_str().to_owned(), 3),
        ],
        "every attempt of the retrying invocation dispatched to its deployment"
    );
    let suspending_dispatches = dispatches_of(&served, &suspending);
    assert!(
        suspending_dispatches.len() >= 2
            && suspending_dispatches
                .iter()
                .all(|(deployment, _)| deployment == first.as_str()),
        "replay on suspend and on resume stayed on {first}: {suspending_dispatches:?}"
    );
    assert_eq!(
        server
            .outcome(&suspending)
            .map(|outcome| { outcome.map(|bytes| String::from_utf8(bytes.to_vec()).unwrap()) }),
        Some(Ok("\"1:yes\"".to_owned()))
    );

    // A fresh invocation of the same services still routes to the newest.
    let fresh = send_invocation(&server, "Counter/fresh/add", "1").await;
    assert_eq!(server.pinned_deployment(&fresh), Some(second));
}

#[tokio::test]
async fn resuming_on_another_deployment_repins_the_invocation() {
    let served = served_log();
    let server = RestateTestServer::new(ServerConfig::default().time(TimeMode::Manual)).unwrap();
    let first = server
        .register_with(endpoint(), "n", served_hook(&served))
        .await
        .unwrap();
    let id = send_invocation(&server, "Flaky/fail", "\"repin\"").await;
    driven_to_paused(&server, &id).await;
    assert_eq!(server.pinned_deployment(&id), Some(first.clone()));
    let second = server
        .register_with(endpoint(), "n+1", served_hook(&served))
        .await
        .unwrap();

    // A resume naming a deployment that does not exist is refused.
    let unknown = DeploymentId::new("dp_unknown");
    assert_eq!(
        server.resume_on(&id, &ResumeDeployment::Id(unknown.clone())),
        Some(Err(ResumeRefusal::UnknownDeployment(unknown)))
    );
    // `?deployment=latest` over the admin route, as an operator sends it.
    let (status, body) = request(
        &server,
        HttpMethod::Patch,
        &format!("invocations/{id}/resume?deployment=latest"),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(server.pinned_deployment(&id), Some(second.clone()));
    server.settle().await;
    assert_eq!(
        dispatches_of(&served, &id).last(),
        Some(&(second.as_str().to_owned(), 4)),
        "the resumed attempt dispatched to the newest deployment"
    );
    // While it is backing off again, a resume is refused by status — only a
    // paused invocation resumes.
    assert_eq!(
        server.resume_on(&id, &ResumeDeployment::Latest),
        Some(Err(ResumeRefusal::Status("backing-off")))
    );
    driven_to_paused(&server, &id).await;
    // A resume naming the first deployment re-pins to it.
    assert_eq!(
        server.resume_on(&id, &ResumeDeployment::Id(first.clone())),
        Some(Ok(()))
    );
    assert_eq!(server.pinned_deployment(&id), Some(first));
    server.settle().await;
    assert_eq!(server.kill(&id), Some(true));
}

#[tokio::test]
async fn removing_a_deployment_with_pinned_invocations_is_refused_unless_forced() {
    let served = served_log();
    let server = RestateTestServer::new(ServerConfig::default().time(TimeMode::Manual)).unwrap();
    let first = server
        .register_with(endpoint(), "n", served_hook(&served))
        .await
        .unwrap();
    // A completed invocation does not keep its deployment pinned.
    assert_eq!(post(&server, "Counter/done/add", "1").await.0, 200);
    let pinned = send_invocation(&server, "Flaky/fail", "\"held\"").await;
    driven_to_paused(&server, &pinned).await;

    // Refused over the admin route and over the API while the paused
    // invocation is pinned to it.
    let (status, body) =
        request(&server, HttpMethod::Delete, &format!("deployments/{first}")).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        server.remove_deployment(&first, false),
        Err(RemoveDeploymentError::Pinned(1))
    );
    let (status, body) = request(&server, HttpMethod::Delete, "deployments/dp_absent").await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(
        server.remove_deployment(&DeploymentId::new("dp_absent"), false),
        Err(RemoveDeploymentError::UnknownDeployment(DeploymentId::new(
            "dp_absent"
        )))
    );

    // Forced removal stands; the pinned invocation's next attempt ends the
    // way a call to a deleted deployment does — retryable.
    let (status, body) = request(
        &server,
        HttpMethod::Delete,
        &format!("deployments/{first}?force=true"),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    assert!(server.deployments().is_empty());
    assert_eq!(server.resume(&pinned), Some(true));
    server.settle().await;
    let view = server
        .invocations()
        .into_iter()
        .find(|view| view.id == pinned)
        .unwrap();
    assert_eq!(view.status, "backing-off");
    assert!(
        view.last_failure
            .as_ref()
            .is_some_and(|(_, message)| message.contains("no longer registered")),
        "{:?}",
        view.last_failure
    );
    driven_to_paused(&server, &pinned).await;

    // Resume on the latest deployment re-routes the orphaned invocation to
    // the replacement build, which finishes unpinned work on its own.
    let second = server
        .register_with(endpoint(), "n+1", served_hook(&served))
        .await
        .unwrap();
    assert_eq!(
        server.resume_on(&pinned, &ResumeDeployment::Latest),
        Some(Ok(()))
    );
    assert_eq!(server.pinned_deployment(&pinned), Some(second.clone()));
    assert_eq!(server.kill(&pinned), Some(true));
    // Nothing uncompleted is pinned anymore: removal is free now.
    assert_eq!(server.remove_deployment(&second, false), Ok(()));
    assert_eq!(server.route_to("Flaky"), None);
}

#[tokio::test]
async fn a_build_can_refuse_a_dispatch_while_staying_the_pinned_target() {
    let served = served_log();
    let server = RestateTestServer::new(ServerConfig::default().time(TimeMode::Manual)).unwrap();
    server
        .register_with(endpoint(), "n", served_hook(&served))
        .await
        .unwrap();
    // Build N+1 turns Flaky away retryably and Resolver terminally, as a
    // build that cannot take those handovers would.
    let second = server
        .register_with(
            endpoint(),
            "n+1",
            DeploymentHooks {
                refuse: Some(Arc::new(|dispatch: &AttemptDispatch| {
                    match dispatch.service.as_str() {
                        "Flaky" => Some(Refusal::Retryable),
                        "Resolver" => Some(Refusal::Terminal),
                        _ => None,
                    }
                })),
                ..served_hook(&served)
            },
        )
        .await
        .unwrap();

    let refused = send_invocation(&server, "Flaky/fail", "\"refused\"").await;
    server.settle().await;
    assert_eq!(
        server.pinned_deployment(&refused),
        Some(second.clone()),
        "the refusal is the deployment's answer, not a re-route"
    );
    assert_eq!(
        counter("refused").load(Ordering::SeqCst),
        0,
        "the refused call never reached the endpoint"
    );
    assert_eq!(
        dispatches_of(&served, &refused),
        vec![(second.as_str().to_owned(), 1)]
    );
    let view = server
        .invocations()
        .into_iter()
        .find(|view| view.id == refused)
        .unwrap();
    assert_eq!(view.status, "backing-off");
    assert!(
        view.last_failure
            .as_ref()
            .is_some_and(|(_, message)| message.contains("refused")),
        "{:?}",
        view.last_failure
    );

    let terminal = send_invocation(&server, "Resolver/resolve", "\"awakeable\"").await;
    server.settle().await;
    let outcome = server.outcome(&terminal).unwrap();
    assert!(
        matches!(outcome, Err((_, ref message)) if message.contains("refused")),
        "{outcome:?}"
    );
    assert_eq!(
        counter("refused").load(Ordering::SeqCst),
        0,
        "the terminally refused call never ran either"
    );
    assert_eq!(server.kill(&refused), Some(true));
}

/// A build that is down for now turns a call away without spending the
/// handler's retry attempts: the call outlasts an outage of more dispatches
/// than its policy allows and runs once the build is back. A build that
/// refuses the same call retryably pauses it at the policy's last attempt.
#[tokio::test]
async fn a_build_down_for_now_spends_none_of_the_retry_attempts() {
    const MAX_ATTEMPTS: u32 = 3;
    let mut config = ServerConfig::default().time(TimeMode::Manual);
    config.retry.max_attempts = Some(MAX_ATTEMPTS);
    let server = RestateTestServer::new(config).unwrap();
    let verdict = Arc::new(Mutex::new(Some(Refusal::Unavailable)));
    server
        .register_with(
            endpoint(),
            "n",
            DeploymentHooks {
                refuse: Some(Arc::new({
                    let verdict = Arc::clone(&verdict);
                    move |_: &AttemptDispatch| {
                        *verdict
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    }
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let set = |next: Option<Refusal>| {
        *verdict
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    };
    let view = |id: &str| {
        server
            .invocations()
            .into_iter()
            .find(|view| view.id == id)
            .unwrap()
    };

    let outlasting = send_invocation(&server, "Counter/outage/add", "2").await;
    server.settle().await;
    // The outage lasts three times the dispatches the policy allows.
    for _ in 0..MAX_ATTEMPTS * 3 {
        let down = view(&outlasting);
        assert_eq!(down.status, "backing-off", "{down:?}");
        assert_eq!(down.retry_count, 0, "an outage spends no retry attempt");
        assert!(
            down.last_failure
                .as_ref()
                .is_some_and(|(_, message)| message.contains("unavailable")),
            "{:?}",
            down.last_failure
        );
        assert!(server.fire_next_timer().is_some());
        server.settle().await;
    }
    assert!(view(&outlasting).attempts > MAX_ATTEMPTS);
    set(None);
    assert!(server.fire_next_timer().is_some());
    server.settle().await;
    assert_eq!(
        server
            .outcome(&outlasting)
            .map(|outcome| outcome.map(|answer| answer.to_vec())),
        Some(Ok(b"2".to_vec())),
        "the call ran once the build was back: {:?}",
        view(&outlasting)
    );

    // The control: the same build refusing retryably pauses the call.
    set(Some(Refusal::Retryable));
    let refused = send_invocation(&server, "Counter/refused/add", "2").await;
    driven_to_paused(&server, &refused).await;
    assert_eq!(view(&refused).attempts, MAX_ATTEMPTS);
}

/// Build N+1 registered while a job is parked in build N: the next new
/// invocation runs on N+1, and the pinned one — through a crash replay —
/// finishes on N.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_build_serves_new_invocations_while_pinned_ones_finish_on_theirs() {
    let served = served_log();
    let backend = lash_restate_test::backend_with_build(
        0x3795,
        ServerConfig::default(),
        "n",
        served_hook(&served),
    )
    .await
    .expect("build the Restate test backend");
    let server = backend.server().clone();
    let [build_n] = server
        .deployments()
        .try_into()
        .unwrap_or_else(|_| panic!("the backend's first build is one deployment"));

    // Job one's attempt parks on a gate the test holds, so it stays in
    // flight while the second build registers.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let job_one = tokio::spawn({
        let backend = backend.clone();
        let gate = Arc::clone(&gate);
        let attempt: lash_restate_test::HandlerAttempt = Arc::new(move |_scoped| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let _permit = gate.acquire().await.expect("the gate never closes");
            })
        });
        async move {
            backend
                .run_in_handler(
                    lash_core::AdmittedScope::turn("upgrade-e2e", "turn-n"),
                    attempt,
                )
                .await
        }
    });
    // A session-scoped job key is a turn-workflow key: the session length,
    // a colon, the session, then the job's ordinal.
    let job_key = |ordinal: u64| {
        lash_restate::turn_invocation_key(
            &lash_core::engine::ShiftRequest {
                session: lash_core::SessionId::from("upgrade-e2e"),
                request: lash_core::engine::ShiftRequestId::new(format!("job--{ordinal}")),
                intended_lane: None,
            },
            0,
        )
    };
    // Wait for its first attempt to be served by build N.
    let pinned_id = loop {
        let found = served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(deployment, _, key, _)| {
                deployment == build_n.as_str() && key.as_deref() == Some(job_key(0).as_str())
            })
            .map(|(_, id, _, _)| id.clone());
        if let Some(id) = found {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(server.pinned_deployment(&pinned_id), Some(build_n.clone()));

    let build_n1 = backend
        .add_build(
            lash_core::engine::BuildGeneration::for_test("n+1"),
            "n+1",
            served_hook(&served),
        )
        .await
        .expect("register build N+1");
    assert_ne!(build_n, build_n1);
    assert_eq!(
        server.route_to("LashTestHandlerHost"),
        Some(build_n1.clone())
    );

    // Crashing the parked attempt replays it — still on build N.
    assert!(server.crash(&pinned_id), "job one's attempt is live");
    loop {
        if dispatches_of(&served, &pinned_id).len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The next job is a new invocation: it routes to build N+1.
    let job_two: lash_restate_test::HandlerAttempt =
        Arc::new(move |_scoped| Box::pin(async move {}));
    backend
        .run_in_handler(
            lash_core::AdmittedScope::turn("upgrade-e2e", "turn-n1"),
            job_two,
        )
        .await
        .expect("job two completes");
    assert!(
        served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(deployment, _, key, _)| {
                deployment == build_n1.as_str() && key.as_deref() == Some(job_key(1).as_str())
            }),
        "job two was served by build N+1"
    );

    // Job one finishes where it started.
    gate.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    tokio::time::timeout(Duration::from_secs(20), job_one)
        .await
        .expect("job one's caller is answered")
        .expect("job one's task finished")
        .expect("job one's handler completed");
    assert_eq!(
        server.pinned_deployment(&pinned_id),
        Some(build_n.clone()),
        "job one stayed pinned to build N for its whole life"
    );
    let build_n_dispatches: Vec<u32> = served
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(deployment, _, key, _)| {
            key.as_deref() == Some(job_key(0).as_str()) && deployment == build_n.as_str()
        })
        .map(|(_, _, _, attempt)| *attempt)
        .collect();
    assert_eq!(build_n_dispatches, vec![1, 2]);
    assert!(
        dispatches_of(&served, &pinned_id)
            .iter()
            .all(|(deployment, _)| deployment == build_n.as_str()),
        "no attempt of job one ever dispatched to build N+1"
    );
}

// ---------------------------------------------------------------------------
// Concurrent started runs (FIG-4871): eager named handles, configured retry
// policies, crash cuts and cancellation through Lash's SDK re-export
// ---------------------------------------------------------------------------

/// What one execution of a call's body does once the test releases it.
#[derive(Clone, Copy)]
enum Step {
    Succeed,
    /// A retryable failure: the run's retry policy decides what follows.
    Transient,
    /// A terminal failure, recorded as the call's result.
    Refuse,
}

/// What a `Calls` handler saw, in order, with every crash marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observed {
    Result(usize),
    Crash,
}

/// One handle's settlement as the handler read it.
type Settled = Result<String, (u16, String)>;

/// The bodies of up to three calls. Each execution counts itself and parks
/// until the test releases it, then does what `script` says for that call
/// and execution; its receipt `call-<call>@<execution>` names exactly which
/// execution produced a recorded result.
struct Bodies {
    executions: [AtomicUsize; 3],
    /// Executions dropped while parked: their closure never finished.
    abandoned: [AtomicUsize; 3],
    release: [tokio::sync::Notify; 3],
    script: fn(usize, usize) -> Step,
    observed: Mutex<Vec<Observed>>,
}

impl Bodies {
    fn new(script: fn(usize, usize) -> Step) -> Arc<Self> {
        Arc::new(Self {
            executions: Default::default(),
            abandoned: Default::default(),
            release: Default::default(),
            script,
            observed: Mutex::default(),
        })
    }

    fn executions(&self) -> [usize; 3] {
        std::array::from_fn(|call| self.executions[call].load(Ordering::SeqCst))
    }

    fn abandoned(&self) -> [usize; 3] {
        std::array::from_fn(|call| self.abandoned[call].load(Ordering::SeqCst))
    }

    fn observed(&self) -> Vec<Observed> {
        self.observed.lock().unwrap().clone()
    }
}

fn always_succeed(_call: usize, _execution: usize) -> Step {
    Step::Succeed
}

/// Counts a parked execution that is dropped before it finishes.
struct Parked(Option<(Arc<Bodies>, usize)>);

impl Drop for Parked {
    fn drop(&mut self) {
        if let Some((bodies, call)) = self.0.take() {
            bodies.abandoned[call].fetch_add(1, Ordering::SeqCst);
        }
    }
}

async fn call_body(bodies: Arc<Bodies>, call: usize) -> HandlerResult<Json<String>> {
    let execution = bodies.executions[call].fetch_add(1, Ordering::SeqCst) + 1;
    let mut parked = Parked(Some((Arc::clone(&bodies), call)));
    bodies.release[call].notified().await;
    parked.0 = None;
    let receipt = format!("call-{call}@{execution}");
    match (bodies.script)(call, execution) {
        Step::Succeed => Ok(Json(receipt)),
        Step::Transient => Err(HandlerError::from(std::io::Error::other(receipt))),
        Step::Refuse => Err(TerminalError::new_with_code(418, receipt).into()),
    }
}

type CallHandle = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Json<String>, TerminalError>> + Send>,
>;

struct Calls {
    bodies: Arc<Bodies>,
    retry: RunRetryPolicy,
}

impl Calls {
    /// Registers `count` calls named `call-<n>`, each under the configured
    /// retry policy, before any of them is awaited.
    fn start(&self, ctx: &Context<'_>, count: usize) -> Vec<CallHandle> {
        (0..count)
            .map(|call| {
                let bodies = Arc::clone(&self.bodies);
                Box::pin(
                    ctx.run(move || call_body(bodies, call))
                        .name(format!("call-{call}"))
                        .retry_policy(self.retry.clone())
                        .start(),
                ) as CallHandle
            })
            .collect()
    }
}

#[restate_sdk::service]
impl Calls {
    /// Starts three calls, then reads every handle in issue order and
    /// returns how each settled.
    #[handler]
    async fn issue(&self, ctx: Context<'_>) -> HandlerResult<Json<Vec<Settled>>> {
        let mut settled = Vec::new();
        for (call, handle) in self.start(&ctx, 3).into_iter().enumerate() {
            let result = handle.await;
            self.bodies
                .observed
                .lock()
                .unwrap()
                .push(Observed::Result(call));
            settled.push(
                result
                    .map(|Json(receipt)| receipt)
                    .map_err(|error| (error.code(), error.message().to_owned())),
            );
        }
        Ok(Json(settled))
    }

    /// Starts three calls and returns at the first failed handle, in issue
    /// order, leaving the later calls unread.
    #[handler]
    async fn first_failure(&self, ctx: Context<'_>) -> HandlerResult<Json<Vec<String>>> {
        let mut receipts = Vec::new();
        for handle in self.start(&ctx, 3) {
            receipts.push(handle.await?.0);
        }
        Ok(Json(receipts))
    }

    /// Starts two calls, drops the first call's result future and returns
    /// the second call's receipt.
    #[handler]
    async fn drop_first(&self, ctx: Context<'_>) -> HandlerResult<Json<String>> {
        let mut handles = self.start(&ctx, 2);
        let second = handles.pop().unwrap();
        drop(handles);
        Ok(Json(second.await?.0))
    }
}

/// A server for `Calls`, marking every crash in the bodies' observations.
async fn calls_server(
    config: ServerConfig,
    bodies: &Arc<Bodies>,
    retry: RunRetryPolicy,
) -> RestateTestServer {
    let endpoint = Endpoint::builder()
        .bind(Calls {
            bodies: Arc::clone(bodies),
            retry,
        })
        .build();
    let server = RestateTestServer::start(endpoint, config).await.unwrap();
    let marked = Arc::clone(bodies);
    assert!(server.on_crash(Arc::new(move |_| {
        marked.observed.lock().unwrap().push(Observed::Crash);
    })));
    server
}

fn streaming_modes() -> [ServerConfig; 2] {
    [
        ServerConfig::default(),
        ServerConfig::default().protocol(lash_restate_test::ProtocolVersion::V7),
    ]
}

/// A retry policy whose delays are short enough for the server to fire on
/// its own.
fn bounded(max_attempts: u32) -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(10))
        .max_attempts(max_attempts)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// The receipts the journal recorded for its run completions, in stored
/// order: a value's receipt, or the receipt a failure's message names.
fn receipts(server: &RestateTestServer, id: &str) -> Vec<String> {
    server
        .journal(id)
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.run_completion())
        .map(|result| match result {
            Ok(value) => serde_json::from_slice(&value).unwrap(),
            Err((_, message)) => message,
        })
        .collect()
}

/// The names the journal's commands carry: only runs are named here.
fn run_names(server: &RestateTestServer, id: &str) -> Vec<String> {
    server
        .journal(id)
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.name)
        .collect()
}

/// What the test waits for after releasing an execution.
enum Then {
    /// Its result is in the journal.
    Recorded,
    /// Its failure ended the attempt and the server scheduled a retry.
    Retried,
    /// The server crashed the attempt on its proposal.
    Crashed,
}

/// Releases `execution` of `call` once it has entered, then waits for
/// `then`.
async fn release(
    server: &RestateTestServer,
    id: &str,
    bodies: &Bodies,
    call: usize,
    execution: usize,
    then: Then,
) {
    let receipt = format!("call-{call}@{execution}");
    until(&format!("{receipt} enters"), || {
        bodies.executions[call].load(Ordering::SeqCst) >= execution
    })
    .await;
    assert_eq!(
        bodies.executions[call].load(Ordering::SeqCst),
        execution,
        "{receipt} is the call's newest execution"
    );
    let before = server.stats();
    bodies.release[call].notify_one();
    match then {
        Then::Recorded => {
            until(&format!("{receipt} is recorded"), || {
                receipts(server, id)
                    .iter()
                    .any(|recorded| recorded.contains(&receipt))
            })
            .await;
        }
        Then::Retried => {
            until(&format!("{receipt} is retried"), || {
                server.stats().retries > before.retries
            })
            .await;
        }
        Then::Crashed => {
            until(&format!("{receipt}'s proposal crashes the attempt"), || {
                server.stats().crashes > before.crashes
            })
            .await;
        }
    }
}

async fn finished(server: &RestateTestServer, id: &str) -> Result<bytes::Bytes, (u32, String)> {
    until(&format!("{id} completes"), || server.outcome(id).is_some()).await;
    server.outcome(id).unwrap()
}

async fn settled(server: &RestateTestServer, id: &str) -> Vec<Settled> {
    serde_json::from_slice(&finished(server, id).await.unwrap()).unwrap()
}

fn ok(receipts: [&str; 3]) -> Vec<Settled> {
    receipts.map(|receipt| Ok(receipt.to_owned())).into()
}

/// The order the tests release a batch's first executions in.
const COMPLETION_ORDER: [usize; 3] = [2, 0, 1];

/// L01: every body enters before any completes; released 2/0/1, each call
/// records its own receipt in that order and the handler reads them in
/// issue order. A forced-serial runner never enters all three, and one
/// batch-sized receipt never records three.
#[tokio::test]
async fn concurrent_runs_record_independent_receipts_in_completion_order() {
    for config in modes() {
        let bodies = Bodies::new(always_succeed);
        let server = calls_server(config, &bodies, bounded(1)).await;
        let id = send_invocation(&server, "Calls/issue", "null").await;
        until("every body enters before any is released", || {
            bodies.executions() == [1, 1, 1]
        })
        .await;
        for call in COMPLETION_ORDER {
            release(&server, &id, &bodies, call, 1, Then::Recorded).await;
        }
        assert_eq!(receipts(&server, &id), ["call-2@1", "call-0@1", "call-1@1"]);
        assert_eq!(
            settled(&server, &id).await,
            ok(["call-0@1", "call-1@1", "call-2@1"])
        );
        assert_eq!(run_names(&server, &id), ["call-0", "call-1", "call-2"]);
        assert_eq!(bodies.executions(), [1, 1, 1]);
    }
}

/// Where a crash cuts a batch released 2/0/1.
#[derive(Clone, Copy, Debug)]
enum Cut {
    /// The attempt crashes while the calls after the first `n` released
    /// are still executing.
    Executing(usize),
    /// The attempt crashes on the proposal of the `n`-th released call,
    /// before the server stores or acknowledges it.
    Proposal(usize),
    /// The attempt crashes on its output after every result is recorded;
    /// the double never cuts the terminal frame after it.
    Output,
}

/// L02: a crash at any await or finalization boundary keeps every recorded
/// result without running its body again; only calls without a recorded
/// result execute again, under the same logical call id. A proposal the
/// server has not acknowledged is never read by the handler and is not a
/// result.
#[tokio::test]
async fn a_crash_reruns_only_calls_without_a_recorded_result() {
    let cuts = [
        Cut::Executing(0),
        Cut::Executing(1),
        Cut::Executing(2),
        Cut::Proposal(0),
        Cut::Proposal(1),
        Cut::Proposal(2),
        Cut::Output,
    ];
    for config in modes() {
        for cut in cuts {
            let scenario = format!("{cut:?} on {config:?}");
            let bodies = Bodies::new(always_succeed);
            let server = calls_server(config.clone(), &bodies, bounded(1)).await;
            let kept = match cut {
                Cut::Executing(kept) => kept,
                Cut::Proposal(kept) => {
                    server.crash_on(
                        CrashRule::new(CrashPoint::BeforeRunResult {
                            name: Some(format!("call-{}", COMPLETION_ORDER[kept])),
                        })
                        .service("Calls"),
                    );
                    kept
                }
                Cut::Output => {
                    server.crash_on(
                        CrashRule::new(CrashPoint::BeforeFrame {
                            ty: lash_restate_test::protocol::MessageType::OutputCommand,
                        })
                        .service("Calls"),
                    );
                    COMPLETION_ORDER.len()
                }
            };
            let id = send_invocation(&server, "Calls/issue", "null").await;
            until(&format!("{scenario}: every body enters"), || {
                bodies.executions() == [1, 1, 1]
            })
            .await;
            let (recorded, rerun) = COMPLETION_ORDER.split_at(kept);
            for &call in recorded {
                release(&server, &id, &bodies, call, 1, Then::Recorded).await;
            }
            match cut {
                Cut::Executing(_) => assert!(server.crash(&id), "{scenario}"),
                Cut::Proposal(_) => {
                    release(&server, &id, &bodies, rerun[0], 1, Then::Crashed).await;
                }
                Cut::Output => {}
            }
            for &call in rerun {
                release(&server, &id, &bodies, call, 2, Then::Recorded).await;
            }

            let receipt = |call: usize| {
                let execution = if recorded.contains(&call) { 1 } else { 2 };
                format!("call-{call}@{execution}")
            };
            assert_eq!(
                receipts(&server, &id),
                COMPLETION_ORDER.map(receipt),
                "{scenario}"
            );
            assert_eq!(
                settled(&server, &id).await,
                [0, 1, 2].map(|call| Settled::Ok(receipt(call))),
                "{scenario}"
            );
            assert_eq!(server.stats().crashes, 1, "{scenario}");
            assert_eq!(
                bodies.executions(),
                [0, 1, 2].map(|call| if recorded.contains(&call) { 1 } else { 2 }),
                "{scenario}"
            );
            assert_eq!(
                run_names(&server, &id),
                ["call-0", "call-1", "call-2"],
                "{scenario}"
            );
            let observed = bodies.observed();
            let before_crash = observed
                .split(|seen| *seen == Observed::Crash)
                .next()
                .unwrap();
            assert!(
                before_crash
                    .iter()
                    .all(|seen| matches!(seen, Observed::Result(call) if recorded.contains(call))),
                "{scenario}: the crashed attempt read only recorded results: {observed:?}"
            );
        }
    }
}

/// A reported retryable failure ends the attempt and is retried under the
/// call's own policy, apart from any crash: recorded siblings are reused,
/// unfinished siblings execute again, and each call's command keeps its
/// logical call id.
#[tokio::test]
async fn reported_transient_failures_retry_only_calls_without_a_recorded_result() {
    fn script(call: usize, execution: usize) -> Step {
        match (call, execution) {
            (0, 1) | (1, 2) => Step::Transient,
            _ => Step::Succeed,
        }
    }
    for config in modes() {
        let bodies = Bodies::new(script);
        let server = calls_server(config, &bodies, bounded(5)).await;
        let id = send_invocation(&server, "Calls/issue", "null").await;
        until("every body enters", || bodies.executions() == [1, 1, 1]).await;
        release(&server, &id, &bodies, 2, 1, Then::Recorded).await;
        release(&server, &id, &bodies, 0, 1, Then::Retried).await;
        until("the unrecorded calls execute again", || {
            bodies.executions() == [2, 2, 1]
        })
        .await;
        release(&server, &id, &bodies, 1, 2, Then::Retried).await;
        release(&server, &id, &bodies, 0, 3, Then::Recorded).await;
        release(&server, &id, &bodies, 1, 3, Then::Recorded).await;

        assert_eq!(
            settled(&server, &id).await,
            ok(["call-0@3", "call-1@3", "call-2@1"])
        );
        assert_eq!(receipts(&server, &id), ["call-2@1", "call-0@3", "call-1@3"]);
        assert_eq!(bodies.executions(), [3, 3, 1]);
        assert_eq!(bodies.abandoned(), [1, 1, 0]);
        let stats = server.stats();
        assert_eq!((stats.retries, stats.crashes), (2, 0));
        assert_eq!(run_names(&server, &id), ["call-0", "call-1", "call-2"]);
    }
}

/// Exhausting a call's bounded policy records a terminal failure as that
/// call's result while its siblings keep theirs. The budget counts failed
/// attempts since the invocation's last recorded entry, so a sibling's
/// result recorded between two failures restarts it: the SDK keeps no
/// independent retry budget per concurrent handle.
#[tokio::test]
async fn exhausting_a_bounded_policy_records_a_terminal_result_for_that_call() {
    fn script(call: usize, _execution: usize) -> Step {
        if call == 0 {
            Step::Transient
        } else {
            Step::Succeed
        }
    }
    for config in modes() {
        // Both siblings recorded before the first failure: three executions.
        let bodies = Bodies::new(script);
        let server = calls_server(config.clone(), &bodies, bounded(3)).await;
        let id = send_invocation(&server, "Calls/issue", "null").await;
        until("every body enters", || bodies.executions() == [1, 1, 1]).await;
        release(&server, &id, &bodies, 1, 1, Then::Recorded).await;
        release(&server, &id, &bodies, 2, 1, Then::Recorded).await;
        release(&server, &id, &bodies, 0, 1, Then::Retried).await;
        release(&server, &id, &bodies, 0, 2, Then::Retried).await;
        release(&server, &id, &bodies, 0, 3, Then::Recorded).await;
        let settled_calls = settled(&server, &id).await;
        assert!(
            matches!(&settled_calls[0], Err((500, message)) if message.contains("call-0@3")),
            "{settled_calls:?}"
        );
        assert_eq!(
            settled_calls[1..],
            [Ok("call-1@1".to_owned()), Ok("call-2@1".to_owned())]
        );
        assert_eq!(bodies.executions(), [3, 1, 1]);
        assert_eq!(server.stats().retries, 2);

        // A sibling recorded between the first two failures: four.
        let bodies = Bodies::new(script);
        let server = calls_server(config, &bodies, bounded(3)).await;
        let id = send_invocation(&server, "Calls/issue", "null").await;
        until("every body enters", || bodies.executions() == [1, 1, 1]).await;
        release(&server, &id, &bodies, 2, 1, Then::Recorded).await;
        release(&server, &id, &bodies, 0, 1, Then::Retried).await;
        until("the unrecorded calls execute again", || {
            bodies.executions() == [2, 2, 1]
        })
        .await;
        release(&server, &id, &bodies, 1, 2, Then::Recorded).await;
        release(&server, &id, &bodies, 0, 2, Then::Retried).await;
        release(&server, &id, &bodies, 0, 3, Then::Retried).await;
        release(&server, &id, &bodies, 0, 4, Then::Recorded).await;
        let settled_calls = settled(&server, &id).await;
        assert!(
            matches!(&settled_calls[0], Err((500, message)) if message.contains("call-0@4")),
            "{settled_calls:?}"
        );
        assert_eq!(
            settled_calls[1..],
            [Ok("call-1@2".to_owned()), Ok("call-2@1".to_owned())]
        );
        assert_eq!(bodies.executions(), [4, 2, 1]);
        assert_eq!(server.stats().retries, 3);
    }
}

/// A terminal failure is recorded once as its call's result: no retry, and
/// its siblings settle independently.
#[tokio::test]
async fn a_terminal_failure_is_recorded_once_as_that_calls_result() {
    fn script(call: usize, _execution: usize) -> Step {
        if call == 1 {
            Step::Refuse
        } else {
            Step::Succeed
        }
    }
    for config in modes() {
        let bodies = Bodies::new(script);
        let server = calls_server(config, &bodies, bounded(5)).await;
        let id = send_invocation(&server, "Calls/issue", "null").await;
        until("every body enters", || bodies.executions() == [1, 1, 1]).await;
        for call in COMPLETION_ORDER {
            release(&server, &id, &bodies, call, 1, Then::Recorded).await;
        }
        assert_eq!(
            settled(&server, &id).await,
            [
                Ok("call-0@1".to_owned()),
                Err((418, "call-1@1".to_owned())),
                Ok("call-2@1".to_owned()),
            ]
        );
        assert_eq!(bodies.executions(), [1, 1, 1]);
        assert_eq!(server.stats().retries, 0);
    }
}

/// When the cancel lands relative to the `n`-th released call's result.
#[derive(Clone, Copy, Debug)]
enum CancelAt {
    /// After the first `n` results are recorded.
    After(usize),
    /// Just before the server stores the `n`-th released call's proposal,
    /// after its body finished.
    BeforeResult(usize),
}

/// L03: cancellation settles every handle exactly once, by journal order:
/// a result recorded before the cancel signal stays readable, every other
/// handle settles as cancelled even when its body finished and its result
/// was stored after the signal, and a replay chooses the same way. On a
/// streaming attempt no call still executing at the cancel ever records a
/// result.
#[tokio::test]
async fn cancellation_settles_each_handle_once_by_journal_order() {
    use lash_restate_test::protocol::MessageType;
    let cases = [
        CancelAt::After(0),
        CancelAt::After(1),
        CancelAt::After(2),
        CancelAt::BeforeResult(0),
        CancelAt::BeforeResult(1),
        CancelAt::BeforeResult(2),
    ];
    for config in modes() {
        for at in cases {
            let scenario = format!("{at:?} on {config:?}");
            let bodies = Bodies::new(always_succeed);
            let server = calls_server(config.clone(), &bodies, bounded(1)).await;
            let id = send_invocation(&server, "Calls/issue", "null").await;
            until(&format!("{scenario}: every body enters"), || {
                bodies.executions() == [1, 1, 1]
            })
            .await;
            let (before, released_after) = match at {
                CancelAt::After(kept) => {
                    let before = &COMPLETION_ORDER[..kept];
                    for &call in before {
                        release(&server, &id, &bodies, call, 1, Then::Recorded).await;
                    }
                    assert_eq!(server.cancel(&id), Some(true), "{scenario}");
                    (before, &[][..])
                }
                CancelAt::BeforeResult(kept) => {
                    server.cancel_on(
                        CrashRule::new(CrashPoint::BeforeRunResult {
                            name: Some(format!("call-{}", COMPLETION_ORDER[kept])),
                        })
                        .service("Calls"),
                    );
                    let (before, rest) = COMPLETION_ORDER.split_at(kept);
                    for &call in before {
                        release(&server, &id, &bodies, call, 1, Then::Recorded).await;
                    }
                    release(&server, &id, &bodies, rest[0], 1, Then::Recorded).await;
                    assert_eq!(server.stats().scripted_cancels, 1, "{scenario}");
                    (before, &rest[..1])
                }
            };
            let unreleased: Vec<usize> = COMPLETION_ORDER
                .into_iter()
                .filter(|call| !before.contains(call) && !released_after.contains(call))
                .collect();
            if config.always_replay {
                // A closed request stream cannot carry the signal to an
                // attempt whose bodies still execute: they finish, the
                // attempt suspends and the replay meets the signal.
                for &call in &unreleased {
                    release(&server, &id, &bodies, call, 1, Then::Recorded).await;
                }
            }

            let expected: Vec<bool> = (0..3).map(|call| before.contains(&call)).collect();
            let readable: Vec<bool> = settled(&server, &id)
                .await
                .into_iter()
                .enumerate()
                .map(|(call, settled)| match settled {
                    Ok(receipt) => {
                        assert_eq!(receipt, format!("call-{call}@1"), "{scenario}");
                        true
                    }
                    Err((code, _)) => {
                        assert_eq!(code, 409, "{scenario}");
                        false
                    }
                })
                .collect();
            assert_eq!(readable, expected, "{scenario}");

            let journal = server.journal(&id).unwrap();
            let signal = journal
                .iter()
                .position(|entry| entry.ty == MessageType::SignalNotification)
                .unwrap_or_else(|| panic!("{scenario}: the cancel signal is journaled"));
            let recorded_before: Vec<String> = journal[..signal]
                .iter()
                .filter_map(|entry| entry.run_completion())
                .map(|result| serde_json::from_slice(&result.unwrap()).unwrap())
                .collect();
            assert_eq!(
                recorded_before,
                before
                    .iter()
                    .map(|call| format!("call-{call}@1"))
                    .collect::<Vec<_>>(),
                "{scenario}"
            );
            assert_eq!(bodies.executions(), [1, 1, 1], "{scenario}");
            if !config.always_replay {
                let recorded_after = journal[signal..]
                    .iter()
                    .filter(|entry| entry.run_completion().is_some())
                    .count();
                assert_eq!(recorded_after, released_after.len(), "{scenario}");
                for call in unreleased {
                    until(&format!("{scenario}: call-{call} is dropped"), || {
                        bodies.abandoned()[call] == 1
                    })
                    .await;
                }
            }
        }
    }
}

/// Returning at a failed handle ends the invocation with that failure; the
/// calls still executing are dropped with it, never record a result and are
/// never executed again.
#[tokio::test]
async fn a_failed_handle_ends_the_invocation_and_drops_unfinished_siblings() {
    fn script(call: usize, _execution: usize) -> Step {
        if call == 0 {
            Step::Refuse
        } else {
            Step::Succeed
        }
    }
    for config in streaming_modes() {
        let bodies = Bodies::new(script);
        let server = calls_server(config, &bodies, bounded(5)).await;
        let id = send_invocation(&server, "Calls/first_failure", "null").await;
        until("every body enters", || bodies.executions() == [1, 1, 1]).await;
        release(&server, &id, &bodies, 0, 1, Then::Recorded).await;
        assert_eq!(
            finished(&server, &id).await,
            Err((418, "call-0@1".to_owned()))
        );
        until("the unfinished siblings are dropped", || {
            bodies.abandoned() == [0, 1, 1]
        })
        .await;
        server.settle().await;
        assert_eq!(receipts(&server, &id), ["call-0@1"]);
        assert_eq!(bodies.executions(), [1, 1, 1]);
        assert_eq!(server.stats().retries, 0);
    }
}

/// Dropping a result future neither cancels nor settles its call: the
/// invocation keeps driving the call and records its result while it awaits
/// another, but a successful return drops a call still executing without
/// recording anything. Settlement is only what was awaited.
#[tokio::test]
async fn dropping_a_result_future_neither_cancels_nor_settles_its_call() {
    for config in streaming_modes() {
        let bodies = Bodies::new(always_succeed);
        let server = calls_server(config.clone(), &bodies, bounded(1)).await;
        let id = send_invocation(&server, "Calls/drop_first", "null").await;
        until("both bodies enter", || bodies.executions() == [1, 1, 0]).await;
        release(&server, &id, &bodies, 0, 1, Then::Recorded).await;
        release(&server, &id, &bodies, 1, 1, Then::Recorded).await;
        assert_eq!(finished(&server, &id).await, Ok("\"call-1@1\"".into()));
        assert_eq!(receipts(&server, &id), ["call-0@1", "call-1@1"]);

        let bodies = Bodies::new(always_succeed);
        let server = calls_server(config, &bodies, bounded(1)).await;
        let id = send_invocation(&server, "Calls/drop_first", "null").await;
        until("both bodies enter", || bodies.executions() == [1, 1, 0]).await;
        release(&server, &id, &bodies, 1, 1, Then::Recorded).await;
        assert_eq!(finished(&server, &id).await, Ok("\"call-1@1\"".into()));
        until("the unawaited call is dropped", || {
            bodies.abandoned() == [1, 0, 0]
        })
        .await;
        assert_eq!(receipts(&server, &id), ["call-1@1"]);
        assert_eq!(run_names(&server, &id), ["call-0", "call-1"]);
    }
}

// ---------------------------------------------------------------------------
// The same laws against a live restate-server
// (scripts/restate-suites.toml: suite `server-double`, leg `live`)
// ---------------------------------------------------------------------------

/// The live oracle's probe: `run` waits on its `go` promise, so the test
/// can hold the invocation in flight while deployments change; `go`
/// resolves it.
struct DeploymentProbe;

#[restate_sdk::workflow(name = "DeploymentProbe")]
impl DeploymentProbe {
    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(tag): Json<String>,
    ) -> HandlerResult<Json<String>> {
        let go = ctx.promise::<String>("go").await?;
        Ok(Json(format!("{tag}:{go}")))
    }

    #[handler]
    async fn go(&self, ctx: SharedWorkflowContext<'_>) -> HandlerResult<()> {
        ctx.resolve_promise::<String>("go", "yes".to_owned());
        Ok(())
    }
}

/// An endpoint bound on loopback and registered against the live server;
/// dropping it stops the listener.
struct LiveEndpoint {
    deployment: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LiveEndpoint {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

fn live_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set by the Restate suite runner"))
}

/// Bind `DeploymentProbe` on `{name}_BIND` and register it with the live
/// Restate admin under `{name}_URL`; the returned value is the deployment.
async fn live_endpoint(
    client: &lash_http_transport::reqwest::Client,
    admin_url: &str,
    name: &str,
) -> LiveEndpoint {
    let bind: std::net::SocketAddr = live_env(&format!("{name}_BIND"))
        .parse()
        .expect("a valid endpoint bind address");
    let url = live_env(&format!("{name}_URL"));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .expect("bind the Restate endpoint");
    let (shutdown, released) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(
            Endpoint::builder().bind(DeploymentProbe).build(),
        )
        .serve_with_cancel(listener, async move {
            let _ = released.await;
        })
        .await;
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(bind).await.is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the {name} endpoint did not open at {bind}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let response = client
        .post(format!("{admin_url}/deployments"))
        .json(&serde_json::json!({ "uri": url }))
        .send()
        .await
        .expect("register the deployment");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(status.is_success(), "registration failed: {status} {body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    LiveEndpoint {
        deployment: body["id"]
            .as_str()
            .expect("registration returns the deployment id")
            .to_owned(),
        shutdown: Some(shutdown),
        task,
    }
}

/// The invocation id a `…/send` to the live ingress returned.
async fn live_send(
    client: &lash_http_transport::reqwest::Client,
    ingress_url: &str,
    path: &str,
) -> String {
    let response = client
        .post(format!("{ingress_url}/{path}/send"))
        .body("\"tag\"")
        .header("content-type", "application/json")
        .send()
        .await
        .expect("submit the invocation");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status.as_u16(), 202, "{path}: {body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    body["invocationId"].as_str().unwrap().to_owned()
}

/// `sys_invocation`'s `pinned_deployment_id` for `id`, `None` while the row
/// or the column is unset.
async fn live_pinned(
    client: &lash_http_transport::reqwest::Client,
    admin_url: &str,
    id: &str,
) -> Option<String> {
    let response = client
        .post(format!("{admin_url}/query"))
        // Without the accept header the admin API answers Arrow IPC.
        .header("accept", "application/json")
        .json(&serde_json::json!({
            "query": format!("SELECT pinned_deployment_id FROM sys_invocation WHERE id = '{id}'"),
        }))
        .send()
        .await
        .expect("query sys_invocation");
    let body: serde_json::Value = response.json().await.ok()?;
    body["rows"].as_array()?.first()?["pinned_deployment_id"]
        .as_str()
        .map(str::to_owned)
}

/// Restate routes a new invocation to the newest deployment that registered
/// its service and pins an in-flight one to the deployment that started it:
/// the laws the double models, checked against the real server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live restate-server: the `server-double` Restate suite runs it"]
async fn live_restate_routes_new_invocations_to_the_newest_deployment_and_keeps_pins() {
    let ingress_url = live_env("RESTATE_INGRESS_URL");
    let admin_url = live_env("RESTATE_ADMIN_URL");
    let client = lash_http_transport::reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build the Restate client");

    let build_n = live_endpoint(&client, &admin_url, "DEP_A").await;
    let pinned = live_send(&client, &ingress_url, "DeploymentProbe/pinned/run").await;
    // Wait for the running invocation's pin to be visible.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while live_pinned(&client, &admin_url, &pinned).await.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "{pinned} never pinned"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        live_pinned(&client, &admin_url, &pinned).await.as_deref(),
        Some(build_n.deployment.as_str()),
        "the first invocation pinned to the only deployment"
    );

    let build_n1 = live_endpoint(&client, &admin_url, "DEP_B").await;
    assert_ne!(build_n.deployment, build_n1.deployment);
    let fresh = live_send(&client, &ingress_url, "DeploymentProbe/fresh/run").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(deployment) = live_pinned(&client, &admin_url, &fresh).await {
            assert_eq!(
                deployment, build_n1.deployment,
                "the new invocation routed to the newest deployment"
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{fresh} never pinned");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        live_pinned(&client, &admin_url, &pinned).await.as_deref(),
        Some(build_n.deployment.as_str()),
        "the in-flight invocation stayed pinned to the deployment that started it"
    );

    // Let both runs finish, then drop both deployments from the shared
    // server.
    for key in ["pinned", "fresh"] {
        let response = client
            .post(format!("{ingress_url}/DeploymentProbe/{key}/go"))
            .body("")
            .send()
            .await
            .expect("resolve the run's promise");
        assert!(
            response.status().is_success(),
            "DeploymentProbe/{key}/go: {}",
            response.status()
        );
    }
    for deployment in [&build_n.deployment, &build_n1.deployment] {
        let response = client
            .delete(format!("{admin_url}/deployments/{deployment}?force=true"))
            .send()
            .await
            .expect("remove the deployment");
        assert!(
            response.status().is_success(),
            "DELETE /deployments/{deployment}: {}",
            response.status()
        );
    }
}
