//! Live Restate SDK witnesses required before effect-group implementation.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

const WITNESS_SERVICE: &str = "EffectGroupSdkWitness";
const WITNESS_WORKFLOW: &str = "EffectGroupSdkWorkflow";

#[derive(Debug, Deserialize, Serialize)]
struct SameKeyRequest {
    same_key: String,
    different_key: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct SameKeyReport {
    first_id: String,
    second_id: String,
    different_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct AttachReport {
    completed_id: String,
    completed_output: String,
    cancelled_id: String,
    cancelled_error_code: u16,
    cancelled_error_message: String,
}

struct EffectGroupSdkTarget;

#[restate_sdk::service(name = "EffectGroupSdkTarget")]
impl EffectGroupSdkTarget {
    #[handler]
    async fn complete(&self, _ctx: Context<'_>, value: String) -> HandlerResult<String> {
        Ok(value)
    }

    #[handler]
    async fn block(&self, ctx: Context<'_>) -> HandlerResult<()> {
        ctx.sleep(Duration::from_secs(60)).await?;
        Ok(())
    }
}

/// The two child invocation ids the coverage witness publishes before it
/// blocks, so the test can cancel the parent and then ask Restate what
/// happened to each child.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CoverageChildIds {
    sent_id: String,
    called_id: String,
}

#[derive(Default)]
struct EffectGroupSdkWitness {
    coverage_children: Arc<std::sync::Mutex<Option<CoverageChildIds>>>,
}

#[restate_sdk::service(name = "EffectGroupSdkWitness")]
impl EffectGroupSdkWitness {
    /// The precondition behind ADR 0099 §2's `.send()` → `.call()` cutover:
    /// **implicit cancellation covers tracked `call` children and deliberately
    /// exempts one-way sends.**
    ///
    /// Nothing in the repository proved this, and the whole reason group
    /// children move to `call` is that under `.send()` implicit cancellation
    /// covers *zero* of them. It is also the reason §4 insists engine
    /// cancellation must never become the sole close protocol: this handler
    /// shows the engine reaching a child without any Lash fence being
    /// consulted, which is a capability to bound rather than to rely on.
    ///
    /// The handler issues one child each way against the same blocking target,
    /// publishes both invocation ids in process, and then parks on the tracked
    /// call. The test cancels *this* invocation and reads both children's
    /// `sys_invocation` status.
    #[handler(name = "cancellation_coverage")]
    async fn cancellation_coverage(&self, ctx: Context<'_>) -> HandlerResult<()> {
        let sent = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .block()
            .send()
            .await?;
        let called = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .block()
            .call();
        let called_id = called.invocation_handle().await?.invocation_id().to_owned();
        *self
            .coverage_children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(CoverageChildIds {
            sent_id: sent.invocation_id().to_owned(),
            called_id,
        });
        // Parking on the call is what makes it tracked. Absorb its outcome for
        // the same reason the dispatcher does: the witness is the parent's
        // cancellation, not the child's value.
        let _ = called.await;
        Ok(())
    }

    #[handler(name = "same_key")]
    async fn same_key(
        &self,
        ctx: Context<'_>,
        Json(request): Json<SameKeyRequest>,
    ) -> HandlerResult<Json<SameKeyReport>> {
        let first = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .complete("same-key-first".to_string())
            .idempotency_key(request.same_key.clone())
            .send()
            .await?;
        let second = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .complete("same-key-second".to_string())
            .idempotency_key(request.same_key)
            .send()
            .await?;
        let different = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .complete("different-key".to_string())
            .idempotency_key(request.different_key)
            .send()
            .await?;
        let report = SameKeyReport {
            first_id: first.invocation_id().to_owned(),
            second_id: second.invocation_id().to_owned(),
            different_id: different.invocation_id().to_owned(),
        };
        if report.first_id != report.second_id {
            return Err(TerminalError::new(format!(
                "same idempotency key produced different invocation ids: {} != {}",
                report.first_id, report.second_id
            ))
            .into());
        }
        if report.first_id == report.different_id {
            return Err(TerminalError::new(format!(
                "different idempotency keys produced the same invocation id: {}",
                report.first_id
            ))
            .into());
        }
        Ok(Json(report))
    }

