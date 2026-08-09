//! HTTP backend for the workflow-graph round-trip example.

use lash::sync::MutexExt;
mod catalog;
mod contract;
mod display;
mod graph;
mod mock_tools;
mod operations;
mod runtime;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lashlang::{
    GraphRenderError, WorkflowGraph, workflow_graph_from_source,
    workflow_graph_from_source_with_facets, workflow_graph_to_source,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

pub use catalog::{SelectWorkflowRequest, WorkflowCatalogEntry};
pub use contract::{
    ChildGroup, DisplayDelta, DisplayState, EdgeData, EditableComprehensionClause,
    EditableProcessField, EditableValue, ExpectedArgumentType, FlowEdge, FlowNode, GraphRoots,
    NodeData, OperationCatalogEntry, OperationField, ProjectWorkflowRequest,
    ProjectWorkflowResponse, RenderErrorResponse, RunEvent, RunStatus, SaveWorkflowResponse,
    SourceProjectionErrorResponse, TypeDiagnostic, TypedVariable, ValidateRequest,
    ValidateResponse, ValidationKind, WorkflowDocument,
};
pub use runtime::RunTiming;

/// Default deterministic workflow served as version 1.
pub const DEFAULT_WORKFLOW: &str = r#"
@label(title: "Onboarding lights", description: "A visible, deterministic workflow")
process onboarding() signals { continue: any } {
  @label(title: "Start onboarding", description: "Set the header state")
  await display.set_status({ key: "phase", value: "starting" })?
  sleep for "400ms"
  await display.show_message({ text: "Welcome to the workflow graph" })?
  await display.set_light({ name: "ready", state: "green" })?
  sleep for "400ms"
  if true {
    await display.set_progress({ pct: 35 })?
  } else {
    await display.show_message({ text: "Alternate path" })?
  }
  @label(title: "Wait for approval", description: "The host auto-fires this signal")
  approval = wait_signal("continue")
  await display.highlight({ target: "checklist" })?
  await display.add_item({ list: "steps", item: "Approved" })?
  count = 0
  while count < 2 { await display.add_item({ list: "steps", item: "Loop item" })? count = count + 1 sleep for "250ms" }
  sleep for "400ms"
  await display.set_progress({ pct: 100 })?
  await display.set_light({ name: "complete", state: "blue" })?
  finish approval
}
"#;

#[derive(Clone)]
pub struct AppState {
    store: Arc<Mutex<WorkflowStore>>,
    timing: RunTiming,
}

struct WorkflowStore {
    versions: Vec<SavedWorkflow>,
}

#[derive(Clone)]
struct SavedWorkflow {
    version: u64,
    source: String,
    graph: WorkflowGraph,
}

impl AppState {
    pub fn new() -> Result<Self, lashlang::WorkflowGraphBuildError> {
        Self::with_run_timing(RunTiming::default())
    }

    pub fn with_run_timing(timing: RunTiming) -> Result<Self, lashlang::WorkflowGraphBuildError> {
        let graph = workflow_graph_from_source(DEFAULT_WORKFLOW)?;
        let source = workflow_graph_to_source(&graph)
            .expect("the default workflow graph should render canonically");
        Ok(Self {
            store: Arc::new(Mutex::new(WorkflowStore {
                versions: vec![SavedWorkflow {
                    version: 1,
                    source,
                    graph,
                }],
            })),
            timing,
        })
    }

    fn current(&self) -> SavedWorkflow {
        self.store
            .lock_recover()
            .versions
            .last()
            .expect("workflow store always has a version")
            .clone()
    }

