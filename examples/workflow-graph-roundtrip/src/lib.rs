//! HTTP backend for the workflow-graph round-trip example.

use lash::sync::MutexExt;
mod catalog;
mod contract;
mod display;
mod edits;
mod graph;
mod operations;
mod runtime;
mod sample_tools;

use std::collections::BTreeMap;
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
use lash::typescript::workflow_graph::{
    GraphRenderError, WorkflowGraphBuildError, workflow_graph_from_source,
    workflow_graph_from_source_with_facets, workflow_graph_to_source,
};
use lash::vm::ir::WorkflowNodeId;
use lash::workflow::{
    WorkflowDraft, WorkflowDraftHandle, WorkflowDraftOpenError, WorkflowEntry, WorkflowGraph,
    workflow_program_from_graph,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

pub use catalog::{SelectWorkflowRequest, WorkflowCatalogEntry};
pub use contract::{
    ChildGroup, DisplayDelta, DisplayState, EdgeData, EditWorkflowRequest, EditableProcessField,
    EditableValue, ErrorBody, ErrorDetail, ExpectedArgumentType, FlowEdge, FlowNode, GraphRoots,
    NodeBody, NodeContainer, NodeData, NodeName, OpenWorkflowRequest, OperationCatalogEntry,
    OperationField, ProjectWorkflowRequest, ProjectWorkflowResponse, RenderErrorResponse, RunEvent,
    RunStatus, SaveWorkflowResponse, SourceProjectionErrorResponse, TypeDiagnostic, TypedVariable,
    ValidateRequest, ValidateResponse, ValidationKind, WorkflowDocument, WorkflowIrResponse,
};
pub use edits::{BindingRef, BodyRef, EditOperation, IrSlot, NodeIr};
pub use runtime::{WorkflowHost, core as workflow_core};

/// Why the example could not start with its built-in workflow.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Source(#[from] WorkflowGraphBuildError),
    #[error(transparent)]
    Document(#[from] WorkflowDraftOpenError),
}

/// Default deterministic workflow served as version 1.
///
/// TypeScript is the only cell language, so this corpus is TypeScript and the
/// lens's canonical text is TypeScript (FIG-3033). Its authored names are
/// spelled as `@label` doc comments (FIG-3047) — the form an editor rename
/// writes back — so the corpus exercises both the authored and the derived
/// naming paths.
pub const DEFAULT_WORKFLOW: &str = r#"/** @label Onboarding — Welcome a new operator and wait for their approval */
const onboarding = async () => {
  /** @label Start the run */
  await display.set_status({ key: "phase", value: "starting" });
  await sleep("400ms");
  await display.show_message({ text: "Welcome to the workflow graph" });
  await display.set_light({ name: "ready", state: "green" });
  await sleep("400ms");
  if (true) {
    await display.set_progress({ pct: 35 });
  } else {
    await display.show_message({ text: "Alternate path" });
  }
  /** @label Wait for approval — Hold until the operator approves the request */
  const approval = await host.approval({});
  await display.highlight({ target: "checklist" });
  await display.add_item({ list: "steps", item: "Approved" });
  let count = 0;
  /** @label Replay the checklist */
  while (count < 2) {
    await display.add_item({ list: "steps", item: "Loop item" });
    count = count + 1;
    await sleep("250ms");
  }
  await sleep("400ms");
  await display.set_progress({ pct: 100 });
  await display.set_light({ name: "complete", state: "blue" });
  return approval;
};
"#;

#[derive(Clone)]
pub struct AppState {
    store: Arc<Mutex<WorkflowStore>>,
    /// Held across a save, and across a run's start: a save releases the pin
    /// of the version it supersedes, which a start must not race.
    publishing: Arc<tokio::sync::Mutex<()>>,
    core: lash::LashCore,
    commands: runtime::CommandClient,
    host: Arc<display::HostTools>,
}

#[derive(Default)]
struct WorkflowStore {
    versions: Vec<SavedWorkflow>,
}

/// One saved version: host revision state. The workflow itself is the
/// document; lash holds its definition.
#[derive(Clone)]
struct SavedWorkflow {
    version: u64,
    /// The workflow as the host edits it. The draft lives across saves, so
    /// every node keeps its handle for as long as edits keep the node.
    draft: WorkflowDraft,
    /// The host-selected entry in the draft, retained through edit handles.
    entry: Result<WorkflowEntry, Arc<runtime::RunError>>,
    /// The document clients are served: the admitted document when lash
    /// admitted the version (a run reports against its ids), else the
    /// draft's own.
    graph: WorkflowGraph,
    /// The id each node of the draft has in `graph`.
    ids: BTreeMap<WorkflowDraftHandle, WorkflowNodeId>,
    /// What a run starts, or why lash did not admit the version.
    published: Result<runtime::Published, Arc<runtime::RunError>>,
}

impl SavedWorkflow {
    /// Edits can change a process's name and document id. Its draft handle
    /// carries the selected entry to the edited document, or records its removal.
    fn edited_entry(
        original: &WorkflowDraft,
        entry: &Result<WorkflowEntry, Arc<runtime::RunError>>,
        draft: &WorkflowDraft,
    ) -> Result<WorkflowEntry, Arc<runtime::RunError>> {
        let entry = entry.as_ref().map_err(Arc::clone)?;
        if let WorkflowEntry::Process(id) = entry
            && let Some(handle) = original.handle(id)
            && draft.process(handle).is_some()
            && let Some(id) = draft.node_id(handle)
        {
            return Ok(WorkflowEntry::Process(id.clone()));
        }
        Err(Arc::new(runtime::RunError::Invalid(
            "the selected workflow entry was removed".into(),
        )))
    }

    /// The handle of each node, by the id clients know it under.
    fn named(&self) -> BTreeMap<WorkflowNodeId, WorkflowDraftHandle> {
        self.ids
            .iter()
            .map(|(handle, id)| (id.clone(), *handle))
            .collect()
    }

    /// The TypeScript lens's view of the version, or why it has none. The
    /// lens runs here, when a client asks to see source; saving printed
    /// nothing.
    fn source(&self) -> Result<String, String> {
        workflow_graph_to_source(self.draft.document()).map_err(|error| error.to_string())
    }

    fn document(&self) -> WorkflowDocument {
        let mut document =
            graph::document_from_graph(self.version, self.source(), faceted(&self.graph));
        document.not_admitted = self
            .published
            .as_ref()
            .err()
            .map(|refusal| refusal.to_string());
        document
    }
}

/// `graph` with the type facets of its program against this host's
/// environment. Facets are derived hints for the forms: they are computed
/// from the document's own IR and are never read back.
fn faceted(graph: &WorkflowGraph) -> WorkflowGraph {
    let Ok(program) = workflow_program_from_graph(graph) else {
        return graph.clone();
    };
    let environment = runtime::host_environment();
    let analysis = lash::vm::analyze_workflow_program(&program, &environment);
    let mut projector =
        lash::vm::ir::WorkflowGraphProjector::new(&program).with_analysis(&analysis);
    if let Some(identity) = &graph.source_identity {
        projector = projector.with_source_identity(identity.clone());
    }
    let projected = projector.project();
    let same_nodes = projected
        .nodes()
        .map(|node| &node.id)
        .eq(graph.nodes().map(|node| &node.id));
    if same_nodes { projected } else { graph.clone() }
}

impl AppState {
    /// The example's state with the built-in workflow saved as version 1.
    pub async fn new(runtime: WorkflowHost) -> Result<Self, StartupError> {
        let core = runtime.core;
        let state = Self {
            store: Arc::new(Mutex::new(WorkflowStore::default())),
            publishing: Arc::new(tokio::sync::Mutex::new(())),
            host: runtime.tools,
            commands: runtime::CommandClient::new(core.clone()),
            core,
        };
        let graph = workflow_graph_from_source(DEFAULT_WORKFLOW)?;
        let draft = WorkflowDraft::open(&graph)?;
        state.open(draft).await;
        Ok(state)
    }

    #[expect(
        clippy::expect_used,
        reason = "AppState::new seeds the store with version 1 and save always pushes"
    )]
    fn current(&self) -> SavedWorkflow {
        self.store
            .lock_recover()
            .versions
            .last()
            .expect("workflow store always has a version")
            .clone()
    }

    /// Saves `draft` as the next version and publishes it: lash admits the
    /// draft's document as a definition under a pin the version holds. A
    /// draft lash refuses is still saved, as a version that cannot run.
    async fn open(&self, draft: WorkflowDraft) -> SavedWorkflow {
        let entry = runtime::select_entry(&draft).map_err(Arc::new);
        self.install(draft, entry).await
    }

    async fn install(
        &self,
        draft: WorkflowDraft,
        entry: Result<WorkflowEntry, Arc<runtime::RunError>>,
    ) -> SavedWorkflow {
        let publication = match &entry {
            Ok(entry) => runtime::publish(&self.core, &draft, entry.clone())
                .await
                .map_err(Arc::new),
            Err(error) => Err(error.clone()),
        };
        let (graph, ids, published) = match publication {
            Ok(publication) => (
                publication.graph,
                publication.ids,
                Ok(publication.published),
            ),
            Err(refusal) => (
                draft.document().clone(),
                runtime::surviving_ids(&draft.correspondence_since_open()),
                Err(refusal),
            ),
        };
        let (saved, superseded) = {
            let mut store = self.store.lock_recover();
            let version = store.versions.last().map_or(1, |saved| saved.version + 1);
            let saved = SavedWorkflow {
                version,
                draft,
                entry,
                graph,
                ids,
                published,
            };
            let superseded = store.versions.last_mut().and_then(|previous| {
                previous
                    .published
                    .as_mut()
                    .ok()
                    .map(|held| held.pin.clone())
            });
            store.versions.push(saved.clone());
            (saved, superseded)
        };
        // Processes already started keep the definition they were admitted
        // under; only the next start needs a pin, and that is the new one.
        if let Some(pin) = superseded
            && let Err(error) = self.core.host_artifacts().release(pin).await
        {
            eprintln!("warning: a superseded workflow version kept its pin: {error}");
        }
        saved
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
        .route("/workflow/ir", get(get_workflow_ir).post(open_workflow_ir))
        .route("/workflow/edits", post(edit_workflow))
        .route("/run", post(run_workflow))
        .route("/approvals/{key}", post(resolve_approval))
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

/// The TypeScript lens as an import: the document a source spells, without
/// saving it.
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
        document: graph::document_from_graph(version, Ok(source), graph),
    }))
}

