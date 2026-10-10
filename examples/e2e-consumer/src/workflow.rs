//! The S38 workflow host: a host that opens, inspects, edits, publishes,
//! runs and shows workflows through `lash::workflow` and the facade alone.
//!
//! It holds no source language. A workflow reaches it as a kernel document,
//! an edit as a kernel edit addressed by site, and a run is shown by folding
//! the process's one feed into the execution overlay of the document the
//! process names. The JSON its routes speak is this host's own (lash defines
//! no wire types for hosts): the documents, edits, correspondences and
//! overlays inside it are lash's, serialized as they are.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow};
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt as _;
use lash::LashCore;
use lash::process::{
    HostArtifactPin, Lifetime, ProcessAwaitOutput, ProcessDefinition, ProcessDefinitionId,
    ProcessExecutionEnvSpec, ProcessLifecycleFact, ProcessObservationStreamItem, ProcessOriginator,
    ProcessStartRequest, ProcessStartTarget, ProcessStatus,
};
use lash::sync::MutexExt as _;
use lash::workflow::document::{Document, Name};
use lash::workflow::edit::{Draft, Edit, Location, Transaction};
use lash::workflow::{
    WorkflowDocument, WorkflowDocumentRead, WorkflowExecutionOverlayAccumulator,
    WorkflowOverlaySettlement, WorkflowOverlayTerminal, WorkflowPublish, WorkflowRead,
};
use serde::Deserialize;
use serde_json::{Value, json};

type ApiResult = Result<Json<Value>, (axum::http::StatusCode, String)>;

fn api_error(error: impl std::fmt::Display) -> (axum::http::StatusCode, String) {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        error.to_string(),
    )
}

/// The environment a process of a published workflow runs under, which is
/// the environment lash admits the workflow against.
fn environment() -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        lash::plugins::AdmittedPluginConfig::default(),
        lash::runtime::SessionPolicy::new(
            lash::TurnBudget::bounded(32),
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        ),
        lash::plugins::SessionToolAccess::ambient(),
    )
}

/// The host's two tools. `review.request` defers: its process parks until
/// the case resolves the completion key the body recorded. `ledger.record`
/// answers at once. Each body appends its delivery to the case's body ledger
/// before anything else, so the case counts every entry of every call.
#[derive(Clone)]
struct Tools {
    ledger: Arc<Mutex<std::fs::File>>,
}

#[expect(
    clippy::expect_used,
    reason = "the host declares valid schemas and bindings"
)]
fn tool_definitions() -> Vec<lash::tools::ToolDefinition> {
    use lash::tools::{ToolBinding, ToolDeclaration, ToolDefinition, ToolDefinitionBindingExt};
    vec![
        ToolDefinition::raw(
            "tool:review_request",
            "review_request",
            "Park until a reviewer decides on the item.",
            json!({"type":"object","properties":{"item":{"type":"string"}},"required":["item"],"additionalProperties":false}),
            json!({"type":"object","properties":{"approved":{"type":"boolean"}},"required":["approved"],"additionalProperties":false}),
        )
        .expect("review schema")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["review"], "request"))
        .with_declaration(ToolDeclaration::deferring(), Some(lash::tools::ParkBound::UntilScopeEnd))
        .expect("a deferring tool declares its park bound"),
        ToolDefinition::raw(
            "tool:ledger_record",
            "ledger_record",
            "Record one entry and answer it.",
            json!({"type":"object","properties":{"entry":{"type":"string"}},"required":["entry"],"additionalProperties":false}),
            json!({"type":"string"}),
        )
        .expect("ledger schema")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["ledger"], "record")),
    ]
}

