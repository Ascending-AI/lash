//! Live Restate SDK witnesses required before effect-group implementation.

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

struct EffectGroupSdkWitness;

#[restate_sdk::service(name = "EffectGroupSdkWitness")]
impl EffectGroupSdkWitness {
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendResponse {
    invocation_id: String,
    status: String,
}

#[test]
#[ignore = "requires an isolated Restate server; run through the EG0 orb gate"]
fn live_effect_group_sdk_preconditions() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build EG0 witness runtime")
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(30), run_live_witnesses())
                .await
                .expect("EG0 witnesses exceeded their 30 second ceiling");
        });
}

async fn run_live_witnesses() {
    let ingress_url = required_url("RESTATE_INGRESS_URL");
    let admin_url = required_url("RESTATE_ADMIN_URL");
    let bind_addr = std::env::var("EG0_RESTATE_ENDPOINT_BIND")
        .expect("EG0_RESTATE_ENDPOINT_BIND must be set by the orb gate")
        .parse::<SocketAddr>()
        .expect("valid EG0_RESTATE_ENDPOINT_BIND");
    let endpoint_url = required_url("EG0_RESTATE_ENDPOINT_URL");
    let workflow_executions = Arc::new(AtomicUsize::new(0));

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("bind EG0 Restate endpoint");
    let endpoint = Endpoint::builder()
        .bind(EffectGroupSdkTarget)
        .bind(EffectGroupSdkWitness)
        .bind(EffectGroupSdkWorkflow {
            executions: Arc::clone(&workflow_executions),
        })
        .build();
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

    let client = reqwest::Client::new();
    let same_key: SameKeyReport = post_json(
        &client,
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
    let first: SendResponse = post_json(&client, format!("{workflow_url}/send"), &"payload").await;
    let second: SendResponse = post_json(&client, format!("{workflow_url}/send"), &"payload").await;
    assert_eq!(first.status, "Accepted");
    assert_eq!(second.status, "PreviouslyAccepted");
    assert_eq!(first.invocation_id, second.invocation_id);
    let attached: String = post_json(
        &client,
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
        &client,
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

    let _ = shutdown_tx.send(());
    server.await.expect("EG0 endpoint server task");
}

fn required_url(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set by the orb gate"))
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

async fn post_json<T, R>(client: &reqwest::Client, url: String, body: &T) -> R
where
    T: Serialize + ?Sized,
    R: for<'de> Deserialize<'de>,
{
    let response = client
        .post(&url)
        .json(body)
        .send()
        .await
        .unwrap_or_else(|error| panic!("POST {url} failed: {error}"));
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .unwrap_or_else(|error| panic!("read POST {url} response: {error}"));
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

async fn post_empty<R>(client: &reqwest::Client, url: String) -> R
where
    R: for<'de> Deserialize<'de>,
{
    let response = client
        .post(&url)
        .send()
        .await
        .unwrap_or_else(|error| panic!("POST {url} failed: {error}"));
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .unwrap_or_else(|error| panic!("read POST {url} response: {error}"));
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
