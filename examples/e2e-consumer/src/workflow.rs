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
    EngineLaunch, HostArtifactPin, Lifetime, ProcessAwaitOutput, ProcessComponentEntry,
    ProcessConfigKey, ProcessDefinition, ProcessDefinitionId, ProcessEnv, ProcessLifecycleFact,
    ProcessObservationStreamItem, ProcessOriginator, ProcessStartRequest, ProcessStatus,
};
use lash::sync::MutexExt as _;
use lash::workflow::authoring::Operation;
use lash::workflow::document::{Document, Name};
use lash::workflow::edit::{Draft, Location, Transaction};
use lash::workflow::{
    WorkflowDocument, WorkflowDocumentRead, WorkflowExecutionOverlayAccumulator,
    WorkflowOverlaySettlement, WorkflowOverlayTerminal, WorkflowRead,
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

/// The process component that contributes the host's tools to a workflow
/// process: its config is `()`.
const HOST_TOOLS: ProcessConfigKey<()> =
    ProcessConfigKey::new("workflow-host-tools", lash::plugins::FormatVersion::ONE);

/// The environment a process of a published workflow runs under, which is
/// the environment lash admits the workflow against: the host's tools, from
/// the one component that provides them, and `processes.start` and
/// `processes.await` from the process controls, for the processes a workflow
/// defines inline.
fn environment() -> Result<ProcessEnv> {
    Ok(ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(HOST_TOOLS, &())
        .component(lash::process_controls::PROCESS_CONTROLS_CONFIG, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()?)
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
    use lash::tools::{ToolDeclaration, ToolDefinition};
    vec![
        ToolDefinition::raw(
            "tool:review_request",
            "review.request",
            "Park until a reviewer decides on the item.",
            json!({"type":"object","properties":{"item":{"type":"string"}},"required":["item"],"additionalProperties":false}),
            json!({"type":"object","properties":{"approved":{"type":"boolean"}},"required":["approved"],"additionalProperties":false}),
        )
        .expect("review schema")
        .with_execution(std::time::Duration::from_secs(120))
        .with_declaration(ToolDeclaration::deferring(), Some(lash::tools::ParkBound::UntilScopeEnd))
        .expect("a deferring tool declares its park bound"),
        ToolDefinition::raw(
            "tool:ledger_record",
            "ledger.record",
            "Record one entry and answer it.",
            json!({"type":"object","properties":{"entry":{"type":"string"}},"required":["entry"],"additionalProperties":false}),
            json!({"type":"string"}),
        )
        .expect("ledger schema")
        .with_execution(std::time::Duration::from_secs(120)),
    ]
}

impl Tools {
    fn attempt(&self, call: &lash::tools::ToolCall<'_>) -> Result<lash::tools::ToolAttemptOutcome> {
        use lash::tools::{PendingCompletion, ToolAttemptOutcome, ToolOutcome};
        let deferred = call.name() == "review.request";
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
        charge_limit: 1_000_000,
        memory: 64 * 1024 * 1024,
        ..workers.config().run_bounds
    };
    let tools: Arc<dyn lash::tools::ToolProvider> = Arc::new(lash::tools::StaticToolProvider::new(
        tool_definitions(),
        Tools {
            ledger: Arc::new(Mutex::new(ledger)),
        },
    ));
    let packages = Arc::new(lash::package::ToolPackageProvider::new(
        HOST_TOOLS.component(),
        lash::tool_provider_key!("tools"),
        tool_definitions()
            .iter()
            .map(|tool| tool.manifest.operation.package.as_slice()[0].clone())
            .collect(),
        tools.clone(),
    ));
    let host_tools = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(HOST_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("tools"), Arc::clone(&tools))
            .with_package_provider(lash::tool_provider_key!("tools"), packages),
    )
    .as_component(HOST_TOOLS.format())?;
    // A process a turn starts runs the turn's recorded model and tools,
    // with the host's tool component and nothing else; a process a workflow
    // starts inline runs under its starter's environment.
    let environment = lash::process::env::policy::from_run(|_| {
        Ok(vec![ProcessComponentEntry::new(HOST_TOOLS, &(), [])?])
    });
    let nested = lash::process::env::policy::restrict_or(lash::process::env::policy::refuse(
        "the process controls start inside a process only",
    ));
    Ok(LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            lash::vm::TYPESCRIPT_DIALECT,
            bounds,
        )))
        .tools(lash::tool_provider_key!("tools"), tools)
        .process_component(Arc::new(host_tools), HOST_TOOLS)
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lash::process::lifetime::session_or_starter,
                nested,
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash::process::lifetime::session_or_starter,
                environment,
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
async fn namespace(
    host: &Host,
) -> Result<(
    lash::package::RecordedPackages,
    lash::workflow::WorkflowEnvironmentRequest,
)> {
    use lash::package::*;
    let context = PublicationContext {
        domain: PublicationDomain(host.core.backend().binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("e2e-workflows".into()),
        root: Segment::new("workflow")?,
    };
    let packages = host
        .core
        .host_artifacts()
        .process_namespace(&host.pin, &environment()?, context.clone())
        .await?;
    let attachment = Attachment {
        members: Default::default(),
        deferred: Default::default(),
        packages: packages
            .snapshot
            .advertisements
            .iter()
            .filter_map(|accepted| match &accepted.advertisement {
                PackageAdvertisement::Enumerated(package) => Some(package.path.clone()),
                _ => None,
            })
            .collect(),
    };
    let request = lash::workflow::WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment,
    };
    Ok((packages, request))
}

