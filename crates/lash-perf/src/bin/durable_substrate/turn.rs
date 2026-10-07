//! The scripted turn: the production session activation and phase runner,
//! with only the protocol, the model and the tool bodies the bench's.
//!
//! The model returns immediately. Following the L12a fixture, it answers a
//! turn's call `r` with `tools_per_round` calls of `benchmark_echo` while
//! `r < rounds`, and then with a text answer. A cell turn instead answers
//! its first call with a TypeScript cell that awaits `ext.echo` in a loop,
//! retaining each answer in its heap, and its second with a text answer.
//! A turn starts from its session's committed head, so a session's prior
//! turns are in the transcript it starts, checkpoints and commits.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::{CommitBudget, EffectId, Response};
use lash_core::llm::types::LlmContentBlock;
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
    MemberResult, PolicyView, RoundTools,
};
use lash_core_execution::{ActorContext, Backend};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    AttemptOutcome, MaterialDigest, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef,
    MaterialRole,
};
use lash_durable::domain::ExecKey;
use lash_durable::{ActorKey, CommitLabel, DomainWrite, FormatSet, MailTx};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionBudgets, ExecutionLimit, ExecutionPolicy, ModelToolReturn, SessionId, ToolCallId,
    ToolCallOutput, ToolFailure, ToolFailureClass, ToolId, TurnId,
};
use lash_vm_broker::CodeCallIdentities;
use lash_vm_broker::cell::{Cell, CellEnd, CellOperations, ResolvedOperation, run_cell};
use lashlang::{ExecutionHostError, ResourceOperation, Value};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::recorder::Recorder;

/// The format set the bench's sessions are written in.
pub const SESSION_FORMATS: &str = "durable-substrate/1";
const ECHO: &str = "benchmark_echo";
const CELL_TOOL: &str = "ext_echo";
const RESULTS: &str = "bench-results:";
const CELL_RESULT: &str = "bench-cell-result:";

/// What one session's turns do.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Script {
    /// Tool rounds before the answer.
    pub rounds: usize,
    /// Parallel `benchmark_echo` calls per round.
    pub tools_per_round: usize,
    /// Host operations the cell awaits; zero runs no cell.
    pub cell_calls: usize,
    /// Bytes of padding each cell operation answers, retained in the heap.
    pub cell_payload: usize,
    /// Hold the first attempt of this model call (0-based) until the node
    /// serving it stops.
    pub hold_call: Option<usize>,
}