    fn save(&self, source: String, graph: WorkflowGraph) -> SavedWorkflow {
        let mut store = self.store.lock_recover();
        let version = store.versions.last().map_or(1, |saved| saved.version + 1);
        let saved = SavedWorkflow {
            version,
            source,
            graph,
        };
        store.versions.push(saved.clone());
        saved
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new().expect("default workflow should be valid")
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/workflows", get(list_workflows))
        .route("/operations", get(list_operations))
        .route("/validate", post(validate_fragment))
        .route("/project", post(project_source))
        .route("/workflow", get(get_workflow).post(save_workflow))
        .route("/workflow/select", post(select_workflow))
        .route("/run", post(run_workflow))
        .route("/healthz", get(healthz))
        .route("/", get(static_index))
        .route("/{*path}", get(static_asset))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

async fn list_workflows() -> Json<Vec<WorkflowCatalogEntry>> {
    Json(catalog::entries())
}

async fn list_operations() -> Json<Vec<OperationCatalogEntry>> {
    Json(operations::entries())
}

async fn validate_fragment(Json(request): Json<ValidateRequest>) -> Json<ValidateResponse> {
    Json(graph::validate_fragment(request))
}

async fn project_source(
    State(state): State<AppState>,
    Json(request): Json<ProjectWorkflowRequest>,
) -> Result<Json<ProjectWorkflowResponse>, SourceProjectionErrorResponse> {
    let version = state.current().version;
    let environment = runtime::host_environment();
    let graph = workflow_graph_from_source_with_facets(&request.source, Some(&environment))
        .map_err(|error| SourceProjectionErrorResponse::invalid_source(error.to_string()))?;
    let source = workflow_graph_to_source(&graph)
        .map_err(|error| SourceProjectionErrorResponse::invalid_source(error.to_string()))?;
    Ok(Json(ProjectWorkflowResponse {
        document: graph::document_from_graph(version, source, graph),
    }))
}

async fn select_workflow(
    State(state): State<AppState>,
    Json(request): Json<SelectWorkflowRequest>,
) -> Result<Json<WorkflowDocument>, RenderErrorResponse> {
    let source = catalog::source(&request.id)
        .ok_or_else(|| RenderErrorResponse::unknown_workflow(&request.id))?;
    let graph = workflow_graph_from_source(source).map_err(RenderErrorResponse::projection)?;
    let source = workflow_graph_to_source(&graph).map_err(RenderErrorResponse::from)?;
    let saved = state.save(source, graph);
    let environment = runtime::host_environment();
    let graph = workflow_graph_from_source_with_facets(&saved.source, Some(&environment))
        .map_err(RenderErrorResponse::projection)?;
    Ok(Json(graph::document_from_graph(
        saved.version,
        saved.source,
        graph,
    )))
}

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, app(state)).await
}

pub async fn serve_addr(addr: SocketAddr, state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve(listener, state).await
}

async fn get_workflow(State(state): State<AppState>) -> Json<WorkflowDocument> {
    let saved = state.current();
    let environment = runtime::host_environment();
    let graph = workflow_graph_from_source_with_facets(&saved.source, Some(&environment))
        .expect("saved workflow source should reproject with type facets");
    Json(graph::document_from_graph(
        saved.version,
        saved.source,
        graph,
    ))
}

async fn save_workflow(
    State(state): State<AppState>,
    Json(document): Json<WorkflowDocument>,
) -> Result<Json<SaveWorkflowResponse>, RenderErrorResponse> {
    let current = state.current();
    if document.version != current.version {
        return Err(RenderErrorResponse::version_conflict(
            document.version,
            current.version,
        ));
    }
    let graph = graph::graph_from_document(document.clone(), &current.graph)?;
    let source = workflow_graph_to_source(&graph).map_err(RenderErrorResponse::from)?;
    let graph = workflow_graph_from_source(&source).map_err(RenderErrorResponse::projection)?;
    let saved = state.save(source, graph);
    let environment = runtime::host_environment();
    let faceted_graph = workflow_graph_from_source_with_facets(&saved.source, Some(&environment))
        .map_err(RenderErrorResponse::projection)?;
    let reprojected = graph::document_from_graph(saved.version, saved.source, faceted_graph);
    let id_map = graph::reconcile_node_ids(&document, &reprojected);
    Ok(Json(SaveWorkflowResponse {
        document: reprojected,
        id_map,
    }))
}

async fn run_workflow(
    State(state): State<AppState>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, RenderErrorResponse> {
    let saved = state.current();
    let prepared = runtime::PreparedRun::new(saved.graph, &saved.source, saved.version)
        .map_err(RenderErrorResponse::run_preparation)?;
    let (tx, rx) = mpsc::channel::<RunEvent>(64);
    let timing = state.timing;
    tokio::spawn(async move {
        prepared.execute(tx, timing).await;
    });
    let stream = ReceiverStream::new(rx).map(|event| {
        let sequence = event.sequence.to_string();
        let json = serde_json::to_string(&event).expect("run events serialize");
        Ok(Event::default().event("run_event").id(sequence).data(json))
    });
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keep-alive"),
    ))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "service": "workflow-graph-roundtrip",
        "status": "ok"
    }))
}

async fn cors(request: axum::extract::Request, next: Next) -> Response {
    if request.method() == Method::OPTIONS {
        return add_cors_headers(StatusCode::NO_CONTENT.into_response());
    }
    add_cors_headers(next.run(request).await)
}

fn add_cors_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    response
}

async fn static_index() -> Response {
    static_response("index.html").await
}

async fn static_asset(AxumPath(path): AxumPath<String>) -> Response {
    static_response(&path).await
}

async fn static_response(path: &str) -> Response {
    let Some(relative) = safe_frontend_path(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let frontend = Path::new(env!("CARGO_MANIFEST_DIR")).join("frontend");
    for root in [frontend.join("dist"), frontend] {
        let path = root.join(&relative);
        if let Ok(bytes) = tokio::fs::read(&path).await {
            return ([(header::CONTENT_TYPE, content_type(&path))], bytes).into_response();
        }
    }
    (
        StatusCode::NOT_FOUND,
        "Frontend not built. Run the frontend dev server or place its build in examples/workflow-graph-roundtrip/frontend/dist.",
    )
        .into_response()
}

fn safe_frontend_path(path: &str) -> Option<PathBuf> {
    let relative = Path::new(path);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(relative.to_path_buf())
}

fn content_type(path: &Path) -> HeaderValue {
    let content_type = match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(content_type)
}

impl IntoResponse for RenderErrorResponse {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl IntoResponse for SourceProjectionErrorResponse {
    fn into_response(self) -> Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(self.body)).into_response()
    }
}

impl From<GraphRenderError> for RenderErrorResponse {
    fn from(error: GraphRenderError) -> Self {
        Self::render(error)
    }
}
