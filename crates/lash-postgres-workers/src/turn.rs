//! The turn workload: one session, one turn, one cell, one `Once` operation.
//!
//! The scripted model answers the turn's first call with a TypeScript cell
//! that calls `ext.write({ x: 7 })`, declared `Once`, and its second call
//! (once the cell's result is in the transcript) with a final answer. The
//! operation's body writes its witness entry, may hold, and answers what it
//! wrote. Everything runs on the production session activation and its
//! per-turn drive; only the protocol, the model and the operation's body are
//! the runbook's. The turn commits the session's real head.

use std::sync::Arc;
use std::time::Duration;

use lash_core::facade_support::{CommitBudget, EffectId, Response};
use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::session::{
    AdmittedInputs, CodeCell, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRow, TurnServices,
    admit_mail,
};
use lash_core::sansio::{ChatContextProjector, PendingToolCall, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, LlmOutputPart, LlmRequest, LlmResponse,
    Message, MessageRole, Part, ProtocolTurnOptions, TurnMachine, TurnMachineConfig,
    facade_support::TurnFinish, facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, BodyOutput, CompletedCall, ExecutionDraft, MemberBody, MemberPin,
    PolicyView, RoundTools,
};
use lash_core_execution::{ActorContext, Backend};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    AttemptOutcome, MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::ExecKey;
use lash_durable::{ActorKey, CommitLabel, DomainWrite, FormatSet, MailTx};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::{ExecutionLimit, ExecutionPolicy, SessionId, ToolCallId, ToolId, TurnId};
use lash_vm_broker::CodeCallIdentities;
use lash_vm_broker::cell::{Cell, CellEnd, CellOperations, ResolvedOperation, run_cell};
use lashlang::{ExecutionHostError, ResourceOperation, Value};

use crate::events::{Event, report};
use crate::witness::{Hold, Witness};

/// The runbook's one session.
pub const SESSION: &str = "workers-session";
/// Its one turn.
pub const RUN: &str = "workers-turn";
/// The `Once` operation the cell calls.
pub const TOOL: &str = "ext_write";

/// The cell the model writes: one `Once` host operation, then a result that
/// carries what the operation answered.
const CELL: &str = "const written = await ext.write({ x: 7 });\nfinish({ written });";
const CELL_PREFIX: &str = "CELL:";
/// What marks the cell's result in the transcript, and so the second call.
pub const RESULT_PREFIX: &str = "cell result: ";
/// What the final answer starts with.
pub const FINAL_PREFIX: &str = "final answer from ";

/// The session's id.
///
/// # Panics
///
/// Never: the id is a valid literal.
#[must_use]
#[expect(clippy::expect_used, reason = "a literal session id is valid")]
pub fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).expect("a valid session id")
}

/// The turn's id.
///
/// # Panics
///
/// Never: the id is a valid literal.
#[must_use]
#[expect(clippy::expect_used, reason = "a literal turn id is valid")]
pub fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).expect("a valid turn id")
}

/// The session's actor.
///
/// # Panics
///
/// Never: the key is a valid literal.
#[must_use]
#[expect(clippy::expect_used, reason = "a literal actor key is valid")]
pub fn actor() -> ActorKey {
    ActorKey::session(SESSION).expect("a valid actor key")
}

