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
    /// The case-owned file the receiver's id is kept in, so a node that
    /// resumes another node's work emits to the same process.
    pub(crate) retained_path: std::path::PathBuf,
    pub(crate) event_type: String,
    pub(crate) ledger: Option<Arc<crate::e2e_commit_ledger::CommitLedger>>,
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
        Ok(outcome) => Ok(Json(serde_json::to_value(outcome).map_err(error)?)),
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

/// The settled outcome of the input a send accepted under turn id `turn`.
async fn attach_turn(
    State(state): State<ReceiverState>,
    Path((session_id, turn)): Path<(lash::SessionId, lash::TurnId)>,
) -> AppResult<Json<serde_json::Value>> {
    let durable = state
        .app
        .core
        .session(session_id.clone())
        .durable()
        .await
        .map_err(error)?;
    let input_id = durable.input_id(&turn);
    attach(State(state), Path((session_id, input_id))).await
}

/// Start the receiver process for `session` and bind the fixture's bodies
/// to it.
async fn setup(
    State(state): State<ReceiverState>,
    Path(session): Path<lash::SessionId>,
) -> AppResult<Json<lash::process::ProcessStartReceipt>> {
    let core = &state.app.core;
    let receipt =
        receiver::register_receiver(core, &session, &state.event_type, core.effect_host())
            .await
            .map_err(error)?;
    std::fs::write(
        &state.retained_path,
        serde_json::to_vec(&receipt.process_id).map_err(error)?,
    )
    .map_err(error)?;
    if state.receiver.set(receipt.process_id.clone()).is_err()
        && state.receiver.get() != Some(&receipt.process_id)
    {
        return Err(error("the fixture is bound to another receiver"));
    }
    Ok(Json(receipt))
}

/// Start a sleeper process for `session`, sleeping for `millis`, and bind
/// the fixture's `handle` body to it.
async fn start_sleeper(
    State(state): State<ReceiverState>,
    Path((session, millis)): Path<(lash::SessionId, i64)>,
) -> AppResult<Json<lash::process::ProcessStartReceipt>> {
    drop(
        state
            .app
            .create_or_open_session(&session, "api.e2e.sleepers")
            .await
            .map_err(error)?,
    );
    let core = &state.app.core;
    let receipt = receiver::start_sleeper(core, &session, millis, core.effect_host())
        .await
        .map_err(error)?;
    if state.receiver.set(receipt.process_id.clone()).is_err() {
        return Err(error("the fixture is bound to another process"));
    }
    Ok(Json(receipt))
}

/// Start a source process for `session` under `key`.
async fn start_source(
    State(state): State<ReceiverState>,
    Path((session, key)): Path<(lash::SessionId, String)>,
) -> AppResult<Json<lash::process::ProcessStartReceipt>> {
    let core = &state.app.core;
    Ok(Json(
        receiver::start_source(core, &session, &key, core.effect_host())
            .await
            .map_err(error)?,
    ))
}

/// The key source process `process` pinned, or null before it is pinned.
async fn source_key(
    State(state): State<ReceiverState>,
    Path(process): Path<lash::ProcessId>,
) -> AppResult<Json<Option<String>>> {
    Ok(Json(
        receiver::source_key(&state.app.core, &process)
            .await
            .map_err(error)?,
    ))
}

/// Drain the node: it releases what it owns at committed phases, then its
/// lease.
async fn drain(State(state): State<ReceiverState>) -> Json<serde_json::Value> {
    Json(match state.app.core.drain().await {
        Ok(report) => serde_json::json!({"drained": format!("{report:?}")}),
        Err(refusal) => serde_json::json!({"refused": format!("{refusal:?}")}),
    })
}

/// Release the case's commit cut `label`.
async fn release_cut(
    State(state): State<ReceiverState>,
    Json(label): Json<String>,
) -> AppResult<Json<serde_json::Value>> {
    let ledger = state
        .ledger
        .as_ref()
        .ok_or_else(|| error("no commit ledger is installed"))?;
    ledger.release(&label);
    Ok(Json(serde_json::json!({"released": label})))
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
        .route(
            "/api/e2e/sessions/{session_id}/turns/{turn}",
            axum::routing::get(attach_turn),
        )
        .route("/api/e2e/completions", axum::routing::post(resolve))
        .route("/api/e2e/control/drain", axum::routing::post(drain))
        .route(
            "/api/e2e/sessions/{session_id}/sources/{key}",
            axum::routing::post(start_source),
        )
        .route(
            "/api/e2e/sources/{process_id}/key",
            axum::routing::get(source_key),
        )
        .route(
            "/api/e2e/sessions/{session_id}/sleepers/{millis}",
            axum::routing::post(start_sleeper),
        )
        .route(
            "/api/e2e/control/cuts/release",
            axum::routing::post(release_cut),
        )
        .route("/api/e2e/receiver/{session_id}", axum::routing::post(setup))
        .route(
            "/api/e2e/receiver/{process_id}/receipts",
            axum::routing::get(receipts),
        )
        .with_state(state)
}
