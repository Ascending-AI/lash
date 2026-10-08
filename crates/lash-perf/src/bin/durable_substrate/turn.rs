//! The scripted turn: the production session activation and phase runner,
//! with only the protocol, the model and the tool bodies the bench's.
//!
//! The model returns immediately. Following the L12a fixture, it answers a
//! turn's call `r` with `tools_per_round` calls of `benchmark_echo` while
//! `r < rounds`, and then with a text answer. A cell session's turns run
//! behind the facade instead ([`crate::cells`]).
//! A turn starts from its session's committed head, so a session's prior
//! turns are in the transcript it starts, checkpoints and commits.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{CommitBudget, EffectId, Response};
use lash_core::llm::types::LlmContentBlock;
use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::session::{
    AdmittedInputs, CellExit, CodeCell, ComposedCall, ModelCallAttempt, ModelPin, OpenTurn,
    PreparedCall, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore, TurnRow, TurnServices,
};
use lash_core::sansio::{ChatContextProjector, PendingToolCall, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, LlmOutputPart, LlmRequest, LlmResponse,
    Message, MessageRole, Part, ProtocolTurnOptions, TurnMachine, TurnMachineConfig,
    facade_support::TurnFinish, facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, CompletedCall, Material, MemberBody, MemberPin, MemberResult, PolicyView,
    RoundTools, SettledOutput,
};
use lash_core_execution::{ActorContext, Backend};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{CompletionSource, MaterialOwner, MaterialRole};
use lash_durable::ActorKey;
use lash_sansio::llm::types::{ProviderRouteIdentity, RecordedRequestTemplate};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionBudgets, ExecutionPolicy, ModelToolReturn, SessionId, ToolCallOutput, ToolFailure,
    ToolFailureClass, ToolId, TurnId,
};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::recorder::Recorder;

const ECHO: &str = "benchmark_echo";
const RESULTS: &str = "bench-results:";
/// A turn's text answer.
pub const ANSWER: &str = "baseline complete";
/// What a turn's admitted input says, before its session's id.
const ADMISSION_PREFIX: &str = "run the benchmark for ";

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

    pub(crate) fn get(&self, session: &SessionId) -> Script {
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

/// Send `run` to `session` with one user input through `producer`, as a
/// host's `send()` does: the input's row and the session actor's wake in
/// one transaction, creating the actor on the session's first input.
///
/// # Errors
///
/// The store refused the input.
pub async fn admit(producer: &Backend, session: &SessionId, run: &TurnId) -> anyhow::Result<()> {
    producer
        .session_store_factory()
        .enqueue_pending_turn_input(
            lash_core_execution::PendingTurnInputDraft::new(
                session.clone(),
                lash_core_execution::TurnInputIngress::NextTurn,
                lash_core_execution::TurnInput::text(format!("{ADMISSION_PREFIX}{session}")),
            )
            .with_source_key(run.as_str()),
        )
        .await
        .map(drop)
        .map_err(|error| anyhow::anyhow!("admit {run}: {error}"))
}

/// The session a rendered request's admitted input names.
pub fn session_of_admission(rendered: &str) -> Option<SessionId> {
    let named = rendered.split(ADMISSION_PREFIX).nth(1)?.split('"').next()?;
    SessionId::try_from(named.to_owned()).ok()
}

/// The messages `row`'s turn starts with: one user message per input its
/// admission took, read back from the session's store.
async fn admitted_messages(backend: &Backend, row: &TurnRow) -> Result<Vec<Message>, TurnError> {
    let catalog = backend.session_store_factory();
    let mut messages = Vec::with_capacity(row.admission.input_ids().len());
    for input in row.admission.input_ids() {
        let read = catalog
            .pending_turn_input(&row.session, input)
            .await
            .map_err(|error| TurnError::Exec(error.to_string()))?
            .ok_or_else(|| TurnError::Exec(format!("input {input} is not stored")))?;
        let text: String = read
            .input
            .input
            .items
            .iter()
            .filter_map(|item| match item {
                lash_core_execution::InputItem::Text { text } => Some(text.as_str()),
                lash_core_execution::InputItem::Attachment { .. } => None,
            })
            .collect();
        messages.push(user_message(format!("input-{input}"), text));
    }
    Ok(messages)
}

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

/// The scripted protocol: tool calls start their round, any other answer
/// finishes the turn; results join the transcript and the model is called
/// again.
#[derive(Debug)]
struct Protocol;

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
            return vec![DriverAction::Finish(TurnOutcome::Finished(
                TurnFinish::AssistantMessage {
                    text: response_text(&llm_response),
                },
            ))];
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
        after_work(user_message(id, text))
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
                    lash_sansio::llm::capability::CacheRetention::Short,
                )
                .with_capability(lash_core::LlmProfileCapability::default())
                .with_extra_body(Default::default()),
            ),
        )
        .with_reasoning(Default::default()),
        turn_budget: lash_core::TurnBudget::bounded(64),
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
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
    /// The node's backend: what an admission's inputs are read from.
    backend: Backend,
    recorder: Arc<Recorder>,
    scripts: Arc<Scripts>,
    driver: Arc<Protocol>,
    /// The services a cell session runs with ([`crate::cells`]).
    cells: Arc<dyn TurnServices>,
}

