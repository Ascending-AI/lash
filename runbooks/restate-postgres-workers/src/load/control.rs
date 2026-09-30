//! A worker's fault-control endpoints (FIG-4169), served on its control port
//! beside the load read endpoint. The fault controller reads which load
//! operations a worker is running before it kills that worker, and the
//! rolling deploy registers, drains and checks generations through the
//! replacing build's own lash API rather than an operator side channel.

use super::ActiveOperations;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use lash_core::engine::BuildGeneration;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// What the control endpoints act through: the worker's one core and the
/// Restate engine its deployment serves.
#[derive(Clone)]
pub struct FaultControl {
    pub worker_id: String,
    pub core: lash::LashCore,
    pub engine: Arc<crate::E2eBackend>,
    pub active: ActiveOperations,
}

/// The build a worker runs, and the load operations it is running now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerActivity {
    pub worker_id: String,
    /// This build's drain generation `G`.
    pub generation: String,
    pub active: Vec<String>,
}

#[derive(Deserialize)]
struct Registration {
    uri: String,
}

type Answer = Result<Json<Value>, (StatusCode, String)>;

fn failed(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn generation(text: &str) -> Result<BuildGeneration, (StatusCode, String)> {
    BuildGeneration::parse(text).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            format!("`{text}` is not a build generation: {error}"),
        )
    })
}

/// `GET /load/active`, `POST /deployment/register`,
/// `POST /generations/{G}/drain` and `GET /generations/{G}/drain`.
pub fn router(control: FaultControl) -> Router {
    Router::new()
        .route("/load/active", get(active))
        .route("/deployment/register", post(register))
        .route(
            "/generations/{generation}/drain",
            post(drain).get(drain_status),
        )
        .with_state(control)
}

async fn active(State(control): State<FaultControl>) -> Json<WorkerActivity> {
    Json(WorkerActivity {
        worker_id: control.worker_id.clone(),
        generation: lash::formats::build_generation().as_str().to_owned(),
        active: control.active.keys(),
    })
}

/// Register this build's deployment at a fresh immutable URI through lash's
/// registration, which refuses a URI another build holds rather than
/// force-registering over it.
async fn register(
    State(control): State<FaultControl>,
    Json(registration): Json<Registration>,
) -> Answer {
    control
        .engine
        .register_deployment(&registration.uri)
        .await
        .map_err(failed)?;
    Ok(Json(json!({
        "uri": registration.uri,
        "generation": lash::formats::build_generation().as_str(),
        "worker_id": control.worker_id,
    })))
}

/// Mark `generation` draining from this, the replacing, build. A build
/// cannot drain itself: lash refuses that.
async fn drain(State(control): State<FaultControl>, Path(text): Path<String>) -> Answer {
    let generation = generation(&text)?;
    let marked = control
        .core
        .drain_generation(&generation)
        .await
        .map_err(failed)?;
    Ok(Json(json!({
        "generation": generation.as_str(),
        "marked": marked,
        "by": lash::formats::build_generation().as_str(),
    })))
}

/// What `generation` still holds, and whether it is drained.
async fn drain_status(State(control): State<FaultControl>, Path(text): Path<String>) -> Answer {
    let generation = generation(&text)?;
    let status = control
        .core
        .generation_drain_status(&generation)
        .await
        .map_err(failed)?;
    let stalled: std::collections::BTreeMap<&str, u64> = status
        .stalled_obligations
        .iter()
        .map(|(kind, count)| (kind.label(), *count))
        .collect();
    Ok(Json(json!({
        "generation": status.generation.as_str(),
        "draining_since_ms": status.draining_since_ms,
        "live_processes": status.live_processes,
        "parked_processes": status.parked_processes,
        "parked_turns": status.parked_turns,
        "in_flight_turns": status.in_flight_turns,
        "closing_sessions": status.closing_sessions,
        "stalled_obligations": stalled,
        "stalled_total": stalled.values().sum::<u64>(),
        "drained": status.drained(),
        "checked_at_ms": status.checked_at,
    })))
}
