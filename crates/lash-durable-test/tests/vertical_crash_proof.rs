//! V0 (FIG-5170): the vertical crash proof, ADR 0132 §15's first kill
//! criterion.
//!
//! One session holds one admitted input. Its turn runs on the production
//! session activation over the production durable store, on simulated nodes
//! A and B: a scripted model answers first with a TypeScript code cell that
//! calls the host operation `ext.write(x)`, declared `Once`, and then with a
//! final answer that uses the cell's result. The body of `ext.write` writes
//! to an [`ExternalWorld`] that survives every node, as the outside world
//! would.
//!
//! The matrix cuts the uncut run at every labelled write, under fail-before,
//! ack-hidden, zombie, abort and commit-then-abort, recovers on the other
//! node, and checks the laws:
//!
//! - P1: every call reached the outside world at most once, and every
//!   admitted body was entered at most once;
//! - P2 (K1, abort at `round.outcome`): the operation's outcome is
//!   `Interrupted`, the cell received it, and the turn committed;
//! - P3 (K2, commit-then-abort at `round.outcome`): the saved `Completed`
//!   value reached the cell, the body ran once;
//! - P4: no hidden replay: the cell's program is entered fresh only before its
//!   first snapshot commits, at most one checkpoint restore, no outcome lookup
//!   for re-running code and no committed ordinal emitted again;
//! - P5: a zombie's writes after its reap are refused;
//! - P6: every admitted operation is named by the snapshot that issued it;
//! - P7: the session head advanced exactly once.

// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::{EffectId, Response};
use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::session::{
    AdmittedInputs, CodeCell, SessionActivation, TurnCommit, TurnDone, TurnDrive, TurnError,
    TurnRow, TurnServices, admit_mail,
};
use lash_core::sansio::{ChatContextProjector, PendingToolCall, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, Message, MessageRole, Part,
    ProtocolTurnOptions, TurnMachine, TurnMachineConfig, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, BodyOutput, CompletedCall, ExecutionDraft, MemberBody, MemberPin,
    PolicyView, Recovery, RoundTools,
};
use lash_core_execution::{ActorContext, Backend};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    AttemptOutcome, MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::{ExecKey, RunRecordKind};
use lash_durable::runner::Activation;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DomainWrite, DurableError, DurableStore, FormatSet,
    LeaseConfig, MailTx,
};
use lash_durable_test::{
    Cut, Fault, Life, Matrix, Scenario, Script, SimClock, SimNodes, SimNodesConfig, Stored,
    Tripwire, Verdict, WriteKind,
};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{ExecutionLimit, ExecutionPolicy, SessionId, ToolCallId, ToolId, TurnId};
use lash_vm_broker::cell::{Cell, CellEnd, CellOperations, ResolvedOperation, run_cell};
use lash_vm_broker::{Checkpoint, CodeCallIdentities};
use lashlang::{ExecutionHostError, ResourceOperation, Value};

use dialect::Dialect;

const FORMATS: &str = "v0";
const SESSION: &str = "v0-session";
const RUN: &str = "v0-turn";
const TOOL: &str = "ext_write";

/// The cell the model writes: one `Once` host operation, then a result
/// that carries what the operation answered.
const CELL: &str = "const written = await ext.write({ x: 7 });\nfinish({ written });";
const CELL_PREFIX: &str = "CELL:";
const RESULT_PREFIX: &str = "cell result: ";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// The outside world: what `ext.write` wrote, per call. It survives every
/// node, so a write a crash cannot undo is visible to the laws.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<BTreeMap<ToolCallId, Vec<serde_json::Value>>>,
}

impl ExternalWorld {
    fn write(&self, call: &ToolCallId, value: serde_json::Value) {
        self.writes
            .lock_recover()
            .entry(call.clone())
            .or_default()
            .push(value);
    }

    fn writes(&self) -> BTreeMap<ToolCallId, Vec<serde_json::Value>> {
        self.writes.lock_recover().clone()
    }
}

