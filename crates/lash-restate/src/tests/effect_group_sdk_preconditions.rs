//! Live Restate SDK witnesses required before effect-group implementation.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{RestateProcessWorkflowInput, RestateRuntimeEffectController};
use lash_core::{
    ProcessCommand, ProcessEffectOutcome, ProcessExecutionContext, ProcessInput,
    ProcessRegistration, ProcessRegistry, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimeScope,
};
use lash_sansio::ProcessId;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use super::endpoint_protocol::{
    invoke_endpoint_with_scripted_responses, restate_one_way_call_idempotency_key,
};
use super::registry_local_executor;

const WITNESS_SERVICE: &str = "EffectGroupSdkWitness";
const WITNESS_WORKFLOW: &str = "EffectGroupSdkWorkflow";
const FIG1489_WITNESS_SERVICE: &str = "Fig1489IngressWitness";

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
    first_output: String,
    different_output: String,
    executions: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct AttachReport {
    completed_id: String,
    completed_output: String,
    cancelled_id: String,
    cancelled_error_code: u16,
    cancelled_error_message: String,
}

struct EffectGroupSdkTarget {
    executions: Arc<AtomicUsize>,
}

#[restate_sdk::service(name = "EffectGroupSdkTarget")]
impl EffectGroupSdkTarget {
    #[handler]
    async fn complete(&self, _ctx: Context<'_>, value: String) -> HandlerResult<String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(value)
    }

    #[handler]
    async fn block(&self, ctx: Context<'_>) -> HandlerResult<()> {
        ctx.sleep(Duration::from_secs(60)).await?;
        Ok(())
    }
}

