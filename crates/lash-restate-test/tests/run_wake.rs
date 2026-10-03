//! Concurrent `ctx.run` bodies wake the handler that awaits them from the
//! frames the attempt already received (FIG-4872). The request stream stays
//! open and the server sends nothing besides each run's own answer — a V6
//! completion notification, a V7 proposal ack — so a result one future read
//! on a sibling's behalf must still wake that sibling: no EOF, stream close or
//! replay may finish the invocation instead.
//!
//! Each law runs three runs whose bodies all enter before any returns, and
//! forces completion order 2/0/1 from inside the bodies: body 2 releases body
//! 0 and body 0 releases body 1 as they return. Run 0's proposal is then sent
//! while run 2's answer is still unread, the window in which SDK 0.11 let the
//! lower-index future drain run 2's answer and left run 2 parked on an empty
//! input channel. Two handler shapes: borrowed runs polled together in index
//! order (the shape that stalled), and runs registered with consuming
//! `start()` and awaited in order (the shape Lash uses).
//!
//! Legs, on the double's V6 and V7 and on a live V7 server:
//!
//! * wake: all three settle durably in completion order, each body runs once,
//!   in one attempt;
//! * transient: body 0 fails retryably on its first execution; the attempt
//!   ends, body 1 is dropped at its barrier, and the retry reuses run 2's
//!   durable receipt without executing it;
//! * cancellation: after run 2 is durable, cancelling the invocation fails
//!   the pending runs with 409, drops their bodies and keeps one attempt;
//! * termination: after run 2 is durable, killing the invocation drops the
//!   pending bodies and none runs again.
//!
//! The live suite's replay leg closes every stream after its replay; there
//! the started shape's wake, transient and termination legs check the same
//! results and body counts, without the one-attempt claims.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]
// Test code; the live leg reads the suite runner's env (RESTATE_INGRESS_URL,
// endpoint binds) — ambient env access is sanctioned in test targets.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::Poll;
use std::time::Duration;

use lash_http_transport::{HttpMethod, HttpRequest, read_http_body_bytes};
use lash_restate_test::{ProtocolVersion, RestateTestServer, ServerConfig};
use restate_sdk::prelude::*;
use tokio::sync::Notify;

// `#[restate_sdk::*]` expansions name `::restate_sdk` absolute paths; the SDK
// reaches this crate through lash's re-export, so the crate answers to that
// name and generated code resolves the modules below at the crate root.
extern crate self as restate_sdk;
#[allow(unused_imports)]
use lash_restate::restate_sdk::{
    context, discovery, endpoint, errors, handler, http_server, ingress, object, prelude, service,
    workflow,
};

/// How long a law waits for anything it is owed. A stalled invocation stays
/// open until the server's one-minute inactivity timeout closes its stream,
/// so a bound well below that tells a stall from a slow host.
const BOUND: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Handlers under test
// ---------------------------------------------------------------------------

/// What one law's bodies do and what they did. Bodies find it by the tag
/// the invocation carries, so a replayed attempt and a live deployment reach
/// the same script.
#[derive(Default)]
struct Script {
    /// Body 2 releases body 0 and body 0 releases body 1 as they return.
    chain: bool,
    /// The body whose first execution fails retryably.
    fail_first: Option<usize>,
    release: [Notify; 3],
    entered: [AtomicUsize; 3],
    returned: [AtomicUsize; 3],
    /// Bodies dropped before they returned.
    dropped: [AtomicUsize; 3],
    handler_attempts: AtomicUsize,
}

impl Script {
    fn counts(counters: &[AtomicUsize; 3]) -> [usize; 3] {
        std::array::from_fn(|index| counters[index].load(Ordering::SeqCst))
    }
}

fn scripts() -> &'static Mutex<HashMap<String, Arc<Script>>> {
    static SCRIPTS: OnceLock<Mutex<HashMap<String, Arc<Script>>>> = OnceLock::new();
    SCRIPTS.get_or_init(Default::default)
}

fn script(tag: &str) -> Arc<Script> {
    Arc::clone(
        scripts()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tag)
            .expect("the law registered its script"),
    )
}

