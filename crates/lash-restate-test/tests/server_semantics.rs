//! The server double's own semantics, on small handlers: the Restate
//! behaviours lash builds on, each checked in streaming and in always-replay
//! mode, under concurrent and serial scheduling.

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
    AttemptDispatch, CrashPoint, CrashRule, DeploymentHooks, DeploymentId, OutsideGates, Refusal,
    RemoveDeploymentError, RestateTestServer, ResumeDeployment, ResumeRefusal, Scheduling,
    ServerConfig, TimeMode,
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

/// [`INGRESS`] for [`Beside`], so its law runs beside the relay's.
static BESIDE_INGRESS: Mutex<Option<RestateTestServer>> = Mutex::new(None);

/// The gates [`Beside`] declares its wait to, when its law declares it.
static BESIDE_GATES: Mutex<Option<OutsideGates>> = Mutex::new(None);

struct Beside;

#[restate_sdk::service]
impl Beside {
    /// From inside a `ctx.run`, waits on a task it spawned beside itself —
    /// as lash's gate watches do — that calls `Counter/{key}/add` through
    /// the server's ingress. The spawned task does not carry the attempt, so
    /// its request lands as one from outside every attempt, while the
    /// handler waits on it without being blocked on the server. When its
    /// law declares [`BESIDE_GATES`], the wait is declared an outside gate.
    #[handler]
    async fn ask(&self, ctx: Context<'_>, Json(key): Json<String>) -> HandlerResult<Json<String>> {
        let server = BESIDE_INGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        let gates = BESIDE_GATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let answer = ctx
            .run(|| async move {
                let _gate = gates.as_ref().map(OutsideGates::enter);
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
        .bind(Gauge)
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
        // Outside any `ctx.run` too: parked on its own request, whose
        // target is ready, the handler gives the turn to that target.
        assert_eq!(
            post(&server, "Relay/ask_direct", "\"direct\"").await,
            (200, "\"1\"".into())
        );
        // The relay's requests are attributed to its attempt, so the turn
        // moves to the counter at once instead of after a stall.
        assert_eq!(server.stats().stall_preemptions, 0);
        *INGRESS.lock().unwrap() = None;
    }
}

/// A handler inside a `ctx.run` waits on a task it spawned, whose request
/// lands as one from outside every attempt. Declared as an outside gate,
/// the wait lets that request land between turns with no stall, in one
/// order per seed on a current-thread runtime; undeclared, the request
/// still lands, once the holder has stalled.
#[tokio::test]
async fn an_outside_request_a_holder_waits_on_lands_at_a_declared_gate_or_after_a_stall() {
    let mut traces = Vec::new();
    for declared in [true, true, true, false] {
        for config in [
            ServerConfig::default()
                .with_seed(7)
                .scheduling(Scheduling::Serial),
            ServerConfig::default()
                .with_seed(7)
                .always_replay(true)
                .scheduling(Scheduling::Serial),
        ] {
            let server = server(config).await;
            *BESIDE_INGRESS.lock().unwrap() = Some(server.clone());
            *BESIDE_GATES.lock().unwrap() = declared.then(|| server.outside_gates());
            let answer = tokio::time::timeout(
                Duration::from_secs(20),
                post(&server, "Beside/ask", "\"beside\""),
            )
            .await
            .expect("the outside request lands");
            assert_eq!(answer, (200, "\"1\"".into()));
            let stalls = server.stats().stall_preemptions;
            if declared {
                assert_eq!(stalls, 0, "a declared gate lands the request between turns");
                traces.push(server.schedule_trace());
            } else {
                assert!(stalls >= 1, "an undeclared wait lands only after a stall");
            }
            *BESIDE_GATES.lock().unwrap() = None;
            *BESIDE_INGRESS.lock().unwrap() = None;
        }
    }
    assert!(
        traces.chunks(2).all(|runs| runs == &traces[..2]),
        "one seed grants the turn in one order: {traces:?}"
    )
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
    let body = read_http_body_bytes(response.body, None, "body")
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Drive `id` to paused under manual time and return its view: the handler
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
    // resume its approval drives.
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
    // Wait for its first attempt to be served by build N.
    let pinned_id = loop {
        let found = served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(deployment, _, key, _)| {
                deployment == build_n.as_str() && key.as_deref() == Some("job-0")
            })
            .map(|(_, id, _, _)| id.clone());
        if let Some(id) = found {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(server.pinned_deployment(&pinned_id), Some(build_n.clone()));

    let build_n1 = backend
        .add_build("n+1", served_hook(&served))
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
                deployment == build_n1.as_str() && key.as_deref() == Some("job-1")
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
            key.as_deref() == Some("job-0") && deployment == build_n.as_str()
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
