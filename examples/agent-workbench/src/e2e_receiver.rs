//! H2's process receiver: realization appends an actual keyed event
//! to the persistent registry. The fixture routes expose those raw receipts.
use crate::{AppError, AppState};
type AppResult<T> = Result<T, AppError>;
use axum::extract::{Path, State};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::{Arc, OnceLock};
#[path = "../../shared/h2_receiver.rs"]
mod receiver;
pub(crate) use receiver::ReceiverEnginePlugin;

#[derive(Clone)]
pub(crate) struct ReceiverState {
    pub(crate) app: AppState,
    pub(crate) receiver: Arc<OnceLock<lash::ProcessId>>,
}

/// The settled outcome, or the typed runtime refusal the follow ended with:
/// a cause the engine typed stays typed for the case reading it.
async fn attach(
    State(state): State<ReceiverState>,
    Path((session_id, input_id)): Path<(lash::SessionId, lash::InputId)>,
) -> AppResult<Json<serde_json::Value>> {
    let durable = state
        .app
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(error)?;
    match durable.attach(input_id.clone()).outcome().await {
        Ok(outcome) => Ok(Json(
            serde_json::to_value(outcome.to_remote(&session_id, &input_id)).map_err(error)?,
        )),
        Err(lash::EmbedError::Runtime(refusal)) => Ok(Json(serde_json::json!({
            "type": "refused",
            "session_id": session_id,
            "input_id": input_id,
            "error": refusal,
        }))),
        Err(other) => Err(error(other)),
    }
}

fn error(message: impl std::fmt::Display) -> AppError {
    AppError::internal(message.to_string())
}

/// Registering the receiver ran as an engine workflow; it waits for L3.
async fn setup(Path(_chat): Path<String>) -> AppResult<Json<lash::process::ProcessStartReceipt>> {
    Err(AppError::no_engine("H2 receiver registration"))
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
    key: String,
    value: serde_json::Value,
}

async fn resolve(
    State(state): State<ReceiverState>,
    Json(request): Json<CompletionRequest>,
) -> AppResult<Json<String>> {
    let answer = state
        .app
        .core
        .completions()
        .resolve(&request.key, lash::Resolution::Ok(request.value))
        .await
        .map_err(AppError::runtime)?;
    Ok(Json(format!("{answer:?}")))
}

pub(crate) fn routes(state: ReceiverState) -> Router {
    Router::new()
        .route(
            "/api/e2e/sessions/{session_id}/inputs/{input_id}",
            axum::routing::get(attach),
        )
        .route("/api/e2e/completions", axum::routing::post(resolve))
        .route("/api/e2e/receiver/{chat_id}", axum::routing::post(setup))
        .route(
            "/api/e2e/receiver/{process_id}/receipts",
            axum::routing::get(receipts),
        )
        .with_state(state)
}