/// The scripts by session, shared by every node, and the holds reached.
#[derive(Default)]
pub struct Scripts {
    scripts: Mutex<HashMap<String, Script>>,
    holds: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl Scripts {
    /// Run `session`'s turns by `script`.
    pub fn set(&self, session: &SessionId, script: Script) {
        self.scripts
            .lock_recover()
            .insert(session.to_string(), script);
    }

    /// Resolve once `session`'s held model call is entered.
    pub fn on_hold(&self, session: &SessionId) -> oneshot::Receiver<()> {
        let (send, receive) = oneshot::channel();
        self.holds.lock_recover().insert(session.to_string(), send);
        receive
    }

    fn get(&self, session: &SessionId) -> Script {
        self.scripts
            .lock_recover()
            .get(session.as_str())
            .copied()
            .unwrap_or_default()
    }

    fn take_hold(&self, session: &SessionId) -> Option<oneshot::Sender<()>> {
        self.holds.lock_recover().remove(session.as_str())
    }
}

/// The session id the bench names `name`.
pub fn session_id(name: &str) -> anyhow::Result<SessionId> {
    SessionId::try_from(name.to_owned()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// The session's actor.
pub fn session_actor(session: &SessionId) -> anyhow::Result<ActorKey> {
    ActorKey::session(session.as_str()).map_err(|error| anyhow::anyhow!("{error}"))
}

/// Create `session` at its creation head in the catalog.
pub async fn create_session(backend: &Backend, session: &SessionId) -> anyhow::Result<()> {
    backend
        .session_store_factory()
        .admit_session(&lash_core_store::testing::store_fixtures::root_session_request(session))
        .await
        .map_err(|error| anyhow::anyhow!("admit {session}: {error}"))?;
    Ok(())
}

/// The mail transaction that admits `run` to `session` with one user
/// input, creating the session's actor when `create` is set: what a
/// producer outside the deployment commits.
pub fn admission(session: &SessionId, run: &TurnId, create: bool) -> anyhow::Result<MailTx> {
    let messages = vec![Message {
        id: format!("{run}-input"),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::text(
            format!("{run}-input.p0"),
            "run the benchmark".to_owned(),
            None,
        )]),
        origin: None,
        reply_marker: None,
    }];
    let inputs = AdmittedInputs {
        run: run.clone(),
        inputs: Vec::new(),
        admission_json: serde_json::to_string(&messages)?,
    };
    let actor = session_actor(session)?;
    let mut tx = MailTx::new();
    if create {
        tx.create_actor(actor.clone(), FormatSet::new(SESSION_FORMATS));
    }
    tx.append(actor, admit_mail(), inputs.mail_body());
    Ok(tx)
}

/// The label a producer's admission commits under.
pub const ADMISSION: CommitLabel = CommitLabel::MAIL_SESSION;

fn text_of(request: &LlmRequest, marker: &str) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter(|block| match block {
            LlmContentBlock::Text { text, .. } => text.starts_with(marker),
            _ => false,
        })
        .count()
}

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

fn user_message(id: String, text: String) -> Message {
    Message {
        id: id.clone(),
        role: MessageRole::User,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), text, None)]),
        origin: None,
        reply_marker: None,
    }
}

fn after_work(message: Message) -> Vec<DriverAction> {
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

/// The scripted protocol: tool calls start their round, a `CELL:` answer
/// runs as a TypeScript cell, any other answer finishes the turn; results
/// join the transcript and the model is called again.
#[derive(Debug)]
struct Protocol;

const CELL_PREFIX: &str = "CELL:";

impl ProtocolDriverHandle<lash_core::HostTurnProtocol> for Protocol {
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
        calls: &lash_sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let ids = calls.call_ids(&llm_response);
        if ids.is_empty() {
            let text = response_text(&llm_response);
            return match text.strip_prefix(CELL_PREFIX) {
                Some(code) => vec![DriverAction::Start(PendingWork::Exec {
                    language: "typescript".to_owned(),
                    code: code.to_owned(),
                    driver_state: lash_core::ProtocolDriverState::new(
                        "bench",
                        serde_json::json!({}),
                    ),
                })],
                None => vec![DriverAction::Finish(TurnOutcome::Finished(
                    TurnFinish::AssistantMessage { text },
                ))],
            };
        }
        let pending = llm_response
            .parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::ToolCall {
                    tool_name,
                    input_json,
                    ..
                } => Some((tool_name.clone(), input_json.clone())),
                _ => None,
            })
            .zip(ids)
            .map(|((tool_name, input_json), call_id)| PendingToolCall {
                call_id,
                provider_call_id: None,
                tool_name,
                args: serde_json::from_str(&input_json).unwrap_or_default(),
                replay: None,
            })
            .collect();
        vec![DriverAction::Start(PendingWork::WaitingForToolResults {
            calls: pending,
            settled: None,
            expansion: Default::default(),
        })]
    }

    fn handle_tool_results(
        &self,
        ctx: DriverContextView<'_>,
        completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<DriverAction> {
        let rendered: Vec<String> = completed
            .iter()
            .map(|call| {
                format!(
                    "{}={}",
                    call.tool_name,
                    serde_json::to_string(&call.output.outcome).unwrap_or_default()
                )
            })
            .collect();
        let id = format!("{}-results-{}", ctx.turn_id(), ctx.protocol_iteration());
        after_work(user_message(
            id,
            format!("{RESULTS}{} {}", ctx.turn_id(), rendered.join("; ")),
        ))
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
            Err(failure) => format!("failed {failure:?}"),
        };
        let id = format!("{}-cell-{}", ctx.turn_id(), ctx.protocol_iteration());
        after_work(user_message(
            id,
            format!("{CELL_RESULT}{} {text}", ctx.turn_id()),
        ))
    }
}

