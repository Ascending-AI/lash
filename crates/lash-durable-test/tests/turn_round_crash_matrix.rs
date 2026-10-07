//! L4 (FIG-5174): a turn's tool round on the production session
//! activation, cut at every commit label.
//!
//! One session runs one turn. The scripted model answers its first call
//! with tool calls and its second, which sees their results, with a final
//! answer. The phase runner admits the round in `model.done`, runs it with
//! the round runner, presents it with the next `model.start`
//! (`round.present+model.start`) and commits the turn. Member bodies write
//! to an [`ExternalWorld`] that survives every node, as the outside world
//! would.
//!
//! The matrix cuts the uncut run at every labelled write under fail-before,
//! ack-hidden, zombie, abort and commit-then-abort, recovers on the other
//! node, and checks:
//!
//! - **F2 / NR-1 / NR-2:** no `Once` body is entered twice; a `Once` whose
//!   outcome did not commit is `Interrupted`; a completed `Once` reached the
//!   outside world once.
//! - **NR-3:** a `Repeatable` started without an outcome reruns at its
//!   ordinal; only its known failure advances it.
//! - **NR-4:** no outcome lookup for re-running code, no committed ordinal
//!   emitted again, and a takeover restores the turn at most once.
//! - **Durable result before tool admission:** no body runs for an
//!   `x_start` that never committed, and every `x_start` commits with the
//!   turn's `model.done`.
//! - **Declared order:** the round is presented, and the next model call
//!   sees its results, in the order the model declared the calls.
//! - **Atomic turn progress:** the turn commits once.
//! - **Turn cancel:** a cancel requested while a member runs ends the
//!   unfinished members `Cancelled` and the turn `Cancelled`, with no model
//!   call after the request.
//! - **F1:** a zombie's writes after its reap are refused.

// Test code.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::{EffectId, Response};
use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::session::{
    AdmittedInputs, CodeCell, SessionActivation, TurnCancelRequest, TurnCommit, TurnDone,
    TurnDrive, TurnError, TurnRow, TurnServices, admit_mail, request_turn_cancel,
};
use lash_core::sansio::{ChatContextProjector, PendingToolCall, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, Message, MessageRole, Part,
    ProtocolTurnOptions, TurnMachine, TurnMachineConfig, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, BodyOutput, CompletedCall, MemberBody, MemberPin, MemberResult,
    PolicyView, RoundTools, RunFold,
};
use lash_core_execution::{ActorContext, Backend, StoreSet};
use lash_core_store::tool_run::{
    AttemptOutcome, KnownFailure, KnownFailureReason, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRef, MaterialRole,
};
use lash_durable::domain::{AdmittedId, ExecKey, OwnerKey, RunRecordKind, RunSeq};
use lash_durable::runner::Activation;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DomainWrite, DurableError, DurableStore, FormatSet,
    LeaseConfig, MailTx,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionBudgets, ExecutionLimit, ExecutionPolicy, ModelToolReturn, SessionId, ToolCallId,
    ToolCallOutput, ToolFailure, ToolFailureClass, ToolId, TurnCancelMode,
    TurnCancelUndeliveredInputPolicy, TurnId,
};

const FORMATS: &str = "l4t";
const SESSION: &str = "l4t-session";
const RUN: &str = "l4t-turn";
const RESULTS_MARKER: &str = "l4t-results";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

fn owner() -> OwnerKey {
    OwnerKey::Turn(session(), run())
}

/// What a member's tool does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    /// A `Once` write that completes after `millis`.
    Write { millis: u64 },
    /// A `Repeatable` write whose first attempt reports a known failure.
    Flaky,
    /// A `Once` write that waits until the turn's cancel stops it.
    Hang,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Write { millis: 0 } => "write_now",
            Self::Write { .. } => "write_slow",
            Self::Flaky => "flaky",
            Self::Hang => "hang",
        }
    }

    fn named(name: &str) -> Self {
        match name {
            "write_now" => Self::Write { millis: 0 },
            "write_slow" => Self::Write { millis: 50 },
            "flaky" => Self::Flaky,
            "hang" => Self::Hang,
            other => panic!("no tool {other}"),
        }
    }

    fn policy(self) -> ExecutionPolicy {
        match self {
            Self::Write { .. } | Self::Hang => ExecutionPolicy::Once,
            Self::Flaky => ExecutionPolicy::repeatable(NonZeroU32::new(3).unwrap(), 100, 1_000),
        }
    }
}