/// Counts a body dropped before it returned.
struct Unreturned {
    script: Arc<Script>,
    index: usize,
    returned: bool,
}

impl Drop for Unreturned {
    fn drop(&mut self) {
        let counters = if self.returned {
            &self.script.returned
        } else {
            &self.script.dropped
        };
        counters[self.index].fetch_add(1, Ordering::SeqCst);
    }
}

/// Run `index`'s body: its first execution waits at its barrier; a later
/// execution (a retry's or a replay's redelivery) returns at once.
async fn body(script: Arc<Script>, index: usize) -> HandlerResult<Json<String>> {
    let execution = script.entered[index].fetch_add(1, Ordering::SeqCst);
    let mut guard = Unreturned {
        script: Arc::clone(&script),
        index,
        returned: false,
    };
    if execution == 0 {
        script.release[index].notified().await;
        if script.fail_first == Some(index) {
            guard.returned = true;
            return Err(HandlerError::from(std::io::Error::other("transient")));
        }
        let next = match index {
            2 => Some(0),
            0 => Some(1),
            _ => None,
        };
        if script.chain
            && let Some(next) = next
        {
            script.release[next].notify_one();
        }
    }
    guard.returned = true;
    Ok(Json(format!("receipt-{index}")))
}

fn retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(10))
        .max_attempts(3)
}

/// Poll every unfinished future in index order on each wake, as
/// `futures::future::join_all` does for a small set.
async fn join_in_order<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none()
                && let Poll::Ready(value) = future.as_mut().poll(cx)
            {
                *output = Some(value);
            }
        }
        if outputs.iter().all(Option::is_some) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    outputs.into_iter().map(Option::unwrap).collect()
}

struct RunWake;

#[restate_sdk::service]
impl RunWake {
    /// Three borrowed runs polled together in index order.
    #[handler]
    async fn joined(
        &self,
        ctx: Context<'_>,
        Json(tag): Json<String>,
    ) -> HandlerResult<Json<Vec<String>>> {
        let script = script(&tag);
        script.handler_attempts.fetch_add(1, Ordering::SeqCst);
        let runs: Vec<_> = (0..3)
            .map(|index| {
                let script = Arc::clone(&script);
                ctx.run(move || body(script, index))
                    .name(format!("attempt-{index}"))
                    .retry_policy(retry_policy())
            })
            .collect();
        let mut receipts = Vec::new();
        for result in join_in_order(runs).await {
            receipts.push(result?.0);
        }
        Ok(Json(receipts))
    }

    /// Three runs registered with consuming `start()`, awaited in order.
    #[handler]
    async fn started(
        &self,
        ctx: Context<'_>,
        Json(tag): Json<String>,
    ) -> HandlerResult<Json<Vec<String>>> {
        let script = script(&tag);
        script.handler_attempts.fetch_add(1, Ordering::SeqCst);
        let runs: Vec<_> = (0..3)
            .map(|index| {
                let script = Arc::clone(&script);
                ctx.run(move || body(script, index))
                    .name(format!("attempt-{index}"))
                    .retry_policy(retry_policy())
                    .start()
            })
            .collect();
        let mut receipts = Vec::new();
        for run in runs {
            receipts.push(run.await?.0);
        }
        Ok(Json(receipts))
    }
}

// ---------------------------------------------------------------------------
// Tiers
// ---------------------------------------------------------------------------

/// The server a law runs against: the in-process double or a live
/// `restate-server` with this process's endpoint registered.
enum Tier {
    Double(RestateTestServer),
    Live(Live),
}

struct Live {
    /// Whether attempts keep their request stream open: false on the
    /// suite's replay leg, whose zero inactivity timeout closes it after
    /// every replay.
    streaming: bool,
    client: lash_http_transport::reqwest::Client,
    ingress_url: String,
    admin_url: String,
    endpoint: LiveEndpoint,
}

/// A durable run result as the journal holds it.
#[derive(Debug, PartialEq, Eq)]
enum Settled {
    Value(String),
    Failure(u32),
}