    #[handler(name = "attach_smoke")]
    async fn attach_smoke(&self, ctx: Context<'_>) -> HandlerResult<Json<AttachReport>> {
        let completed = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .complete("completed-result".to_string())
            .send()
            .await?;
        let completed_id = completed.invocation_id().to_owned();
        let completed_output = ctx
            .invocation_handle(completed_id.clone())
            .attach::<String>()
            .await?;

        let cancelled = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .block()
            .send()
            .await?;
        let cancelled_id = cancelled.invocation_id().to_owned();
        ctx.invocation_handle(cancelled_id.clone()).cancel();
        let cancelled_error = match ctx
            .invocation_handle(cancelled_id.clone())
            .attach::<()>()
            .await
        {
            Ok(()) => {
                return Err(TerminalError::new(
                    "attach to a cancelled invocation unexpectedly succeeded",
                )
                .into());
            }
            Err(error) => error,
        };

        Ok(Json(AttachReport {
            completed_id,
            completed_output,
            cancelled_id,
            cancelled_error_code: cancelled_error.code(),
            cancelled_error_message: cancelled_error.message().to_string(),
        }))
    }

    #[handler(name = "attach_workflow")]
    async fn attach_workflow(
        &self,
        ctx: Context<'_>,
        invocation_id: String,
    ) -> HandlerResult<String> {
        Ok(ctx
            .invocation_handle(invocation_id)
            .attach::<String>()
            .await?)
    }
}

struct EffectGroupSdkWorkflow {
    executions: Arc<AtomicUsize>,
}

#[restate_sdk::workflow(name = "EffectGroupSdkWorkflow")]
impl EffectGroupSdkWorkflow {
    #[handler]
    async fn run(&self, _ctx: WorkflowContext<'_>, input: String) -> HandlerResult<String> {
        let execution = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::time::sleep(Duration::from_millis(750)).await;
        Ok(format!("{input}:execution-{execution}"))
    }
}

/// A handler that always fails retryably, under a small invoker retry policy
/// that pauses the invocation once it is exhausted: the witness for engine
/// retry exhaustion parking, on the real server and on the double alike.
struct EffectGroupSdkFlaky {
    attempts: Arc<AtomicUsize>,
}

#[restate_sdk::service(name = "EffectGroupSdkFlaky")]
impl EffectGroupSdkFlaky {
    #[handler(
        name = "fail",
        invocation_retry_policy(
            initial_interval = "10ms",
            factor = 1.0,
            max_interval = "10ms",
            max_attempts = 3,
            on_max_attempts = "pause",
        )
    )]
    async fn fail(&self, _ctx: Context<'_>) -> HandlerResult<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(HandlerError::from(std::io::Error::other(
            "the flaky witness always fails retryably",
        )))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendResponse {
    invocation_id: String,
    status: String,
}

#[test]
#[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
fn live_effect_group_sdk_preconditions() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build EG0 witness runtime")
        .block_on(async {
            // Raised from 30s with the cancellation-coverage witness, which
            // waits on cancellation propagation and `sys_invocation`
            // visibility rather than on a local handler returning.
            tokio::time::timeout(Duration::from_secs(120), run_witnesses(WitnessServer::Live))
                .await
                .expect("EG0 witnesses exceeded their 120 second ceiling");
        });
}

/// The same witnesses on the in-process `lash-restate-test` server double: the
/// parity check that keeps the double's semantics the real server's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effect_group_sdk_preconditions_on_the_server_double() {
    tokio::time::timeout(
        Duration::from_secs(120),
        run_witnesses(WitnessServer::InProcess),
    )
    .await
    .expect("EG0 witnesses on the server double exceeded their 120 second ceiling");
}

enum WitnessServer {
    Live,
    InProcess,
}

/// Where the witnesses reach Restate: its ingress and admin APIs over one
/// transport.
struct Witness {
    transport: Arc<dyn lash_http_transport::HttpTransport>,
    ingress_url: String,
    admin_url: String,
    /// The in-process server, kept alive for the transport's sake.
    _server: Option<lash_restate_test::RestateTestServer>,
}

impl Witness {
    fn admin(&self) -> crate::RestateAdminClient {
        crate::RestateAdminClient::new(crate::RestateConnection::with_transport(
            self.admin_url.clone(),
            Arc::clone(&self.transport),
        ))
    }
}