/// What the scenario varies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The model calls a slow `Once` write, a flaky `Repeatable` and a quick
    /// `Once` write, in that order, so their outcomes commit out of order.
    Mixed,
    /// The model calls one tool that runs until the turn's cancel, which a
    /// host requests once the member's body is running.
    CancelWhileRunning,
}

impl Mode {
    fn tools(self) -> Vec<Tool> {
        match self {
            Self::Mixed => vec![
                Tool::Write { millis: 50 },
                Tool::Flaky,
                Tool::Write { millis: 0 },
            ],
            Self::CancelWhileRunning => vec![Tool::Hang],
        }
    }
}

/// The outside world: every body entry, per call and attempt. It survives
/// every node, so a write a crash cannot undo is visible to the laws.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<BTreeMap<ToolCallId, Vec<u32>>>,
}

impl ExternalWorld {
    fn write(&self, call: &ToolCallId, attempt: u32) {
        self.writes
            .lock_recover()
            .entry(call.clone())
            .or_default()
            .push(attempt);
    }

    fn writes(&self, call: &ToolCallId) -> Vec<u32> {
        self.writes
            .lock_recover()
            .get(call)
            .cloned()
            .unwrap_or_default()
    }
}

/// The scripted protocol: a response with tool calls starts their round;
/// their results join the transcript and the model is called again; any
/// other answer finishes the turn with it.
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
                args: serde_json::from_str(&input_json).unwrap(),
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
            .map(|call| format!("{}={}", call.tool_name, render_output(&call.output)))
            .collect();
        let id = format!("{RESULTS_MARKER}-{}", ctx.protocol_iteration());
        let message = Message {
            id: id.clone(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(
                format!("{id}.p0"),
                format!("{RESULTS_MARKER}: {}", rendered.join("; ")),
                None,
            )]),
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

    fn handle_exec_result(
        &self,
        _ctx: DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        _result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<DriverAction> {
        Vec::new()
    }
}

fn render_output(output: &ToolCallOutput) -> String {
    serde_json::to_string(&output.outcome).unwrap()
}

/// What every node's drive saw, kept across nodes.
#[derive(Debug, Default)]
struct Seen {
    /// The text of each model call's request, in send order.
    requests: Vec<String>,
    /// Whether a model call started after the turn's cancel was requested.
    called_after_cancel: bool,
    cancel_requested: bool,
}

#[derive(Clone)]
struct L4Services {
    mode: Mode,
    seen: Arc<Mutex<Seen>>,
    world: Arc<ExternalWorld>,
    backend: Arc<Mutex<Option<Backend>>>,
    clock: Arc<Mutex<Option<Arc<SimClock>>>>,
}

