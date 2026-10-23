//! HTTP backend for the workflow round-trip example.
//!
//! The workflow is a kernel document. The backend holds one draft of it,
//! applies kernel edits to the draft as transactions, publishes every saved
//! version through lash, runs the published definition, and shows a run as
//! lash's execution overlay of the document's sites.

use lash::sync::MutexExt;
mod authoring_http;
mod catalog;
mod contract;
mod display;
pub mod host_guide;
mod runtime;
mod sample_tools;

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
use lash::schema::Name;
use lash::workflow::document::{Captions, FunctionRegistry, Site, print_document};
use lash::workflow::edit::{Draft, Location, Transaction};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

pub use catalog::{CatalogGroup, LoweringSize, SelectWorkflowRequest, WorkflowCatalogEntry};
pub use contract::{
    AuthoredSource, CallData, CaptionWorkflowRequest, Dialect, DisplayDelta, DisplayState,
    EditWorkflowRequest, EditWorkflowResponse, EnvironmentView, ErrorBody, ErrorDetail,
    ErrorResponse, OpenWorkflowRequest, RunEvent, RunPath, RunStatus, WorkflowView,
};
pub use runtime::{WorkflowHost, core as workflow_core};

/// Why the example could not start with its built-in workflow.
#[derive(Debug, thiserror::Error)]
#[error("the built-in workflow does not open: {0}")]
pub struct StartupError(String);

#[derive(Clone)]
pub struct AppState {
    namespace_pin: lash::process::HostArtifactPin,
    store: Arc<Mutex<Option<SavedWorkflow>>>,
    /// Held across a save, and across a run's start: a save releases the pin
    /// of the version it supersedes, which a start must not race.
    publishing: Arc<tokio::sync::Mutex<()>>,
    core: lash::LashCore,
    /// The worker the example prints and reads workflow source with.
    workers: lash::vm::WorkerService,
    commands: runtime::CommandClient,
    host: Arc<display::HostTools>,
    catalog: Arc<catalog::Catalog>,
    function_labels: std::collections::BTreeMap<String, String>,
}

/// The saved version: host revision state. The workflow itself is the
/// draft's document; lash holds its definition.
#[derive(Clone)]
struct SavedWorkflow {
    namespace: Option<runtime::Namespace>,
    version: u64,
    /// The workflow as the host edits it.
    draft: Draft,
    /// The entry of the document a run starts.
    entry: Name,
    /// What a run starts, or why lash did not admit the version.
    published: Result<runtime::Published, Arc<runtime::RunError>>,
    /// The source the workflow was imported from, if it was.
    authored: Option<AuthoredSource>,
    /// The host's caption sidecar for the draft's document. It is the
    /// authority for captions: importing or reading source replaces it
    /// wholesale, and an edit rekeys it.
    captions: Captions,
    /// Captioned sites an edit wrote at or beneath since their caption was
    /// last saved: the author should check they still describe the step.
    review: Vec<Site>,
    dialect: Dialect,
    function_labels: std::collections::BTreeMap<String, String>,
}

/// How a saved version came to be, which says what authored source and
/// captions it has.
pub(crate) enum Origin {
    /// Opened as a kernel document: it has no authored source or captions.
    Document,
    /// Imported from this source, with the captions it spells.
    Imported {
        packages: Box<runtime::Namespace>,
        dialect: Dialect,
        text: String,
        map: Option<lash::workflow::document::SourceMap>,
        captions: Captions,
    },
    /// An edit of the saved version, of its document or of its captions: it
    /// keeps that version's source, which no longer spells the version.
    Edited {
        captions: Captions,
        review: Vec<Site>,
    },
    /// Read back from printed source, with the captions it spells.
    Read {
        dialect: Dialect,
        captions: Captions,
    },
}

