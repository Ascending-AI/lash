//! H2's external process receiver: realization appends an actual keyed event
//! to the persistent registry. The fixture routes expose those raw receipts.
use crate::state::{AppError, AppResult, AppStateData};
use axum::extract::{Path, State};
use axum::http::StatusCode;
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
}

#[restate_sdk::workflow(name = "AgentServiceH2Receiver")]
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
        );
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
    pub(crate) app: AppStateData,
    pub(crate) receiver: Arc<OnceLock<lash::ProcessId>>,
    pub(crate) retained_path: PathBuf,
    pub(crate) event_type: String,
}

fn error(message: impl std::fmt::Display) -> AppError {
    AppError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: message.to_string(),
    }
}

async fn setup(
    State(state): State<ReceiverState>,
    Path(chat): Path<String>,
) -> AppResult<Json<lash::process::ProcessStartReceipt>> {
    let session = state
        .app
        .open_session(
            &chat,
            crate::routes::LlmProfileChoice {
                key: lash::LlmProfileKey::new(state.app.default_profile()),
                reasoning: Default::default(),
            },
        )
        .await?;
    let receipt: lash::process::ProcessStartReceipt = state
        .app
        .restate_ingress()
        .call_workflow_json(
            "AgentServiceH2Receiver",
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
    let receipts = receiver::receiver_events(state.app.core(), &process_id)
        .await
        .map_err(error)?;
    Ok(Json(serde_json::to_value(receipts).map_err(error)?))
}

pub(crate) fn routes(state: ReceiverState) -> Router {
    Router::new()
        .route("/api/e2e/receiver/{chat_id}", axum::routing::post(setup))
        .route(
            "/api/e2e/receiver/{process_id}/receipts",
            axum::routing::get(receipts),
        )
        .with_state(state)
}