/// A material reference to an operation's payload: journal-local, named by a
/// digest of its bytes.
fn material(role: MaterialRole, payload: &str) -> MaterialRef {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut hasher);
    MaterialRef {
        owner: MaterialOwner::Run {
            opener: EffectOpener::turn(session(), run()),
        },
        role,
        location: MaterialLocation::JournalLocal,
        digest: MaterialDigest::parse(&format!("{:064x}", hasher.finish())).unwrap(),
    }
}

/// `ext.write`: a `Once` host operation whose body writes to the outside
/// world and answers what it wrote.
struct ExtWrite {
    world: Arc<ExternalWorld>,
}

impl CellOperations for ExtWrite {
    fn resolve(
        &self,
        call: ToolCallId,
        operation: &ResourceOperation,
    ) -> Result<ResolvedOperation, ExecutionHostError> {
        if operation.operation != "write" {
            return Err(ExecutionHostError::new("only ext.write is declared"));
        }
        let args = serde_json::to_value(&operation.args)
            .map_err(|error| ExecutionHostError::new(error.to_string()))?;
        let request = args.to_string();
        let draft = ExecutionDraft::new(
            call.clone(),
            ToolId::new(TOOL),
            material(MaterialRole::PreparedRequest, &request),
            ExecutionPolicy::Once,
            ExecutionLimit {
                expires_at: u64::MAX,
                max_slice: Duration::from_secs(60),
            },
            None,
        );
        let world = Arc::clone(&self.world);
        Ok(ResolvedOperation {
            draft,
            body: Box::new(move |_cancel| {
                Box::pin(async move {
                    world.write(&call, args.clone());
                    let output = serde_json::json!({ "ok": true, "wrote": args }).to_string();
                    BodyOutput {
                        outcome: AttemptOutcome::Completed(material(
                            MaterialRole::AttemptOutput,
                            &output,
                        )),
                        material: Some(output),
                    }
                })
            }),
        })
    }

    fn value(
        &self,
        outcome: &AttemptOutcome,
        material: Option<&str>,
    ) -> Result<Value, ExecutionHostError> {
        match (outcome, material) {
            (AttemptOutcome::Completed(_), Some(payload)) => serde_json::from_str(payload)
                .map(lashlang::from_json)
                .map_err(|error| ExecutionHostError::new(error.to_string())),
            (AttemptOutcome::Interrupted, _) => {
                Err(ExecutionHostError::new("ext.write was interrupted"))
            }
            (outcome, _) => Err(ExecutionHostError::new(format!(
                "ext.write ended {outcome:?}"
            ))),
        }
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new([(ToolId::new(TOOL), ExecutionPolicy::Once)])
    }
}

/// The scripted protocol: a response that starts with [`CELL_PREFIX`] runs
/// as a TypeScript cell, any other ends the turn with it; a cell's result
/// enters the transcript and the next iteration calls the model again.
#[derive(Debug)]
struct ScriptedProtocol;

