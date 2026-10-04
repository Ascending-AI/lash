//! H2's external process receiver: realization appends an actual keyed event
//! to the persistent registry. The fixture routes expose those raw receipts.
#![allow(
    deprecated,
    reason = "fixture follows the current Restate SDK workflow API"
)]
use crate::{AppError, AppState};
type AppResult<T> = Result<T, AppError>;
use axum::extract::{Path, State};
use axum::{Json, Router};
use lash::restate::{RestateAuthorityId, RestateRuntimeEffectController, restate_sdk};
use lash::runtime::{AdmittedScope, ScopedEffectController};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
#[path = "../../shared/h2_receiver.rs"]
mod receiver;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ReceiverRequest {
    session: lash::SessionId,
    event_type: String,
}

pub(crate) struct ReceiverWorkflow {
    pub(crate) core: lash::LashCore,
    pub(crate) authority: RestateAuthorityId,
    pub(crate) namespace: lash::restate::RestateNamespace,
}

#[restate_sdk::workflow(name = "WorkbenchH2Receiver")]
impl ReceiverWorkflow {
    #[restate_sdk::handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        restate_sdk::serde::Json(request): restate_sdk::serde::Json<ReceiverRequest>,
    ) -> HandlerResult<restate_sdk::serde::Json<lash::process::ProcessStartReceipt>> {
        let controller = RestateRuntimeEffectController::new(
            ctx,
            self.authority.clone(),
            self.core.build_generation().clone(),
        )
        .in_namespace(self.namespace.clone());
        let scope = AdmittedScope::runtime_operation(format!("h2-receiver:{}", request.session));
        let scoped = ScopedEffectController::borrowed(&controller, scope)
            .map_err(TerminalError::from_error)?;
        let receipt =
            receiver::register_receiver(&self.core, &request.session, &request.event_type, scoped)
                .await
                .map_err(|error| TerminalError::new(format!("register H2 receiver: {error:#}")))?;
        Ok(restate_sdk::serde::Json(receipt))
    }
}

#[derive(Clone)]
pub(crate) struct ReceiverState {
    pub(crate) app: AppState,
    pub(crate) receiver: Arc<OnceLock<lash::ProcessId>>,
    pub(crate) retained_path: PathBuf,
    pub(crate) event_type: String,
    pub(crate) namespace: lash::restate::RestateNamespace,
}

impl ReceiverState {
    fn ingress(&self) -> lash::restate::RestateIngressClient {
        lash::restate::RestateIngressClient::new(lash::restate::RestateConnection::with_client(
            &self.app.restate_ingress_url,
            self.app.restate_http.clone(),
        ))
    }
}

async fn attach(
    State(state): State<ReceiverState>,
    Path((session_id, input_id)): Path<(lash::SessionId, lash::InputId)>,
) -> AppResult<Json<lash::remote::turn_result::RemoteSendOutcome>> {
    let durable = state
        .app
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(error)?;
    let outcome = durable
        .attach(input_id.clone())
        .outcome()
        .await
        .map_err(error)?;
    Ok(Json(outcome.to_remote(&session_id, &input_id)))
}

fn error(message: impl std::fmt::Display) -> AppError {
    AppError::internal(message.to_string())
}

async fn setup(
    State(state): State<ReceiverState>,
    Path(chat): Path<String>,
) -> AppResult<Json<lash::process::ProcessStartReceipt>> {
    let session_id = lash::SessionId::parse(&chat).map_err(error)?;
    let session = state
        .app
        .core
        .session(session_id)
        .open()
        .await
        .map_err(error)?;
    let receipt: lash::process::ProcessStartReceipt = state
        .ingress()
        .call_workflow_json(
            "WorkbenchH2Receiver",
            &chat,
            "run",
            &ReceiverRequest {
                session: session.session_id().clone(),
                event_type: state.event_type,
            },
        )
        .await
        .map_err(error)?;
    if let Some(previous) = state.receiver.get() {
        if previous != &receipt.process_id {
            return Err(error("receiver identity changed on reopen"));
        }
    } else {
        state
            .receiver
            .set(receipt.process_id.clone())
            .map_err(|_| error("receiver concurrently bound"))?;
    }
    let bytes = serde_json::to_vec(&receipt.process_id).map_err(error)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(state.retained_path)
        .map_err(error)?;
    use std::io::Write;
    file.write_all(&bytes).map_err(error)?;
    file.sync_all().map_err(error)?;
    Ok(Json(receipt))
}

async fn receipts(
    State(state): State<ReceiverState>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let process_id = lash::ProcessId::parse(&id).map_err(error)?;
    if state.receiver.get() != Some(&process_id) {
        return Err(error("receiver receipt requested for another process"));
    }
    let receipts = receiver::receiver_events(&state.app.core, &process_id)
        .await
        .map_err(error)?;
    Ok(Json(serde_json::to_value(receipts).map_err(error)?))
}

#[derive(Deserialize)]
struct CompletionRequest {
    key: lash::AwaitEventKey,
    value: serde_json::Value,
}

async fn resolve(
    State(state): State<ReceiverState>,
    Json(request): Json<CompletionRequest>,
) -> AppResult<Json<lash::ResolveOutcome>> {
    Ok(Json(
        state
            .app
            .core
            .completions()
            .resolve(request.key, lash::Resolution::Ok(request.value))
            .await
            .map_err(AppError::runtime)?,
    ))
}

async fn handover(
    State(state): State<ReceiverState>,
    Path(chat): Path<String>,
) -> AppResult<Json<lash::restate::Reply<u64>>> {
    Ok(Json(
        state
            .ingress()
            .call_object_json(
                &state.namespace.service_name("LashDurableWaitIndex"),
                &chat,
                "hand_over_turns",
                &lash::restate::Call::new(lash::restate::RestateDurableWaitHandOverRequest {
                    generation: state.app.core.build_generation().clone(),
                }),
            )
            .await
            .map_err(error)?,
    ))
}

pub(crate) fn routes(state: ReceiverState) -> Router {
    Router::new()
        .route(
            "/api/e2e/sessions/{session_id}/inputs/{input_id}",
            axum::routing::get(attach),
        )
        .route("/api/e2e/completions", axum::routing::post(resolve))
        .route(
            "/api/e2e/sessions/{chat_id}/handover",
            axum::routing::post(handover),
        )
        .route("/api/e2e/receiver/{chat_id}", axum::routing::post(setup))
        .route(
            "/api/e2e/receiver/{process_id}/receipts",
            axum::routing::get(receipts),
        )
        .with_state(state)
}
