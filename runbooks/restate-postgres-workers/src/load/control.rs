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
        generation: control.core.build_generation().as_str().to_owned(),
        active: control.active.keys(),
    })
}

/// Register this build's deployment at an immutable URI through lash's
/// registration, which refuses a URI another build holds rather than
/// force-registering over it. The worker registers for itself because the
/// generation its lanes are named by exists only where its core does: a
/// process that serves no endpoint has no generation to register under.
///
/// A name another authority holds answers `409 Conflict`: that refusal is
/// permanent, where every other failure may pass on a retry.
async fn register(
    State(control): State<FaultControl>,
    Json(registration): Json<Registration>,
) -> Answer {
    control
        .engine
        .register_deployment(&registration.uri)
        .await
        .map_err(|error| match error {
            lash_restate::RestateRegistrationError::NameTaken { .. } => {
                (StatusCode::CONFLICT, error.to_string())
            }
            error => failed(error),
        })?;
    Ok(Json(json!({
        "uri": registration.uri,
        "generation": control.core.build_generation().as_str(),
        "worker_id": control.worker_id,
    })))
}

/// Why a worker did not register its deployment.
#[derive(Debug)]
pub enum WorkerRegistrationError {
    /// Another authority holds one of the deployment's names: permanent.
    NameTaken(String),
    /// The worker or the admin API it registers through is not ready, or
    /// refused: a retry may pass.
    Unregistered(String),
}

impl std::fmt::Display for WorkerRegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::NameTaken(refusal) | Self::Unregistered(refusal)) = self;
        f.write_str(refusal)
    }
}

impl std::error::Error for WorkerRegistrationError {}

/// Have the worker whose control endpoint is `control_url` register its
/// deployment at `uri` (`POST /deployment/register`).
pub async fn register_through_worker(
    client: &reqwest::Client,
    control_url: &str,
    uri: &str,
) -> Result<(), WorkerRegistrationError> {
    let response = client
        .post(format!(
            "{}/deployment/register",
            control_url.trim_end_matches('/')
        ))
        .json(&json!({ "uri": uri }))
        .send()
        .await
        .map_err(|error| WorkerRegistrationError::Unregistered(error.to_string()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let refusal = response.text().await.unwrap_or_default();
    Err(if status == StatusCode::CONFLICT {
        WorkerRegistrationError::NameTaken(refusal)
    } else {
        WorkerRegistrationError::Unregistered(format!("{status}: {refusal}"))
    })
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
        "by": control.core.build_generation().as_str(),
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