fn response_text(response: &LlmResponse) -> String {
    response
        .parts
        .iter()
        .filter_map(|part| match part {
            LlmOutputPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for ScriptedProtocol {
    fn prepare_protocol_iteration(&self, ctx: DriverContextView<'_>) -> Vec<DriverAction> {
        match ctx.project_llm_request(false) {
            Ok(request) => vec![DriverAction::Start(PendingWork::Llm {
                request,
                driver_state: None,
            })],
            Err(error) => lash_sansio::sansio::stored_history_refusal_actions(error),
        }
    }

    fn handle_llm_success(
        &self,
        _ctx: DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        _calls: &lash_sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let text = response_text(&llm_response);
        match text.strip_prefix(CELL_PREFIX) {
            Some(code) => vec![DriverAction::Start(PendingWork::Exec {
                language: "typescript".to_owned(),
                code: code.to_owned(),
                driver_state: lash_core::ProtocolDriverState::new("v0", serde_json::json!({})),
            })],
            None => vec![DriverAction::Finish(TurnOutcome::Finished(
                TurnFinish::AssistantMessage { text },
            ))],
        }
    }

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        ctx: DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        let text = match result {
            Ok(response) => response
                .observations
                .iter()
                .map(|observation| observation.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            Err(failure) => format!("{RESULT_PREFIX}failed {failure:?}"),
        };
        let id = format!("v0-result-{}", ctx.protocol_iteration());
        let message = Message {
            id: id.clone(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(format!("{id}.p0"), text, None)]),
            origin: None,
            reply_marker: None,
        };
        vec![
            DriverAction::AppendEvents(vec![lash_core::SessionHistoryRecord::Conversation(
                lash_core::session_model::ConversationRecord::from_message(message),
            )]),
            DriverAction::AdvanceProtocolIteration,
            DriverAction::Start(PendingWork::Checkpoint {
                checkpoint: lash_core::CheckpointKind::AfterWork,
                on_empty: lash_core::sansio::CheckpointResumeAction::PrepareIteration,
            }),
        ]
    }
}

/// The deployment's turn services in the scenario: the scripted protocol
/// and model, and cells compiled from TypeScript and run from their
/// snapshots.
#[derive(Clone)]
struct V0Services {
    backend: Backend,
    world: Arc<ExternalWorld>,
    driver: Arc<ScriptedProtocol>,
    cells: Arc<Mutex<Vec<ExecKey>>>,
}

/// One turn's drive: its machine, answered by the scenario's services.
struct V0Drive {
    services: V0Services,
    run: TurnId,
    machine: TurnMachine,
}

fn host_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["ext"],
            "Ext",
            "write",
            "tool:ext/write",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "x": { "type": "number" } },
                    "required": ["x"]
                }),
                serde_json::json!({}),
            ),
        )
        .expect("ext.write's contract");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all())
}

fn compile(code: &str) -> Result<Arc<lashlang::CompiledProgram>, String> {
    let linked = lash_typescript::link(code, &host_environment()).map_err(|d| d.to_string())?;
    lashlang::compile(&linked.artifact, lashlang::Entry::Main, None)
        .map(Arc::new)
        .map_err(|error| error.to_string())
}