impl SavedWorkflow {
    async fn view(
        &self,
        core: &lash::LashCore,
        workers: &lash::vm::WorkerService,
        _functions: &FunctionRegistry,
    ) -> WorkflowView {
        let document = self.draft.document().clone();
        // Saved views link only the declarations retained by their publication.
        let derived = match &self.published {
            Ok(published) => match core
                .host_artifacts()
                .execution_document(&published.reference)
                .await
            {
                Ok(lash::workflow::WorkflowDocumentRead::Read(read)) => Ok(*read),
                other => Err(format!("stored document unavailable: {other:?}")),
            },
            Err(_) => Err("the draft has no admitted Link".into()),
        };
        let mut authored = self.authored.clone();
        if let Some(authored) = &mut authored
            && let Some(id) = &authored.source_map_id
        {
            match core.host_artifacts().read_source_map(id).await {
                Ok(lash::workflow::SourceMapRead::Read { map, source }) => {
                    authored.text = source;
                    authored.source_map = Some(map);
                }
                other => authored.map_unavailable = Some(format!("{other:?}")),
            }
        }
        let source_dialect = self.dialect;
        let lens = match source_dialect {
            Dialect::TypeScript => "generated_typescript",
            Dialect::Python => "generated_python",
        };
        let (view, view_unavailable, printed, source_unavailable) = match &derived {
            Ok(derived) => {
                let (mut view, view_unavailable) = match self
                    .published
                    .as_ref()
                    .map_err(|error| error.to_string())
                    .and_then(|published| {
                        lash::workflow::view::derive(derived, &published.context)
                            .map_err(|error| error.to_string())
                    }) {
                    Ok(view) => (Some(view), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                let options = lash::dialect::PrintOptions {
                    source_map: true,
                    ..Default::default()
                };
                let captions = Some(&self.captions);
                let format = match source_dialect {
                    Dialect::TypeScript => lash::dialect::SourceFormat::TypeScriptV1,
                    Dialect::Python => lash::dialect::SourceFormat::PythonV1,
                };
                // The worker prints with the library it reads exports back with.
                let (printed, source_unavailable, cause) = match workers
                    .print(derived.document(), captions, format, options)
                    .await
                {
                    Ok(printed) => (Some(printed), None, None),
                    Err(error) => {
                        use lash::dialect::PrintError;
                        use lash::vm::SourceServiceError;
                        use lash::workflow::view::ReadableRefusal;
                        let cause = match &error {
                            SourceServiceError::Print(PrintError::Bytes { limit }) => {
                                ReadableRefusal::Bytes { limit: *limit }
                            }
                            SourceServiceError::Print(PrintError::Depth { limit }) => {
                                ReadableRefusal::Depth { limit: *limit }
                            }
                            SourceServiceError::Print(PrintError::Encode(error)) => {
                                ReadableRefusal::Encoding {
                                    message: error.to_string(),
                                }
                            }
                            other => ReadableRefusal::Invalid {
                                message: other.to_string(),
                            },
                        };
                        (None, Some(error.to_string()), Some(cause))
                    }
                };
                if let Some(view) = &mut view {
                    if let Some(printed) = &printed {
                        if let Some(map) = &printed.source_map
                            && let Err(refusal) =
                                view.readable_ranges(&document, lens.into(), &printed.source, map)
                        {
                            view.readable = lash::workflow::view::ReadableLens::Refused {
                                name: lens.into(),
                                cause: lash::workflow::view::ReadableRefusal::Invalid {
                                    message: refusal.to_string(),
                                },
                            };
                        }
                    } else if let Some(cause) = cause {
                        view.readable = lash::workflow::view::ReadableLens::Refused {
                            name: lens.into(),
                            cause,
                        };
                    }
                }
                (view, view_unavailable, printed, source_unavailable)
            }
            Err(refusal) => (
                None,
                Some(refusal.to_string()),
                None,
                Some(refusal.to_string()),
            ),
        };
        let (caption_view, captions_unavailable) = match (&view, &derived) {
            (Some(view), Ok(derived)) => match view.captions(derived, &self.captions) {
                Ok(joined) => (Some(joined), None),
                Err(refusal) => (None, Some(refusal.to_string())),
            },
            _ => (None, None),
        };
        WorkflowView {
            version: self.version,
            entry: self.entry.clone(),
            identity: self.draft.identity().to_string(),
            definition: self
                .published
                .as_ref()
                .ok()
                .map(|published| published.definition.id.to_string()),
            not_admitted: self
                .published
                .as_ref()
                .err()
                .map(|refusal| refusal.to_string()),
            text: print_document(&document),
            view,
            view_unavailable,
            source_map: printed
                .as_ref()
                .and_then(|printed| printed.source_map.clone()),
            captions_dropped: printed
                .as_ref()
                .map(|printed| printed.dropped.clone())
                .unwrap_or_default(),
            source: printed.map(|printed| printed.source),
            source_unavailable,
            authored,
            source_dialect,
            caption_guide: match source_dialect {
                Dialect::TypeScript => lash::typescript::CAPTIONS.guide,
                Dialect::Python => lash::python::CAPTIONS.guide,
            },
            function_labels: self.function_labels.clone(),
            captions: self.captions.clone(),
            caption_view,
            captions_unavailable,
            captions_to_review: self.review.clone(),
            document,
        }
    }

    /// The captions moved through an applied edit of this version's draft
    /// to `result`, and the sites to review after it: those the edit wrote
    /// at or beneath, and those still awaiting review.
    fn rekeyed(
        &self,
        correspondence: &lash::workflow::document::Correspondence,
        result: &lash::workflow::document::Document,
    ) -> Result<(Captions, Vec<Site>), ErrorResponse> {
        let rekeyed = self
            .captions
            .rekey(correspondence, self.draft.document(), result)
            .ok_or_else(|| {
                ErrorResponse::invalid(
                    "captions",
                    "the edit does not start at the captions' document",
                )
            })?;
        let mut review: Vec<Site> = self
            .review
            .iter()
            .filter_map(|site| correspondence.successor(site))
            .filter(|site| rekeyed.captions.at(site).is_some())
            .cloned()
            .chain(rekeyed.touched)
            .collect();
        review.sort();
        review.dedup();
        Ok((rekeyed.captions, review))
    }
}

impl AppState {
    /// The example's state with the default built-in workflow saved as
    /// version 1.
    pub async fn new(runtime: WorkflowHost) -> Result<Self, StartupError> {
        let core = runtime.core;
        let environment = runtime::workflow_environment(&core, &runtime.pin)
            .await
            .map_err(|error| StartupError(error.to_string()))?;
        let catalog = catalog::Catalog::load(&core, &runtime.pin, &environment)
            .await
            .map_err(|error| StartupError(error.to_string()))?;
        let release =
            lash::vm::helper_release().map_err(|error| StartupError(error.to_string()))?;
        let function_labels = release
            .labels
            .iter()
            .map(|(id, json)| {
                let display: serde_json::Value =
                    serde_json::from_str(json).map_err(|error| StartupError(error.to_string()))?;
                let title = display
                    .as_str()
                    .or_else(|| display.get("title").and_then(serde_json::Value::as_str))
                    .filter(|title| !title.trim().is_empty())
                    .ok_or_else(|| StartupError(format!("helper {id} has no display title")))?;
                let function = release
                    .helpers
                    .registered(id)
                    .ok_or_else(|| StartupError(format!("labelled helper {id} is absent")))?;
                Ok((function.name().to_owned(), title.to_owned()))
            })
            .collect::<Result<_, StartupError>>()?;
        let state = Self {
            namespace_pin: runtime.pin,
            store: Arc::new(Mutex::new(None)),
            publishing: Arc::new(tokio::sync::Mutex::new(())),
            host: runtime.tools,
            workers: runtime.workers,
            commands: runtime::CommandClient::new(core.clone()),
            core,
            catalog: Arc::new(catalog),
            function_labels,
        };
        let opening = state
            .catalog
            .default_workflow()
            .ok_or_else(|| StartupError("the catalog has no default workflow".into()))?;
        let draft = Draft::open(opening.document.clone())
            .map_err(|error| StartupError(format!("{error:?}")))?;
        state
            .install(draft, opening.entry.clone(), opening.origin())
            .await;
        Ok(state)
    }

    #[expect(
        clippy::expect_used,
        reason = "AppState::new saves version 1 before the state is shared"
    )]
    fn current(&self) -> SavedWorkflow {
        self.store
            .lock_recover()
            .clone()
            .expect("the workflow store always has a version")
    }