async fn run_witnesses(target: WitnessServer) {
    let workflow_executions = Arc::new(AtomicUsize::new(0));
    let coverage_children = Arc::new(std::sync::Mutex::new(None));
    let flaky_attempts = Arc::new(AtomicUsize::new(0));
    let endpoint = Endpoint::builder()
        .bind(EffectGroupSdkTarget)
        .bind(EffectGroupSdkWitness {
            coverage_children: Arc::clone(&coverage_children),
        })
        .bind(EffectGroupSdkWorkflow {
            executions: Arc::clone(&workflow_executions),
        })
        .bind(EffectGroupSdkFlaky {
            attempts: Arc::clone(&flaky_attempts),
        })
        .build();

    let (witness, live_server) = match target {
        WitnessServer::Live => {
            let ingress_url = required_url("RESTATE_INGRESS_URL");
            let admin_url = required_url("RESTATE_ADMIN_URL");
            let bind_addr = std::env::var("EG0_RESTATE_ENDPOINT_BIND")
                .expect(
                    "EG0_RESTATE_ENDPOINT_BIND must be set by `just effect-group-conformance-e2e`",
                )
                .parse::<SocketAddr>()
                .expect("valid EG0_RESTATE_ENDPOINT_BIND");
            let endpoint_url = required_url("EG0_RESTATE_ENDPOINT_URL");
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .expect("bind EG0 Restate endpoint");
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(async move {
                HttpServer::new(endpoint)
                    .serve_with_cancel(listener, async {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });
            wait_for_endpoint(bind_addr).await;
            register_deployment(&admin_url, &endpoint_url).await;
            (
                Witness {
                    transport: Arc::new(lash_http_transport::ReqwestHttpTransport::new()),
                    ingress_url,
                    admin_url,
                    _server: None,
                },
                Some((shutdown_tx, server)),
            )
        }
        WitnessServer::InProcess => {
            let server = lash_restate_test::RestateTestServer::start(
                endpoint,
                lash_restate_test::ServerConfig::default(),
            )
            .await
            .expect("start the Restate server double");
            let url = server.ingress_url().trim_end_matches('/').to_owned();
            (
                Witness {
                    transport: server.transport(),
                    ingress_url: url.clone(),
                    admin_url: url,
                    _server: Some(server),
                },
                None,
            )
        }
    };
    let ingress_url = witness.ingress_url.clone();
    let client = &witness;
    let same_key: SameKeyReport = post_json(
        client,
        format!("{ingress_url}/{WITNESS_SERVICE}/same_key"),
        &SameKeyRequest {
            same_key: "eg0-same-key".to_string(),
            different_key: "eg0-different-key".to_string(),
        },
    )
    .await;
    assert_eq!(same_key.first_id, same_key.second_id);
    assert_ne!(same_key.first_id, same_key.different_id);
    println!(
        "EG0_WITNESS same-key=>same-id PASS same={} different={}",
        same_key.first_id, same_key.different_id
    );

    let workflow_url = format!("{ingress_url}/{WITNESS_WORKFLOW}/eg0-workflow/run");
    let first: SendResponse = post_json(client, format!("{workflow_url}/send"), &"payload").await;
    let second: SendResponse = post_json(client, format!("{workflow_url}/send"), &"payload").await;
    assert_eq!(first.status, "Accepted");
    assert_eq!(second.status, "PreviouslyAccepted");
    assert_eq!(first.invocation_id, second.invocation_id);
    let attached: String = post_json(
        client,
        format!("{ingress_url}/{WITNESS_SERVICE}/attach_workflow"),
        &first.invocation_id,
    )
    .await;
    let executions = workflow_executions.load(Ordering::SeqCst);
    assert_eq!(attached, "payload:execution-1");
    assert_eq!(executions, 1);
    println!(
        "EG0_WITNESS workflow-exactly-once-per-key PASS first={} second={} id={} attached={} executions={executions}",
        first.status, second.status, first.invocation_id, attached
    );

    let attach: AttachReport = post_empty(
        client,
        format!("{ingress_url}/{WITNESS_SERVICE}/attach_smoke"),
    )
    .await;
    assert_eq!(attach.completed_output, "completed-result");
    assert!(!attach.cancelled_error_message.is_empty());
    println!(
        "EG0_WITNESS attach-smoke PASS completed_id={} output={} cancelled_id={} terminal_code={} terminal_message={:?}",
        attach.completed_id,
        attach.completed_output,
        attach.cancelled_id,
        attach.cancelled_error_code,
        attach.cancelled_error_message
    );

    witness_implicit_cancellation_covers_calls_not_sends(client, &coverage_children).await;

    witness_retry_exhaustion_pauses_until_resumed(client, &flaky_attempts).await;

    if let Some((shutdown_tx, server)) = live_server {
        let _ = shutdown_tx.send(());
        server.await.expect("EG0 endpoint server task");
    }
}

/// Engine retry exhaustion parks: a handler that fails retryably under
/// `max_attempts = 3, on_max_attempts = "pause"` is paused after exactly its
/// third attempt and runs no fourth until an operator resumes it; the resume
/// starts a fresh retry loop.
async fn witness_retry_exhaustion_pauses_until_resumed(
    client: &Witness,
    attempts: &Arc<AtomicUsize>,
) {
    use crate::{RestateInvocationId, RestateInvocationLifecycle};

    let ingress_url = &client.ingress_url;
    let flaky: SendResponse = post_empty(
        client,
        format!("{ingress_url}/EffectGroupSdkFlaky/fail/send"),
    )
    .await;
    let id = RestateInvocationId::new(flaky.invocation_id.clone());
    let admin = client.admin();
    let paused = RestateInvocationLifecycle::Unknown("paused".to_owned());
    let is_paused = || async {
        let status = admin.invocation_status(&id).await.ok()??;
        (status.status == paused).then_some(())
    };
    poll_until(
        Duration::from_secs(30),
        "the flaky invocation to pause",
        is_paused,
    )
    .await;
    let first_loop = attempts.load(Ordering::SeqCst);
    assert_eq!(
        first_loop, 3,
        "the pause lands after exactly max_attempts attempts"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "a paused invocation runs no further attempt"
    );

    let resume = lash_http_transport::HttpRequest::new(
        lash_http_transport::HttpMethod::Patch,
        format!(
            "{}/invocations/{}/resume",
            client.admin_url, flaky.invocation_id
        ),
        "",
    );
    let response = client
        .transport
        .send(resume, Some(Duration::from_secs(30)))
        .await
        .expect("resume the paused invocation");
    assert!(
        response.is_success(),
        "resume of a paused invocation answered {}",
        response.status
    );
    poll_until(
        Duration::from_secs(30),
        "the resumed invocation to run and pause again",
        || async {
            (attempts.load(Ordering::SeqCst) > first_loop)
                .then_some(())
                .and(is_paused().await)
        },
    )
    .await;
    let after_resume = attempts.load(Ordering::SeqCst);
    println!(
        "EG0_WITNESS retry-exhaustion-pauses-until-resumed PASS first_loop={first_loop} after_resume={after_resume}"
    );
    let _ = admin.kill_invocation_for_test_cleanup(&id).await;
}

/// Drives `cancellation_coverage` and reads the two children back out of
/// `sys_invocation`.
///
/// Deadlines here are generous on purpose. The prelude banks that this box's
/// Restate suites fail at whatever fixed wall-clock deadline they reach first
/// under load, and cancellation propagation plus `sys_invocation` visibility
/// are both asynchronous, so a tight bound would turn a scheduling delay into
/// a false claim about SDK semantics.
async fn witness_implicit_cancellation_covers_calls_not_sends(
    client: &Witness,
    coverage_children: &Arc<std::sync::Mutex<Option<CoverageChildIds>>>,
) {
    use crate::RestateInvocationId;

    let ingress_url = &client.ingress_url;
    let parent: SendResponse = post_empty(
        client,
        format!("{ingress_url}/{WITNESS_SERVICE}/cancellation_coverage/send"),
    )
    .await;

    let children = poll_until(
        Duration::from_secs(30),
        "coverage children published",
        || async {
            coverage_children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        },
    )
    .await;

    let admin = client.admin();
    let sent_id = RestateInvocationId::new(children.sent_id.clone());
    let called_id = RestateInvocationId::new(children.called_id.clone());
    assert_ne!(
        children.sent_id, children.called_id,
        "the two children must be distinct invocations"
    );

    admin
        .cancel_invocation(&RestateInvocationId::new(parent.invocation_id.clone()))
        .await
        .expect("cancel the coverage witness invocation");

    // The tracked call must stop without anyone cancelling it directly.
    let called_status = poll_until(
        Duration::from_secs(30),
        "the `call` child to stop being open after its caller was cancelled",
        || closed_status(&admin, &called_id),
    )
    .await;

    // The one-way send must be untouched by the same cancellation. Read it
    // after the call has already closed, so this is not merely a race the
    // poll above won by arriving first.
    let sent_status = admin
        .invocation_status(&sent_id)
        .await
        .expect("read the `send` child status")
        .expect("the `send` child exists");
    assert!(
        sent_status.is_still_active(),
        "implicit cancellation must exempt one-way sends, but the `send` child is {:?}; \
         if this ever fails, ADR 0099 §2's reason for moving group children to `call` is wrong",
        sent_status.status
    );

    println!(
        "EG0_WITNESS implicit-cancellation-covers-calls-not-sends PASS parent={} called={} called_status={:?} sent={} sent_status={:?}",
        parent.invocation_id,
        children.called_id,
        called_status,
        children.sent_id,
        sent_status.status
    );

    // The send child sleeps well past this suite; leave nothing running.
    let _ = admin.kill_invocation_for_test_cleanup(&sent_id).await;
    let _ = admin.kill_invocation_for_test_cleanup(&called_id).await;
}

/// The invocation's lifecycle once it is no longer open, or `None` while it is.
async fn closed_status(
    admin: &crate::RestateAdminClient,
    id: &crate::RestateInvocationId,
) -> Option<crate::RestateInvocationLifecycle> {
    let status = admin.invocation_status(id).await.ok()??;
    (!status.is_still_active()).then_some(status.status)
}

async fn poll_until<T, P, F>(budget: Duration, what: &str, mut probe: P) -> T
where
    P: FnMut() -> F,
    F: std::future::Future<Output = Option<T>>,
{
    let deadline = std::time::Instant::now() + budget;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {budget:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn required_url(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set by `just effect-group-conformance-e2e`"))
        .trim_end_matches('/')
        .to_string()
}

async fn wait_for_endpoint(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "EG0 Restate endpoint did not open at {addr}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn register_deployment(admin_url: &str, endpoint_url: &str) {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build Restate admin client");
    let response = client
        .post(format!("{admin_url}/deployments"))
        .json(&serde_json::json!({
            "uri": endpoint_url,
            "force": true,
            "breaking": true,
        }))
        .send()
        .await
        .expect("register EG0 Restate deployment");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "Restate deployment registration failed: {status} {body}"
    );
}

async fn post_json<T, R>(client: &Witness, url: String, body: &T) -> R
where
    T: Serialize + ?Sized,
    R: for<'de> Deserialize<'de>,
{
    let body = serde_json::to_vec(body).expect("encode the witness request");
    post(
        client,
        lash_http_transport::HttpRequest::post(&url, body)
            .with_header("content-type", "application/json"),
    )
    .await
}

async fn post_empty<R>(client: &Witness, url: String) -> R
where
    R: for<'de> Deserialize<'de>,
{
    post(client, lash_http_transport::HttpRequest::post(&url, "")).await
}

async fn post<R>(client: &Witness, request: lash_http_transport::HttpRequest) -> R
where
    R: for<'de> Deserialize<'de>,
{
    let url = request.url.clone();
    let response = client
        .transport
        .send(request, Some(Duration::from_secs(60)))
        .await
        .unwrap_or_else(|error| panic!("POST {url} failed: {error}"));
    let status = response.status;
    let bytes = lash_http_transport::read_http_body_bytes(response.body, None, "witness body")
        .await
        .unwrap_or_else(|error| panic!("read POST {url} response: {error}"));
    let status = http::StatusCode::from_u16(status).expect("a valid HTTP status");
    assert!(
        status.is_success(),
        "POST {url} failed: {status} {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        panic!(
            "decode POST {url} response as JSON: {error}; body={}",
            String::from_utf8_lossy(&bytes)
        )
    })
}