fn machine_config(
    session: &SessionId,
    run: &TurnId,
    driver: &Arc<ScriptedProtocol>,
) -> TurnMachineConfig {
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: Arc::clone(driver) as _,
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("v0-model"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "scripted".to_string(),
                    std::num::NonZeroUsize::MIN.saturating_add(127_999),
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default())
                .with_request_defaults(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::bounded(8),
        no_progress_budget: Default::default(),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        session_id: session.clone(),
        agent_frame_id: "v0-frame".to_string(),
        turn_id: run.clone(),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

#[async_trait::async_trait]
impl TurnServices for V0Services {
    fn execution_budgets(&self, _session: &SessionId) -> lash_core::ExecutionBudgets {
        lash_core::ExecutionBudgets::default()
    }

    async fn machine_config(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
    ) -> Result<TurnMachineConfig, TurnError> {
        Ok(machine_config(&row.session, &row.run, &self.driver))
    }

    async fn start(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        // The scenario admits a turn with the messages it starts from.
        let messages: Vec<Message> = serde_json::from_str(&row.admission_json)
            .map_err(|error| TurnError::Exec(error.to_string()))?;
        let machine = TurnMachine::new(
            machine_config(&row.session, &row.run, &self.driver),
            messages,
            Default::default(),
            0,
        );
        Ok(self.drive(row, machine))
    }

    async fn resume(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
        machine: TurnMachine,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        Ok(self.drive(row, machine))
    }
}

impl V0Services {
    fn drive(&self, row: &TurnRow, machine: TurnMachine) -> Box<dyn TurnDrive> {
        Box::new(V0Drive {
            services: self.clone(),
            run: row.run.clone(),
            machine,
        })
    }
}

#[async_trait::async_trait]
impl TurnDrive for V0Drive {
    fn machine(&mut self) -> &mut TurnMachine {
        &mut self.machine
    }

    fn tools(&mut self) -> Arc<dyn RoundTools> {
        Arc::new(NoTools)
    }

    async fn local(&mut self, _cx: &ActorContext, effect: Effect) -> Result<(), TurnError> {
        match effect {
            Effect::SyncExecutionEnvironment { id } => {
                self.machine
                    .handle_response(Response::ExecutionEnvironmentSynced {
                        id,
                        result: Ok(ExecutionEnvironmentSync {
                            system_prompt: Arc::from("v0"),
                            tool_specs: Arc::new(Vec::new()),
                            projector_turn_inputs: Default::default(),
                        }),
                    });
            }
            Effect::Checkpoint { id, .. } => {
                self.machine.handle_response(Response::Checkpoint {
                    id,
                    delivery: Default::default(),
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// The scripted model: before the transcript holds a cell result it
    /// answers with the cell; after, with a final answer that quotes it.
    async fn model_call(
        &mut self,
        _cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        _attempt: u32,
        _limit: ExecutionLimit,
    ) -> Result<(), TurnError> {
        let rendered = serde_json::to_string(&*request).expect("a request encodes");
        let text = match rendered.find(RESULT_PREFIX) {
            None => format!("{CELL_PREFIX}{CELL}"),
            Some(at) => {
                let quoted: String = rendered[at..].chars().take(160).collect();
                format!("final answer from {quoted}")
            }
        };
        self.machine.handle_response(Response::LlmComplete {
            id,
            result: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text,
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
            text_streamed: false,
        });
        Ok(())
    }

    async fn restart_live_stream(&mut self, _cx: &ActorContext) -> Result<(), TurnError> {
        Ok(())
    }

    async fn exec_cell(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        exec: ExecKey,
        cell: CodeCell,
        with: Vec<DomainWrite>,
    ) -> Result<(), TurnError> {
        {
            let mut cells = self.services.cells.lock_recover();
            if !cells.contains(&exec) {
                cells.push(exec.clone());
            }
        }
        let program = compile(&cell.code).map_err(TurnError::Exec)?;
        let operations = ExtWrite {
            world: Arc::clone(&self.services.world),
        };
        let identities =
            CodeCallIdentities::cell(EffectOpener::turn(session(), run()), exec.stored());
        let end = run_cell(
            cx,
            Cell {
                exec,
                program,
                identities,
                operations: &operations,
            },
            with,
        )
        .await
        .map_err(|error| TurnError::Exec(error.to_string()))?;
        let (text, value) = match end {
            CellEnd::Finished(value) => (format!("{RESULT_PREFIX}{value}"), value),
            CellEnd::Failed(error) => (
                format!("{RESULT_PREFIX}failed {error}"),
                serde_json::Value::String(error),
            ),
        };
        self.machine.handle_response(Response::ExecResult {
            id,
            result: Ok(ExecResponse {
                observations: vec![lash_core::Observation {
                    text,
                    value,
                    projection: Default::default(),
                }],
                output_archive: None,
                calls: Vec::new(),
                printed_images: Vec::new(),
                error: None,
                degraded_bindings: Vec::new(),
                terminal_finish: None,
                terminal_finish_retained: None,
                suspended: false,
            }),
        });
        Ok(())
    }

    async fn finish(
        &mut self,
        _cx: &ActorContext,
        done: TurnDone,
    ) -> Result<TurnCommit, TurnError> {
        SessionHead::load(&self.services.backend, &session(), commit_budget())
            .await?
            .commit(&self.run, done)
    }
}

fn commit_budget() -> lash_core::facade_support::CommitBudget {
    lash_core::facade_support::CommitBudget::bounded(1024 * 1024, 512)
}

/// The V0 scenario on one dialect, fresh for every matrix cell.
struct V0 {
    dialect: Dialect,
    postgres_url: Option<String>,
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    cells: Arc<Mutex<Vec<ExecKey>>>,
    backend: Mutex<Option<Backend>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl V0 {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            world: Arc::default(),
            tripwire: Arc::default(),
            cells: Arc::default(),
            backend: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    /// The one cell the turn ran: every node names it by the same effect id,
    /// recomputed from the committed checkpoint.
    fn exec(&self) -> Result<ExecKey, String> {
        match self.cells.lock_recover().as_slice() {
            [exec] => Ok(exec.clone()),
            cells => Err(format!("the turn ran {} cells: {cells:?}", cells.len())),
        }
    }
}

#[async_trait::async_trait]
impl Scenario for V0 {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(Backend::for_testing(stores));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: LeaseConfig::default(),
            decodes: vec![FormatSet::new(FORMATS)],
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        let backend = self
            .backend
            .lock_recover()
            .clone()
            .expect("the database is built first");
        Arc::new(SessionActivation::new(
            backend.clone(),
            Arc::new(V0Services {
                backend: backend.clone(),
                world: Arc::clone(&self.world),
                driver: Arc::new(ScriptedProtocol),
                cells: Arc::clone(&self.cells),
            }),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        self.seed(nodes).await?;
        // A starts and claims first, B once A is settled: on a database that
        // runs the two claims concurrently, either could win a race, and
        // the matrix cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        self.laws(nodes, cut).await
    }
}

impl V0 {
    /// Admit the session at its creation head and seed its turn's admission
    /// mail, uncut: the producer is outside the deployment under test.
    async fn seed(&self, nodes: &SimNodes) -> Result<(), String> {
        // The session exists in the catalog at its creation head before
        // its turn is admitted.
        let backend = self
            .backend
            .lock_recover()
            .clone()
            .ok_or("the database is built first")?;
        let catalog: Arc<dyn lash_core_store::store::RuntimeStore> =
            backend.session_store_factory();
        lash_core_store::testing::store_fixtures::admit_conformance_session(&catalog, &session())
            .await;
        let messages = vec![Message {
            id: "v0-input".to_owned(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(
                "v0-input.p0".to_owned(),
                "write x, then tell me what was written".to_owned(),
                None,
            )]),
            origin: None,
            reply_marker: None,
        }];
        let inputs = AdmittedInputs {
            run: run(),
            inputs: Vec::new(),
            admission_json: serde_json::to_string(&messages).map_err(|e| e.to_string())?,
        };
        let mut seed = MailTx::new();
        seed.create_actor(actor(), FormatSet::new(FORMATS)).append(
            actor(),
            admit_mail(),
            inputs.mail_body(),
        );
        // The producer is outside the deployment under test: its mail is
        // seeded straight into the database, uncut.
        nodes
            .database()
            .commit_mail(seed, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// The scenario's laws after a run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();
        let counts = self.tripwire.counts();
        let exec = match self.exec() {
            Ok(exec) => exec,
            Err(error) => return vec![error],
        };

        // P1: the outside world saw each call at most once, and no admitted
        // body was entered twice.
        let writes = self.world.writes();
        for (call, entries) in &writes {
            if entries.len() > 1 {
                violations.push(format!(
                    "P1: call {call} reached the world {} times",
                    entries.len()
                ));
            }
        }
        for (id, entered) in &counts.bodies {
            if *entered > 1 {
                violations.push(format!("P1: body {id:?} entered {entered} times"));
            }
        }

        // The turn committed: no unfinished turn is left.
        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not commit: {other:?}")),
        }

        // The cell's operation and what the cell received.
        let rows = database
            .run_records(&exec.owner())
            .await
            .unwrap_or_default();
        let fold = round::fold(
            &rows,
            &PolicyView::new([(ToolId::new(TOOL), ExecutionPolicy::Once)]),
        );
        let outcomes: Vec<Recovery> = match &fold {
            Ok(fold) => fold
                .recoveries()
                .iter()
                .map(|(_, recovery)| recovery.clone())
                .collect(),
            Err(refusal) => {
                violations.push(format!("the cell's records do not fold: {refusal}"));
                Vec::new()
            }
        };
        let stored = cell_end(database, &exec).await;
        let end = match stored
            .as_ref()
            .and_then(|checkpoint| checkpoint.end.as_ref())
        {
            Some(recorded) => match CellEnd::of(recorded) {
                Ok(CellEnd::Finished(value)) => value.to_string(),
                Ok(CellEnd::Failed(error)) => error,
                Err(error) => {
                    violations.push(format!("the cell's stored end does not decode: {error}"));
                    String::new()
                }
            },
            None => {
                let other = &stored;
                violations.push(format!("the cell has no stored end: {other:?}"));
                String::new()
            }
        };
        match outcomes.as_slice() {
            [Recovery::Settled(AttemptOutcome::Completed(_))] => {
                if !end.contains("\"ok\":true") {
                    violations.push(format!(
                        "P3: the cell did not receive the completed value: {end}"
                    ));
                }
                if writes.len() != 1 {
                    violations.push(format!(
                        "P3: a completed write reached the world {} times",
                        writes.len()
                    ));
                }
            }
            [Recovery::Settled(AttemptOutcome::Interrupted)] => {
                if !end.contains("interrupted") {
                    violations.push(format!("P2: the cell did not receive Interrupted: {end}"));
                }
            }
            other => violations.push(format!("the operation did not settle once: {other:?}")),
        }

        // P6: every admitted run is named by the snapshot that issued it.
        if let (Some(snapshot), Ok(_)) = (&stored, &fold) {
            for row in rows.iter().filter(|row| row.kind == RunRecordKind::Admit) {
                let named = snapshot
                    .ledger
                    .operations
                    .keys()
                    .any(|operation| operation.run == row.run.0);
                if !named {
                    violations.push(format!(
                        "P6: admitted run {:?} has no snapshot naming it",
                        row.run
                    ));
                }
            }
        }

        // P7: the head advanced exactly once.
        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if commits != 1 {
            violations.push(format!("P7: the turn committed {commits} times"));
        }

        // P4: no hidden replay.
        let unsnapshotted = trace
            .iter()
            .filter(|write| {
                write.point.label == CommitLabel::CELL_SNAPSHOT_ADMIT && !write.committed()
            })
            .count();
        let programs = counts.vm_programs.get(&exec).copied().unwrap_or(0);
        if programs > 1 + unsnapshotted {
            violations.push(format!(
                "P4: the cell's program was entered fresh {programs} times with {unsnapshotted} uncommitted snapshots"
            ));
        }
        if counts.outcome_lookups.values().sum::<usize>() != 0 {
            violations.push("P4: an outcome was looked up for re-running code".to_owned());
        }
        if counts.committed_ordinals.values().sum::<usize>() != 0 {
            violations.push("P4: a committed ordinal was emitted again".to_owned());
        }
        let restores = counts
            .restores
            .get(&(session(), run()))
            .copied()
            .unwrap_or(0);
        if restores > 1 {
            violations.push(format!("P4: the turn was restored {restores} times"));
        }

        if let Some(cut) = cut {
            violations.extend(kill_laws(cut, &outcomes, programs, restores, &writes));
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// The cell's last snapshot.
async fn cell_end(database: &Arc<dyn DurableStore>, exec: &ExecKey) -> Option<Checkpoint> {
    let row = database.snapshot(exec).await.ok()??;
    serde_json::from_str(&row.snapshot_ref).ok()
}

/// K1 and K2: a node killed at `round.outcome` after the body ran.
fn kill_laws(
    cut: &Cut,
    outcomes: &[Recovery],
    programs: usize,
    restores: usize,
    writes: &BTreeMap<ToolCallId, Vec<serde_json::Value>>,
) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.point.label != CommitLabel::ROUND_OUTCOME || !cut.fault.kills() {
        return violations;
    }
    let name = match cut.fault {
        Fault::Abort => "K1",
        _ => "K2",
    };
    let expected = match cut.fault {
        Fault::Abort => matches!(outcomes, [Recovery::Settled(AttemptOutcome::Interrupted)]),
        _ => matches!(outcomes, [Recovery::Settled(AttemptOutcome::Completed(_))]),
    };
    if !expected {
        violations.push(format!("{name}: the operation settled as {outcomes:?}"));
    }
    if writes.values().map(Vec::len).sum::<usize>() != 1 {
        violations.push(format!(
            "{name}: the body did not run exactly once: {writes:?}"
        ));
    }
    if programs != 1 {
        violations.push(format!(
            "{name}: the cell's program was entered fresh {programs} times"
        ));
    }
    if restores != 1 {
        violations.push(format!(
            "{name}: the turn was restored {restores} times, not once"
        ));
    }
    violations
}

/// P5: once a zombie's actors moved, every owner write it attempts is
/// refused with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["P5: the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("P5: zombie write {write} was {other:?}")),
        }
    }
    violations
}

/// The matrix the spec names: every commit label of the uncut run under
/// fail-before, ack-hidden and zombie, plus the kills K1 and K2 come from.
fn matrix() -> Matrix {
    Matrix::new()
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
}

async fn prove(dialect: Dialect, postgres_url: Option<String>) {
    let report = matrix()
        .run(|| V0::new(dialect, postgres_url.clone()))
        .await;
    let labels: Vec<&str> = report.labels().iter().map(|label| label.as_str()).collect();
    let k1 = report.cells.iter().any(|cell| {
        cell.point.label == CommitLabel::ROUND_OUTCOME
            && cell.fault == Fault::Abort
            && cell.verdict == Verdict::Held
    });
    let k2 = report.cells.iter().any(|cell| {
        cell.point.label == CommitLabel::ROUND_OUTCOME
            && cell.fault == Fault::CommitThenAbort
            && cell.verdict == Verdict::Held
    });
    eprintln!(
        "V0 {dialect:?}: {} cells over {} labels ({}) x {} faults",
        report.cells.len(),
        labels.len(),
        labels.join(", "),
        5
    );
    report.assert_held();
    assert!(k1, "K1 (abort at round.outcome) was not cut");
    assert!(k2, "K2 (commit-then-abort at round.outcome) was not cut");
    for label in [
        CommitLabel::TURN_ADMIT,
        CommitLabel::MODEL_START,
        CommitLabel::CELL_SNAPSHOT_ADMIT,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::CELL_SNAPSHOT,
        CommitLabel::TURN_COMMIT,
    ] {
        assert!(
            report.labels().contains(&label),
            "the matrix never cut {label}"
        );
    }
}

/// The scenario runs to its commit uncut, through every commit label, on one
/// owner with one body entry and one fresh program entry.
#[tokio::test]
async fn the_uncut_turn_commits_through_every_label() {
    let report = Matrix::new()
        .faults(&[])
        .run(|| V0::new(Dialect::SqliteMemory, None))
        .await;
    let labels: Vec<CommitLabel> = report
        .baseline
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    assert_eq!(
        labels,
        vec![
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::CELL_SNAPSHOT_ADMIT,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::CELL_SNAPSHOT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ]
    );
}

/// V0 on SQLite in memory: every cell of the matrix holds P1 to P7.
#[tokio::test]
async fn a_once_operation_killed_after_its_work_resumes_without_replay_on_sqlite_memory() {
    prove(Dialect::SqliteMemory, None).await;
}

/// V0 on a SQLite file: every cell of the matrix holds P1 to P7.
#[tokio::test]
async fn a_once_operation_killed_after_its_work_resumes_without_replay_on_sqlite_file() {
    prove(Dialect::SqliteFile, None).await;
}

/// V0 on PostgreSQL: every cell of the matrix holds P1 to P7.
#[tokio::test]
async fn a_once_operation_killed_after_its_work_resumes_without_replay_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    prove(Dialect::Postgres, Some(url)).await;
}

/// Cold resume: the whole deployment dies right after the cell's end
/// commits, and a fresh deployment over the same database, with services
/// and an activation that hold nothing of the first one, takes the turn
/// over. A restore loads state and re-runs no code: the turn is restored
/// once, the cell's end is read from its snapshot without entering its
/// program or its operation again, and the only writes left are the second
/// model call's `model.start`, `turn.commit` and the release.
async fn cold_resume(dialect: Dialect, postgres_url: Option<String>) {
    let first = V0::new(dialect, postgres_url.clone());
    let clock = SimClock::new();
    let database = first.database(Arc::clone(&clock)).await;
    let script = Script::new();
    script.cut_on("a", CommitLabel::CELL_SNAPSHOT, 1, Fault::CommitThenAbort);
    let before = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        first.config(),
        first.activation(),
    );
    first.seed(&before).await.expect("the turn is seeded");
    before.start("a");
    before.quiesce().await;
    while before.script().cuts().is_empty() {
        assert!(
            before.step().await.is_some(),
            "the first deployment stalled before the cell's end committed:\n{}",
            before.script().rendered_trace()
        );
    }
    before.kill("a");
    before.quiesce().await;
    assert_eq!(before.life("a"), Life::Dead);
    assert!(
        !first.done(&before).await,
        "the turn finished before the deployment died"
    );

    // A new process: the same database and the same outside world, nothing
    // else of the first deployment.
    let cold = V0 {
        dialect,
        postgres_url,
        world: Arc::clone(&first.world),
        tripwire: Arc::clone(&first.tripwire),
        cells: Arc::default(),
        backend: Mutex::new(first.backend.lock_recover().clone()),
        keep: Mutex::default(),
    };
    let after = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        cold.config(),
        cold.activation(),
    );
    after.start("c");
    let horizon = clock.logical_ms() + 600_000;
    while !cold.done(&after).await {
        assert!(
            clock.logical_ms() < horizon,
            "the cold deployment is not done after 600 s of virtual time:\n{}",
            after.script().rendered_trace()
        );
        assert!(
            after.step().await.is_some(),
            "the cold deployment stalled:\n{}",
            after.script().rendered_trace()
        );
    }
    after.quiesce().await;

    let mut violations = cold.laws(&after, None).await;
    let counts = first.tripwire.counts();
    let restores = counts
        .restores
        .get(&(session(), run()))
        .copied()
        .unwrap_or(0);
    if restores != 1 {
        violations.push(format!(
            "the cold owner restored the turn {restores} times, not once"
        ));
    }
    let programs: usize = counts.vm_programs.values().sum();
    if programs != 1 {
        violations.push(format!(
            "the cell's program was entered {programs} times; only the first deployment enters it"
        ));
    }
    let labels: Vec<CommitLabel> = after
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    if labels
        != [
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ]
    {
        violations.push(format!(
            "the cold owner committed {labels:?}, not the second model call's start, the turn's commit and the release"
        ));
    }
    assert!(
        violations.is_empty(),
        "cold resume on {dialect:?}:\n  {}\n{}",
        violations.join("\n  "),
        after.script().rendered_trace()
    );
}

#[tokio::test]
async fn a_cold_restart_restores_from_state_and_reruns_no_code_on_sqlite_memory() {
    cold_resume(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn a_cold_restart_restores_from_state_and_reruns_no_code_on_sqlite_file() {
    cold_resume(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn a_cold_restart_restores_from_state_and_reruns_no_code_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    cold_resume(Dialect::Postgres, Some(url)).await;
}

/// The scenario's model calls no tool: its work runs in a code cell.
struct NoTools;

impl RoundTools for NoTools {
    fn pin(&self, _call: &PendingToolCall, _now_ms: u64) -> MemberPin {
        unreachable!("the vertical scenario calls no tool")
    }

    fn policies(&self) -> PolicyView {
        PolicyView::default()
    }

    fn body(&self, _call: &PendingToolCall, _execution: &AdmittedExecution) -> MemberBody {
        unreachable!("the vertical scenario calls no tool")
    }

    fn resolved(
        &self,
        _call: &PendingToolCall,
        _execution: &AdmittedExecution,
        _source: &lash_core_store::tool_run::CompletionSource,
        _metadata: Option<&str>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::BodyOutput {
        unreachable!("the vertical scenario calls no tool")
    }

    fn completed(
        &self,
        _call: &PendingToolCall,
        _outcome: &AttemptOutcome,
        _material: Option<&str>,
    ) -> CompletedCall {
        unreachable!("the vertical scenario calls no tool")
    }
}