impl Tools {
    fn attempt(&self, call: &lash::tools::ToolCall<'_>) -> Result<lash::tools::ToolAttemptOutcome> {
        use lash::tools::{PendingCompletion, ToolAttemptOutcome, ToolOutcome};
        let deferred = call.name() == "review_request";
        let completion = deferred
            .then(|| call.context.completion_key())
            .transpose()?
            .map(|key| key.as_str().to_owned());
        let mut line = serde_json::to_vec(&json!({
            "tool": call.name(),
            "call_id": call.context.call_id().to_string(),
            "attempt": call.context.attempt_number(),
            "run": call.context.logical_run().map(|run| run.to_string()),
            "owner": serde_json::to_value(call.context.owner())?,
            "completion": completion,
            "process": call.context.enclosing_process().map(ToString::to_string),
            "args": call.args,
            "at_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        }))?;
        line.push(b'\n');
        {
            let mut file = self.ledger.lock_recover();
            file.write_all(&line)?;
            file.sync_data()?;
        }
        if deferred {
            return Ok(ToolAttemptOutcome::pending(PendingCompletion::new()));
        }
        Ok(ToolOutcome::ok(call.args["entry"].clone()).into())
    }
}

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Tools {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        match self.attempt(&call) {
            Ok(outcome) => outcome,
            Err(error) => {
                lash::tools::ToolOutcome::err_fmt(format_args!("workflow host body: {error:#}"))
                    .into()
            }
        }
    }
}

/// The workflow host's core: lash's VM process engine over `backend`, the
/// host's two tools, and `processes.start` for the processes a workflow
/// defines inline.
pub fn builder(backend: lash::Backend) -> Result<lash::LashCoreBuilder> {
    let bodies = PathBuf::from(
        std::env::var_os("E2E_CONSUMER_WORKFLOW_BODIES")
            .context("E2E_CONSUMER_WORKFLOW_BODIES is required")?,
    );
    builder_over(backend, &bodies)
}

/// [`builder`] with its body ledger at `bodies`.
fn builder_over(backend: lash::Backend, bodies: &std::path::Path) -> Result<lash::LashCoreBuilder> {
    let ledger = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(bodies)
        .with_context(|| format!("open {}", bodies.display()))?;
    let workers = lash::vm::WorkerService::default();
    let bounds = lash::vm::RunBounds {
        charge: 1_000_000,
        memory: 64 * 1024 * 1024,
        ..workers.config().run_bounds
    };
    Ok(LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers, bounds,
        )))
        .tools(Arc::new(lash::tools::StaticToolProvider::new(
            tool_definitions(),
            Tools {
                ledger: Arc::new(Mutex::new(ledger)),
            },
        )))
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash::process::lifetime::session_or_starter,
            ),
        )))
}

/// One follower of one process: the feed it attached, folded into the
/// execution overlay of the document the process names.
struct Observer {
    state: tokio::sync::watch::Sender<Observed>,
}

#[derive(Clone, Default)]
struct Observed {
    /// The document the process named when the observer attached.
    document: Value,
    overlay: Value,
    /// The feed's items, in order, as this host saw them.
    items: Vec<Value>,
    /// The committed terminal the feed delivered, or the snapshot showed.
    terminal: Option<String>,
    error: Option<String>,
}

pub struct Workflows {
    core: LashCore,
    /// What this boot publishes is held under one pin of its own.
    pin: HostArtifactPin,
    draft: Mutex<Option<Draft>>,
    observers: Mutex<BTreeMap<String, Arc<Observer>>>,
}

type Host = Arc<Workflows>;

pub fn router(core: LashCore) -> Router {
    Router::new()
        .route("/workflow/environment", get(read_environment))
        .route("/workflow/requirements", post(read_requirements))
        .route("/workflow/draft", post(open_draft).get(read_draft))
        .route("/workflow/draft/edits", post(edit_draft))
        .route("/workflow/draft/publish", post(publish_draft))
        .route("/workflow/definition", post(read_definition))
        .route("/workflow/runs", post(start_run))
        .route("/workflow/runs/{process}", get(read_run))
        .route("/workflow/runs/{process}/output", get(run_output))
        .route("/workflow/runs/{process}/observers", post(attach_observer))
        .route("/workflow/observers/{observer}", get(read_observer))
        .with_state(host(core))
}

fn host(core: LashCore) -> Host {
    Arc::new(Workflows {
        core,
        pin: HostArtifactPin::mint(),
        draft: Mutex::new(None),
        observers: Mutex::new(BTreeMap::new()),
    })
}

fn definition_json(definition: &ProcessDefinition) -> Result<Value> {
    Ok(json!({"id": definition.id, "signature": serde_json::to_value(&definition.signature)?}))
}

/// The draft as the case reads it: the identity a transaction names as its
/// base, and the document, whose sites are how the case names the nodes of
/// its edits.
fn draft_json(draft: &Draft) -> Result<Value> {
    Ok(json!({
        "identity": draft.identity(),
        "document": serde_json::to_value(draft.document())?,
    }))
}