async fn select_workflow(
    State(state): State<AppState>,
    Json(request): Json<SelectWorkflowRequest>,
) -> Result<Json<WorkflowDocument>, RenderErrorResponse> {
    let source = catalog::source(&request.id)
        .ok_or_else(|| RenderErrorResponse::unknown_workflow(&request.id))?;
    let graph = workflow_graph_from_source(source).map_err(RenderErrorResponse::projection)?;
    let draft = WorkflowDraft::open(&graph).map_err(RenderErrorResponse::open)?;
    let _publishing = state.publishing.lock().await;
    Ok(Json(state.open(draft).await.document()))
}

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, app(state)).await
}

pub async fn serve_addr(addr: SocketAddr, state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve(listener, state).await
}

async fn get_workflow(State(state): State<AppState>) -> Json<WorkflowDocument> {
    Json(state.current().document())
}

/// The saved workflow as the typed document it is, with each node's
/// statement and the expressions of it a generic editor can replace.
async fn get_workflow_ir(
    State(state): State<AppState>,
) -> Result<Json<WorkflowIrResponse>, RenderErrorResponse> {
    let saved = state.current();
    // The draft's own document is what an edit addresses: a slot path read
    // here is the path a `replaceExpression` takes.
    let nodes = saved
        .ids
        .iter()
        .filter_map(|(handle, id)| Some((id, saved.draft.node(*handle)?)))
        .map(|(id, node)| Ok((id.to_string(), edits::node_ir(node)?)))
        .collect::<Result<_, edits::EditError>>()
        .map_err(RenderErrorResponse::edit)?;
    Ok(Json(WorkflowIrResponse {
        version: saved.version,
        graph: saved.draft.document().clone(),
        nodes,
    }))
}

