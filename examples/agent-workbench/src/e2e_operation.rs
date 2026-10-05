//! S17 ("operation Run is recoverably explicit", L08/L14) runs on the agent-workbench product host.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use lash::durability::EffectOpener;
use lash::plugins::{
    AttemptStream, BehaviorRevision, FormatVersion, PluginCallbackIdentity, PluginDeclaration,
    PluginError, PluginFactory, PluginFailureClass, PluginOperation, PluginOperationOutcome,
    PluginRegistrar, PluginRevision, PluginSessionContext, PluginTask, PluginTaskContext,
    SessionParam, SessionPlugin,
};
use lash::runtime::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, BeforeCheckReply, CapacityScope,
    DeclaredStartObligation, ExecutionScope, ExternalCancelPolicy, PresentationBinding,
    RunCoordinator, RuntimeEffectControllerError, SegmentOrdinal, SingletonAttempt,
    SingletonBodyOutcome, SingletonCapture, SingletonPreparedRequest, SingletonPresentationError,
    SingletonToolCall, SingletonToolHandlers,
};
use lash::sync::MutexExt as _;
use lash::tools::{ToolDeclaration, ToolIntentKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{AppError, AppState};

type AppResult<T> = Result<T, AppError>;

const PLUGIN: &str = "agent-workbench-e2e-operation";
const TOOL: &str = "workbench.echo";
const CALLBACK: &str = "tool:echo";

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
    namespace: String,
}

impl OperationPlugin {
    pub(crate) fn new(controls: Arc<Controls>, namespace: String) -> Self {
        Self {
            controls,
            namespace,
        }
    }
}

impl PluginFactory for OperationPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }
    fn declaration(&self) -> PluginDeclaration {
        PluginDeclaration::initial(PluginFactory::id(self))
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self {
            controls: self.controls.clone(),
            namespace: self.namespace.clone(),
        }))
    }
}

impl SessionPlugin for OperationPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let controls = self.controls.clone();
        let namespace = self.namespace.clone();
        reg.operations()
            .typed_task::<WorkbenchOperation, _, _>(move |ctx, output| {
                let controls = controls.clone();
                let namespace = namespace.clone();
                async move { operation(ctx, controls, namespace, output).await }
            })?;
        Ok(())
    }
}

pub(crate) struct WorkbenchOperation;

impl PluginOperation for WorkbenchOperation {
    const NAME: &'static str = "e2e.workbench.operation";
    const DESCRIPTION: &'static str = "Workbench engine-owned operation with one native tool";
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

async fn operation(
    ctx: PluginTaskContext,
    controls: Arc<Controls>,
    namespace: String,
    output: String,
) -> Result<PluginOperationOutcome<String>, String> {
    let ExecutionScope::SessionOperation {
        session_id,
        operation_id,
    } = ctx.scoped_effect_controller.execution_scope()
    else {
        return Err("workbench operation task has no admitted operation owner".to_owned());
    };
    let revision = PluginRevision::new(PLUGIN, BehaviorRevision::ONE);
    let callback = PluginCallbackIdentity {
        owner: revision.clone(),
        key: CALLBACK.into(),
    };
    let call = SingletonToolCall {
        owner: EffectOpener::session_operation(session_id.clone(), operation_id.clone()),
        segment: SegmentOrdinal(0),
        call_id: lash::ToolCallId::derive(
            &namespace,
            lash::ToolCallRoot::host_submission(operation_id).map_err(|error| error.to_string())?,
            &[],
        ),
        tool_name: TOOL.into(),
        arguments: serde_json::json!(output),
        declaration: ToolDeclaration::default(),
        binding: AdmittedBinding {
            executable: callback.clone(),
            preparation: callback,
            presentation: PresentationBinding {
                presenter: None,
                steps: Vec::new(),
            },
        },
        available: vec![revision.clone()],
        cancel: ExternalCancelPolicy::Ignore,
        environment: None,
    };
    let token = ctx.cancellation_token.clone();
    let handlers = Echo {
        output: output.clone(),
        controls,
        token,
    };
    let mut run = RunCoordinator::open(
        &ctx.scoped_effect_controller,
        call.owner.clone(),
        call.segment,
        vec![revision],
    );
    let decided = run
        .start_round(
            std::slice::from_ref(&call),
            CapacityScope::Held,
            Arc::new(handlers),
            Default::default(),
        )
        .await
        .map_err(|error| error.to_string())?;
    if decided.is_empty() {
        while run
            .progress()
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {}
    }
    run.close().await.map_err(|error| error.to_string())?;
    Ok(PluginOperationOutcome::new(output))
}

struct Echo {
    output: String,
    controls: Arc<Controls>,
    token: lash::CancellationToken,
}

#[lash::async_trait]
impl SingletonToolHandlers for Echo {
    async fn prepare(&self, call: &SingletonToolCall) -> Result<Value, String> {
        Ok(call.arguments.clone())
    }
    async fn before_checks(
        &self,
        _: &SingletonToolCall,
        _: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.controls
            .enter(
                BodyReceipt {
                    key: self.output.clone(),
                    call_id: Some(attempt.call_id.to_string()),
                    attempt: Some(attempt.attempt.get()),
                },
                &self.token,
            )
            .await;
        Ok(SingletonBodyOutcome::Done {
            output: self.output.clone(),
            commands: Default::default(),
            intents: Vec::new(),
            start: None,
        })
    }
    async fn after_checks(
        &self,
        _: &lash::ToolCallId,
        _: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }
    async fn run_cancel_requested(&self) -> Result<bool, String> {
        Ok(self.token.is_cancelled())
    }
    async fn realize_declarations(
        &self,
        _: &lash::ToolCallId,
        intents: &[ToolIntentKind],
    ) -> Result<(), String> {
        if intents.is_empty() {
            Ok(())
        } else {
            Err("workbench echo admits no declarations".into())
        }
    }
    async fn present(
        &self,
        _: &lash::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        Ok(capture.output().unwrap_or_default().to_owned())
    }
    fn emit_stream(&self, _: &lash::ToolCallId, _: &AttemptStream) {}
    async fn launch_start(&self, _: &DeclaredStartObligation) -> Result<lash::ProcessId, String> {
        Err("workbench echo admits no process start".into())
    }
    async fn discharge_start(
        &self,
        _: &DeclaredStartObligation,
        _: &lash::ProcessId,
        _: bool,
    ) -> Result<(), String> {
        Err("workbench echo has no consumer hold".into())
    }
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
        .core
        .session(session_id)
        .open()
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
    let result = session.run(run.clone()).result().await.map_err(error)?;
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