/// An admitted document as the case reads it: what an execution names it
/// by, the document, and the execution sites lash derives from it.
fn document_json(document: &WorkflowDocument) -> Result<Value> {
    let sites = document
        .graph()
        .execution_sites()
        .iter()
        .map(|site| {
            Ok(json!({
                "site": serde_json::to_value(&site.site)?,
                "statement": serde_json::to_value(&site.statement)?,
                "kind": format!("{:?}", site.kind),
                "loops": serde_json::to_value(&site.loops)?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "reference": serde_json::to_value(document.reference())?,
        "document": serde_json::to_value(document.document())?,
        "execution_sites": sites,
    }))
}

/// What a document is written and checked against here: the effects a
/// process of it is offered, each with its signature, and the library
/// functions the workers hold, each name with its identity. Whoever writes a
/// document for this host reads the identities it references from here.
async fn read_environment(State(host): State<Host>) -> ApiResult {
    let environment = host
        .core
        .host_artifacts()
        .workflow_environment(&environment())
        .await
        .map_err(api_error)?
        .context("this core reads no workflow documents")
        .map_err(api_error)?;
    let functions: BTreeMap<String, String> = environment
        .functions()
        .iter()
        .map(|(id, function)| (function.definition.name.to_string(), id.to_string()))
        .collect();
    Ok(Json(json!({
        "effects": serde_json::to_value(environment.effects()).map_err(api_error)?,
        "functions": functions,
    })))
}

/// What `document`'s code requires of this host: the effects it performs
/// and every library function it reaches, directly or through another
/// function's body, by identity. It is what the document's manifest must
/// list to be admitted here; whoever writes the document writes it in.
async fn read_requirements(
    State(host): State<Host>,
    Json(document): Json<Box<Document>>,
) -> ApiResult {
    let environment = host
        .core
        .host_artifacts()
        .workflow_environment(&environment())
        .await
        .map_err(api_error)?
        .context("this core reads no workflow documents")
        .map_err(api_error)?;
    let required = lash::workflow::graph::requirements(&document, environment.functions());
    Ok(Json(json!({
        "effects": serde_json::to_value(&required.effects).map_err(api_error)?,
        "functions": serde_json::to_value(&required.functions).map_err(api_error)?,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum OpenDraft {
    /// A kernel document, as a generator or a front end outside this host
    /// made it.
    Document(Box<Document>),
    /// The admitted document of a published definition, read through lash.
    Definition(ProcessDefinitionId),
}

async fn open_draft(State(host): State<Host>, Json(request): Json<OpenDraft>) -> ApiResult {
    let document = match request {
        OpenDraft::Document(document) => *document,
        OpenDraft::Definition(definition) => {
            match host
                .core
                .host_artifacts()
                .definition_graph(&definition)
                .await
                .map_err(api_error)?
            {
                WorkflowRead::Inspected(inspection) => inspection.document.document().clone(),
                other => return Err(api_error(format!("no workflow to open: {other:?}"))),
            }
        }
    };
    let draft = Draft::open(document, None).map_err(api_error)?;
    let answer = draft_json(&draft).map_err(api_error)?;
    *host.draft.lock_recover() = Some(draft);
    Ok(Json(answer))
}

async fn read_draft(State(host): State<Host>) -> ApiResult {
    let draft = host.draft.lock_recover();
    let draft = draft
        .as_ref()
        .context("no draft is open")
        .map_err(api_error)?;
    Ok(Json(draft_json(draft).map_err(api_error)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditDraft {
    /// Kernel edits, as lash serializes them. Their sites are sites of the
    /// document the draft holds.
    edits: Vec<Edit>,
}

/// Apply `edits` to the draft as one transaction against the environment a
/// process of it would run under: all of them, or none and the typed
/// diagnostics.
async fn edit_draft(State(host): State<Host>, Json(request): Json<EditDraft>) -> ApiResult {
    let environment = host
        .core
        .host_artifacts()
        .workflow_environment(&environment())
        .await
        .map_err(api_error)?
        .context("this core reads no workflow documents")
        .map_err(api_error)?;
    let mut slot = host.draft.lock_recover();
    let draft = slot
        .as_mut()
        .context("no draft is open")
        .map_err(api_error)?;
    let transaction = Transaction {
        base: draft.identity(),
        edits: request.edits,
    };
    match draft.apply(&transaction, &environment.checker()) {
        Ok(applied) => Ok(Json(json!({
            "applied": true,
            "correspondence": serde_json::to_value(&applied.correspondence).map_err(api_error)?,
            "draft": draft_json(draft).map_err(api_error)?,
        }))),
        Err(refusal) => Ok(Json(json!({
            "applied": false,
            "diagnostics": refusal
                .diagnostics
                .iter()
                .map(|diagnostic| json!({
                    "edit": diagnostic.edit,
                    "site": match &diagnostic.location {
                        Some(Location::Base(site) | Location::Edited(site)) => {
                            serde_json::to_value(site).unwrap_or(Value::Null)
                        }
                        None => Value::Null,
                    },
                    "message": diagnostic.kind.to_string(),
                }))
                .collect::<Vec<_>>(),
        }))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishDraft {
    /// The entry of the draft's document a run of the definition starts.
    entry: Name,
}

/// Publish the draft as a definition under this boot's pin.
async fn publish_draft(State(host): State<Host>, Json(request): Json<PublishDraft>) -> ApiResult {
    let draft = host
        .draft
        .lock_recover()
        .clone()
        .context("no draft is open")
        .map_err(api_error)?;
    let publish = host
        .core
        .host_artifacts()
        .publish_workflow(&host.pin, &draft, &request.entry, &environment())
        .await
        .map_err(api_error)?;
    Ok(Json(match publish {
        WorkflowPublish::Published(publication) => json!({
            "published": true,
            "definition": definition_json(&publication.definition).map_err(api_error)?,
            "workflow": document_json(&publication.document).map_err(api_error)?,
            "correspondence":
                serde_json::to_value(&publication.correspondence).map_err(api_error)?,
        }),
        WorkflowPublish::Refused(refusal) => json!({
            "published": false,
            "refused": refusal.to_string(),
        }),
        WorkflowPublish::Unsupported { engine_kind } => json!({
            "published": false,
            "unsupported": engine_kind.to_string(),
        }),
    }))
}

/// A published definition as lash reads it: its identity and signature,
/// its engine, and its admitted document.
async fn read_definition(
    State(host): State<Host>,
    Json(request): Json<ReadDefinition>,
) -> ApiResult {
    Ok(Json(workflow_read_json(
        host.core
            .host_artifacts()
            .definition_graph(&request.definition)
            .await
            .map_err(api_error)?,
    )?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadDefinition {
    definition: ProcessDefinitionId,
}

fn workflow_read_json(read: WorkflowRead) -> Result<Value, (axum::http::StatusCode, String)> {
    Ok(match read {
        WorkflowRead::Inspected(inspection) => json!({
            "read": "inspected",
            "definition": definition_json(&inspection.definition).map_err(api_error)?,
            "engine_kind": inspection.engine_kind.to_string(),
            "workflow": document_json(&inspection.document).map_err(api_error)?,
        }),
        WorkflowRead::Unavailable(what) => {
            json!({"read": "unavailable", "what": format!("{what:?}")})
        }
        WorkflowRead::Unsupported { engine_kind } => {
            json!({"read": "unsupported", "engine_kind": engine_kind.to_string()})
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRun {
    definition: ProcessDefinitionId,
    /// The host start key: a second start under it is the same process.
    key: String,
    args: serde_json::Map<String, Value>,
}

/// Start a process of a published definition. The definition is named by
/// id alone: lash reads its signature, and the environment is published
/// under this boot's pin.
async fn start_run(State(host): State<Host>, Json(request): Json<StartRun>) -> ApiResult {
    let artifacts = host.core.host_artifacts();
    let definition = artifacts
        .get_definition(&request.definition)
        .await
        .map_err(api_error)?
        .with_context(|| format!("no definition {}", request.definition))
        .map_err(api_error)?;
    let environment = host
        .core
        .resolve_process_environment(environment())
        .map_err(api_error)?;
    let env_ref = artifacts
        .publish_process_env(&host.pin, &environment)
        .await
        .map_err(api_error)?;
    let receipt = host
        .core
        .processes()
        .start(
            ProcessStartRequest::new(
                ProcessStartTarget::Definition {
                    definition_id: definition.id.clone(),
                    signature_claim: Some(definition.signature.clone()),
                    args: request.args,
                },
                ProcessOriginator::host(),
                Lifetime::Detached,
            )
            .with_host_start_key(request.key)
            .with_env_ref(env_ref),
            host.core.effect_host(),
        )
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"process": receipt.process_id})))
}

fn process_id(process: String) -> Result<lash::ProcessId, (axum::http::StatusCode, String)> {
    process.parse().map_err(api_error)
}

/// The process as its row holds it, and the workflow lash reads for it.
async fn read_run(State(host): State<Host>, Path(process): Path<String>) -> ApiResult {
    let process = process_id(process)?;
    let processes = host.core.processes();
    let snapshot = processes
        .observe(&process)
        .snapshot()
        .await
        .map_err(api_error)?;
    let lash::process::ProcessReadView::Retained(view) = snapshot.read_view else {
        return Err(api_error("the process is not retained"));
    };
    let workflow = workflow_read_json(processes.graph(&process).await.map_err(api_error)?)?;
    Ok(Json(json!({
        "status": format!("{:?}", view.process.status()),
        "waits": view.process.waits().iter().map(|wait| &wait.kind).collect::<Vec<_>>(),
        "process": serde_json::to_value(&view.process).map_err(api_error)?,
        "document": match &view.document {
            lash::process::ProcessDocumentIdentity::Available(reference) => {
                serde_json::to_value(reference).map_err(api_error)?
            }
            other => json!({"unavailable": format!("{other:?}")}),
        },
        "workflow": workflow,
    })))
}

/// The process's settled output; the request waits for it.
async fn run_output(State(host): State<Host>, Path(process): Path<String>) -> ApiResult {
    let process = process_id(process)?;
    let output = host
        .core
        .processes()
        .await_output(&process)
        .await
        .map_err(api_error)?;
    Ok(Json(match output {
        ProcessAwaitOutput::Settled { output } => json!({
            "settled": true,
            "success": output.is_success(),
            "value": if output.is_success() { output.value_for_projection() } else { Value::Null },
            "output": format!("{output:?}"),
        }),
        other => json!({"settled": false, "output": format!("{other:?}")}),
    }))
}

fn settlement(status: ProcessStatus, at_ms: Option<u64>) -> Option<WorkflowOverlaySettlement> {
    let terminal = match status {
        ProcessStatus::Completed => WorkflowOverlayTerminal::Completed,
        ProcessStatus::Failed => WorkflowOverlayTerminal::Failed,
        ProcessStatus::Cancelled => WorkflowOverlayTerminal::Cancelled,
        ProcessStatus::Abandoned => WorkflowOverlayTerminal::Abandoned,
        _ => return None,
    };
    Some(WorkflowOverlaySettlement {
        terminal,
        occurred_at: at_ms
            .and_then(|at| i64::try_from(at).ok())
            .and_then(chrono::DateTime::from_timestamp_millis),
    })
}

/// Attach a new follower to `process` now: read its snapshot, follow its
/// one recovering feed from the snapshot's cursor, read the document the
/// snapshot names and fold what the feed delivers into that document's
/// overlay. The follower keeps folding after this request answers.
async fn attach_observer(State(host): State<Host>, Path(process): Path<String>) -> ApiResult {
    let process = process_id(process)?;
    let observed = host.core.processes().observe(&process);
    let snapshot = observed.snapshot().await.map_err(api_error)?;
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    let lash::process::ProcessReadView::Retained(view) = snapshot.read_view else {
        return Err(api_error("the process is not retained"));
    };
    let lash::process::ProcessDocumentIdentity::Available(reference) = &view.document else {
        return Err(api_error(format!(
            "the process names no document: {:?}",
            view.document
        )));
    };
    let document = match host
        .core
        .host_artifacts()
        .execution_document(reference)
        .await
        .map_err(api_error)?
    {
        WorkflowDocumentRead::Read(document) => *document,
        other => return Err(api_error(format!("the document does not read: {other:?}"))),
    };
    let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
    accumulator.set_document(document.overlay_document());
    let mut state = Observed {
        document: document_json(&document).map_err(api_error)?,
        ..Observed::default()
    };
    if let Some(settled) = settlement(
        view.process.status(),
        view.process.lifecycle.terminal_at_ms(),
    ) {
        accumulator.settle(settled);
        state.terminal = Some(format!("{:?}", settled.terminal));
    }
    state.overlay = serde_json::to_value(accumulator.snapshot()).map_err(api_error)?;
    let attached = json!({
        "status": format!("{:?}", view.process.status()),
        "document": state.document,
        "overlay": state.overlay,
    });
    let (sender, _) = tokio::sync::watch::channel(state);
    let observer = Arc::new(Observer { state: sender });
    let id = {
        let mut observers = host.observers.lock_recover();
        let id = format!("observer-{}", observers.len() + 1);
        observers.insert(id.clone(), observer.clone());
        id
    };
    tokio::spawn(async move {
        let outcome: Result<()> = async {
            while observer.state.borrow().terminal.is_none() {
                let item = feed
                    .next()
                    .await
                    .ok_or_else(|| anyhow!("the process feed ended"))??;
                let (record, terminal) = match item {
                    ProcessObservationStreamItem::Event(event) => match &event.payload {
                        lash::process::ProcessObservationEventPayload::LanguageExecution(
                            observation,
                        ) => {
                            accumulator.observe(observation)?;
                            (
                                json!({"item": "language", "execution": serde_json::to_value(&observation.execution)?}),
                                None,
                            )
                        }
                        lash::process::ProcessObservationEventPayload::StepBodyStarted(
                            observation,
                        ) => {
                            accumulator.step_body_started(observation)?;
                            (
                                json!({"item": "step_body_started", "step": serde_json::to_value(&observation.step)?}),
                                None,
                            )
                        }
                        lash::process::ProcessObservationEventPayload::Committed { event } => {
                            let terminal = match &event.fact {
                                ProcessLifecycleFact::Terminal { outcome, .. } => {
                                    settlement(outcome.status().into(), Some(event.occurred_at_ms))
                                }
                                _ => None,
                            };
                            if let Some(settled) = terminal {
                                accumulator.settle(settled);
                            }
                            (
                                json!({"item": "committed", "sequence": event.sequence, "fact": format!("{:?}", event.fact)}),
                                terminal.map(|settled| format!("{:?}", settled.terminal)),
                            )
                        }
                    },
                    ProcessObservationStreamItem::Gap { replacement, .. } => {
                        let cause = format!("{replacement:?}");
                        let read_view = replacement.into_read_view();
                        // A gap retires the provisional history; the durable
                        // view it carries may already be terminal.
                        accumulator.reset_live();
                        let terminal = match &read_view {
                            lash::process::ProcessReadView::Retained(view) => settlement(
                                view.process.status(),
                                view.process.lifecycle.terminal_at_ms(),
                            ),
                            _ => None,
                        };
                        if let Some(settled) = terminal {
                            accumulator.settle(settled);
                        }
                        (
                            json!({"item": "gap", "cause": cause}),
                            terminal.map(|settled| format!("{:?}", settled.terminal)),
                        )
                    }
                };
                let overlay = serde_json::to_value(accumulator.snapshot())?;
                observer.state.send_modify(|state| {
                    state.items.push(record);
                    state.overlay = overlay;
                    if terminal.is_some() {
                        state.terminal = terminal;
                    }
                });
            }
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            observer
                .state
                .send_modify(|state| state.error = Some(format!("{error:#}")));
        }
    });
    Ok(Json(json!({"observer": id, "attached": attached})))
}

#[derive(Deserialize)]
struct ReadObserver {
    /// `terminal`: answer once the follower saw the process end.
    #[serde(default)]
    until: Option<String>,
}

/// What a follower has folded so far.
async fn read_observer(
    State(host): State<Host>,
    Path(observer): Path<String>,
    Query(query): Query<ReadObserver>,
) -> ApiResult {
    let observer = host
        .observers
        .lock_recover()
        .get(&observer)
        .cloned()
        .with_context(|| format!("no observer {observer}"))
        .map_err(api_error)?;
    let mut states = observer.state.subscribe();
    let state = if query.until.as_deref() == Some("terminal") {
        states
            .wait_for(|state| state.terminal.is_some() || state.error.is_some())
            .await
            .map_err(api_error)?
            .clone()
    } else {
        states.borrow().clone()
    };
    Ok(Json(json!({
        "document": state.document,
        "overlay": state.overlay,
        "items": state.items,
        "terminal": state.terminal,
        "error": state.error,
    })))
}

#[cfg(test)]
#[expect(clippy::panic, reason = "test module: these laws fail by panicking")]
mod tests {
    use super::*;
    use lash::workflow::document::{Action, Expr, Literal, Node, Site, Stmt, Unit, parse_document};
    use lash::workflow::edit::Position;

    /// The workflow S38 runs, as a kernel document whose library functions
    /// are named and resolved against the host's environment.
    const ORDER_REVIEW: &str = include_str!("workflow/order_review.kernel");
    const ENTRY: &str = "order_review";

    async fn workflow_host(bodies: &std::path::Path) -> Host {
        let stores = Arc::new(
            lash::sqlite::SqliteStoreSet::memory()
                .await
                .expect("open a SQLite memory store set"),
        );
        let backend = lash::durable::DurableBackendBuilder::new(stores)
            .build()
            .expect("the durable backend builds");
        let core = builder_over(backend, bodies)
            .expect("the workflow host's builder")
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("workflow-host"),
                lash::persistence::LeaseIncarnationId::new("workflow-host-boot"),
            ))
            .expect("the core builds");
        host(core)
    }

    fn answered(answer: ApiResult) -> Value {
        match answer {
            Ok(Json(value)) => value,
            Err((status, message)) => panic!("the route answers: {status} {message}"),
        }
    }

    /// The fixture as a document this host admits: each `@{name}` is the
    /// identity the host's environment gives that library function, each
    /// effect carries the signature the host offers it under, and the
    /// manifest lists every function the host says the code reaches.
    async fn fixture(host: &Host) -> Document {
        let environment = answered(read_environment(State(host.clone())).await);
        let mut text = ORDER_REVIEW.to_owned();
        for (name, id) in environment["functions"].as_object().expect("functions") {
            text = text.replace(
                &format!("@{{{name}}}"),
                &format!("@{}", id.as_str().expect("an identity")),
            );
        }
        let mut document = parse_document(&text).expect("the fixture parses");
        for (effect, signature) in &mut document.manifest.effects {
            *signature = serde_json::from_value(environment["effects"][effect.to_string()].clone())
                .unwrap_or_else(|error| panic!("the host offers `{effect}`: {error}"));
        }
        let required = answered(
            read_requirements(State(host.clone()), Json(Box::new(document.clone()))).await,
        );
        document.manifest.functions = serde_json::from_value(required["functions"].clone())
            .expect("the functions the document reaches");
        document
    }

    /// Every site of the entry's body whose node `wanted` accepts, in
    /// document order.
    fn sites(document: &Document, wanted: impl Fn(Node<'_>) -> bool) -> Vec<Site> {
        fn walk(
            node: Node<'_>,
            site: Site,
            wanted: &impl Fn(Node<'_>) -> bool,
            found: &mut Vec<Site>,
        ) {
            if wanted(node) {
                found.push(site.clone());
            }
            for (index, child) in (0u32..).zip(node.children()) {
                walk(child, site.child(index), wanted, found);
            }
        }
        let entry = Name::new(ENTRY);
        let mut found = Vec::new();
        walk(
            Node::Block(&document.functions[&entry].body),
            Site::new(Unit::Function(entry), Vec::new()),
            &wanted,
            &mut found,
        );
        found
    }

    fn one(document: &Document, what: &str, wanted: impl Fn(Node<'_>) -> bool) -> Site {
        let found = sites(document, wanted);
        let [site] = found.as_slice() else {
            panic!("the entry has one {what}: {found:?}");
        };
        site.clone()
    }

    fn is_text(node: Node<'_>, text: &str) -> bool {
        matches!(node, Node::Expr(Expr::Literal(Literal::Text(value))) if value == text)
    }

    fn performs(node: Node<'_>, effect: &str) -> bool {
        matches!(node, Node::Action(Action::Perform { effect: performed, .. })
            if performed.to_string() == effect)
    }

    /// The statements of `main { <text> }`, as kernel text spells them.
    fn statements(text: &str) -> Vec<Stmt> {
        parse_document(&format!(
            "kernel 1\nnumbers float\neffect ledger.record(input: Any) -> Any\n\nmain {{\n{text}\n}}\n"
        ))
        .expect("the statements parse")
        .main
    }

    fn expression(text: &str) -> Expr {
        let Ok([Stmt::Finish { value }]) =
            <[Stmt; 1]>::try_from(statements(&format!("finish {text}")))
        else {
            panic!("`finish {text}` is one statement");
        };
        value
    }

    /// The four edits S38 makes: inside the `try` region one more recorded
    /// entry and another final status; inside the inner loop a higher review
    /// threshold and every line recorded twice.
    fn edits(document: &Document) -> Vec<Edit> {
        let region = one(document, "try", |node| {
            matches!(node, Node::Stmt(Stmt::Try(_)))
        });
        let inner = sites(document, |node| {
            matches!(node, Node::Stmt(Stmt::For { .. }))
        })
        .pop()
        .expect("the inner loop");
        let inner_body = inner.child(1);
        let record = sites(document, |node| {
            matches!(node, Node::Stmt(Stmt::Do { action }) if performs(Node::Action(action), "ledger.record"))
        })
        .into_iter()
        .find(|site| site.path.starts_with(&inner_body.path))
        .expect("the inner loop records");
        let [bind, perform] = statements(
            "let signed = {entry: \"signed-off\"}\ndo perform ledger.record(signed) as Any",
        )
        .try_into()
        .expect("two statements");
        vec![
            Edit::InsertStatement {
                at: Position::end(region.child(0)),
                statement: bind,
            },
            Edit::InsertStatement {
                at: Position::end(region.child(0)),
                statement: perform,
            },
            Edit::ReplaceExpression {
                expression: one(document, "status literal", |node| is_text(node, "reviewed")),
                with: expression("\"signed-off\""),
            },
            Edit::ReplaceExpression {
                expression: one(document, "threshold literal", |node| {
                    matches!(node, Node::Expr(Expr::Literal(Literal::Int(_))))
                }),
                with: expression("3"),
            },
            Edit::CloneStatement {
                statement: record,
                to: Position::end(inner_body),
            },
        ]
    }

    fn order() -> Value {
        json!({"groups": [
            {"name": "a", "lines": [{"sku": "a1", "qty": 5}, {"sku": "a2", "qty": 1}]},
            {"name": "b", "lines": [{"sku": "b1", "qty": 3}, {"sku": "b2", "qty": 4}]},
            {"name": "c", "lines": [{"sku": "c1", "qty": 1}, {"sku": "c2", "qty": 6}]},
        ]})
    }

    fn bodies(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("the body ledger reads")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a ledger line"))
            .collect()
    }

    /// Run `definition` over the order to its end, approving each review as
    /// its body records it, with a follower attached from the start.
    async fn run(
        host: &Host,
        bodies_at: &std::path::Path,
        definition: &Value,
        key: &str,
    ) -> (String, Value, Value) {
        let mut args = serde_json::Map::new();
        args.insert("order".to_owned(), order());
        let started = answered(
            start_run(
                State(host.clone()),
                Json(StartRun {
                    definition: serde_json::from_value(definition["id"].clone())
                        .expect("a definition id"),
                    key: key.to_owned(),
                    args,
                }),
            )
            .await,
        );
        let process = started["process"].as_str().expect("a process").to_owned();
        let attached = answered(attach_observer(State(host.clone()), Path(process.clone())).await);
        let observer = attached["observer"]
            .as_str()
            .expect("an observer")
            .to_owned();
        let output = tokio::spawn(run_output(State(host.clone()), Path(process.clone())));
        let mut approved = std::collections::BTreeSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !output.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "the {key} run settles"
            );
            for body in bodies(bodies_at) {
                let Some(completion) = body["completion"].as_str() else {
                    continue;
                };
                if body["process"] == process.as_str() && approved.insert(completion.to_owned()) {
                    host.core
                        .completions()
                        .resolve(completion, lash::Resolution::Ok(json!({"approved": true})))
                        .await
                        .expect("the review resolves");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let output = answered(output.await.expect("the output request"));
        let observed = answered(
            read_observer(
                State(host.clone()),
                Path(observer),
                Query(ReadObserver {
                    until: Some("terminal".to_owned()),
                }),
            )
            .await,
        );
        (process, output, observed)
    }

    /// FIG-5757: the workflow host works on kernel documents alone. It admits
    /// a document written against its environment, reads the definition back,
    /// applies kernel edits inside the `try` region and the inner loop as one
    /// transaction, publishes the edited document as a second definition,
    /// and runs both: each run ends as its own document says, and a follower
    /// folds each run's feed into an overlay of that document's sites.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_workflow_host_admits_edits_republishes_runs_and_shows_a_kernel_document() {
        let directory = tempfile::tempdir().expect("a body ledger directory");
        let bodies_at = directory.path().join("bodies.jsonl");
        let host = workflow_host(&bodies_at).await;
        let document = fixture(&host).await;

        answered(
            open_draft(
                State(host.clone()),
                Json(OpenDraft::Document(Box::new(document.clone()))),
            )
            .await,
        );
        let publish = || async {
            answered(
                publish_draft(
                    State(host.clone()),
                    Json(PublishDraft {
                        entry: Name::new(ENTRY),
                    }),
                )
                .await,
            )
        };
        let first = publish().await;
        assert_eq!(
            first["published"], true,
            "the document is admitted: {first}"
        );
        let read = answered(
            read_definition(
                State(host.clone()),
                Json(ReadDefinition {
                    definition: serde_json::from_value(first["definition"]["id"].clone())
                        .expect("a definition id"),
                }),
            )
            .await,
        );
        assert_eq!(read["read"], "inspected");
        assert_eq!(read["workflow"], first["workflow"]);
        assert_eq!(
            read["workflow"]["document"],
            serde_json::to_value(&document).expect("the document serializes")
        );

        let edited = answered(
            edit_draft(
                State(host.clone()),
                Json(EditDraft {
                    edits: edits(&document),
                }),
            )
            .await,
        );
        assert_eq!(edited["applied"], true, "the edits apply: {edited}");
        let second = publish().await;
        assert_eq!(
            second["published"], true,
            "the edited document is admitted: {second}"
        );
        assert_ne!(second["definition"]["id"], first["definition"]["id"]);
        assert_ne!(
            second["workflow"]["reference"]["document"],
            first["workflow"]["reference"]["document"]
        );

        for (key, publication, status, approved, skipped, recorded) in [
            (
                "generated",
                &first,
                "reviewed",
                json!(["a/a1", "b/b1", "b/b2", "c/c2"]),
                json!(["a2", "c1"]),
                6,
            ),
            (
                "edited",
                &second,
                "signed-off",
                json!(["a/a1", "b/b2", "c/c2"]),
                json!(["a2", "b1", "c1"]),
                13,
            ),
        ] {
            let (process, output, observed) =
                run(&host, &bodies_at, &publication["definition"], key).await;
            assert_eq!(
                output["value"],
                json!({
                    "status": status,
                    "approved": approved,
                    "skipped": skipped,
                    "audited": {"status": status},
                }),
                "the {key} run ends as its own document says: {output}"
            );
            let records = bodies(&bodies_at)
                .into_iter()
                .filter(|body| {
                    body["process"] == process.as_str() && body["tool"] == "ledger_record"
                })
                .count();
            assert_eq!(
                records, recorded,
                "the {key} run records what its document says"
            );
            assert!(
                observed["error"].is_null() && observed["terminal"] == "Completed",
                "the follower follows the {key} run to its end: {observed}"
            );
            let overlay = &observed["overlay"];
            assert_eq!(
                overlay["document"]["reference"],
                publication["workflow"]["reference"]
            );
            assert!(
                overlay["status"] == "completed" && overlay["mismatches"] == json!([]),
                "the {key} overlay settles on its document's sites: {overlay}"
            );
            // The overlay lists only execution sites of the run's document,
            // and the review site shows one occurrence for each line the
            // document's threshold reviews.
            let ran: Document = serde_json::from_value(publication["workflow"]["document"].clone())
                .expect("the published document");
            let review =
                serde_json::to_value(one(&ran, "review", |node| performs(node, "review.request")))
                    .expect("a site");
            let listed = overlay["sites"].as_array().expect("the overlay's sites");
            let known = publication["workflow"]["execution_sites"]
                .as_array()
                .expect("the document's execution sites");
            assert!(
                listed
                    .iter()
                    .all(|row| known.iter().any(|site| site["site"] == row["site"]["site"])),
                "the {key} overlay lists only its document's sites: {listed:?}"
            );
            let reviews = approved.as_array().map_or(0, Vec::len);
            let row = listed
                .iter()
                .find(|row| row["site"]["site"] == review)
                .expect("the overlay lists the review site");
            assert!(
                row["status"] == "completed"
                    && row["occurrence"] == reviews - 1
                    && row["summary"]["terminal_count"] == reviews,
                "the {key} review site ran once for each reviewed line: {row}"
            );
        }
    }
}
