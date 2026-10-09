//! The S38 workflow host: a host that opens, inspects, edits, publishes,
//! runs and shows workflows through `lash::workflow` and the facade alone.
//!
//! It holds no TypeScript lens. A workflow reaches it as a typed document,
//! an edit as typed IR addressed by node id and slot path, and a run is shown
//! by folding the process's one feed into the execution overlay of the
//! document the process names. The JSON its routes speak is this host's own
//! (lash defines no wire types for hosts): the typed values inside it are
//! lash's, serialized as they are.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
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
use lash::vm::ir::{Expr, WorkflowBodySlot, WorkflowNodeId, WorkflowSlotPath};
use lash::workflow::{
    WorkflowBodyRef, WorkflowCorrespondence, WorkflowCorrespondenceEntry, WorkflowDocumentRead,
    WorkflowDraft, WorkflowEdit, WorkflowEditTransaction, WorkflowEntry,
    WorkflowExecutionOverlayAccumulator, WorkflowGraph, WorkflowNodeSource,
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
        .with_declaration(ToolDeclaration::deferring())
        .with_park(lash::tools::ParkBound::UntilScopeEnd),
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
    let ledger = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&bodies)
        .with_context(|| format!("open {}", bodies.display()))?;
    let config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .channel(lash::rlm::RlmChannel::Cell)
        .build();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        config,
        Arc::new(lash::rlm::TypescriptDialect),
        &backend,
    );
    Ok(LashCore::rlm_builder(backend, factory)
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
    draft: Mutex<Option<WorkflowDraft>>,
    observers: Mutex<BTreeMap<String, Arc<Observer>>>,
}

type Host = Arc<Workflows>;

pub fn router(core: LashCore) -> Router {
    Router::new()
        .route("/workflow/draft", post(open_draft).get(read_draft))
        .route("/workflow/draft/edits", post(edit_draft))
        .route("/workflow/draft/publish", post(publish_draft))
        .route("/workflow/definition", post(read_definition))
        .route("/workflow/runs", post(start_run))
        .route("/workflow/runs/{process}", get(read_run))
        .route("/workflow/runs/{process}/output", get(run_output))
        .route("/workflow/runs/{process}/observers", post(attach_observer))
        .route("/workflow/observers/{observer}", get(read_observer))
        .with_state(Arc::new(Workflows {
            core,
            pin: HostArtifactPin::mint(),
            draft: Mutex::new(None),
            observers: Mutex::new(BTreeMap::new()),
        }))
}

fn definition_json(definition: &ProcessDefinition) -> Result<Value> {
    Ok(json!({"id": definition.id, "signature": serde_json::to_value(&definition.signature)?}))
}

/// The draft as the case reads it: its revision and its document, whose
/// node ids are how the case names the nodes of its edits.
fn draft_json(draft: &WorkflowDraft) -> Result<Value> {
    Ok(json!({
        "revision": draft.revision().to_string(),
        "graph": serde_json::to_value(draft.document())?,
    }))
}

fn source_json(source: &WorkflowNodeSource, draft: &WorkflowDraft) -> Value {
    let id = |handle| draft.node_id(handle).map(ToString::to_string);
    match source {
        WorkflowNodeSource::Authored => json!({"kind": "authored"}),
        WorkflowNodeSource::Clone { of } => json!({"kind": "clone", "of": id(*of)}),
        WorkflowNodeSource::Derived { from } => json!({"kind": "derived", "from": id(*from)}),
    }
}