async fn read_environment(State(host): State<Host>) -> ApiResult {
    let environment = host
        .core
        .host_artifacts()
        .workflow_environment(&namespace(&host).await.map_err(api_error)?.1)
        .await
        .map_err(api_error)?
        .context("this core reads no workflow documents")
        .map_err(api_error)?;
    let functions: BTreeMap<String, String> = environment
        .functions()
        .iter()
        .map(|(id, function)| (function.definition().name.to_string(), id.to_string()))
        .collect();
    Ok(Json(json!({
        "namespaces": serde_json::to_value(environment.namespaces()).map_err(api_error)?,
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
        .workflow_environment(&namespace(&host).await.map_err(api_error)?.1)
        .await
        .map_err(api_error)?
        .context("this core reads no workflow documents")
        .map_err(api_error)?;
    let required = lash::workflow::graph::requirements(&document, environment.functions());
    Ok(Json(json!({
        "imports": serde_json::to_value(&required.imports).map_err(api_error)?,
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
    let draft = Draft::open(document).map_err(api_error)?;
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
    edits: Vec<Operation>,
}

/// Apply `edits` to the draft as one transaction against the environment a
/// process of it would run under: all of them, or none and the typed
/// diagnostics.
async fn edit_draft(State(host): State<Host>, Json(request): Json<EditDraft>) -> ApiResult {
    let environment = host
        .core
        .host_artifacts()
        .workflow_environment(&namespace(&host).await.map_err(api_error)?.1)
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
                    "message": diagnostic.kind.to_string(), "cause": diagnostic.kind, "location": diagnostic.location,
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
    let (_, namespace) = namespace(&host).await.map_err(api_error)?;
    let start = host
        .core
        .host_artifacts()
        .publish_workflow(
            &host.pin,
            &lash::workflow::document::DocumentBytes::from(draft.document().clone()),
            &lash::package::StartSelector::Export(request.entry),
            &namespace,
            None,
        )
        .await
        .map_err(api_error)?;
    let descriptor = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked { start },
    })
    .map_err(api_error)?;
    let definition = host
        .core
        .host_artifacts()
        .publish_definition(&host.pin, &descriptor)
        .await
        .map_err(api_error)?;
    let WorkflowRead::Inspected(inspection) = host
        .core
        .host_artifacts()
        .definition_graph(&definition.id)
        .await
        .map_err(api_error)?
    else {
        return Err(api_error("published workflow cannot be inspected"));
    };
    Ok(Json(
        json!({ "published": true, "definition": definition_json(&definition).map_err(api_error)?,
            "workflow": document_json(&inspection.document).map_err(api_error)?,
            "correspondence": serde_json::to_value(draft.correspondence_since_open()).map_err(api_error)?,
        }),
    ))
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
    let (packages, _) = namespace(&host).await.map_err(api_error)?;
    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&definition.id)
        .await
        .map_err(api_error)?
    else {
        return Err(api_error("definition unavailable"));
    };
    let env = artifacts
        .link_process_env(
            &environment().map_err(api_error)?,
            inspection.document.reference(),
            &packages,
            &BTreeMap::new(),
        )
        .await
        .map_err(api_error)?;
    let env_ref = artifacts
        .publish_process_env(&host.pin, &env)
        .await
        .map_err(api_error)?;
    // The run states its kernel settings: this build's dialect and bounds.
    let settings = host
        .core
        .processes()
        .engine_defaults(lash::process::LASH_VM_ENGINE_KIND, &Value::Null)
        .map_err(api_error)?;
    let receipt = host
        .core
        .processes()
        .start(
            ProcessStartRequest::definition(
                definition.id.clone(),
                Some(definition.signature.clone()),
                request.args,
                EngineLaunch::new(env_ref, settings),
                ProcessOriginator::host(),
                Lifetime::Detached,
            )
            .with_host_start_key(request.key),
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
        "process": serde_json::to_value(view.process.projection()).map_err(api_error)?,
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
        WorkflowDocumentRead::Read(read) => *read,
        other => return Err(api_error(format!("the document does not read: {other:?}"))),
    };
    let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
    accumulator.set_document(lash::workflow::workflow_overlay_document(&document));
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
mod tests {
    use super::*;

    fn bodies(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("the body ledger reads")
            // The writer can still be appending the last record. Its newline
            // publishes the complete line; the next read picks up a fragment.
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str(line).expect("a ledger line"))
            .collect()
    }

    /// FIG-5854: a concurrent ledger read sees only newline-terminated
    /// records; an append's trailing fragment becomes visible when complete.
    #[test]
    fn a_body_ledger_read_waits_for_the_append_newline() {
        let directory = tempfile::tempdir().expect("a body ledger directory");
        let path = directory.path().join("bodies.jsonl");
        let mut file = std::fs::File::create(&path).expect("create the ledger");
        let first = json!({"entry": "first"});
        let second = json!({"entry": "second"});
        let partial = b"{\"entry\":\"first\"}\n{\"entry\":\"sec";
        file.write_all(partial).expect("append a partial record");
        println!(
            "actual ledger snapshot: {:?}; expected completed snapshot: {:?}",
            std::str::from_utf8(partial).expect("UTF-8 ledger"),
            "{\"entry\":\"first\"}\n{\"entry\":\"second\"}\n"
        );
        assert_eq!(bodies(&path), vec![first.clone()]);
        file.write_all(b"ond\"}").expect("complete the JSON value");
        assert_eq!(bodies(&path), vec![first.clone()]);
        file.write_all(b"\n").expect("publish the record's newline");
        assert_eq!(bodies(&path), vec![first, second]);
    }
}