fn machine_config(session: &SessionId, run: &TurnId) -> TurnMachineConfig {
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: Arc::new(ScriptedProtocol),
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("l4t-model"),
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
        agent_frame_id: "l4t-frame".to_string(),
        turn_id: run.clone(),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

#[async_trait::async_trait]
impl TurnServices for L4Services {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        ExecutionBudgets::default()
    }

    async fn machine_config(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
    ) -> Result<TurnMachineConfig, TurnError> {
        Ok(machine_config(&row.session, &row.run))
    }

    async fn start(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        let messages: Vec<Message> = serde_json::from_str(&row.admission_json)
            .map_err(|error| TurnError::Exec(error.to_string()))?;
        let machine = TurnMachine::new(
            machine_config(&row.session, &row.run),
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

impl L4Services {
    fn drive(&self, row: &TurnRow, machine: TurnMachine) -> Box<dyn TurnDrive> {
        Box::new(L4Drive {
            services: self.clone(),
            run: row.run.clone(),
            machine,
        })
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the backend is built")
    }
}

/// One turn of the scenario: the scripted model, environment and tools.
struct L4Drive {
    services: L4Services,
    run: TurnId,
    machine: TurnMachine,
}

#[async_trait::async_trait]
impl TurnDrive for L4Drive {
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
                            system_prompt: Arc::from("l4t"),
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
        _attempt: u32,
        _limit: ExecutionLimit,
    ) -> Result<(), TurnError> {
        let rendered = serde_json::to_string(&*request).expect("a request encodes");
        let second = rendered.contains(RESULTS_MARKER);
        {
            let mut seen = self.services.seen.lock_recover();
            if seen.cancel_requested {
                seen.called_after_cancel = true;
            }
            seen.requests.push(rendered);
        }
        let parts = if second {
            vec![LlmOutputPart::Text {
                text: "done".to_owned(),
                response_meta: None,
            }]
        } else {
            self.services
                .mode
                .tools()
                .into_iter()
                .enumerate()
                .map(|(index, tool)| LlmOutputPart::ToolCall {
                    call_id: format!("provider-{index}"),
                    tool_name: tool.name().to_owned(),
                    input_json: format!("{{\"slot\":{index}}}"),
                    replay: None,
                })
                .collect()
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
        Arc::new(Catalog {
            services: self.services.clone(),
        })
    }

    async fn exec_cell(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _exec: ExecKey,
        _cell: CodeCell,
        _with: Vec<DomainWrite>,
    ) -> Result<(), TurnError> {
        Err(TurnError::Exec("the L4 scenario runs no cell".to_owned()))
    }

    async fn finish(
        &mut self,
        _cx: &ActorContext,
        done: TurnDone,
    ) -> Result<TurnCommit, TurnError> {
        SessionHead::load(&self.services.backend(), &session(), commit_budget())
            .await?
            .commit(&self.run, done)
    }
}

fn commit_budget() -> lash_core::facade_support::CommitBudget {
    lash_core::facade_support::CommitBudget::bounded(1024 * 1024, 512)
}

/// A member's output, owned by the turn: its journal-local material.
fn output_material(text: &str) -> MaterialRef {
    MaterialPayload::new(
        MaterialOwner::Run {
            opener: lash_core_store::effect_opener::EffectOpener::turn(session(), run()),
        },
        MaterialRole::AttemptOutput,
        None,
        text.to_owned(),
    )
    .reference(MaterialLocation::JournalLocal)
    .unwrap()
}

/// The scenario's catalog: each tool's policy, body and answer.
struct Catalog {
    services: L4Services,
}

impl RoundTools for Catalog {
    fn pin(&self, call: &PendingToolCall, now_ms: u64) -> MemberPin {
        let tool = Tool::named(&call.tool_name);
        let budget = ExecutionBudgets::default().tool_default();
        MemberPin {
            tool: ToolId::new(tool.name()),
            policy: tool.policy(),
            limit: ExecutionLimit::starting_at(now_ms, budget, budget),
        }
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new(
            [
                Tool::Write { millis: 0 },
                Tool::Write { millis: 50 },
                Tool::Flaky,
                Tool::Hang,
            ]
            .into_iter()
            .map(|tool| (ToolId::new(tool.name()), tool.policy())),
        )
    }

    fn body(&self, call: &PendingToolCall, execution: &AdmittedExecution) -> MemberBody {
        let world = Arc::clone(&self.services.world);
        let services = self.services.clone();
        let clock = self.services.clock.lock_recover().clone().expect("a clock");
        let tool = Tool::named(&call.tool_name);
        let call = call.call_id.clone();
        let attempt = execution.attempt();
        Box::new(move |token| {
            Box::pin(async move {
                match tool {
                    Tool::Write { millis } if millis > 0 => {
                        lash_core_ids::clock::Clock::sleep(&*clock, Duration::from_millis(millis))
                            .await;
                    }
                    Tool::Hang => {
                        world.write(&call, attempt);
                        let answer = request_turn_cancel(
                            &services.backend(),
                            TurnCancelRequest {
                                session: session(),
                                run: run(),
                                request_id: "l4t-cancel".to_owned(),
                                origin: None,
                                reason: Some("the host cancelled".to_owned()),
                                undelivered: TurnCancelUndeliveredInputPolicy::Defer,
                                mode: TurnCancelMode::Immediate,
                            },
                        )
                        .await;
                        if answer.is_ok() {
                            services.seen.lock_recover().cancel_requested = true;
                        }
                        token.cancelled().await;
                        return MemberResult::from(BodyOutput::from(AttemptOutcome::Cancelled {
                            evidence: Default::default(),
                        }));
                    }
                    Tool::Write { .. } | Tool::Flaky => {}
                }
                world.write(&call, attempt);
                let text = format!("{}#{attempt}", tool.name());
                let outcome = if tool == Tool::Flaky && attempt == 1 {
                    AttemptOutcome::Failed(KnownFailure {
                        output: output_material(&text),
                        reason: KnownFailureReason::Reported,
                        suggested_delay_ms: None,
                    })
                } else {
                    AttemptOutcome::Completed(output_material(&text))
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
                "l4t_unsettled",
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

/// Admit the scenario's session to the catalog, at its creation head.
async fn create_session(backend: &Backend) {
    let catalog: Arc<dyn lash_core_store::store::RuntimeStore> = backend.session_store_factory();
    lash_core_store::testing::store_fixtures::admit_conformance_session(&catalog, &session()).await;
}

/// The scenario on SQLite in memory, fresh for every matrix cell.
struct L4 {
    mode: Mode,
    seen: Arc<Mutex<Seen>>,
    world: Arc<ExternalWorld>,
    tripwire: Arc<Tripwire>,
    backend: Arc<Mutex<Option<Backend>>>,
    clock: Arc<Mutex<Option<Arc<SimClock>>>>,
}

impl L4 {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            seen: Arc::default(),
            world: Arc::default(),
            tripwire: Arc::default(),
            backend: Arc::default(),
            clock: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl Scenario for L4 {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        *self.clock.lock_recover() = Some(Arc::clone(&clock));
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("an in-memory store set opens");
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let stores: Arc<dyn StoreSet> = Arc::new(stores);
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
            backend,
            Arc::new(L4Services {
                mode: self.mode,
                seen: Arc::clone(&self.seen),
                world: Arc::clone(&self.world),
                backend: Arc::clone(&self.backend),
                clock: Arc::clone(&self.clock),
            }),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let backend = self
            .backend
            .lock_recover()
            .clone()
            .expect("the database is built first");
        create_session(&backend).await;
        let admission = vec![Message {
            id: "l4t-input".to_owned(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(
                "l4t-input.p0".to_owned(),
                "use the tools".to_owned(),
                None,
            )]),
            origin: None,
            reply_marker: None,
        }];
        let inputs = AdmittedInputs {
            run: run(),
            inputs: Vec::new(),
            admission_json: serde_json::to_string(&admission).map_err(|e| e.to_string())?,
        };
        let mut seed = MailTx::new();
        seed.create_actor(actor(), FormatSet::new(FORMATS)).append(
            actor(),
            admit_mail(),
            inputs.mail_body(),
        );
        nodes
            .database()
            .commit_mail(seed, CommitLabel::MAIL_SESSION)
            .await
            .map_err(|error| error.to_string())?;
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
        let mut violations = Vec::new();
        let database = nodes.database();
        let trace = nodes.script().trace();
        let counts = self.tripwire.counts();
        let seen = std::mem::take(&mut *self.seen.lock_recover());
        let committed = |label: CommitLabel| {
            trace
                .iter()
                .filter(|write| write.point.label == label && write.committed())
                .count()
        };

        match database.turn(&session()).await {
            Ok(None) => {}
            other => violations.push(format!("the turn did not end: {other:?}")),
        }

        // NR-4.
        if !counts.outcome_lookups.is_empty() || !counts.committed_ordinals.is_empty() {
            violations.push(format!(
                "NR-4: outcome lookups {:?}, committed ordinals emitted again {:?}",
                counts.outcome_lookups, counts.committed_ordinals
            ));
        }
        let restores = counts
            .restores
            .get(&(session(), run()))
            .copied()
            .unwrap_or(0);
        if restores > 1 {
            violations.push(format!("NR-4: the turn was restored {restores} times"));
        }

        let rows = match database.run_records(&owner()).await {
            Ok(rows) => rows,
            Err(error) => return vec![format!("the run records do not read: {error}")],
        };
        let fold = match round::fold(&rows, &PolicyView::default()) {
            Ok(fold) => fold,
            Err(refusal) => return vec![format!("the run records do not fold: {refusal}")],
        };

        // Durable result before tool admission: a body runs only for an
        // x_start that committed, and every first x_start committed with
        // the turn's model.done.
        let committed_starts: BTreeSet<AdmittedId> = rows
            .iter()
            .filter(|row| row.kind == RunRecordKind::XStart)
            .map(|row| AdmittedId {
                owner: row.owner.clone(),
                run: row.run,
                ordinal: row.ordinal,
            })
            .collect();
        for (id, entries) in &counts.bodies {
            if *entries > 0 && !committed_starts.contains(id) {
                violations.push(format!(
                    "a body ran for {id:?}, whose x_start never committed"
                ));
            }
        }
        if !rows.is_empty() && committed(CommitLabel::MODEL_DONE) != 1 {
            violations.push(format!(
                "the round was admitted in {} model.done commits",
                committed(CommitLabel::MODEL_DONE)
            ));
        }

        match self.mode {
            Mode::Mixed => {
                violations.extend(mixed_laws(&fold, &self.world, &self.tripwire, &seen));
                let commits = committed(CommitLabel::TURN_COMMIT);
                if commits != 1 {
                    violations.push(format!("the turn committed {commits} times"));
                }
            }
            Mode::CancelWhileRunning => {
                violations.extend(cancel_laws(&fold, &self.world, &seen));
                // The turn ends once. A member interrupted before its body
                // asked for the cancel leaves a turn that answers and
                // commits; a member the cancel stopped leaves a cancelled
                // turn.
                let commits = committed(CommitLabel::TURN_COMMIT);
                let cancels = committed(CommitLabel::TURN_CANCEL);
                let stopped = fold.rounds().flat_map(|view| view.members()).any(|member| {
                    matches!(member.outcome(), Some(AttemptOutcome::Cancelled { .. }))
                });
                if commits + cancels != 1 || (stopped && cancels != 1) {
                    violations.push(format!(
                        "cancel: {commits} head commits and {cancels} cancel terminals"
                    ));
                }
            }
        }

        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// The calls of the round, in declared order, as the fold holds them.
fn declared(fold: &RunFold) -> Option<(RunSeq, Vec<ToolCallId>)> {
    let view = fold.rounds().next()?;
    Some((
        view.run(),
        view.members()
            .iter()
            .map(|member| member.call().clone())
            .collect(),
    ))
}

/// F2, NR-1 to NR-3 and declared order over the mixed round.
fn mixed_laws(
    fold: &RunFold,
    world: &ExternalWorld,
    tripwire: &Tripwire,
    seen: &Seen,
) -> Vec<String> {
    let mut violations = Vec::new();
    let Some((run, calls)) = declared(fold) else {
        return vec!["the round was never admitted".to_owned()];
    };
    let view = fold.round(run).unwrap();
    if view.presented() != Some(calls.as_slice()) {
        violations.push(format!(
            "the presentation {:?} is not the declared order {calls:?}",
            view.presented()
        ));
    }
    let tools = Mode::Mixed.tools();
    if view.members().len() != tools.len() {
        return vec![format!(
            "the round admitted {} members",
            view.members().len()
        )];
    }
    for (tool, member) in tools.iter().zip(view.members()) {
        let call = member.call();
        let entries: usize = member
            .starts()
            .iter()
            .map(|ordinal| {
                tripwire.bodies(&AdmittedId {
                    owner: owner(),
                    run,
                    ordinal: *ordinal,
                })
            })
            .sum();
        let writes = world.writes(call);
        let Some(outcome) = member.outcome() else {
            violations.push(format!("{call} has no final outcome"));
            continue;
        };
        match tool {
            Tool::Write { .. } | Tool::Hang => {
                if entries > 1 || writes.len() > 1 {
                    violations.push(format!(
                        "F2: Once {call} was entered {entries} times, wrote {writes:?}"
                    ));
                }
                match outcome {
                    AttemptOutcome::Completed(_) if writes.len() == 1 => {}
                    AttemptOutcome::Interrupted => {}
                    other => violations.push(format!(
                        "F2: Once {call} settled {other:?} after writing {writes:?}"
                    )),
                }
            }
            Tool::Flaky => {
                if !matches!(outcome, AttemptOutcome::Completed(_)) {
                    violations.push(format!("{call} settled {outcome:?}"));
                }
                if member.starts().len() != 2 {
                    violations.push(format!(
                        "NR-3: {call} took {} attempts; only its one known failure advances it",
                        member.starts().len()
                    ));
                }
                if writes.iter().any(|attempt| *attempt > 2) {
                    violations.push(format!("NR-3: {call} wrote {writes:?}"));
                }
            }
        }
    }
    // The model's second call sees the results in declared order.
    match seen
        .requests
        .iter()
        .find(|request| request.contains(RESULTS_MARKER))
    {
        Some(request) => {
            let positions: Vec<Option<usize>> = tools
                .iter()
                .map(|tool| request.find(&format!("{}=", tool.name())))
                .collect();
            if positions.iter().any(Option::is_none)
                || positions.windows(2).any(|pair| pair[0] >= pair[1])
            {
                violations.push(format!(
                    "the model saw the results out of declared order: {request}"
                ));
            }
        }
        None if seen.requests.is_empty() => {}
        None => violations.push("the model never saw the round's results".to_owned()),
    }
    violations
}

/// A cancel requested while a member runs ends it `Cancelled` (or
/// `Interrupted`, when its owner died first) and calls the model no more.
fn cancel_laws(fold: &RunFold, world: &ExternalWorld, seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    if seen.called_after_cancel {
        violations.push("cancel: a model call started after the request".to_owned());
    }
    let Some((run, _)) = declared(fold) else {
        return violations;
    };
    for member in fold.round(run).unwrap().members() {
        let writes = world.writes(member.call());
        if writes.len() > 1 {
            violations.push(format!("F2: Once {} wrote {writes:?}", member.call()));
        }
        match member.outcome() {
            Some(AttemptOutcome::Cancelled { .. } | AttemptOutcome::Interrupted) => {}
            other => violations.push(format!(
                "cancel: {} settled {other:?} after the turn's cancel",
                member.call()
            )),
        }
    }
    violations
}

/// F1: once a zombie's actors moved, every owner write it attempts is
/// refused with `OwnershipLost`.
fn zombie_laws(cut: &Cut, trace: &[lash_durable_test::Write]) -> Vec<String> {
    let mut violations = Vec::new();
    if cut.fault != Fault::Zombie || cut.kind != WriteKind::Actor {
        return violations;
    }
    let Some(at) = trace.iter().position(|write| {
        write.node == cut.node && write.point == cut.point && write.cut == Some(cut.fault)
    }) else {
        return vec!["F1: the zombie's cut write is not in the trace".to_owned()];
    };
    for write in trace[at..]
        .iter()
        .filter(|write| write.node == cut.node && write.kind == WriteKind::Actor)
    {
        match &write.stored {
            Stored::Refused(DurableError::OwnershipLost(_)) => {}
            other => violations.push(format!("F1: zombie write {write} was {other:?}")),
        }
    }
    violations
}

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

async fn prove(mode: Mode, labels: &[CommitLabel]) {
    let report = matrix().run(|| L4::new(mode)).await;
    eprintln!(
        "L4 turn {mode:?}: {} cells over labels {:?}",
        report.cells.len(),
        report
            .labels()
            .iter()
            .map(|label| label.as_str())
            .collect::<Vec<_>>()
    );
    report.assert_held();
    for label in labels {
        assert!(
            report.labels().contains(label),
            "the matrix never cut {label}"
        );
    }
}

/// A turn whose model calls three tools, mixing `Once` and `Repeatable`,
/// holds F2, NR-1 to NR-4, declared order and atomic progress at every
/// commit label of the turn and its round.
#[tokio::test]
async fn a_turns_tool_round_resumes_at_every_label_without_replay() {
    prove(
        Mode::Mixed,
        &[
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_START,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::TURN_COMMIT,
        ],
    )
    .await;
}

/// A turn cancel while a member runs ends the member `Cancelled` and the
/// turn `Cancelled`, at every cut, with no model call after the request.
#[tokio::test]
async fn a_cancel_while_a_member_runs_ends_the_round_and_the_turn() {
    prove(
        Mode::CancelWhileRunning,
        &[CommitLabel::MODEL_DONE, CommitLabel::TURN_CANCEL],
    )
    .await;
}
