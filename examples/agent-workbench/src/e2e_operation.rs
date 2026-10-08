//! S17 ("operation Run is recoverably explicit", L08/L14) runs on the agent-workbench product host:
//! a plugin task whose body the case holds, followed and reattached by its Run ID.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use lash::plugins::{
    FormatVersion, PluginDeclaration, PluginError, PluginFactory, PluginFailureClass,
    PluginOperation, PluginOperationOutcome, PluginRegistrar, PluginSessionContext, PluginTask,
    PluginTaskContext, SessionParam, SessionPlugin,
};
use lash::sync::MutexExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{AppError, AppState};

type AppResult<T> = Result<T, AppError>;

const PLUGIN: &str = "agent-workbench-e2e-operation";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BodyReceipt {
    pub key: String,
    pub call_id: Option<String>,
    pub attempt: Option<u32>,
}

/// Host-owned controls stay outside the engine journal: a gate per entered
/// key plus the body receipts the routes report.
#[derive(Default)]
pub(crate) struct Controls {
    gates: Mutex<BTreeMap<String, Arc<Notify>>>,
    entered: Mutex<Vec<BodyReceipt>>,
}

impl Controls {
    fn receipts(&self) -> Vec<BodyReceipt> {
        self.entered.lock_recover().clone()
    }

    fn release(&self, key: &str) -> bool {
        let gates = self.gates.lock_recover();
        if let Some(gate) = gates.get(key) {
            gate.notify_one();
            true
        } else {
            false
        }
    }

    async fn enter(&self, receipt: BodyReceipt, cancel: &lash::CancellationToken) {
        let gate = {
            let mut gates = self.gates.lock_recover();
            gates
                .entry(receipt.key.clone())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        // A visible receipt implies the gate exists. Notify keeps a release
        // delivered before the body begins awaiting it.
        self.entered.lock_recover().push(receipt);
        tokio::select! {
            _ = gate.notified() => {},
            _ = cancel.cancelled() => {},
        }
    }
}

/// The plugin carrying the fixture task. Serving and registration cores both
/// push it so the bound deployment generation matches.
pub(crate) struct OperationPlugin {
    controls: Arc<Controls>,
}

impl OperationPlugin {
    pub(crate) fn new(controls: Arc<Controls>) -> Self {
        Self { controls }
    }
}

impl PluginFactory for OperationPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self {
            controls: self.controls.clone(),
        }))
    }
}

impl lash::plugins::PluginDefinition for OperationPlugin {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(PLUGIN)
    }
}

impl SessionPlugin for OperationPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let controls = self.controls.clone();
        reg.operations()
            .typed_task::<WorkbenchOperation, _, _>(move |ctx, output| {
                let controls = controls.clone();
                async move { operation(ctx, controls, output).await }
            })?;
        Ok(())
    }
}

pub(crate) struct WorkbenchOperation;

impl PluginOperation for WorkbenchOperation {
    const NAME: &'static str = "e2e.workbench.operation";
    const DESCRIPTION: &'static str = "Workbench operation whose body the case holds";
    const SESSION_PARAM: SessionParam = SessionParam::Required;
    type Args = String;
    type Output = String;
    type Error = String;
    const ERROR_TYPE: &'static str = "e2e.workbench.operation";
    /// The fixture operation returns a Serde string as its typed error.
    /// version_surface = "coexist"
    /// version_guard(items(Error))
    const ERROR_VERSION: FormatVersion = FormatVersion::ONE;
    fn error_class(_: &String) -> PluginFailureClass {
        PluginFailureClass::Terminal
    }
}
impl PluginTask for WorkbenchOperation {}

/// The task's body: it enters the host's gate under its key and returns its
/// output once the case releases it. Nothing in it is durable until the
/// operation's outcome commits, so a node that dies holding it leaves the
/// operation to run again on the node that claims the session.
async fn operation(
    ctx: PluginTaskContext,
    controls: Arc<Controls>,
    output: String,
) -> Result<PluginOperationOutcome<String>, String> {
    controls
        .enter(
            BodyReceipt {
                key: output.clone(),
                call_id: ctx.session_id.as_ref().map(ToString::to_string),
                attempt: None,
            },
            &ctx.cancellation_token,
        )
        .await;
    Ok(PluginOperationOutcome::new(output))
}

#[derive(Clone)]
pub(crate) struct OperationState {
    pub(crate) app: AppState,
    pub(crate) controls: Arc<Controls>,
}

fn error(message: impl std::fmt::Display) -> AppError {
    AppError::internal(message.to_string())
}

#[derive(Deserialize)]
struct OperationRequest {
    key: String,
    output: String,
}

/// Submits the fixture task and drops the caller's handle: the recorded Run
/// ID is the only way a follower reattaches.
async fn start(
    State(state): State<OperationState>,
    Path(session_id): Path<lash::SessionId>,
    Json(request): Json<OperationRequest>,
) -> AppResult<Json<Value>> {
    let session = state
        .app
        .create_or_open_session(&session_id, "api.e2e.operations")
        .await
        .map_err(error)?;
    let task = session
        .plugin_operations()
        .start_task::<WorkbenchOperation>(request.output, request.key)
        .await
        .map_err(error)?;
    let run = task.run().clone();
    drop(task);
    Ok(Json(json!({"run": run, "admitted": true})))
}

async fn follow(
    State(state): State<OperationState>,
    Path((session_id, run)): Path<(lash::SessionId, lash::TurnId)>,
) -> AppResult<Json<Value>> {
    let session = state
        .app
        .core
        .session(session_id)
        .durable()
        .await
        .map_err(error)?;
    let result = session
        .run(run.clone().into())
        .result()
        .await
        .map_err(error)?;
    Ok(Json(json!({"run": run, "output": result.output})))
}

async fn admission(
    State(state): State<OperationState>,
    Path(session_id): Path<lash::SessionId>,
) -> AppResult<Json<Value>> {
    let session = state
        .app
        .core
        .session(session_id)
        .durable()
        .await
        .map_err(error)?;
    let unfinished = session.unfinished_run().await.map_err(error)?;
    Ok(Json(
        json!({"unfinished": unfinished.map(|unfinished| unfinished.run)}),
    ))
}

async fn bodies(State(state): State<OperationState>) -> Json<Vec<BodyReceipt>> {
    Json(state.controls.receipts())
}

async fn release(State(state): State<OperationState>, Path(key): Path<String>) -> Json<Value> {
    Json(json!({"released": state.controls.release(&key)}))
}

pub(crate) fn routes(state: OperationState) -> Router {
    Router::new()
        .route("/api/e2e/sessions/{session_id}/operations", post(start))
        .route(
            "/api/e2e/sessions/{session_id}/operations/{run}",
            get(follow),
        )
        .route("/api/e2e/sessions/{session_id}/admission", get(admission))
        .route("/api/e2e/operations/bodies", get(bodies))
        .route("/api/e2e/operations/{key}/release", post(release))
        .with_state(state)
}