    async fn environment(&self) -> Result<lash::workflow::WorkflowEnvironment, ErrorResponse> {
        runtime::workflow_environment(&self.core, &self.namespace_pin)
            .await
            .map_err(|error| ErrorResponse::invalid("environment", error))
    }

    /// Saves `draft` as the next version and publishes it: lash admits the
    /// draft's document as a definition under a pin the version holds. A
    /// draft lash refuses is still saved, as a version that cannot run.
    async fn install(
        &self,
        draft: Draft,
        entry: Name,
        origin: Origin,
    ) -> (SavedWorkflow, Option<runtime::Publication>) {
        let source = match &origin {
            Origin::Imported {
                map: Some(map),
                text,
                ..
            } => Some((map, text.as_str())),
            _ => None,
        };
        let existing_packages = match &origin {
            Origin::Imported { packages, .. } => Some((**packages).clone()),
            Origin::Edited { .. } | Origin::Read { .. } => self.current().namespace,
            Origin::Document => None,
        };
        let selected = match existing_packages {
            Some(packages) => Ok(packages),
            None => runtime::namespace(&self.core, &self.namespace_pin).await,
        };
        let packages = selected.as_ref().ok().cloned();
        let publication = match selected {
            Ok(packages) => runtime::publish(&self.core, &draft, &entry, packages, source).await,
            Err(error) => Err(error),
        };
        let (published, publication) = match publication {
            Ok(publication) => (Ok(publication.published.clone()), Some(publication)),
            Err(refusal) => (Err(Arc::new(refusal)), None),
        };
        let (saved, superseded) = {
            let mut store = self.store.lock_recover();
            let version = store.as_ref().map_or(1, |saved| saved.version + 1);
            let dialect = match &origin {
                Origin::Document => Dialect::TypeScript,
                Origin::Imported { dialect, .. } | Origin::Read { dialect, .. } => *dialect,
                Origin::Edited { .. } => store
                    .as_ref()
                    .map_or(Dialect::TypeScript, |saved| saved.dialect),
            };
            let (captions, review) = match &origin {
                Origin::Document => (Captions::empty(draft.identity()), Vec::new()),
                Origin::Imported { captions, .. } | Origin::Read { captions, .. } => {
                    (captions.clone(), Vec::new())
                }
                Origin::Edited { captions, review } => (captions.clone(), review.clone()),
            };
            let authored = match origin {
                Origin::Document | Origin::Read { .. } => None,
                Origin::Imported { dialect, text, .. } => Some(AuthoredSource {
                    dialect,
                    text,
                    version,
                    source_map_id: published
                        .as_ref()
                        .ok()
                        .and_then(|published| published.source_map),
                    source_map: None,
                    map_unavailable: None,
                }),
                Origin::Edited { .. } => store
                    .as_ref()
                    .and_then(|saved| saved.authored.clone())
                    .map(|mut authored| {
                        authored.source_map_id = None;
                        authored.source_map = None;
                        authored.map_unavailable = None;
                        authored
                    }),
            };
            let saved = SavedWorkflow {
                namespace: packages,
                version,
                draft,
                entry,
                published,
                authored,
                captions,
                review,
                dialect,
                function_labels: self.function_labels.clone(),
            };
            let superseded = store
                .replace(saved.clone())
                .and_then(|previous| previous.published.ok())
                .map(|held| held.pin);
            (saved, superseded)
        };
        // Processes already started keep the definition they were admitted
        // under; only the next start needs a pin, and that is the new one.
        if let Some(pin) = superseded
            && let Err(error) = self.core.host_artifacts().release(pin).await
        {
            eprintln!("warning: a superseded workflow version kept its pin: {error}");
        }
        (saved, publication)
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/workflows", get(list_workflows))
        .route("/environment", get(read_environment))
        .route("/workflow", get(get_workflow).post(open_workflow))
        .route("/workflow/select", post(select_workflow))
        .route("/workflow/edits", post(edit_workflow))
        .route("/workflow/source", post(authoring_http::import_source))
        .route("/workflow/plan", post(authoring_http::preview))
        .route("/workflow/captions", post(authoring_http::caption))
        .route("/run", post(run_workflow))
        .route("/approvals/{key}", post(resolve_approval))
        .route("/healthz", get(healthz))
        .route("/", get(static_index))
        .route("/{*path}", get(static_asset))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

async fn list_workflows(State(state): State<AppState>) -> Json<Vec<WorkflowCatalogEntry>> {
    Json(state.catalog.entries())
}

/// What a document must be written against to be admitted here.
async fn read_environment(
    State(state): State<AppState>,
) -> Result<Json<EnvironmentView>, ErrorResponse> {
    let environment = state.environment().await?;
    Ok(Json(EnvironmentView {
        namespaces: serde_json::to_value(environment.namespaces())
            .map_err(|error| ErrorResponse::invalid("environment", error))?,
        functions: environment
            .functions()
            .iter()
            .map(|(id, function)| (function.definition().name.to_string(), id.to_string()))
            .collect(),
    }))
}

async fn get_workflow(State(state): State<AppState>) -> Result<Json<WorkflowView>, ErrorResponse> {
    let _publishing = state.publishing.lock().await;
    let environment = state.environment().await?;
    Ok(Json(
        state
            .current()
            .view(&state.core, &state.workers, environment.functions())
            .await,
    ))
}

async fn select_workflow(
    State(state): State<AppState>,
    Json(request): Json<SelectWorkflowRequest>,
) -> Result<Json<WorkflowView>, ErrorResponse> {
    let environment = state.environment().await?;
    let example = state
        .catalog
        .get(&request.id, request.dialect)
        .ok_or_else(|| ErrorResponse::unknown_workflow(&request.id))?
        .map_err(|refusal| ErrorResponse::invalid("dialect", refusal))?;
    let entry = request.entry.unwrap_or_else(|| example.entry.clone());
    if !example.document.exports.contains_key(&entry) {
        return Err(ErrorResponse::invalid(
            "entry",
            "Select a declared workflow entry",
        ));
    }
    let draft = Draft::open(example.document.clone())
        .map_err(|error| ErrorResponse::invalid("document", format!("{error:?}")))?;
    let _publishing = state.publishing.lock().await;
    let (saved, _) = state.install(draft, entry, example.origin()).await;
    Ok(Json(
        saved
            .view(&state.core, &state.workers, environment.functions())
            .await,
    ))
}

/// Opens a workflow given as a kernel document, with the manifest this
/// host admits it under filled in.
async fn open_workflow(
    State(state): State<AppState>,
    Json(request): Json<OpenWorkflowRequest>,
) -> Result<Json<WorkflowView>, ErrorResponse> {
    let environment = state.environment().await?;
    let document = runtime::validate_document(*request.document)
        .map_err(|error| ErrorResponse::invalid("document", error))?;
    let draft = Draft::open(document)
        .map_err(|error| ErrorResponse::invalid("document", format!("{error:?}")))?;
    let _publishing = state.publishing.lock().await;
    let (saved, _) = state.install(draft, request.entry, Origin::Document).await;
    Ok(Json(
        saved
            .view(&state.core, &state.workers, environment.functions())
            .await,
    ))
}

/// Applies kernel edits to the saved draft as one transaction and
/// publishes the result. A refused transaction changes nothing and answers
/// each diagnostic with the edit and the site it is about.
async fn edit_workflow(
    State(state): State<AppState>,
    request: Result<Json<EditWorkflowRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<EditWorkflowResponse>, ErrorResponse> {
    let Json(request) = request.map_err(authoring_http::unsupported)?;
    let environment = state.environment().await?;
    let _publishing = state.publishing.lock().await;
    let current = state.current();
    if request.version != current.version {
        return Err(ErrorResponse::version_conflict(
            request.version,
            current.version,
        ));
    }
    let mut draft = current.draft.clone();
    let transaction = Transaction {
        base: draft.identity(),
        edits: request.edits,
    };
    let applied = draft
        .apply(&transaction, &environment.checker())
        .map_err(|refusal| {
            let diagnostics = refusal
                .diagnostics
                .iter()
                .map(|diagnostic| {
                    serde_json::json!({
                        "edit": diagnostic.edit,
                        "site": match &diagnostic.location {
                            Some(Location::Base(site) | Location::Edited(site)) => {
                                serde_json::to_value(site).unwrap_or_default()
                            }
                            None => serde_json::Value::Null,
                        },
                        "message": diagnostic.kind.to_string(),
                        "cause": diagnostic.kind, "location": diagnostic.location,
                    })
                })
                .collect::<Vec<_>>();
            ErrorResponse::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "edit_refused",
                "the edit transaction was refused",
                serde_json::json!({ "diagnostics": diagnostics }),
            )
        })?;
    let (captions, review) = current.rekeyed(&applied.correspondence, draft.document())?;
    let (saved, _) = state
        .install(draft, current.entry, Origin::Edited { captions, review })
        .await;
    Ok(Json(EditWorkflowResponse {
        workflow: saved
            .view(&state.core, &state.workers, environment.functions())
            .await,
        // The published document is the draft's, so the transaction's
        // correspondence is also where each node is in the definition.
        correspondence: applied.correspondence,
    }))
}

pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, app(state)).await
}