/// Create the session at its creation head in the catalog, then commit the
/// mail that creates its actor and admits its turn: what a producer outside
/// the deployment does.
///
/// # Errors
///
/// The catalog or the store refused.
pub async fn admit(backend: &Backend) -> Result<(), String> {
    backend
        .session_store_factory()
        .admit_session(&lash_core_store::testing::store_fixtures::root_session_request(&session()))
        .await
        .map_err(|error| format!("admit the session: {error}"))?;
    let messages = vec![Message {
        id: "workers-input".to_owned(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::text(
            "workers-input.p0".to_owned(),
            "write x, then tell me what was written".to_owned(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    }];
    let inputs = AdmittedInputs {
        run: run(),
        inputs: Vec::new(),
        admission_json: serde_json::to_string(&messages).map_err(|error| error.to_string())?,
    };
    let mut seed = MailTx::new();
    seed.create_actor(actor(), FormatSet::new(crate::SESSION_FORMATS))
        .append(actor(), admit_mail(), inputs.mail_body());
    backend
        .durable()
        .commit_mail(seed, CommitLabel::MAIL_SESSION)
        .await
        .map_err(|error| format!("admit the turn: {error}"))?;
    Ok(())
}

/// A material reference to an operation's payload: journal-local, named by a
/// digest of its bytes.
#[expect(clippy::expect_used, reason = "64 hex digits always parse as a digest")]
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
        digest: MaterialDigest::parse(&format!("{:064x}", hasher.finish()))
            .expect("a 64-digit hex digest"),
    }
}

/// `ext.write`: a `Once` host operation whose body writes its witness entry,
/// holds where the case asks, and answers what it wrote.
struct ExtWrite {
    witness: Witness,
    hold: Hold,
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
                max_slice: Duration::from_secs(600),
            },
            None,
        );
        let witness = self.witness.clone();
        let hold = self.hold;
        Ok(ResolvedOperation {
            draft,
            body: Box::new(move |_cancel| {
                Box::pin(async move {
                    let id = call.to_string();
                    witness.entered(&id, TOOL).await;
                    report(
                        witness.node(),
                        Event::Body {
                            call: id.clone(),
                            tool: TOOL.to_owned(),
                            phase: "entered".to_owned(),
                        },
                    );
                    match hold {
                        Hold::Step => std::future::pending::<()>().await,
                        Hold::StepUntilRelease => witness.released().await,
                        Hold::Nothing | Hold::Model | Hold::ModelUntilRelease => {}
                    }
                    witness.returned(&id, TOOL).await;
                    report(
                        witness.node(),
                        Event::Body {
                            call: id,
                            tool: TOOL.to_owned(),
                            phase: "returned".to_owned(),
                        },
                    );
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
                driver_state: lash_core::ProtocolDriverState::new("workers", serde_json::json!({})),
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
        let id = format!("workers-result-{}", ctx.protocol_iteration());
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

/// The node's turn services: the scripted protocol and model, and cells
/// compiled from TypeScript and run from their snapshots.
#[derive(Clone)]
pub struct WorkerServices {
    witness: Witness,
    hold: Hold,
    driver: Arc<ScriptedProtocol>,
}

impl WorkerServices {
    /// Services that write to `witness` and hold at `hold`.
    #[must_use]
    pub fn new(witness: Witness, hold: Hold) -> Self {
        Self {
            witness,
            hold,
            driver: Arc::new(ScriptedProtocol),
        }
    }

    fn drive(&self, row: &TurnRow, machine: TurnMachine) -> Box<dyn TurnDrive> {
        Box::new(WorkerDrive {
            services: self.clone(),
            run: row.run.clone(),
            machine,
        })
    }
}

/// One turn's drive on this node: its machine, answered by the services.
struct WorkerDrive {
    services: WorkerServices,
    run: TurnId,
    machine: TurnMachine,
}

fn host_environment() -> Result<lashlang::LashlangHostEnvironment, String> {
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
        .map_err(|error| error.to_string())?;
    Ok(lashlang::LashlangHostEnvironment::new(
        catalog,
        lashlang::LashlangAbilities::all(),
    ))
}

fn compile(code: &str) -> Result<Arc<lashlang::CompiledProgram>, String> {
    let linked = lash_typescript::link(code, &host_environment()?).map_err(|d| d.to_string())?;
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
                lash_sansio::llm_profile::LlmProfileKey::new("workers-model"),
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
        agent_frame_id: "workers-frame".to_string(),
        turn_id: run.clone(),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

#[async_trait::async_trait]
impl TurnServices for WorkerServices {
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
        _head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        // The turn is admitted with the messages it starts from.
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

#[async_trait::async_trait]
impl TurnDrive for WorkerDrive {
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
                            system_prompt: Arc::from("workers"),
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
    /// The first attempt of the first call holds where the case asks.
    async fn model_call(
        &mut self,
        _cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        attempt: u32,
        _limit: ExecutionLimit,
    ) -> Result<(), TurnError> {
        #[expect(clippy::expect_used, reason = "a model request always encodes")]
        let rendered = serde_json::to_string(&*request).expect("a request encodes");
        let result = rendered.find(RESULT_PREFIX);
        let call = if result.is_some() { 2 } else { 1 };
        let witness = &self.services.witness;
        witness.model_attempt(call, attempt).await;
        report(witness.node(), Event::ModelAttempt { call, attempt });
        if call == 1 && attempt == 1 {
            match self.services.hold {
                Hold::Model => std::future::pending::<()>().await,
                Hold::ModelUntilRelease => witness.released().await,
                Hold::Nothing | Hold::Step | Hold::StepUntilRelease => {}
            }
        }
        let text = match result {
            None => format!("{CELL_PREFIX}{CELL}"),
            Some(at) => {
                let quoted: String = rendered[at..].chars().take(160).collect();
                format!("{FINAL_PREFIX}{quoted}")
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
        if let ExecKey::Cell(_, _, cell) = &exec {
            report(
                self.services.witness.node(),
                Event::Cell {
                    cell: cell.as_str().to_owned(),
                },
            );
        }
        let program = compile(&cell.code).map_err(TurnError::Exec)?;
        let operations = ExtWrite {
            witness: self.services.witness.clone(),
            hold: self.services.hold,
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
        head: &SessionHead,
    ) -> Result<TurnCommit, TurnError> {
        head.commit(&self.run, done, commit_budget())
    }
}

/// The bounds of the turn's head commit.
fn commit_budget() -> CommitBudget {
    CommitBudget::bounded(1024 * 1024, 512)
}

/// The turn's model calls no tool: its `Once` work runs in its code cell.
struct NoTools;

impl RoundTools for NoTools {
    fn pin(&self, _call: &PendingToolCall, _now_ms: u64) -> MemberPin {
        unreachable!("the runbook's model calls no tool")
    }

    fn policies(&self) -> PolicyView {
        PolicyView::default()
    }

    fn body(&self, _call: &PendingToolCall, _execution: &AdmittedExecution) -> MemberBody {
        unreachable!("the runbook's model calls no tool")
    }

    fn resolved(
        &self,
        _call: &PendingToolCall,
        _execution: &AdmittedExecution,
        _source: &lash_core_store::tool_run::CompletionSource,
        _metadata: Option<&str>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> BodyOutput {
        unreachable!("the runbook's model calls no tool")
    }

    fn completed(
        &self,
        _call: &PendingToolCall,
        _outcome: &AttemptOutcome,
        _material: Option<&str>,
    ) -> CompletedCall {
        unreachable!("the runbook's model calls no tool")
    }
}