impl Tier {
    async fn send(&self, handler: &str, tag: &str) -> String {
        let path = format!("RunWake/{handler}/send");
        let body = serde_json::to_string(tag).unwrap();
        let (status, response) = match self {
            Self::Double(server) => double_request(server, HttpMethod::Post, &path, body).await,
            Self::Live(live) => {
                let response = live
                    .client
                    .post(format!("{}/{path}", live.ingress_url))
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .expect("submit the invocation");
                (
                    response.status().as_u16(),
                    response.text().await.unwrap_or_default(),
                )
            }
        };
        assert_eq!(status, 202, "{path}: {response}");
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        response["invocationId"].as_str().unwrap().to_owned()
    }

    /// The invocation's response once it completes; `None` when it has not
    /// within `within`.
    async fn outcome(&self, id: &str, within: Duration) -> Option<(u16, String)> {
        let path = format!("restate/invocation/{id}/attach");
        let attach = async {
            match self {
                Self::Double(server) => {
                    double_request(server, HttpMethod::Get, &path, String::new()).await
                }
                Self::Live(live) => {
                    let response = live
                        .client
                        .get(format!("{}/{path}", live.ingress_url))
                        .send()
                        .await
                        .expect("attach to the invocation");
                    (
                        response.status().as_u16(),
                        response.text().await.unwrap_or_default(),
                    )
                }
            }
        };
        tokio::time::timeout(within, attach).await.ok()
    }

    /// The run results the invocation's journal holds, in stored order.
    async fn settled(&self, id: &str) -> Vec<Settled> {
        match self {
            Self::Double(server) => server
                .journal(id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| entry.run_completion())
                .map(|result| match result {
                    Ok(value) => Settled::Value(serde_json::from_slice(&value).unwrap()),
                    Err((code, _)) => Settled::Failure(code),
                })
                .collect(),
            Self::Live(live) => live
                .query(&format!(
                    "SELECT entry_json FROM sys_journal WHERE id = '{id}' \
                     AND entry_type = 'Notification: Run' ORDER BY index"
                ))
                .await
                .into_iter()
                .map(|row| {
                    let entry: serde_json::Value =
                        serde_json::from_str(row["entry_json"].as_str().unwrap()).unwrap();
                    live_settled(&entry)
                })
                .collect(),
        }
    }

    async fn cancel(&self, id: &str) {
        match self {
            Self::Double(server) => assert!(server.cancel(id).is_some(), "{id} is known"),
            Self::Live(live) => live.admin_patch(&format!("invocations/{id}/cancel")).await,
        }
    }

    async fn kill(&self, id: &str) {
        match self {
            Self::Double(server) => {
                assert!(server.kill_and_await(id).await.is_some(), "{id} is known");
            }
            Self::Live(live) => live.admin_patch(&format!("invocations/{id}/kill")).await,
        }
    }