pub async fn serve_addr(addr: SocketAddr, state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve(listener, state).await
}

#[expect(
    clippy::expect_used,
    reason = "RunEvent is a serde struct, so to_string cannot fail"
)]
async fn run_workflow(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ErrorResponse> {
    let request: contract::RunRequest = if body.is_empty() {
        Default::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|error| ErrorResponse::invalid("invalid_run_request", error))?
    };
    let (tx, rx) = mpsc::channel::<Result<RunEvent, runtime::RunError>>(64);
    let key = uuid::Uuid::new_v4().to_string();
    let (version, started) = {
        let _publishing = state.publishing.lock().await;
        let saved = state.current();
        let published = saved
            .published
            .as_ref()
            .map_err(|refusal| ErrorResponse::invalid("run_preparation", refusal))?;
        if let Some(version) = request.version
            && version != saved.version
        {
            return Err(ErrorResponse::version_conflict(version, saved.version));
        }
        let started = state
            .commands
            .start(published.start_request(&key, request.inputs))
            .await;
        (saved.version, started)
    };
    let started = started.map_err(|error| ErrorResponse::invalid("run_preparation", error))?;
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
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    let approved = payload
        .get("approved")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            ErrorResponse::invalid("run_preparation", "approval requires an approved boolean")
        })?;
    let answer = state
        .core
        .completions()
        .resolve(
            &key,
            lash::Resolution::Ok(serde_json::json!({"approved": approved})),
        )
        .await
        .map_err(|error| ErrorResponse::invalid("run_preparation", error))?;
    match answer {
        lash::durable::ResolveAnswer::Resolved | lash::durable::ResolveAnswer::AlreadyResolved => {
            state.host.forget_approval(&key);
            Ok(Json(serde_json::json!({"accepted": true})))
        }
        other => Err(ErrorResponse::invalid(
            "run_preparation",
            format!("approval resolution refused: {other:?}"),
        )),
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
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("frontend")
        .join(&relative);
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type(&path))], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
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

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}