/// A correspondence in this host's JSON: one entry per node, by outcome.
/// `draft` names the source node of an inserted one by its draft id.
fn correspondence_json(correspondence: &WorkflowCorrespondence, draft: &WorkflowDraft) -> Value {
    use WorkflowCorrespondenceEntry as Entry;
    let entries: Vec<Value> = correspondence
        .entries
        .iter()
        .map(|entry| match entry {
            Entry::Retained { from, to, .. } => {
                json!({"outcome": "retained", "from": from, "to": to})
            }
            Entry::Moved { from, to, .. } => json!({"outcome": "moved", "from": from, "to": to}),
            Entry::Inserted { to, source, .. } => {
                json!({"outcome": "inserted", "to": to, "source": source_json(source, draft)})
            }
            Entry::Deleted { from, .. } => json!({"outcome": "deleted", "from": from}),
            Entry::Split { from, into, .. } => json!({
                "outcome": "split",
                "from": from,
                "into": into.iter().map(|(_, id)| id).collect::<Vec<_>>(),
            }),
            Entry::Merged { from, to, .. } => json!({"outcome": "merged", "from": from, "to": to}),
            Entry::Unmatched { from, .. } => json!({"outcome": "unmatched", "from": from}),
            other => json!({"outcome": format!("{other:?}")}),
        })
        .collect();
    json!({
        "base": correspondence.base.to_string(),
        "revision": correspondence.revision.to_string(),
        "entries": entries,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum OpenDraft {
    /// A typed document, as a generator or a lens outside this host made it.
    Graph(WorkflowGraph),
    /// The admitted document of a published definition, read through lash.
    Definition(ProcessDefinitionId),
}

async fn open_draft(State(host): State<Host>, Json(request): Json<OpenDraft>) -> ApiResult {
    let graph = match request {
        OpenDraft::Graph(graph) => graph,
        OpenDraft::Definition(definition) => {
            match host
                .core
                .host_artifacts()
                .definition_graph(&definition)
                .await
                .map_err(api_error)?
            {
                WorkflowRead::Inspected(inspection) => inspection.document.graph,
                other => return Err(api_error(format!("no workflow to open: {other:?}"))),
            }
        }
    };
    let draft = WorkflowDraft::open(&graph).map_err(api_error)?;
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

/// One typed edit, naming its nodes by their ids in the draft's document.
#[derive(Deserialize)]
#[serde(tag = "op", deny_unknown_fields, rename_all = "snake_case")]
enum EditRequest {
    InsertNode {
        body: BodyRequest,
        #[serde(default)]
        before: Option<WorkflowNodeId>,
        statement: Expr,
    },
    CloneNode {
        node: WorkflowNodeId,
        body: BodyRequest,
        #[serde(default)]
        before: Option<WorkflowNodeId>,
    },
    ReplaceExpression {
        node: WorkflowNodeId,
        slot: WorkflowSlotPath,
        expression: Expr,
    },
}

/// A child body of a container node.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BodyRequest {
    node: WorkflowNodeId,
    slot: String,
}

fn typed_edit(draft: &WorkflowDraft, edit: EditRequest) -> Result<WorkflowEdit> {
    let handle = |id: &WorkflowNodeId| {
        draft
            .handle(id)
            .with_context(|| format!("the draft has no node `{id}`"))
    };
    let body = |body: &BodyRequest| {
        Ok::<_, anyhow::Error>(WorkflowBodyRef::Child {
            node: handle(&body.node)?,
            slot: match body.slot.as_str() {
                "then" => WorkflowBodySlot::Then,
                "else" => WorkflowBodySlot::Else,
                "loop_body" => WorkflowBodySlot::LoopBody,
                "try_body" => WorkflowBodySlot::TryBody,
                "catch" => WorkflowBodySlot::Catch,
                "finally" => WorkflowBodySlot::Finally,
                "scope" => WorkflowBodySlot::Scope,
                other => bail!("no body slot `{other}`"),
            },
        })
    };
    let anchor = |before: &Option<WorkflowNodeId>| before.as_ref().map(handle).transpose();
    Ok(match edit {
        EditRequest::InsertNode {
            body: place,
            before,
            statement,
        } => WorkflowEdit::InsertNode {
            body: body(&place)?,
            before: anchor(&before)?,
            statement,
        },
        EditRequest::CloneNode {
            node,
            body: place,
            before,
        } => WorkflowEdit::CloneNode {
            node: handle(&node)?,
            body: body(&place)?,
            before: anchor(&before)?,
        },
        EditRequest::ReplaceExpression {
            node,
            slot,
            expression,
        } => WorkflowEdit::ReplaceExpression {
            node: handle(&node)?,
            slot,
            expression,
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditDraft {
    edits: Vec<EditRequest>,
}

/// Apply `edits` to the draft as one transaction: all of them, or none and
/// the typed diagnostics.
async fn edit_draft(State(host): State<Host>, Json(request): Json<EditDraft>) -> ApiResult {
    let mut slot = host.draft.lock_recover();
    let draft = slot
        .as_mut()
        .context("no draft is open")
        .map_err(api_error)?;
    let edits = request
        .edits
        .into_iter()
        .map(|edit| typed_edit(draft, edit))
        .collect::<Result<Vec<_>>>()
        .map_err(api_error)?;
    match draft.apply(WorkflowEditTransaction {
        base: draft.revision(),
        edits,
    }) {
        Ok(correspondence) => Ok(Json(json!({
            "applied": true,
            "correspondence": correspondence_json(&correspondence, draft),
            "draft": draft_json(draft).map_err(api_error)?,
        }))),
        Err(refusal) => Ok(Json(json!({
            "applied": false,
            "diagnostics": refusal
                .diagnostics
                .iter()
                .map(|diagnostic| json!({
                    "edit": diagnostic.edit,
                    "code": diagnostic.kind.code(),
                    "message": diagnostic.kind.to_string(),
                }))
                .collect::<Vec<_>>(),
        }))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishDraft {
    /// The process a run of the definition starts, by the id of its
    /// container in the draft's document.
    entry: WorkflowNodeId,
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
        .publish_workflow(
            &host.pin,
            &draft,
            WorkflowEntry::Process(request.entry),
            &environment(),
        )
        .await
        .map_err(api_error)?;
    Ok(Json(match publish {
        WorkflowPublish::Published(publication) => json!({
            "published": true,
            "definition": definition_json(&publication.definition).map_err(api_error)?,
            "entry": publication.document.entry,
            "graph": serde_json::to_value(&publication.document.graph).map_err(api_error)?,
            "correspondence": correspondence_json(&publication.correspondence, &draft),
        }),
        WorkflowPublish::Refused(refusal) => json!({
            "published": false,
            "refused": refusal.to_string(),
            "diagnostics": format!("{:?}", refusal.diagnostics),
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
            "entry": inspection.document.entry,
            "graph": serde_json::to_value(&inspection.document.graph).map_err(api_error)?,
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
    let env_ref = artifacts
        .publish_process_env(&host.pin, &environment())
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
        "waits": view.process.waits().iter().map(|wait| wait.key()).collect::<Vec<_>>(),
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
        document: json!({
            "reference": serde_json::to_value(&document.reference).map_err(api_error)?,
            "entry": document.entry,
            "graph": serde_json::to_value(&document.graph).map_err(api_error)?,
        }),
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
                    ProcessObservationStreamItem::Gap { observation, gap } => {
                        // A gap retires the provisional history; the durable
                        // view it carries may already be terminal.
                        accumulator.reset_live();
                        let terminal = match &observation.read_view {
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
                            json!({"item": "gap", "cause": format!("{:?}", gap.cause)}),
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