/// Opens a workflow given as its typed document: no source is involved.
async fn open_workflow_ir(
    State(state): State<AppState>,
    Json(request): Json<OpenWorkflowRequest>,
) -> Result<Json<WorkflowDocument>, RenderErrorResponse> {
    let graph =
        WorkflowGraph::decode_json_value(request.graph).map_err(RenderErrorResponse::decode)?;
    let draft = WorkflowDraft::open(&graph).map_err(RenderErrorResponse::open)?;
    let _publishing = state.publishing.lock().await;
    Ok(Json(state.open(draft).await.document()))
}

/// The response to an edit: the new version, and where each node the
/// client named is now. A node the edit removed has no entry.
fn saved_response(
    saved: &SavedWorkflow,
    handles: impl IntoIterator<Item = (String, WorkflowDraftHandle)>,
) -> SaveWorkflowResponse {
    let id_map = handles
        .into_iter()
        .filter_map(|(submitted, handle)| Some((submitted, saved.ids.get(&handle)?.to_string())))
        .collect();
    SaveWorkflowResponse {
        document: saved.document(),
        id_map,
    }
}

/// Saves the document a form editor submits. The document is turned into
/// typed edits of the draft it was read from, by node identity, and the
/// edited draft is published; the response maps every id the document used
/// to the id that node has now, read from lash's edit correspondence.
async fn save_workflow(
    State(state): State<AppState>,
    Json(document): Json<WorkflowDocument>,
) -> Result<Json<SaveWorkflowResponse>, RenderErrorResponse> {
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    if document.version != current.version {
        return Err(RenderErrorResponse::version_conflict(
            document.version,
            current.version,
        ));
    }
    WorkflowGraph::admit_schema_version_for_fleet(
        document.schema_version,
        lash::persistence::FleetFormat::current(),
    )
    .map_err(|refusal| RenderErrorResponse::render(refusal.into()))?;
    // A document names the nodes of the workflow it was read from. One that
    // came from the source pane is a TypeScript import with no edit history:
    // it is a new workflow, read again from its own source into a new draft.
    let imported = !document.source.is_empty() && current.source().as_ref() != Ok(&document.source);
    let (draft, named, base, entry) = if imported {
        let graph = workflow_graph_from_source(&document.source)
            .map_err(RenderErrorResponse::projection)?;
        let draft = WorkflowDraft::open(&graph).map_err(RenderErrorResponse::open)?;
        let named = draft
            .opened()
            .map(|(handle, id)| (id.clone(), handle))
            .collect();
        let entry = runtime::select_entry(&draft).map_err(Arc::new);
        (draft, named, graph, entry)
    } else {
        (
            current.draft.clone(),
            current.named(),
            current.graph.clone(),
            current.entry.clone(),
        )
    };
    let target = graph::graph_from_document(document, &base)?;
    let original = draft.clone();
    let applied =
        edits::apply_document(draft, &named, &base, &target).map_err(RenderErrorResponse::edit)?;
    let entry = SavedWorkflow::edited_entry(&original, &entry, &applied.draft);
    let saved = state.install(applied.draft, entry).await;
    Ok(Json(saved_response(&saved, applied.handles)))
}