fn machine_config(session: &SessionId, run: &TurnId, driver: &Arc<Protocol>) -> TurnMachineConfig {
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: Arc::clone(driver) as _,
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("bench-model"),
                lash_sansio::llm_profile::LlmProfileMetadata::new(
                    "scripted".to_string(),
                    NonZeroUsize::MIN.saturating_add(127_999),
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default())
                .with_request_defaults(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::bounded(64),
        no_progress_budget: Default::default(),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        autonomous: false,
        session_id: session.clone(),
        agent_frame_id: "bench-frame".to_string(),
        turn_id: run.clone(),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

fn commit_budget() -> CommitBudget {
    CommitBudget::bounded(64 * 1024 * 1024, 1 << 20)
}

/// One node's turn services.
#[derive(Clone)]
pub struct BenchServices {
    backend: Backend,
    recorder: Arc<Recorder>,
    scripts: Arc<Scripts>,
    driver: Arc<Protocol>,
}

impl BenchServices {
    /// Services over `backend`, reporting to `recorder`, running `scripts`.
    pub fn new(backend: Backend, recorder: Arc<Recorder>, scripts: Arc<Scripts>) -> Self {
        Self {
            backend,
            recorder,
            scripts,
            driver: Arc::new(Protocol),
        }
    }

    fn drive(&self, row: &TurnRow, machine: TurnMachine) -> Box<dyn TurnDrive> {
        Box::new(BenchDrive {
            services: self.clone(),
            session: row.session.clone(),
            run: row.run.clone(),
            script: self.scripts.get(&row.session),
            machine,
        })
    }
}

#[async_trait::async_trait]
impl TurnServices for BenchServices {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        ExecutionBudgets::default()
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
        let window = SessionHead::load(&self.backend, &row.session, commit_budget())
            .await?
            .window()?;
        let admitted: Vec<Message> = serde_json::from_str(&row.admission_json)
            .map_err(|error| TurnError::Exec(error.to_string()))?;
        let messages = window.then(admitted);
        let machine = TurnMachine::in_window(
            machine_config(&row.session, &row.run, &self.driver),
            window,
            messages,
            Vec::new(),
            0,
            Vec::new(),
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

struct BenchDrive {
    services: BenchServices,
    session: SessionId,
    run: TurnId,
    script: Script,
    machine: TurnMachine,
}

impl BenchDrive {
    fn cell_source(&self) -> String {
        let pad = "x".repeat(self.script.cell_payload);
        format!(
            "const kept = [];\nfor (let i = 0; i < {calls}; i++) {{\n  const answer = await ext.echo({{ i: i, pad: \"{pad}\" }});\n  kept.push(answer);\n}}\nfinish({{ calls: kept.length }});",
            calls = self.script.cell_calls,
        )
    }
}

#[async_trait::async_trait]
impl TurnDrive for BenchDrive {
    fn machine(&mut self) -> &mut TurnMachine {
        &mut self.machine
    }

    async fn local(&mut self, _cx: &ActorContext, effect: Effect) -> Result<(), TurnError> {
        match effect {
            Effect::SyncExecutionEnvironment { id } => {
                self.machine
                    .handle_response(Response::ExecutionEnvironmentSynced {
                        id,
                        result: Ok(ExecutionEnvironmentSync {
                            system_prompt: Arc::from("bench"),
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

    async fn model_call(
        &mut self,
        _cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        attempt: u32,
        _limit: ExecutionLimit,
    ) -> Result<(), TurnError> {
        self.services.recorder.model_call(&self.session);
        let script = self.script;
        let results_marker = format!("{RESULTS}{} ", self.run);
        let cell_marker = format!("{CELL_RESULT}{} ", self.run);
        let rounds_done = text_of(&request, &results_marker);
        let cells_done = text_of(&request, &cell_marker);
        let call = rounds_done + cells_done;
        if script.hold_call == Some(call)
            && attempt == 1
            && let Some(reached) = self.services.scripts.take_hold(&self.session)
        {
            let _ = reached.send(());
            std::future::pending::<()>().await;
        }
        let parts = if script.cell_calls > 0 && cells_done == 0 {
            vec![LlmOutputPart::Text {
                text: format!("{CELL_PREFIX}{}", self.cell_source()),
                response_meta: None,
            }]
        } else if script.cell_calls == 0 && rounds_done < script.rounds {
            (0..script.tools_per_round)
                .map(|tool| LlmOutputPart::ToolCall {
                    call_id: format!("round-{rounds_done}-tool-{tool}"),
                    tool_name: ECHO.to_owned(),
                    input_json: serde_json::json!({"value": "baseline", "ordinal": rounds_done})
                        .to_string(),
                    replay: None,
                })
                .collect()
        } else {
            vec![LlmOutputPart::Text {
                text: "baseline complete".to_owned(),
                response_meta: None,
            }]
        };
        self.machine.handle_response(Response::LlmComplete {
            id,
            result: Ok(LlmResponse {
                parts,
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

    fn tools(&mut self) -> Arc<dyn RoundTools> {
        Arc::new(EchoTools {
            session: self.session.clone(),
            run: self.run.clone(),
        })
    }

    async fn exec_cell(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        exec: ExecKey,
        cell: CodeCell,
        with: Vec<DomainWrite>,
    ) -> Result<(), TurnError> {
        let program = compile(&cell.code).map_err(TurnError::Exec)?;
        let operations = CellEcho {
            session: self.session.clone(),
            run: self.run.clone(),
        };
        let identities = CodeCallIdentities::cell(
            EffectOpener::turn(self.session.clone(), self.run.clone()),
            exec.stored(),
        );
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
            CellEnd::Finished(value) => (value.to_string(), value),
            CellEnd::Failed(error) => (format!("failed {error}"), serde_json::Value::String(error)),
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
        SessionHead::load(&self.services.backend, &self.session, commit_budget())
            .await?
            .commit(&self.run, done)
    }
}

fn output_material(session: &SessionId, run: &TurnId, text: &str) -> Option<MaterialRef> {
    MaterialPayload::new(
        MaterialOwner::Run {
            opener: EffectOpener::turn(session.clone(), run.clone()),
        },
        MaterialRole::AttemptOutput,
        None,
        text.to_owned(),
    )
    .reference(MaterialLocation::JournalLocal)
    .ok()
}

/// `benchmark_echo`: a `Once` tool that answers its arguments at once.
struct EchoTools {
    session: SessionId,
    run: TurnId,
}

impl RoundTools for EchoTools {
    fn pin(&self, _call: &PendingToolCall, now_ms: u64) -> MemberPin {
        let budget = ExecutionBudgets::default().tool_default();
        MemberPin {
            tool: ToolId::new(ECHO),
            policy: ExecutionPolicy::Once,
            limit: ExecutionLimit::starting_at(now_ms, budget, budget),
            wait: None,
        }
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new([(ToolId::new(ECHO), ExecutionPolicy::Once)])
    }

    fn resolved(
        &self,
        _call: &PendingToolCall,
        _execution: &AdmittedExecution,
        _source: &lash_core_store::tool_run::CompletionSource,
        _metadata: Option<&str>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> BodyOutput {
        // No tool of this catalog parks, so no park ever resolves.
        BodyOutput::from(AttemptOutcome::Interrupted)
    }

    fn body(&self, call: &PendingToolCall, _execution: &AdmittedExecution) -> MemberBody {
        let text = call.args.to_string();
        let material = output_material(&self.session, &self.run, &text);
        Box::new(move |_token| {
            Box::pin(async move {
                let outcome = match material {
                    Some(material) => AttemptOutcome::Completed(material),
                    None => AttemptOutcome::Interrupted,
                };
                MemberResult::from(BodyOutput {
                    outcome,
                    material: Some(text),
                })
            })
        })
    }

    fn completed(
        &self,
        call: &PendingToolCall,
        outcome: &AttemptOutcome,
        material: Option<&str>,
    ) -> CompletedCall {
        let output = match (outcome, material) {
            (AttemptOutcome::Completed(_), Some(text)) => ToolCallOutput::success(text),
            (other, _) => ToolCallOutput::failure(ToolFailure::runtime(
                ToolFailureClass::Execution,
                "bench_unsettled",
                format!("{other:?}"),
            )),
        };
        CompletedCall {
            call_id: call.call_id.clone(),
            provider_call_id: call.provider_call_id.clone(),
            tool_name: call.tool_name.clone(),
            args: call.args.clone(),
            model_return: ModelToolReturn::from_output(call.tool_name.clone(), &output),
            output,
            intent_outcomes: Vec::new(),
            replay: call.replay.clone(),
        }
    }
}

fn digest(payload: &str) -> Result<MaterialDigest, ExecutionHostError> {
    use sha2::Digest as _;
    let hex: String = sha2::Sha256::digest(payload.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    MaterialDigest::parse(&hex).map_err(|error| ExecutionHostError::new(format!("{error:?}")))
}

/// `ext.echo`: a `Once` host operation the cell awaits; it answers its
/// arguments, padding included.
struct CellEcho {
    session: SessionId,
    run: TurnId,
}

impl CellEcho {
    fn material(
        &self,
        role: MaterialRole,
        payload: &str,
    ) -> Result<MaterialRef, ExecutionHostError> {
        Ok(MaterialRef {
            owner: MaterialOwner::Run {
                opener: EffectOpener::turn(self.session.clone(), self.run.clone()),
            },
            role,
            location: MaterialLocation::JournalLocal,
            digest: digest(payload)?,
        })
    }
}

impl CellOperations for CellEcho {
    fn resolve(
        &self,
        call: ToolCallId,
        operation: &ResourceOperation,
    ) -> Result<ResolvedOperation, ExecutionHostError> {
        if operation.operation != "echo" {
            return Err(ExecutionHostError::new("only ext.echo is declared"));
        }
        let args = serde_json::to_value(&operation.args)
            .map_err(|error| ExecutionHostError::new(error.to_string()))?;
        let request = args.to_string();
        let draft = ExecutionDraft::new(
            call,
            ToolId::new(CELL_TOOL),
            self.material(MaterialRole::PreparedRequest, &request)?,
            ExecutionPolicy::Once,
            ExecutionLimit {
                expires_at: u64::MAX,
                max_slice: Duration::from_secs(600),
            },
            None,
        );
        let output = request;
        let material = self.material(MaterialRole::AttemptOutput, &output)?;
        Ok(ResolvedOperation {
            draft,
            body: Box::new(move |_cancel| {
                Box::pin(async move {
                    BodyOutput {
                        outcome: AttemptOutcome::Completed(material),
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
            (outcome, _) => Err(ExecutionHostError::new(format!(
                "ext.echo ended {outcome:?}"
            ))),
        }
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new([(ToolId::new(CELL_TOOL), ExecutionPolicy::Once)])
    }
}

fn host_environment() -> Result<lashlang::LashlangHostEnvironment, String> {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["ext"],
            "Ext",
            "echo",
            "tool:ext/echo",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "i": { "type": "number" }, "pad": { "type": "string" } },
                    "required": ["i", "pad"]
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