impl BenchServices {
    /// Services over `backend`, reporting to `recorder`, running `scripts`,
    /// with a cell session's turns on `cells`.
    pub fn new(
        backend: Backend,
        recorder: Arc<Recorder>,
        scripts: Arc<Scripts>,
        cells: Arc<dyn TurnServices>,
    ) -> Self {
        Self {
            backend,
            recorder,
            scripts,
            driver: Arc::new(Protocol),
            cells,
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

    fn runs_cells(&self, session: &SessionId) -> bool {
        self.scripts.get(session).cell_calls > 0
    }
}

#[async_trait::async_trait]
impl TurnServices for BenchServices {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        ExecutionBudgets::recommended()
    }

    async fn start(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        if self.runs_cells(&row.session) {
            return self.cells.start(cx, row, head).await;
        }
        let window = head.window()?;
        let messages = window.then(admitted_messages(&self.backend, row).await?);
        let machine = TurnMachine::in_window(
            machine_config(&row.session, &row.run, &self.driver),
            window,
            messages,
            Vec::new(),
            0,
        );
        Ok(self.drive(row, machine))
    }

    async fn resume(
        &self,
        cx: &ActorContext,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        if self.runs_cells(&restore.row().session) {
            return self.cells.resume(cx, restore).await;
        }
        let row = restore.row().clone();
        let restored = restore
            .restore(machine_config(&row.session, &row.run, &self.driver))
            .await?;
        Ok(OpenTurn {
            drive: self.drive(&restored.row, restored.machine),
            pending: restored.pending,
            row: restored.row,
        })
    }

    async fn apply_commands(
        &self,
        cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError> {
        self.cells.apply_commands(cx, admitted).await
    }
}

struct BenchDrive {
    services: BenchServices,
    session: SessionId,
    run: TurnId,
    script: Script,
    machine: TurnMachine,
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
                            tool_specs: Arc::new(Vec::new()),
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

    /// The scenario composes no sections: its request is lowered as its
    /// canonical encoding.
    async fn prepare_call(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _call: u32,
        request: Arc<LlmRequest>,
    ) -> Result<PreparedCall, TurnError> {
        let route = ProviderRouteIdentity {
            provider: "perf".into(),
            endpoint: "https://perf.test/v1".into(),
            model: "scripted".into(),
        };
        let template = RecordedRequestTemplate::of_request(route, &request)
            .map_err(|error| TurnError::Exec(error.to_string()))?;
        Ok(PreparedCall::Admit(Box::new(ComposedCall {
            request,
            prompt: None,
            template,
        })))
    }

    async fn model_call(
        &mut self,
        _cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        _admitted: &lash_sansio::llm::types::AdmittedSend,
        attempt: ModelCallAttempt,
    ) -> Result<(), TurnError> {
        let ModelCallAttempt {
            ordinal: attempt,
            cancel,
            ..
        } = attempt;
        self.services.recorder.model_call(&self.session);
        let script = self.script;
        let results_marker = format!("{RESULTS}{} ", self.run);
        let rounds_done = text_of(&request, &results_marker);
        let call = rounds_done;
        if script.hold_call == Some(call)
            && attempt == 1
            && let Some(reached) = self.services.scripts.take_hold(&self.session)
        {
            let _ = reached.send(());
            cancel.cancelled().await;
            return Ok(());
        }
        let parts = if rounds_done < script.rounds {
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
                text: ANSWER.to_owned(),
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

    async fn restart_live_stream(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _pin: &ModelPin,
    ) -> Result<(), TurnError> {
        Ok(())
    }

    fn tools(&mut self) -> Result<Arc<dyn RoundTools>, TurnError> {
        Ok(Arc::new(EchoTools {
            session: self.session.clone(),
            run: self.run.clone(),
        }))
    }

    async fn exec_cell(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _cell: CodeCell,
    ) -> Result<CellExit, TurnError> {
        // The scripted protocol starts no cell: a cell session runs behind
        // the facade.
        Err(TurnError::Exec(format!(
            "the scripted session {} runs no cell",
            self.session
        )))
    }

    async fn finish(
        &mut self,
        _cx: &ActorContext,
        done: TurnDone,
        head: &SessionHead,
    ) -> Result<TurnCommit, TurnError> {
        head.commit(&self.run, done, commit_budget()).await
    }

    /// No cell runs, so none is stopped.
    fn stop_cell(&mut self) {}

    /// Nothing is held for the commit.
    async fn committed(&mut self) {}
}

fn output_material(session: &SessionId, run: &TurnId, text: String) -> Material {
    Material::journal_local(
        MaterialOwner::Run {
            opener: EffectOpener::turn(session.clone(), run.clone()),
        },
        MaterialRole::AttemptOutput,
        text,
    )
}

/// `benchmark_echo`: a `Once` tool that answers its arguments at once.
struct EchoTools {
    session: SessionId,
    run: TurnId,
}

impl RoundTools for EchoTools {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn pin(&self, _call: &PendingToolCall, now_ms: u64) -> MemberPin {
        // The echo answers at once: its host sets it a short body bound.
        MemberPin::admitted(
            ToolId::new(ECHO),
            ExecutionPolicy::Once,
            lash_sansio::ToolBounds {
                execution: std::time::Duration::from_secs(30),
                park: None,
            },
            now_ms,
        )
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new([(ToolId::new(ECHO), ExecutionPolicy::Once)])
    }

    fn resolved(
        &self,
        _call: &PendingToolCall,
        _execution: &AdmittedExecution,
        _parked: &Material<CompletionSource>,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> SettledOutput {
        // No tool of this catalog parks, so no park ever resolves.
        SettledOutput::Interrupted
    }

    fn body(&self, call: &PendingToolCall, _execution: &AdmittedExecution) -> MemberBody {
        let output = output_material(&self.session, &self.run, call.args.to_string());
        Box::new(move |_token| {
            Box::pin(async move { MemberResult::from(SettledOutput::Completed(output)) })
        })
    }

    fn completed(&self, call: &PendingToolCall, output: &SettledOutput) -> CompletedCall {
        let output = match output {
            SettledOutput::Completed(material) => ToolCallOutput::success(material.payload()),
            other => ToolCallOutput::failure(ToolFailure::runtime(
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