/// Applies the typed edits of the generic structured editor as one
/// transaction and publishes the result.
async fn edit_workflow(
    State(state): State<AppState>,
    Json(request): Json<EditWorkflowRequest>,
) -> Result<Json<SaveWorkflowResponse>, RenderErrorResponse> {
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    if request.version != current.version {
        return Err(RenderErrorResponse::version_conflict(
            request.version,
            current.version,
        ));
    }
    let named = current.named();
    let draft = edits::apply_operations(current.draft.clone(), &named, request.edits)
        .map_err(RenderErrorResponse::edit)?;
    let entry = SavedWorkflow::edited_entry(&current.draft, &current.entry, &draft);
    let saved = state.install(draft, entry).await;
    Ok(Json(saved_response(
        &saved,
        named
            .into_iter()
            .map(|(id, handle)| (id.to_string(), handle)),
    )))
}

#[expect(
    clippy::expect_used,
    reason = "RunEvent is a serde struct, so to_string cannot fail"
)]
async fn run_workflow(
    State(state): State<AppState>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, RenderErrorResponse> {
    let (tx, rx) = mpsc::channel::<Result<RunEvent, runtime::RunError>>(64);
    let key = uuid::Uuid::new_v4().to_string();
    let (version, started) = {
        let _publishing = state.publishing.lock().await;
        let saved = state.current();
        let published = saved
            .published
            .as_ref()
            .map_err(RenderErrorResponse::run_preparation)?;
        let started = state.commands.start(published.start_request(&key)).await;
        (saved.version, started)
    };
    let started = started.map_err(RenderErrorResponse::run_preparation)?;
    let core = state.core;
    let host = state.host;
    tokio::spawn(async move {
        if let Err(error) =
            runtime::observe(core, started.process_id, version, tx.clone(), host).await
        {
            let _ = tx.send(Err(error)).await;
        }
    });
    let stream = ReceiverStream::new(rx).map(|event| {
        let event = match event {
            Ok(event) => event,
            Err(error) => return Ok(Event::default().event("run_error").data(error.to_string())),
        };
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

async fn resolve_approval(
    State(state): State<AppState>,
    AxumPath(key): AxumPath<String>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, RenderErrorResponse> {
    let approved = payload
        .get("approved")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            RenderErrorResponse::run_preparation("approval requires an approved boolean")
        })?;
    let answer = state
        .core
        .completions()
        .resolve(
            &key,
            lash::Resolution::Ok(serde_json::json!({"approved": approved})),
        )
        .await
        .map_err(RenderErrorResponse::run_preparation)?;
    match answer {
        lash::durable::ResolveAnswer::Resolved | lash::durable::ResolveAnswer::AlreadyResolved => {
            state.host.forget_approval(&key);
            Ok(Json(serde_json::json!({"accepted": true})))
        }
        other => Err(RenderErrorResponse::run_preparation(format!(
            "approval resolution refused: {other:?}"
        ))),
    }
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

#[cfg(test)]
mod save_tests {
    use super::*;

    /// FIG-3630: a document projected from source that is not the saved
    /// workflow saves as that source. Its process is a lifted literal whose
    /// origin the save re-derives from the document's own source, and its
    /// parameter type survives as the annotation that lowers to it.
    #[tokio::test]
    async fn a_projected_workflow_with_a_lifted_process_saves_as_its_source() {
        let stores = lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory stores");
        let backend = lash::durable::DurableBackendBuilder::new(std::sync::Arc::new(stores))
            .build()
            .expect("the durable backend");
        let core = workflow_core(backend).expect("workflow core");
        let state = AppState::new(core).await.expect("default workflow");
        let Json(projected) = project_source(
            State(state.clone()),
            Json(ProjectWorkflowRequest {
                source: "const typed = async (name: string) => {\n  return name;\n};\n".to_string(),
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("the source projects"));
        let source = projected.document.source.clone();
        let Json(saved) = save_workflow(State(state), Json(projected.document))
            .await
            .unwrap_or_else(|error| panic!("the projected workflow saves: {:?}", error.body));
        assert_eq!(saved.document.source, source);
        assert!(saved.document.source.contains("async (name: string)"));
    }
}