    /// The service protocol version the invocation runs on.
    async fn protocol(&self, id: &str) -> u64 {
        match self {
            Self::Double(server) => server
                .invocations()
                .into_iter()
                .find(|invocation| invocation.id == id)
                .map(|_| self.expected_protocol())
                .expect("the double knows the invocation"),
            Self::Live(live) => {
                let query = format!(
                    "SELECT pinned_service_protocol_version FROM sys_invocation \
                     WHERE id = '{id}'"
                );
                let deadline = tokio::time::Instant::now() + BOUND;
                loop {
                    let rows = live.query(&query).await;
                    if let Some(version) = rows
                        .first()
                        .and_then(|row| row["pinned_service_protocol_version"].as_u64())
                    {
                        return version;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "{id} pins a protocol within {BOUND:?}: {rows:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    /// Whether each attempt's request stream stays open until it ends; the
    /// double runs every law streaming.
    fn streaming(&self) -> bool {
        match self {
            Self::Double(_) => true,
            Self::Live(live) => live.streaming,
        }
    }

    /// The protocol the law expects: the double's configured one, or V7,
    /// which the live suite enables.
    fn expected_protocol(&self) -> u64 {
        match self {
            Self::Double(server) => match server.config().protocol {
                ProtocolVersion::V6 => 6,
                ProtocolVersion::V7 => 7,
            },
            Self::Live(_) => 7,
        }
    }

    /// The double's view of the invocation, for a failure message.
    fn view(&self, id: &str) -> String {
        match self {
            Self::Double(server) => format!(
                "{:?}",
                server
                    .invocations()
                    .into_iter()
                    .find(|invocation| invocation.id == id)
            ),
            Self::Live(live) => format!("live deployment {}", live.endpoint.deployment),
        }
    }
}

async fn double_request(
    server: &RestateTestServer,
    method: HttpMethod,
    path: &str,
    body: String,
) -> (u16, String) {
    let request = HttpRequest::new(method, format!("{}/{path}", server.ingress_url()), body)
        .with_header("content-type", "application/json");
    let response = server.transport().send(request, None).await.unwrap();
    let status = response.status;
    let body = read_http_body_bytes(response.body, 16 * 1024 * 1024, None, "body")
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// A run completion notification's result, as `sys_journal` renders it.
fn live_settled(entry: &serde_json::Value) -> Settled {
    let result = &entry["Notification"]["Completion"]["Run"]["result"];
    if let Some(value) = result.get("Success") {
        let bytes: Vec<u8> = serde_json::from_value(value.clone()).unwrap();
        return Settled::Value(serde_json::from_slice(&bytes).unwrap());
    }
    let code = result["Failure"]["code"]
        .as_u64()
        .unwrap_or_else(|| panic!("an unrecognised run completion entry: {entry}"));
    Settled::Failure(u32::try_from(code).unwrap())
}

impl Live {
    async fn query(&self, query: &str) -> Vec<serde_json::Value> {
        let response = self
            .client
            .post(format!("{}/query", self.admin_url))
            // Without the accept header the admin API answers Arrow IPC.
            .header("accept", "application/json")
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .expect("query the admin API");
        let body: serde_json::Value = response.json().await.expect("a JSON query answer");
        body["rows"].as_array().cloned().unwrap_or_default()
    }

    async fn admin_patch(&self, path: &str) {
        let response = self
            .client
            .patch(format!("{}/{path}", self.admin_url))
            .send()
            .await
            .expect("reach the admin API");
        assert!(
            response.status().is_success(),
            "PATCH {path}: {}",
            response.status()
        );
    }
}

// ---------------------------------------------------------------------------
// Laws
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Leg {
    Wake,
    Transient,
    Cancellation,
    Termination,
}

/// Each handler shape with the legs it runs while the request stream stays
/// open, then the legs it runs when every stream closes right after its
/// replay (the live suite's replay leg).
///
/// Borrowed runs register when first polled, so a replay awaits run 0 before
/// runs 1 and 2 are registered again and cannot replay partial results: they
/// run only the open-stream wake leg, as the stall witness. Started runs,
/// registered in order before any await, run every leg. With the input
/// closed a cancellation reaches the SDK only once its executing bodies
/// finish, and the cancellation leg holds them at their barriers, so it
/// needs the open stream.
const SHAPES: [(&str, &[Leg], &[Leg]); 2] = [
    ("joined", &[Leg::Wake], &[]),
    (
        "started",
        &[
            Leg::Wake,
            Leg::Transient,
            Leg::Cancellation,
            Leg::Termination,
        ],
        &[Leg::Wake, Leg::Transient, Leg::Termination],
    ),
];

/// Wait until `ready` holds, failing with `what` after [`BOUND`].
async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(BOUND, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} within {BOUND:?}"));
}

async fn settled_eventually(tier: &Tier, id: &str, count: usize) -> Vec<Settled> {
    let deadline = tokio::time::Instant::now() + BOUND;
    loop {
        let settled = tier.settled(id).await;
        if settled.len() >= count {
            return settled;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{count} run results durable within {BOUND:?}; journal holds {settled:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn receipts(indexes: [usize; 3]) -> Vec<Settled> {
    indexes
        .iter()
        .map(|index| Settled::Value(format!("receipt-{index}")))
        .collect()
}

/// Run `leg` with `shape`'s handler on `tier`, under a tag unique to `label`.
async fn law(tier: &Tier, label: &str, shape: &str, leg: Leg) {
    let streaming = tier.streaming();
    let context = format!("{label} {shape} {leg:?}");
    let tag = format!("{label}-{shape}-{leg:?}-{}", std::process::id());
    let script = Arc::new(Script {
        chain: matches!(leg, Leg::Wake | Leg::Transient),
        fail_first: matches!(leg, Leg::Transient).then_some(0),
        ..Script::default()
    });
    scripts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(tag.clone(), Arc::clone(&script));

    let id = tier.send(shape, &tag).await;
    eventually(&format!("{context}: every body enters"), || {
        Script::counts(&script.entered) == [1, 1, 1]
    })
    .await;
    assert_eq!(
        Script::counts(&script.returned),
        [0, 0, 0],
        "{context}: no body returns before it is released"
    );
    let protocol = tier.protocol(&id).await;
    assert_eq!(
        protocol,
        tier.expected_protocol(),
        "{context}: negotiated protocol"
    );
    script.release[2].notify_one();

    match leg {
        Leg::Wake => {
            let outcome = tier.outcome(&id, BOUND).await;
            let settled = tier.settled(&id).await;
            assert_eq!(
                outcome,
                Some((200, r#"["receipt-0","receipt-1","receipt-2"]"#.to_owned())),
                "{context}: the invocation completes from the results it received, without \
                 another input frame; bodies entered {:?} and returned {:?}, journal holds \
                 {settled:?}, {}",
                Script::counts(&script.entered),
                Script::counts(&script.returned),
                tier.view(&id),
            );
            assert_eq!(settled, receipts([2, 0, 1]), "{context}: completion order");
            assert_eq!(Script::counts(&script.entered), [1, 1, 1], "{context}");
            assert_eq!(Script::counts(&script.returned), [1, 1, 1], "{context}");
            // With the stream closed after each replay, every resumption is
            // an attempt of its own.
            if streaming {
                assert_eq!(
                    script.handler_attempts.load(Ordering::SeqCst),
                    1,
                    "{context}: one attempt, no stream close or replay"
                );
            }
        }
        Leg::Transient => {
            let outcome = tier.outcome(&id, BOUND).await;
            assert_eq!(
                outcome,
                Some((200, r#"["receipt-0","receipt-1","receipt-2"]"#.to_owned())),
                "{context}: the retry completes the invocation; {}",
                tier.view(&id)
            );
            // The redelivered bodies return at once, in either order.
            let settled = tier.settled(&id).await;
            assert!(
                settled == receipts([2, 0, 1]) || settled == receipts([2, 1, 0]),
                "{context}: run 2's durable receipt is reused, the others settle once on the \
                 retry: {settled:?}"
            );
            assert_eq!(
                Script::counts(&script.entered),
                [2, 2, 1],
                "{context}: only unfinished bodies run again"
            );
            assert_eq!(
                Script::counts(&script.dropped),
                [0, 1, 0],
                "{context}: the failed attempt dropped body 1 at its barrier"
            );
            if streaming {
                assert_eq!(
                    script.handler_attempts.load(Ordering::SeqCst),
                    2,
                    "{context}"
                );
            }
        }
        Leg::Cancellation | Leg::Termination => {
            assert_eq!(
                settled_eventually(tier, &id, 1).await,
                receipts([2, 2, 2])[..1],
                "{context}: run 2 is durable"
            );
            if matches!(leg, Leg::Cancellation) {
                tier.cancel(&id).await;
            } else {
                tier.kill(&id).await;
            }
            let outcome = tier.outcome(&id, BOUND).await;
            let (status, _) = outcome.as_ref().unwrap_or_else(|| {
                panic!(
                    "{context}: the invocation ends within {BOUND:?}; {}",
                    tier.view(&id)
                )
            });
            assert_eq!(*status, 409, "{context}: {outcome:?}");
            eventually(
                &format!("{context}: the pending bodies are dropped"),
                || Script::counts(&script.dropped) == [1, 1, 0],
            )
            .await;
            assert_eq!(
                tier.settled(&id).await[..1],
                receipts([2, 2, 2])[..1],
                "{context}: run 2's receipt stays durable"
            );
            assert_eq!(
                Script::counts(&script.entered),
                [1, 1, 1],
                "{context}: no body runs again"
            );
            if streaming {
                assert_eq!(
                    script.handler_attempts.load(Ordering::SeqCst),
                    1,
                    "{context}"
                );
            }
        }
    }
    eprintln!("{context}: passed on negotiated protocol V{protocol}");
}

async fn double(protocol: ProtocolVersion) -> Tier {
    let endpoint = Endpoint::builder().bind(RunWake).build();
    Tier::Double(
        RestateTestServer::start(endpoint, ServerConfig::default().protocol(protocol))
            .await
            .unwrap(),
    )
}

async fn on_the_double(label: &str, protocol: ProtocolVersion) {
    for (shape, legs, _) in SHAPES {
        for &leg in legs {
            let tier = double(protocol).await;
            law(&tier, label, shape, leg).await;
        }
    }
}

/// The wake, transient, cancellation and termination legs on the double's
/// V6, request stream open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_runs_wake_their_handler_without_another_input_frame_on_v6() {
    on_the_double("v6", ProtocolVersion::V6).await;
}

/// The same legs on the double's V7, where each run's answer is a proposal
/// ack.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_runs_wake_their_handler_without_another_input_frame_on_v7() {
    on_the_double("v7", ProtocolVersion::V7).await;
}

// ---------------------------------------------------------------------------
// Live V7
// ---------------------------------------------------------------------------

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

async fn live() -> Tier {
    let ingress_url = live_env("RESTATE_INGRESS_URL");
    let admin_url = live_env("RESTATE_ADMIN_URL");
    let client = lash_http_transport::reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build the Restate client");
    let bind: std::net::SocketAddr = live_env("RW_ENDPOINT_BIND")
        .parse()
        .expect("a valid endpoint bind address");
    let url = live_env("RW_ENDPOINT_URL");
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .expect("bind the Restate endpoint");
    let (shutdown, released) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        restate_sdk::http_server::HttpServer::new(Endpoint::builder().bind(RunWake).build())
            .serve_with_cancel(listener, async move {
                let _ = released.await;
            })
            .await;
    });
    let response = client
        .post(format!("{admin_url}/deployments"))
        .json(&serde_json::json!({ "uri": url }))
        .send()
        .await
        .expect("register the deployment");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(status.is_success(), "registration failed: {status} {body}");
    let deployment: serde_json::Value = serde_json::from_str(&body).unwrap();
    let version: serde_json::Value = client
        .get(format!("{admin_url}/version"))
        .send()
        .await
        .expect("read the server version")
        .json()
        .await
        .expect("a JSON version answer");
    eprintln!(
        "live restate-server {}, protocol_v7 feature {}, deployment advertises protocol {}..={}",
        version["version"],
        version["features"]["protocol_v7"],
        deployment["min_protocol_version"],
        deployment["max_protocol_version"],
    );
    Tier::Live(Live {
        streaming: std::env::var("LASH_RESTATE_SUITE_LEG").as_deref() != Ok("replay"),
        client,
        ingress_url,
        admin_url,
        endpoint: LiveEndpoint {
            deployment: deployment["id"]
                .as_str()
                .expect("registration returns the deployment id")
                .to_owned(),
            shutdown: Some(shutdown),
            task,
        },
    })
}

/// The same legs against a live V7 server: the `run-wake` suite enables the
/// server's V7 protocol, and each law checks the invocation negotiated it.
/// On the suite's replay leg it runs the legs that hold with every stream
/// closed after its replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live restate-server: the `run-wake` Restate suite runs it"]
async fn live_restate_v7_completed_runs_wake_their_handler_without_another_input_frame() {
    let tier = live().await;
    let label = if tier.streaming() {
        "live"
    } else {
        "live-replay"
    };
    for (shape, streaming_legs, replay_legs) in SHAPES {
        let legs = if tier.streaming() {
            streaming_legs
        } else {
            replay_legs
        };
        for &leg in legs {
            law(&tier, label, shape, leg).await;
        }
    }
}