struct EffectGroupSdkWitness {
    target_executions: Arc<AtomicUsize>,
}

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
        let first_output = ctx
            .invocation_handle(first.invocation_id().to_owned())
            .attach::<String>()
            .await?;
        let executions_after_duplicate = self.target_executions.load(Ordering::SeqCst);
        if first_output != "same-key-first" || executions_after_duplicate != 1 {
            return Err(TerminalError::new(format!(
                "changed-payload duplicate did not attach to the first realization: output={first_output:?}, executions={executions_after_duplicate}"
            ))
            .into());
        }
        let different = ctx
            .service_client::<EffectGroupSdkTargetClient>()
            .complete("different-key".to_string())
            .idempotency_key(request.different_key)
            .send()
            .await?;
        let different_output = ctx
            .invocation_handle(different.invocation_id().to_owned())
            .attach::<String>()
            .await?;
        let report = SameKeyReport {
            first_id: first.invocation_id().to_owned(),
            second_id: second.invocation_id().to_owned(),
            different_id: different.invocation_id().to_owned(),
            first_output,
            different_output,
            executions: self.target_executions.load(Ordering::SeqCst),
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

#[derive(Debug, Deserialize, Serialize)]
struct Fig1489SubmitRequest {
    replay_key: String,
    payload: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct Fig1489SubmitReport {
    invocation_id: String,
}

struct Fig1489IngressWitness {
    registry: Arc<dyn ProcessRegistry>,
}

#[restate_sdk::service(name = "Fig1489IngressWitness")]
impl Fig1489IngressWitness {
    #[handler]
    async fn submit(
        &self,
        ctx: Context<'_>,
        Json(request): Json<Fig1489SubmitRequest>,
    ) -> HandlerResult<Json<Fig1489SubmitReport>> {
        let process_id = ProcessId::from(request.replay_key.clone());
        let registration = ProcessRegistration::new(
            process_id.clone(),
            ProcessInput::External {
                metadata: serde_json::json!({ "payload": request.payload }),
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
        );
        let controller = RestateRuntimeEffectController::new(ctx);
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    RuntimeInvocation::effect(
                        RuntimeScope::new("fig1489-live-session"),
                        "tool-intent-ingress:0",
                        RuntimeEffectKind::Process,
                        request.replay_key,
                    ),
                    RuntimeEffectCommand::process(ProcessCommand::Start {
                        registration,
                        observers: Vec::new(),
                        env_spec: None,
                        execution_context: Box::new(ProcessExecutionContext::default()),
                    }),
                ),
                registry_local_executor(Arc::clone(&self.registry)),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Start { record },
        } = outcome
        else {
            return Err(TerminalError::new("FIG-1489 start returned the wrong outcome").into());
        };
        let invocation_id = record
            .external_ref
            .as_ref()
            .and_then(|external_ref| external_ref.metadata.as_ref())
            .and_then(|metadata| metadata.get("invocation_id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| TerminalError::new("FIG-1489 start omitted the invocation id"))?
            .to_string();
        Ok(Json(Fig1489SubmitReport { invocation_id }))
    }

    #[handler]
    async fn attach(&self, ctx: Context<'_>, invocation_id: String) -> HandlerResult<String> {
        Ok(ctx
            .invocation_handle(invocation_id)
            .attach::<String>()
            .await?)
    }
}

struct Fig1489ProcessWorkflow {
    executions: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

#[restate_sdk::workflow(name = "LashProcessWorkflow")]
impl Fig1489ProcessWorkflow {
    #[handler]
    async fn run(
        &self,
        _ctx: WorkflowContext<'_>,
        Json(input): Json<RestateProcessWorkflowInput>,
    ) -> HandlerResult<String> {
        let expected_key = input.registration.id.as_str();
        let execution = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        self.release
            .acquire()
            .await
            .map_err(TerminalError::from_error)?
            .forget();
        Ok(format!("{expected_key}:execution-{execution}"))
    }
}

#[tokio::test]
async fn fig1489_process_start_command_carries_the_effect_replay_key() {
    let registry: Arc<dyn ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let endpoint = Endpoint::builder()
        .bind(Fig1489IngressWitness { registry })
        .build();
    let replay_key = "tool-intent:v2:fig1489-protocol";
    let output = invoke_endpoint_with_scripted_responses(
        &endpoint,
        FIG1489_WITNESS_SERVICE,
        "submit",
        "fig1489-source-invocation",
        &Fig1489SubmitRequest {
            replay_key: replay_key.to_string(),
            payload: "protocol-proof".to_string(),
        },
        vec!["inv_fig1489_protocol".to_string()],
        Vec::new(),
    )
    .await
    .expect("invoke FIG-1489 source handler");

    assert_eq!(
        restate_one_way_call_idempotency_key(&output).as_deref(),
        Some(replay_key),
        "the generated LashProcessWorkflow/run send command must carry the effect replay key"
    );
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
#[ignore = "requires an isolated Restate server; run by `just effect-group-conformance-e2e`"]
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
        .expect("EG0_RESTATE_ENDPOINT_BIND must be set by `just effect-group-conformance-e2e`")
        .parse::<SocketAddr>()
        .expect("valid EG0_RESTATE_ENDPOINT_BIND");
    let endpoint_url = required_url("EG0_RESTATE_ENDPOINT_URL");
    let workflow_executions = Arc::new(AtomicUsize::new(0));
    let target_executions = Arc::new(AtomicUsize::new(0));
    let fig1489_executions = Arc::new(AtomicUsize::new(0));
    let fig1489_release = Arc::new(tokio::sync::Semaphore::new(0));
    let fig1489_registry: Arc<dyn ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("bind EG0 Restate endpoint");
    let endpoint = Endpoint::builder()
        .bind(EffectGroupSdkTarget {
            executions: Arc::clone(&target_executions),
        })
        .bind(EffectGroupSdkWitness {
            target_executions: Arc::clone(&target_executions),
        })
        .bind(EffectGroupSdkWorkflow {
            executions: Arc::clone(&workflow_executions),
        })
        .bind(Fig1489IngressWitness {
            registry: Arc::clone(&fig1489_registry),
        })
        .bind(Fig1489ProcessWorkflow {
            executions: Arc::clone(&fig1489_executions),
            release: Arc::clone(&fig1489_release),
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
    assert_eq!(same_key.first_output, "same-key-first");
    assert_eq!(same_key.different_output, "different-key");
    assert_eq!(same_key.executions, 2);
    println!(
        "EG0_WITNESS same-key=>same-id PASS same={} different={}",
        same_key.first_id, same_key.different_id
    );

    assert_controller_owned_duplicate_submission_attaches_original(
        &client,
        &ingress_url,
        &fig1489_executions,
        &fig1489_release,
    )
    .await;

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

async fn assert_controller_owned_duplicate_submission_attaches_original(
    client: &reqwest::Client,
    ingress_url: &str,
    executions: &AtomicUsize,
    release: &tokio::sync::Semaphore,
) {
    let submit = |replay_key: &str, payload: &str| Fig1489SubmitRequest {
        replay_key: replay_key.to_string(),
        payload: payload.to_string(),
    };
    let fig1489_url = format!("{ingress_url}/{FIG1489_WITNESS_SERVICE}/submit");
    let first: Fig1489SubmitReport = post_json(
        &client,
        fig1489_url.clone(),
        &submit("tool-intent:v2:fig1489-same", "original"),
    )
    .await;
    let start_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while executions.load(Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < start_deadline,
            "the original workflow did not begin before the duplicate probe"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let duplicate: Fig1489SubmitReport = post_json(
        &client,
        fig1489_url.clone(),
        &submit("tool-intent:v2:fig1489-same", "original"),
    )
    .await;
    assert_eq!(first.invocation_id, duplicate.invocation_id);
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the duplicate arrives while the original workflow is still blocked"
    );
    release.add_permits(1);
    let attached: String = post_json(
        &client,
        format!("{ingress_url}/{FIG1489_WITNESS_SERVICE}/attach"),
        &first.invocation_id,
    )
    .await;
    let retained_duplicate: Fig1489SubmitReport = post_json(
        &client,
        fig1489_url.clone(),
        &submit("tool-intent:v2:fig1489-same", "original"),
    )
    .await;
    assert_eq!(first.invocation_id, retained_duplicate.invocation_id);
    assert_eq!(attached, "tool-intent:v2:fig1489-same:execution-1");
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    let distinct: Fig1489SubmitReport = post_json(
        &client,
        fig1489_url,
        &submit("tool-intent:v2:fig1489-distinct", "distinct"),
    )
    .await;
    assert_ne!(first.invocation_id, distinct.invocation_id);
    release.add_permits(1);
    let distinct_attached: String = post_json(
        &client,
        format!("{ingress_url}/{FIG1489_WITNESS_SERVICE}/attach"),
        &distinct.invocation_id,
    )
    .await;
    assert_eq!(
        distinct_attached,
        "tool-intent:v2:fig1489-distinct:execution-2"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    println!(
        "FIG1489_WITNESS controller-owned-ingress PASS original={} duplicate={} retained={} distinct={} executions={}",
        first.invocation_id,
        duplicate.invocation_id,
        retained_duplicate.invocation_id,
        distinct.invocation_id,
        executions.load(Ordering::SeqCst)
    );
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
