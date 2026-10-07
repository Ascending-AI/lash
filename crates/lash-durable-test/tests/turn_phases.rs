//! L3 (FIG-5172): a turn's phases on the production session activation,
//! cut at every commit label.
//!
//! One session holds one admitted input. A scripted model answers its first
//! call with "again", which closes the protocol iteration, and its second
//! with a final answer. The matrix cuts the uncut run at every labelled
//! write under fail-before, ack-hidden, zombie, abort and commit-then-abort,
//! recovers on the other node, and checks:
//!
//! - **Model re-send:** every attempt of one model call carries the same
//!   request bytes, attempts count up from where the pin left them, and an
//!   attempt after the first restarts the session's live stream first. A
//!   crash after `model.start` commits re-sends the call as attempt 2.
//! - **Atomic turn progress:** the head advances once, with the terminal.
//! - **F1:** a zombie's owner writes after its reap are refused.
//! - **NR-4:** no outcome is looked up for re-running code, and a claim
//!   restores the turn from its checkpoint at most once.
//! - **L-C1:** with a model total shorter than a failover, a call pinned by
//!   a node that died is never sent again: its deadline is not refreshed.
//! - **Turn cancel:** a cancel requested while the model streams ends the
//!   turn `Cancelled` in one `turn.cancel` commit with no head advance, and
//!   no model call starts after the request, whatever node finalizes it.
//! - **After-step cancel:** an `AfterStep` cancel requested from outside the
//!   actor while the model streams lets that call finish, and the turn stops
//!   at the next phase boundary, before its next model call.
//! - **Poison (FIG-5230):** a turn whose checkpoint does not decode fails
//!   every pass of its claim; the session parks at the activation-loop
//!   budget instead of looping.
//! - **Owner-cached head (FIG-5207):** when another writer moves the session
//!   head while the turn runs, the head commit over the head the owner
//!   cached is refused once, the cache is evicted, and the turn commits over
//!   the moved head.
//! - **Queued withdraw (FIG-5262):** a host's cancel of a second input,
//!   queued behind the running turn, answers `Withdrawn`, and that input
//!   never runs.
//! - **Withdraw or cancel (FIG-5262):** a cancel sent at the admission cut
//!   either withdraws the input, which never runs, or cancels the run that
//!   took it: never both, never neither.
//! - **Admission at `model.start` (FIG-5255):** every new model call composes
//!   its prompt before admission and a resend never does. IDENTITY: the
//!   turn's calls are 1 and 2 in order, and every attempt of a call is that
//!   call. FENCE and CUT-ACK: every attempt the model receives is of an
//!   admitted call, whose row pins that call with the composition the
//!   attempt carries, so no owner sends what it did not admit and a lost
//!   acknowledgement sends the admitted composition. A cut at `model.start`
//!   whose commit landed composes the call once; one whose commit did not
//!   land composes it again.

// Test code.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/seed.rs"]
mod seed;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::{EffectId, Response};
use lash_core::runtime::durable::head::{HeadCache, SessionHead};
use lash_core::runtime::durable::session::{
    AdmittedInputs, CellExit, CodeCell, ComposedCall, OpenTurn, PhaseCheckpoint, SessionActivation,
    SessionParkReason, TurnCancelRequest, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore,
    TurnRow, TurnServices, UnfinishedPhase, request_turn_cancel,
};
use lash_core::sansio::PendingToolCall;
use lash_core::sansio::{ChatContextProjector, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, Message, MessageRole, Part,
    ProtocolTurnOptions, TurnMachine, TurnMachineConfig, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, CompletedCall, MemberBody, MemberPin, PolicyView, RoundTools,
};
use lash_core_execution::{ActorContext, Backend};
use lash_core_store::store::{AdmittedInputIds, AdmittedTurnRows, RunAdmissionRecord};
use lash_durable::domain::{MailAnswer, MailDomainWrite, TurnCancelAnswer, TurnWrite};
use lash_durable::runner::Activation;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DomainRefusal, DomainWrite, DurableError, DurableStore,
    MailTx,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionBudgets, ExecutionBudgetsConfig, ExecutionLimit, ProviderAttemptLimits, SessionId,
    TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId,
};

use dialect::Dialect;

const SESSION: &str = "l3-session";
const RUN: &str = "l3-turn";
const AGAIN: &str = "again";
const AGAIN_MARKER: &str = "l3-again-";
/// What starts the instructions a call's composition lowers into its
/// request.
const COMPOSED: &str = "l3-composed";

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn run() -> TurnId {
    TurnId::try_from(RUN.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// What the scenario varies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Two model calls, then the commit.
    Plain,
    /// The first model call's deadline (5 s) is shorter than a failover.
    ShortDeadline,
    /// The first model call requests the turn's cancel, then streams until
    /// it is dropped.
    CancelWhileStreaming,
    /// The first model call requests an `AfterStep` cancel of the turn, then
    /// answers "again", which would call the model a second time.
    AfterStepWhileStreaming,
    /// Another writer moves the session head while the second model call
    /// streams its first attempt.
    HeadMovesUnderTheTurn,
    /// A second input waits behind the turn; the turn's first model call
    /// has the host cancel it, then the turn runs as [`Mode::Plain`].
    QueuedCancel,
    /// A host cancels the turn's run as its admission is cut; the model
    /// streams until a cancel stops it.
    CancelAtAdmission,
}

const QUEUED_RUN: &str = "l3-queued-turn";

fn queued_run() -> TurnId {
    TurnId::try_from(QUEUED_RUN.to_owned()).unwrap()
}

/// The scripted protocol: an "again" answer closes the iteration and calls
/// the model again; any other answer finishes the turn with it.
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
        ctx: DriverContextView<'_>,
        _request: Arc<LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        llm_response: LlmResponse,
        _calls: &lash_sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<DriverAction> {
        let text = response_text(&llm_response);
        if text != AGAIN {
            return vec![DriverAction::Finish(TurnOutcome::Finished(
                TurnFinish::AssistantMessage { text },
            ))];
        }
        let id = format!("{AGAIN_MARKER}{}", ctx.protocol_iteration());
        let message = Message {
            id: id.clone(),
            role: MessageRole::Assistant,
            parts: shared_parts(vec![Part::text(format!("{id}.p0"), id.clone(), None)]),
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

    fn handle_tool_results(
        &self,
        _ctx: DriverContextView<'_>,
        _completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<DriverAction> {
        Vec::new()
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

/// One attempt the model saw.
#[derive(Clone, Debug)]
struct Call {
    /// The run that called.
    run: TurnId,
    /// Whether the transcript already held the first answer: which call of
    /// the turn this is.
    second: bool,
    attempt: u32,
    request: String,
    /// The composition the attempt carries: its call and render.
    composed: Option<Composition>,
    /// The call the turn's row pinned when the attempt was sent, and
    /// whether its stored checkpoint carries the attempt's composition.
    admitted: Option<(u32, bool)>,
    /// Whether the turn's cancel had been requested when it started.
    after_cancel: bool,
}

/// One composition of a model call's prompt: the call it composed, and the
/// render, counted across every node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Composition {
    call: u32,
    render: u64,
}

impl Composition {
    fn instructions(self) -> String {
        format!("{COMPOSED} call={} render={}", self.call, self.render)
    }

    /// The composition `request` carries.
    fn of(request: &LlmRequest) -> Option<Self> {
        let text = request.instructions.as_deref()?.strip_prefix(COMPOSED)?;
        let mut fields = text.split_whitespace().map(|field| field.split_once('='));
        let call = match fields.next()? {
            Some(("call", call)) => call.parse().ok()?,
            _ => return None,
        };
        let render = match fields.next()? {
            Some(("render", render)) => render.parse().ok()?,
            _ => return None,
        };
        Some(Self { call, render })
    }
}

/// What every node's services saw, kept across nodes.
#[derive(Debug, Default)]
struct Seen {
    calls: Vec<Call>,
    /// Every composition, in the order the owners composed them.
    compositions: Vec<Composition>,
    restarts: Vec<u32>,
    cancel_requested: bool,
    /// Calls that requested the cancel, and how many of them answered.
    cancel_requests: usize,
    answered_after_request: usize,
    /// The head revisions another writer moved the head from and to.
    moved: Option<(u64, u64)>,
    /// The input the host cancels, as the store accepted it.
    queued: Option<lash_core::InputId>,
    /// What the host's cancel of the queued input answered.
    queued_cancel: Option<lash_core::facade_support::TurnCancelOutcome>,
    /// What the cancel sent at the admission cut answered.
    admission_cancel: Option<Result<TurnCancelAnswer, String>>,
}

#[derive(Clone)]
struct L3Services {
    mode: Mode,
    seen: Arc<Mutex<Seen>>,
    backend: Arc<Mutex<Option<Backend>>>,
}

fn machine_config(session: &SessionId, run: &TurnId) -> TurnMachineConfig {
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: Arc::new(ScriptedProtocol),
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("l3-model"),
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
        agent_frame_id: "l3-frame".to_string(),
        turn_id: run.clone(),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

#[async_trait::async_trait]
impl TurnServices for L3Services {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        match self.mode {
            Mode::ShortDeadline => {
                let total = Duration::from_secs(5);
                ExecutionBudgets::new(ExecutionBudgetsConfig {
                    model_total: total,
                    provider: ProviderAttemptLimits::new(total, total, total, 1)
                        .expect("provider limits"),
                    ..ExecutionBudgetsConfig::default()
                })
                .expect("budgets")
            }
            Mode::Plain
            | Mode::CancelWhileStreaming
            | Mode::AfterStepWhileStreaming
            | Mode::HeadMovesUnderTheTurn
            | Mode::QueuedCancel
            | Mode::CancelAtAdmission => ExecutionBudgets::default(),
        }
    }

    async fn start(
        &self,
        _cx: &ActorContext,
        row: &TurnRow,
        head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        // The turn starts from the session head's window, with the messages
        // its admission took.
        let window = head.window()?;
        let messages = window.then(seed::admitted_messages(&self.backend(), row).await?);
        let machine = TurnMachine::in_window(
            machine_config(&row.session, &row.run),
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
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let row = restore.row().clone();
        let restored = restore
            .restore(machine_config(&row.session, &row.run))
            .await?;
        Ok(OpenTurn {
            drive: self.drive(&restored.row, restored.machine),
            pending: restored.pending,
            row: restored.row,
        })
    }

    async fn apply_commands(
        &self,
        _cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError> {
        Err(TurnError::Exec(format!(
            "the scenario sends no command, yet run {} is one",
            admitted.run
        )))
    }
}

impl L3Services {
    fn drive(&self, row: &TurnRow, machine: TurnMachine) -> Box<dyn TurnDrive> {
        Box::new(L3Drive {
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

/// One turn of the scenario: the scripted model and environment.
struct L3Drive {
    services: L3Services,
    run: TurnId,
    machine: TurnMachine,
}

#[async_trait::async_trait]
impl TurnDrive for L3Drive {
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

    /// A composition the scenario can tell apart: its call and a render
    /// count across every node, lowered into the request's instructions.
    async fn compose_call(
        &mut self,
        _cx: &ActorContext,
        call: u32,
        request: Arc<LlmRequest>,
    ) -> Result<Result<ComposedCall, lash_core::LlmCallError>, TurnError> {
        let composition = {
            let mut seen = self.services.seen.lock_recover();
            let composition = Composition {
                call,
                render: u64::try_from(seen.compositions.len()).unwrap() + 1,
            };
            seen.compositions.push(composition);
            composition
        };
        let mut request = LlmRequest::clone(&request);
        request.instructions = Some(Arc::from(composition.instructions()));
        Ok(Ok(ComposedCall {
            request: Arc::new(request),
            records: Vec::new(),
        }))
    }

    async fn model_call(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        attempt: u32,
        _limit: ExecutionLimit,
    ) -> Result<(), TurnError> {
        let rendered = serde_json::to_string(&*request).expect("a request encodes");
        let second = rendered.contains(AGAIN_MARKER);
        let composed = Composition::of(&request);
        // What the rows admitted when this attempt is sent.
        let admitted =
            cx.durable_reads()?
                .turn(&session())
                .await?
                .and_then(|row| match &row.phase {
                    UnfinishedPhase::Model { pin, checkpoint } => Some((
                        pin.call,
                        composed
                            .is_some_and(|composed| checkpoint.contains(&composed.instructions())),
                    )),
                    UnfinishedPhase::Admitted | UnfinishedPhase::Tools { .. } => None,
                });
        if self.services.mode == Mode::HeadMovesUnderTheTurn && second && attempt == 1 {
            let backend = self.services.backend();
            let from = SessionHead::load(&backend, &session())
                .await
                .expect("the head loads")
                .revision();
            let to = move_head(
                &backend,
                "l3-other-writer",
                vec![session_message(
                    "l3-other-note",
                    MessageRole::User,
                    "a note another writer committed while the turn ran",
                )],
            )
            .await;
            self.services.seen.lock_recover().moved = Some((from, to));
        }
        let first_streaming_call = {
            let mut seen = self.services.seen.lock_recover();
            let after_cancel = seen.cancel_requested;
            seen.calls.push(Call {
                run: self.run.clone(),
                second,
                attempt,
                request: rendered,
                composed,
                admitted,
                after_cancel,
            });
            let cancels = matches!(
                self.services.mode,
                Mode::CancelWhileStreaming | Mode::AfterStepWhileStreaming
            );
            cancels && !after_cancel
        };
        if first_streaming_call {
            let mode = match self.services.mode {
                Mode::AfterStepWhileStreaming => TurnCancelMode::AfterStep,
                _ => TurnCancelMode::Immediate,
            };
            let answer = request_turn_cancel(
                &self.services.backend(),
                TurnCancelRequest {
                    session: session(),
                    run: run(),
                    request_id: "l3-cancel".to_owned(),
                    origin: None,
                    reason: Some("the host cancelled".to_owned()),
                    undelivered: TurnCancelUndeliveredInputPolicy::Defer,
                    mode,
                },
            )
            .await;
            if answer.is_ok() {
                let mut seen = self.services.seen.lock_recover();
                seen.cancel_requested = true;
                seen.cancel_requests += 1;
            }
            if mode == TurnCancelMode::Immediate {
                // The stream never ends on its own: only the cancel stops it.
                return std::future::pending().await;
            }
            // Outlive at least one wake of the owner's cancel watch before
            // answering: an after-step request must not stop the call. The
            // wait is on the deployment's virtual clock, so the run's shape
            // does not turn on the host's load.
            lash_core_ids::clock::Clock::sleep(&**cx.clock(), Duration::from_millis(50)).await;
            self.services.seen.lock_recover().answered_after_request += 1;
        }
        match self.services.mode {
            // The stream never ends on its own: only the cancel stops it.
            Mode::CancelAtAdmission => return std::future::pending().await,
            // While its turn runs, the host withdraws the queued input,
            // once, as Figments withdraws a chat message the user took back.
            Mode::QueuedCancel if self.run == run() => {
                let unasked = self.services.seen.lock_recover().queued_cancel.is_none();
                if unasked {
                    let request = lash_core::facade_support::TurnCancelRequest::new(
                        lash_core::facade_support::TurnAddress::new(session(), queued_run()),
                        "l3-withdraw",
                        None,
                    );
                    let receipt =
                        lash_core::facade_support::TurnWorkDriver::new(self.services.backend())
                            .request_cancel(request)
                            .await
                            .map_err(|error| TurnError::Exec(error.to_string()))?;
                    self.services.seen.lock_recover().queued_cancel = Some(receipt.outcome);
                }
            }
            _ => {}
        }
        let text = if second { "done" } else { AGAIN };
        self.machine.handle_response(Response::LlmComplete {
            id,
            result: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: text.to_owned(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
            text_streamed: false,
        });
        Ok(())
    }

    fn tools(&mut self) -> Result<Arc<dyn RoundTools>, TurnError> {
        Ok(Arc::new(NoTools))
    }

    async fn restart_live_stream(&mut self, _cx: &ActorContext) -> Result<(), TurnError> {
        let mut seen = self.services.seen.lock_recover();
        let next = seen.calls.last().map_or(0, |call| call.attempt);
        seen.restarts.push(next);
        Ok(())
    }

    async fn exec_cell(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _cell: CodeCell,
    ) -> Result<CellExit, TurnError> {
        Err(TurnError::Exec("the L3 scenario runs no cell".to_owned()))
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

/// The L3 scenario's catalog: its protocol never calls a tool.
struct NoTools;

impl RoundTools for NoTools {
    fn pin(&self, _call: &PendingToolCall, _now_ms: u64) -> MemberPin {
        unreachable!("the L3 scenario calls no tool")
    }

    fn policies(&self) -> PolicyView {
        PolicyView::default()
    }

    fn body(&self, _call: &PendingToolCall, _execution: &AdmittedExecution) -> MemberBody {
        unreachable!("the L3 scenario calls no tool")
    }

    fn resolved(
        &self,
        _call: &PendingToolCall,
        _execution: &AdmittedExecution,
        _parked: &lash_core_execution::runtime::actor::round::Material<
            lash_core_store::tool_run::CompletionSource,
        >,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::SettledOutput {
        unreachable!("the L3 scenario calls no tool")
    }

    fn completed(
        &self,
        _call: &PendingToolCall,
        _output: &lash_core_execution::runtime::actor::round::SettledOutput,
    ) -> CompletedCall {
        unreachable!("the L3 scenario calls no tool")
    }
}

fn commit_budget() -> lash_core::facade_support::CommitBudget {
    lash_core::facade_support::CommitBudget::bounded(1024 * 1024, 512)
}

/// The L3 scenario on one dialect, fresh for every matrix cell.
struct L3 {
    mode: Mode,
    dialect: Dialect,
    postgres_url: Option<String>,
    seen: Arc<Mutex<Seen>>,
    tripwire: Arc<Tripwire>,
    backend: Arc<Mutex<Option<Backend>>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl L3 {
    fn new(mode: Mode, dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            mode,
            dialect,
            postgres_url,
            seen: Arc::default(),
            tripwire: Arc::default(),
            backend: Arc::default(),
            keep: Mutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl Scenario for L3 {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(sim::backend(stores));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self
                .backend
                .lock_recover()
                .as_ref()
                .expect("the database is built first")
                .formats()
                .decodes(),
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
            Arc::new(L3Services {
                mode: self.mode,
                seen: Arc::clone(&self.seen),
                backend: Arc::clone(&self.backend),
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
        let input = seed::send_turn(&backend, &session(), &run(), "think twice").await?;
        match self.mode {
            Mode::QueuedCancel => {
                let queued =
                    seed::queue_turn(&backend, &session(), &queued_run(), "taken back").await?;
                self.seen.lock_recover().queued = Some(queued);
            }
            Mode::CancelAtAdmission => self.seen.lock_recover().queued = Some(input),
            _ => {}
        }
        if self.mode == Mode::CancelAtAdmission {
            cancel_at_admission(Arc::clone(nodes), Arc::clone(&self.seen));
        }
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
        // No row stays bound to an ended run, however it ended: the session
        // admits its next input (FIG-5172).
        match database.session_mailbox(&session()).await {
            Ok(mailbox) if mailbox.bound_run.is_none() => {}
            other => violations.push(format!("the ended run still holds its rows: {other:?}")),
        }

        // NR-4: no outcome lookups for re-running code; at most one restore
        // from the checkpoint per node that took the turn over.
        if counts.outcome_lookups.values().sum::<usize>() != 0 {
            violations.push("NR-4: an outcome was looked up for re-running code".to_owned());
        }
        let restores = counts
            .restores
            .get(&(session(), run()))
            .copied()
            .unwrap_or(0);
        if restores > 1 {
            violations.push(format!("NR-4: the turn was restored {restores} times"));
        }

        // Model re-send: one request per call, attempts counting up, and a
        // live restart before every attempt after the first.
        for second in [false, true] {
            let attempts: Vec<&Call> = seen.calls.iter().filter(|c| c.second == second).collect();
            if let Some(first) = attempts.first()
                && attempts.iter().any(|call| call.request != first.request)
            {
                violations.push(format!(
                    "re-send: call {} was sent with different request bytes",
                    u8::from(second) + 1
                ));
            }
            let numbers: Vec<u32> = attempts.iter().map(|call| call.attempt).collect();
            if numbers.windows(2).any(|pair| pair[1] <= pair[0]) {
                violations.push(format!(
                    "re-send: call {} attempts went {numbers:?}",
                    u8::from(second) + 1
                ));
            }
        }
        let resent: Vec<u32> = seen
            .calls
            .iter()
            .filter(|call| call.attempt > 1)
            .map(|call| call.attempt)
            .collect();
        if seen.restarts.len() != resent.len() {
            violations.push(format!(
                "live incarnation: {} re-sent attempts {resent:?} but {} live restarts",
                resent.len(),
                seen.restarts.len()
            ));
        }

        violations.extend(admission_laws(&seen));
        // Uncut, every call composes once: nothing resends.
        if cut.is_none() {
            for call in [1, 2] {
                let composed = compositions_of(&seen, call);
                if composed > 1 {
                    violations.push(format!(
                        "admission: uncut call {call} composed {composed} times"
                    ));
                }
            }
        }

        match self.mode {
            Mode::Plain | Mode::ShortDeadline | Mode::HeadMovesUnderTheTurn => {
                // Atomic turn progress: one head advance, with its terminal.
                let commits = committed(CommitLabel::TURN_COMMIT);
                if commits != 1 {
                    violations.push(format!("the turn committed {commits} times"));
                }
            }
            Mode::CancelWhileStreaming | Mode::AfterStepWhileStreaming => {
                let commits = committed(CommitLabel::TURN_COMMIT);
                let cancels = committed(CommitLabel::TURN_CANCEL);
                if commits != 0 || cancels != 1 {
                    violations.push(format!(
                        "cancel: {commits} head commits and {cancels} cancel terminals"
                    ));
                }
                if seen.calls.iter().any(|call| call.after_cancel) {
                    violations.push(format!(
                        "cancel: a model call started after the request: {:?}",
                        seen.calls
                    ));
                }
                // A call dies with its node; only a cancel must not stop it.
                let killed = cut.is_some_and(|cut| {
                    matches!(
                        cut.fault,
                        Fault::Abort | Fault::CommitThenAbort | Fault::Zombie
                    )
                });
                if self.mode == Mode::AfterStepWhileStreaming
                    && !killed
                    && (seen.cancel_requests == 0
                        || seen.answered_after_request != seen.cancel_requests)
                {
                    violations.push(format!(
                        "after-step: {} calls requested the cancel but {} answered",
                        seen.cancel_requests, seen.answered_after_request
                    ));
                }
            }
            Mode::QueuedCancel => {
                // The running turn commits once; the queued input never runs.
                let commits = committed(CommitLabel::TURN_COMMIT);
                if commits != 1 {
                    violations.push(format!("the turn committed {commits} times"));
                }
                violations.extend(queued_withdraw_laws(database.as_ref(), &seen).await);
            }
            Mode::CancelAtAdmission => {
                violations
                    .extend(withdraw_or_cancel_laws(database.as_ref(), &trace, &seen, cut).await);
            }
        }

        if self.mode == Mode::HeadMovesUnderTheTurn {
            let backend = self.backend.lock_recover().clone().expect("the backend");
            violations.extend(head_moved_laws(&backend, &trace, &seen).await);
        }

        if let Some(cut) = cut {
            violations.extend(cut_laws(self.mode, cut, &seen));
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// Queued withdraw (FIG-5262): the host's cancel of the input queued behind
/// the running turn answered `Withdrawn`, and the input never ran: no run
/// took it, no model call was made for its run, and the session holds no
/// open mail.
async fn queued_withdraw_laws(database: &dyn DurableStore, seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    match &seen.queued_cancel {
        Some(lash_core::facade_support::TurnCancelOutcome::Withdrawn { input })
            if Some(input) == seen.queued.as_ref() => {}
        other => violations.push(format!(
            "queued withdraw: the cancel of queued input {:?} answered {other:?}",
            seen.queued
        )),
    }
    if seen.calls.iter().any(|call| call.run == queued_run()) {
        violations.push(format!(
            "queued withdraw: the withdrawn input's run called the model: {:?}",
            seen.calls
        ));
    }
    match database.turn_end(&session(), &queued_run()).await {
        Ok(None) => {}
        other => violations.push(format!(
            "queued withdraw: the withdrawn input's run ended: {other:?}"
        )),
    }
    match database.session_mailbox(&session()).await {
        Ok(mailbox) if mailbox.inputs.is_empty() => {}
        other => violations.push(format!(
            "queued withdraw: the session still holds open input: {other:?}"
        )),
    }
    violations
}

/// Send the host's cancel of the turn's run at the cut of its admission, or,
/// uncut, once the admission left the store: from a producer outside every
/// node, as Figments cancels an input it queued (FIG-5262).
fn cancel_at_admission(nodes: Arc<SimNodes>, seen: Arc<Mutex<Seen>>) {
    tokio::spawn(async move {
        let script = nodes.script();
        let admitted = async {
            loop {
                let trace = script.trace();
                if trace.iter().any(|write| {
                    write.point.label == CommitLabel::TURN_ADMIT && write.stored != Stored::Pending
                }) {
                    return;
                }
                let settled = trace
                    .iter()
                    .filter(|write| write.stored != Stored::Pending)
                    .count();
                script.settled(settled + 1).await;
            }
        };
        tokio::select! {
            _ = script.first_cut() => {}
            () = admitted => {}
        }
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::RequestTurnCancel(TurnCancelRequest {
            session: session(),
            run: run(),
            request_id: "l3-at-admission".to_owned(),
            origin: None,
            reason: Some("the host took the input back".to_owned()),
            undelivered: TurnCancelUndeliveredInputPolicy::Defer,
            mode: TurnCancelMode::Immediate,
        }));
        let answer = match nodes
            .producer("host")
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
        {
            Ok(mut commit) => match commit.answers.pop() {
                Some(MailAnswer::RequestTurnCancel(answer)) => Ok(answer),
                other => Err(format!("the cancel was answered {other:?}")),
            },
            Err(error) => Err(error.to_string()),
        };
        seen.lock_recover().admission_cancel = Some(answer);
    });
}

/// Withdraw or cancel (FIG-5262): the cancel sent at the admission cut did
/// exactly one thing. Withdrawn, no admission of the input landed, its run
/// never called the model and has no end; a cancelled run, the input's
/// one admission landed and its run ended `Cancelled` with no head commit.
/// A cut that holds the admission out of the store until a failover leaves
/// the withdraw to win, and an uncut cancel, sent once the admission left
/// the store, cancels the run.
async fn withdraw_or_cancel_laws(
    database: &dyn DurableStore,
    trace: &[lash_durable_test::Write],
    seen: &Seen,
    cut: Option<&Cut>,
) -> Vec<String> {
    let mut violations = Vec::new();
    let committed = |label: CommitLabel| {
        trace
            .iter()
            .filter(|write| write.point.label == label && write.committed())
            .count()
    };
    let (admits, cancels, commits) = (
        committed(CommitLabel::TURN_ADMIT),
        committed(CommitLabel::TURN_CANCEL),
        committed(CommitLabel::TURN_COMMIT),
    );
    let end = database.turn_end(&session(), &run()).await;
    let ended_cancelled = matches!(
        &end,
        Ok(Some(end)) if matches!(end.cause, lash_core_store::store::RunTerminalCause::Cancelled { .. })
    );
    match &seen.admission_cancel {
        Some(Ok(TurnCancelAnswer::Withdrawn { input })) => {
            if Some(input) != seen.queued.as_ref() {
                violations.push(format!(
                    "withdraw: {input} was withdrawn, not the input {:?}",
                    seen.queued
                ));
            }
            if admits != 0 || cancels != 0 || commits != 0 || !seen.calls.is_empty() {
                violations.push(format!(
                    "both: the input was withdrawn, yet {admits} admissions, {cancels} cancel \
                     terminals, {commits} head commits and {} model calls landed",
                    seen.calls.len()
                ));
            }
            if !matches!(end, Ok(None)) {
                violations.push(format!("both: the withdrawn input's run ended: {end:?}"));
            }
        }
        Some(Ok(TurnCancelAnswer::Requested)) => {
            if admits != 1 || cancels != 1 || commits != 0 || !ended_cancelled {
                violations.push(format!(
                    "cancelled run: {admits} admissions, {cancels} cancel terminals and \
                     {commits} head commits landed, and the run ended {end:?}"
                ));
            }
        }
        other => violations.push(format!(
            "neither: the cancel at the admission cut answered {other:?}"
        )),
    }
    let withdrawn = matches!(
        seen.admission_cancel,
        Some(Ok(TurnCancelAnswer::Withdrawn { .. }))
    );
    match cut.map(|cut| cut.fault) {
        Some(Fault::Abort | Fault::Zombie) if !withdrawn => violations.push(format!(
            "an admission held out of the store did not leave the withdraw to win: {:?}",
            seen.admission_cancel
        )),
        None if withdrawn => violations
            .push("a cancel sent after the admission left the store withdrew its input".to_owned()),
        _ => {}
    }
    violations
}

/// Admission at `model.start` (FIG-5255), in every cell.
///
/// - IDENTITY: the turn's first call is call 1 and its second call 2, and
///   every attempt of a call carries that call's one composition.
/// - FENCE and CUT-ACK: every attempt the model receives is of an admitted
///   call: the turn's row, read as the attempt is sent, pins that call and
///   stores the composition the attempt carries.
fn admission_laws(seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    for call in &seen.calls {
        let ordinal = if call.second { 2 } else { 1 };
        match call.composed {
            Some(composed) if composed.call == ordinal => {}
            other => violations.push(format!(
                "IDENTITY: attempt {} of call {ordinal} carried composition {other:?}",
                call.attempt
            )),
        }
        if call.admitted != Some((ordinal, true)) {
            violations.push(format!(
                "FENCE: attempt {} of call {ordinal} was sent while the rows admitted {:?}",
                call.attempt, call.admitted
            ));
        }
    }
    for ordinal in [1, 2] {
        let mut sent = seen
            .calls
            .iter()
            .filter(|call| {
                call.composed
                    .is_some_and(|composed| composed.call == ordinal)
            })
            .filter_map(|call| call.composed);
        if let Some(first) = sent.next()
            && sent.any(|other| other != first)
        {
            violations.push(format!(
                "IDENTITY: call {ordinal} was sent with two compositions: {:?}",
                seen.calls
            ));
        }
    }
    violations
}

/// How often call `call` composed its prompt.
fn compositions_of(seen: &Seen, call: u32) -> usize {
    seen.compositions
        .iter()
        .filter(|composition| composition.call == call)
        .count()
}

/// The laws of one cut: a node killed after `model.start` committed, and
/// how often a call cut at its `model.start` composed (FIG-5255).
fn cut_laws(mode: Mode, cut: &Cut, seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
    if mode == Mode::Plain && cut.point.label == CommitLabel::MODEL_START {
        // Uncut up to its cut, a plain turn's nth `model.start` admits its
        // nth call. A crash after admission resends the admitted call and
        // composes nothing; one before it composes the call again.
        let call = u32::try_from(cut.point.nth).unwrap();
        let expected = match cut.fault {
            Fault::CommitThenAbort | Fault::AckHidden => Some(1),
            Fault::FailBefore | Fault::Abort => Some(2),
            _ => None,
        };
        let composed = compositions_of(seen, call);
        if let Some(expected) = expected
            && composed != expected
        {
            violations.push(format!(
                "admission: call {call} cut at model.start under {:?} composed {composed} times, \
                 not {expected}",
                cut.fault
            ));
        }
    }
    let killed_after_pin =
        cut.point.label == CommitLabel::MODEL_START && matches!(cut.fault, Fault::CommitThenAbort);
    if !killed_after_pin {
        return violations;
    }
    let max_attempt = seen
        .calls
        .iter()
        .map(|call| call.attempt)
        .max()
        .unwrap_or(0);
    match mode {
        Mode::Plain => {
            if max_attempt != 2 {
                violations.push(format!(
                    "re-send: a call pinned by a killed node was not re-sent as attempt 2: {:?}",
                    seen.calls
                ));
            }
        }
        Mode::ShortDeadline => {
            if max_attempt > 1 {
                violations.push(format!(
                    "L-C1: a call whose pinned deadline passed was sent again: {:?}",
                    seen.calls
                ));
            }
        }
        Mode::CancelWhileStreaming
        | Mode::AfterStepWhileStreaming
        | Mode::HeadMovesUnderTheTurn
        | Mode::QueuedCancel
        | Mode::CancelAtAdmission => {}
    }
    violations
}

/// FIG-5207: the turn's head commit over the head its owner cached is
/// refused once the head moved; the owner evicts the head, reloads it, and
/// the turn commits over the moved head. No commit over the stale head
/// lands.
async fn head_moved_laws(
    backend: &Backend,
    trace: &[lash_durable_test::Write],
    seen: &Seen,
) -> Vec<String> {
    let Some((from, to)) = seen.moved else {
        return vec!["owner cache: the head never moved under the turn".to_owned()];
    };
    let mut violations = Vec::new();
    let refused: Vec<&lash_durable_test::Write> = trace
        .iter()
        .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && !write.committed())
        .collect();
    let stale = refused
        .iter()
        .filter(|write| {
            matches!(
                &write.stored,
                Stored::Refused(DurableError::Domain(DomainRefusal::HeadMoved { expected, .. }))
                    if *expected == from
            )
        })
        .count();
    if stale != 1 || refused.len() != 1 {
        violations.push(format!(
            "owner cache: the head commit over the cached head {from} was refused {stale} times \
             among {} refused head commits: {refused:?}",
            refused.len()
        ));
    }
    let head = SessionHead::load(backend, &session())
        .await
        .expect("the head loads")
        .revision();
    if head != to + 1 {
        violations.push(format!(
            "owner cache: the head is at {head}, not one past the moved head {to}"
        ));
    }
    violations
}

/// F1: once a zombie's actors moved, every owner write it attempts is
/// refused with `OwnershipLost`, so nothing it wrote is visible.
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

async fn prove(mode: Mode, labels: &[CommitLabel], dialect: Dialect) {
    prove_on(matrix(), mode, labels, dialect).await;
}

async fn prove_on(matrix: Matrix, mode: Mode, labels: &[CommitLabel], dialect: Dialect) {
    let postgres_url = match dialect {
        Dialect::Postgres => match dialect::postgres_url() {
            Some(url) => Some(url),
            None => {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                return;
            }
        },
        Dialect::SqliteMemory | Dialect::SqliteFile => None,
    };
    let make = || L3::new(mode, dialect, postgres_url.clone());
    let report = if mode == Mode::ShortDeadline {
        // L-C1 pins a 5s model deadline that must expire during failover;
        // keep the default's 17.25s recovery bound for this timing law.
        matrix
            .lease(lash_durable::LeaseConfig::default())
            .run(make)
            .await
    } else {
        matrix.run_test(make).await
    };
    eprintln!(
        "L3 {mode:?} on {dialect:?}: {} cells over labels {:?}",
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

const PLAIN: &[CommitLabel] = &[
    CommitLabel::TURN_ADMIT,
    CommitLabel::MODEL_START,
    CommitLabel::TURN_COMMIT,
];
const SHORT_DEADLINE: &[CommitLabel] = &[CommitLabel::MODEL_START];
const CANCEL: &[CommitLabel] = &[CommitLabel::MODEL_START, CommitLabel::TURN_CANCEL];

/// A turn of two model calls holds the re-send, atomic-progress, F1 and
/// NR-4 laws at every commit label.
#[tokio::test]
async fn a_turn_of_two_model_calls_resumes_at_every_label_without_replay() {
    prove(Mode::Plain, PLAIN, Dialect::SqliteMemory).await;
}

#[tokio::test]
async fn a_turn_of_two_model_calls_resumes_at_every_label_without_replay_on_sqlite_file() {
    prove(Mode::Plain, PLAIN, Dialect::SqliteFile).await;
}

#[tokio::test]
async fn a_turn_of_two_model_calls_resumes_at_every_label_without_replay_on_postgres() {
    prove(Mode::Plain, PLAIN, Dialect::Postgres).await;
}

/// L-C1: a call whose node died after pinning it is not sent again once its
/// deadline passed; the deadline is not refreshed on resume.
#[tokio::test]
async fn a_model_call_whose_pinned_deadline_passed_is_never_sent_again() {
    prove(Mode::ShortDeadline, SHORT_DEADLINE, Dialect::SqliteMemory).await;
}

#[tokio::test]
async fn a_model_call_whose_pinned_deadline_passed_is_never_sent_again_on_sqlite_file() {
    prove(Mode::ShortDeadline, SHORT_DEADLINE, Dialect::SqliteFile).await;
}

#[tokio::test]
async fn a_model_call_whose_pinned_deadline_passed_is_never_sent_again_on_postgres() {
    prove(Mode::ShortDeadline, SHORT_DEADLINE, Dialect::Postgres).await;
}

/// A cancel requested while the model streams ends the turn `Cancelled`, at
/// every cut, with no model call after the request.
#[tokio::test]
async fn a_cancel_while_streaming_ends_the_turn_and_a_crash_mid_cancel_finalizes_it() {
    prove(Mode::CancelWhileStreaming, CANCEL, Dialect::SqliteMemory).await;
}

#[tokio::test]
async fn a_cancel_while_streaming_ends_the_turn_and_a_crash_mid_cancel_finalizes_it_on_sqlite_file()
{
    prove(Mode::CancelWhileStreaming, CANCEL, Dialect::SqliteFile).await;
}

#[tokio::test]
async fn a_cancel_while_streaming_ends_the_turn_and_a_crash_mid_cancel_finalizes_it_on_postgres() {
    prove(Mode::CancelWhileStreaming, CANCEL, Dialect::Postgres).await;
}

/// An `AfterStep` cancel requested from outside the actor while the model
/// streams lets the call finish and ends the turn `Cancelled` at the next
/// phase boundary, before its next model call, at every cut.
#[tokio::test]
async fn an_after_step_cancel_lets_the_streaming_call_finish_and_stops_at_the_next_boundary() {
    prove(Mode::AfterStepWhileStreaming, CANCEL, Dialect::SqliteMemory).await;
}

#[tokio::test]
async fn an_after_step_cancel_lets_the_streaming_call_finish_and_stops_at_the_next_boundary_on_sqlite_file()
 {
    prove(Mode::AfterStepWhileStreaming, CANCEL, Dialect::SqliteFile).await;
}

#[tokio::test]
async fn an_after_step_cancel_lets_the_streaming_call_finish_and_stops_at_the_next_boundary_on_postgres()
 {
    prove(Mode::AfterStepWhileStreaming, CANCEL, Dialect::Postgres).await;
}

/// The labels the queued withdraw is cut at: the running turn's commit,
/// after which the session's next drain finds the withdrawn input.
const QUEUED: &[CommitLabel] = &[CommitLabel::TURN_COMMIT];

/// Queued withdraw (FIG-5262): a host's cancel of an input queued behind
/// the running turn answers `Withdrawn`, and the input never runs, at every
/// cut of the running turn's commit.
#[tokio::test]
async fn a_cancelled_queued_input_is_withdrawn_and_never_runs() {
    prove_on(
        matrix().labels(QUEUED),
        Mode::QueuedCancel,
        QUEUED,
        Dialect::SqliteMemory,
    )
    .await;
}

#[tokio::test]
async fn a_cancelled_queued_input_is_withdrawn_and_never_runs_on_sqlite_file() {
    prove_on(
        matrix().labels(QUEUED),
        Mode::QueuedCancel,
        QUEUED,
        Dialect::SqliteFile,
    )
    .await;
}

#[tokio::test]
async fn a_cancelled_queued_input_is_withdrawn_and_never_runs_on_postgres() {
    prove_on(
        matrix().labels(QUEUED),
        Mode::QueuedCancel,
        QUEUED,
        Dialect::Postgres,
    )
    .await;
}

/// The admission is the cut the withdraw races.
const ADMISSION: &[CommitLabel] = &[CommitLabel::TURN_ADMIT];

/// Withdraw or cancel (FIG-5262): a cancel sent at every cut of the input's
/// admission withdraws the input or cancels the run that took it, exactly
/// one of the two.
#[tokio::test]
async fn a_cancel_at_the_admission_cut_withdraws_or_cancels_exactly_once() {
    prove_on(
        matrix().labels(ADMISSION),
        Mode::CancelAtAdmission,
        ADMISSION,
        Dialect::SqliteMemory,
    )
    .await;
}

#[tokio::test]
async fn a_cancel_at_the_admission_cut_withdraws_or_cancels_exactly_once_on_sqlite_file() {
    prove_on(
        matrix().labels(ADMISSION),
        Mode::CancelAtAdmission,
        ADMISSION,
        Dialect::SqliteFile,
    )
    .await;
}

#[tokio::test]
async fn a_cancel_at_the_admission_cut_withdraws_or_cancels_exactly_once_on_postgres() {
    prove_on(
        matrix().labels(ADMISSION),
        Mode::CancelAtAdmission,
        ADMISSION,
        Dialect::Postgres,
    )
    .await;
}

/// C1 (FIG-5230): a session whose unfinished turn names a checkpoint no
/// build decodes. Its first claim leaves that row, as a defect would; from
/// then on every pass of the production activation fails restoring it.
struct Poisoned {
    dialect: Dialect,
    postgres_url: Option<String>,
    tripwire: Arc<Tripwire>,
    backend: Arc<Mutex<Option<Backend>>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl Poisoned {
    fn new(dialect: Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            tripwire: Arc::default(),
            backend: Arc::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }
}

/// The session activation, after the first claim writes the poisoned row.
struct PoisonFirst {
    poisoned: std::sync::atomic::AtomicBool,
    session: SessionActivation,
}

#[async_trait::async_trait]
impl Activation for PoisonFirst {
    async fn activate(&self, owned: lash_durable::runner::Owned) -> lash_durable::runner::Exit {
        if !self
            .poisoned
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let mut tx = owned
                .begin()
                .await
                .expect("the first claim reads its actor");
            tx.write(DomainWrite::Turn(TurnWrite::Admit {
                session: session(),
                run: run(),
                admission: RunAdmissionRecord::Turn {
                    took: AdmittedTurnRows::Batch {
                        id: lash_core::BatchId::from("poisoned-batch"),
                    },
                },
                turn_deadline: None,
            }));
            tx.write(DomainWrite::Turn(TurnWrite::Advance {
                session: session(),
                run: run(),
                phase: UnfinishedPhase::Tools {
                    run: lash_durable::domain::RunSeq(1),
                    checkpoint: "not a turn checkpoint".to_owned(),
                },
                iteration: 0,
            }));
            owned
                .commit(tx, CommitLabel::TURN_ADMIT)
                .await
                .expect("the poisoned row commits");
        }
        self.session.activate(owned).await
    }
}

#[async_trait::async_trait]
impl Scenario for Poisoned {
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
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(PoisonFirst {
            poisoned: std::sync::atomic::AtomicBool::new(false),
            session: SessionActivation::new(
                self.backend(),
                Arc::new(L3Services {
                    mode: Mode::Plain,
                    seen: Arc::default(),
                    backend: Arc::clone(&self.backend),
                }),
                Arc::clone(&self.tripwire) as _,
            ),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        seed::send_turn(&self.backend(), &session(), &run(), "never runs").await?;
        nodes.start("a");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Parked
        )
    }

    async fn check(&self, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let budget = self.backend().config().settings().activation_loop_budget;
        let snapshot = match nodes.database().actor(&actor()).await {
            Ok(Some(snapshot)) => snapshot,
            other => return vec![format!("the session's actor is gone: {other:?}")],
        };
        let mut violations = Vec::new();
        if snapshot.state != ActorState::Parked {
            violations.push(format!("the session is {:?}, not parked", snapshot.state));
        }
        let reason = snapshot
            .park
            .as_deref()
            .map(serde_json::from_str::<SessionParkReason>);
        match reason {
            Some(Ok(SessionParkReason::PassLoop {
                failed_passes,
                error,
            })) if failed_passes == budget && error.contains("does not decode") => {}
            other => violations.push(format!(
                "the session parked for {other:?}, not after {budget} undecodable restores"
            )),
        }
        violations
    }
}

/// C1 (FIG-5230): a session whose checkpoint does not decode fails every
/// pass, and parks at the activation-loop budget instead of looping on its
/// claim.
async fn prove_poisoned(dialect: Dialect) {
    let postgres_url = match dialect {
        Dialect::Postgres => match dialect::postgres_url() {
            Some(url) => Some(url),
            None => {
                eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
                return;
            }
        },
        Dialect::SqliteMemory | Dialect::SqliteFile => None,
    };
    let report = Matrix::new()
        .faults(&[])
        .horizon(Duration::from_secs(600))
        .run_test(|| Poisoned::new(dialect, postgres_url.clone()))
        .await;
    report.assert_held();
}

#[tokio::test]
async fn an_undecodable_checkpoint_parks_the_session_after_the_budget() {
    prove_poisoned(Dialect::SqliteMemory).await;
}

#[tokio::test]
async fn an_undecodable_checkpoint_parks_the_session_after_the_budget_on_sqlite_file() {
    prove_poisoned(Dialect::SqliteFile).await;
}

#[tokio::test]
async fn an_undecodable_checkpoint_parks_the_session_after_the_budget_on_postgres() {
    prove_poisoned(Dialect::Postgres).await;
}

/// A message of the scenario's session.
fn session_message(id: &str, role: MessageRole, text: &str) -> Message {
    Message {
        id: id.to_owned(),
        role,
        parts: shared_parts(vec![Part::text(format!("{id}.p0"), text.to_owned(), None)]),
        origin: None,
        reply_marker: None,
    }
}

/// Commit `messages` over the session head, as run `run` answering
/// would: the head moves to its next revision, which it answers.
async fn move_head(backend: &Backend, run: &str, messages: Vec<Message>) -> u64 {
    let commit = SessionHead::load(backend, &session())
        .await
        .expect("the head loads")
        .commit(
            &TurnId::try_from(run.to_owned()).unwrap(),
            TurnDone {
                messages: messages.into(),
                event_delta: Vec::new(),
                protocol_iteration: 0,
                outcome: Some(TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: format!("{run} answered"),
                })),
            },
            commit_budget(),
        )
        .await
        .expect("the head commit builds");
    let catalog: Arc<dyn lash_core_store::store::RuntimeStore> = backend.session_store_factory();
    catalog
        .commit_runtime_state(
            lash_core_store::store::decode_session_commit(&commit.commit_json)
                .expect("the head commit decodes"),
        )
        .await
        .expect("the head commit applies");
    commit.expected_head + 1
}

/// FIG-5206: a turn's checkpoint pins the committed window the turn started
/// from instead of holding it. A restore after the session head has moved
/// past the pinned revision reads the window at the pin and rebuilds exactly
/// the turn's starting messages and history, so the pinned model request is
/// re-delivered byte-identical.
async fn restore_after_the_head_moved(dialect: Dialect, postgres_url: Option<String>) {
    let keep = Mutex::default();
    let (stores, _database) =
        dialect::open(dialect, postgres_url.as_deref(), SimClock::new(), &keep).await;
    let backend = Backend::for_testing(stores);
    let catalog: Arc<dyn lash_core_store::store::RuntimeStore> = backend.session_store_factory();
    lash_core_store::testing::store_fixtures::admit_conformance_session(&catalog, &session()).await;
    let earlier = vec![
        session_message("earlier-question", MessageRole::User, "an earlier question"),
        session_message(
            "earlier-answer",
            MessageRole::Assistant,
            "an earlier answer",
        ),
    ];
    move_head(&backend, "l3-earlier", earlier.clone()).await;

    // The turn starts from the head's window and waits on its model call.
    let head = SessionHead::load(&backend, &session())
        .await
        .expect("the head loads");
    let window = head.window().expect("the head's window");
    let mut machine = TurnMachine::in_window(
        machine_config(&session(), &run()),
        window.clone(),
        window.then(vec![session_message(
            "l3-input",
            MessageRole::User,
            "the turn's own input",
        )]),
        Vec::new(),
        0,
        Vec::new(),
    );
    let pinned = loop {
        match machine
            .poll_effect()
            .expect("the machine reaches its model call")
        {
            Effect::SyncExecutionEnvironment { id } => {
                machine.handle_response(Response::ExecutionEnvironmentSynced {
                    id,
                    result: Ok(ExecutionEnvironmentSync::default()),
                });
            }
            Effect::LlmCall { request, .. } => break request,
            _ => {}
        }
    };
    let checkpoint = serde_json::to_string(&PhaseCheckpoint {
        saved: machine.checkpoint(),
        plugin_state: None,
        delivered: Vec::new(),
        delivered_work: Vec::new(),
    })
    .expect("checkpoint encodes");

    // The head moves past the pinned revision while the turn is open.
    let mut later = earlier;
    later.push(session_message(
        "later-note",
        MessageRole::User,
        "a note committed after the turn started",
    ));
    move_head(&backend, "l3-later", later).await;
    let moved = SessionHead::load(&backend, &session())
        .await
        .expect("the head loads");
    assert!(
        moved.revision() > head.revision(),
        "the head moved past {}",
        head.revision()
    );

    let row = TurnRow {
        session: session(),
        run: run(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Batch {
                id: lash_core::BatchId::from("l3-batch"),
            },
        },
        phase: UnfinishedPhase::Model {
            pin: lash_durable::domain::ModelPin {
                call: 1,
                attempt: 1,
                request_ref: "pinned".to_owned(),
                deadline: lash_durable::DurableInstant(i64::MAX),
            },
            checkpoint,
        },
        iteration: 0,
        model_calls: 1,
        turn_deadline: None,
        written_epoch: lash_durable::Epoch(1),
        cancel: None,
    };
    let cx = ActorContext::detached(backend.clone());
    let mut heads = HeadCache::default();
    let restored = TurnRestore::new(&cx, &row, &mut heads)
        .restore(machine_config(&session(), &run()))
        .await
        .expect("the turn restores over its pinned window");
    assert_eq!(
        serde_json::to_string(restored.machine.messages().as_slice()).unwrap(),
        serde_json::to_string(machine.messages().as_slice()).unwrap(),
        "the restored turn starts from its own window, not the moved head's"
    );
    assert_eq!(
        serde_json::to_string(restored.machine.events().as_slice()).unwrap(),
        serde_json::to_string(machine.events().as_slice()).unwrap(),
        "the restored turn's history is its window's"
    );
    let Some(Effect::LlmCall { request, .. }) = restored.pending else {
        panic!("the restored turn re-delivers its model call");
    };
    assert_eq!(
        serde_json::to_string(request.as_ref()).unwrap(),
        serde_json::to_string(pinned.as_ref()).unwrap(),
        "the re-delivered model request is the pinned one"
    );
}

#[tokio::test]
async fn a_restore_after_the_head_moved_rebuilds_the_window_the_turn_started_from() {
    restore_after_the_head_moved(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn a_restore_after_the_head_moved_rebuilds_the_window_the_turn_started_from_on_sqlite_file() {
    restore_after_the_head_moved(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn a_restore_after_the_head_moved_rebuilds_the_window_the_turn_started_from_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    restore_after_the_head_moved(Dialect::Postgres, Some(url)).await;
}

/// FIG-5207: a head commit refused because another writer moved the head
/// under the turn evicts the owner's cached head, and the turn reloads it
/// and commits over the moved head, never over the stale one.
#[tokio::test]
async fn a_head_commit_refused_on_a_moved_head_evicts_the_cached_head_and_the_turn_commits_over_the_moved_one()
 {
    prove_on(
        Matrix::new().faults(&[]),
        Mode::HeadMovesUnderTheTurn,
        &[],
        Dialect::SqliteMemory,
    )
    .await;
}

#[tokio::test]
async fn a_head_commit_refused_on_a_moved_head_evicts_the_cached_head_and_the_turn_commits_over_the_moved_one_on_sqlite_file()
 {
    prove_on(
        Matrix::new().faults(&[]),
        Mode::HeadMovesUnderTheTurn,
        &[],
        Dialect::SqliteFile,
    )
    .await;
}

#[tokio::test]
async fn a_head_commit_refused_on_a_moved_head_evicts_the_cached_head_and_the_turn_commits_over_the_moved_one_on_postgres()
 {
    prove_on(
        Matrix::new().faults(&[]),
        Mode::HeadMovesUnderTheTurn,
        &[],
        Dialect::Postgres,
    )
    .await;
}

/// Send `session` its turn's first input, under source key `run`, claim its
/// actor on a node of its own and admit run `run` with that input, binding
/// it the way the session actor's mail drain does. Answers the claim's
/// epoch.
async fn admit_running_turn(
    backend: &Backend,
    database: &dyn DurableStore,
    session: &SessionId,
    run: &TurnId,
) -> lash_durable::Epoch {
    use lash_core_execution::{PendingTurnInputDraft, TurnInput, TurnInputIngress};
    let catalog: Arc<dyn lash_core_store::store::RuntimeStore> = backend.session_store_factory();
    lash_core_store::testing::store_fixtures::admit_conformance_session(&catalog, session).await;
    let first = catalog
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                session.clone(),
                TurnInputIngress::NextTurn,
                TurnInput::text("go"),
            )
            .with_source_key(run.as_str()),
        )
        .await
        .expect("the turn's input is accepted");
    let admission = RunAdmissionRecord::Turn {
        took: AdmittedTurnRows::Inputs {
            ids: AdmittedInputIds::new(vec![first.input_id.clone()]).unwrap(),
        },
    };
    use lash_durable::domain::{DomainWrite, SessionMailWrite, TurnWrite};
    let actor = ActorKey::session(session.as_str()).unwrap();
    let snapshot = database
        .actor(&actor)
        .await
        .unwrap()
        .expect("the input woke the session");
    let lease = database
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new(format!("owner-of-{session}")),
            decodes: vec![snapshot.formats.clone()],
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = database.claim(&lease, 1).await.unwrap();
    assert_eq!(claimed.len(), 1, "the woken session was not claimed");
    let mut tx = database.begin(&actor, claimed[0].epoch).await.unwrap();
    tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
        session: session.clone(),
        run: run.clone(),
        inputs: vec![first.input_id],
        batches: Vec::new(),
    }));
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session.clone(),
        run: run.clone(),
        admission,
        turn_deadline: None,
    }));
    tx.ack_seen();
    database
        .commit(tx, CommitLabel::TURN_ADMIT)
        .await
        .expect("the turn is admitted");
    claimed[0].epoch
}

/// FIG-5221: input a host sends to a running durable turn is admitted as
/// that turn's active-turn input. The store reads the run's admission, as
/// the session actor recorded it, to know which turns the run executes.
async fn input_to_a_running_turn_is_admitted(dialect: Dialect, postgres_url: Option<String>) {
    use lash_core_execution::{PendingTurnInputDraft, TurnInput, TurnInputIngress, TurnInputState};
    let keep = Mutex::default();
    let (stores, database) =
        dialect::open(dialect, postgres_url.as_deref(), SimClock::new(), &keep).await;
    let backend = Backend::for_testing(stores);
    admit_running_turn(&backend, database.as_ref(), &session(), &run()).await;

    let catalog: Arc<dyn lash_core_store::store::RuntimeStore> = backend.session_store_factory();
    let steered = catalog
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            session(),
            TurnInputIngress::active_turn(run(), Default::default()),
            TurnInput::text("steer"),
        ))
        .await
        .expect("input to the running turn is admitted");
    assert!(
        matches!(steered.state, TurnInputState::PendingActive(_))
            && steered.state.active_turn_id() == Some(&run()),
        "the steering input waits for the running turn: {steered:?}"
    );
}

#[tokio::test]
async fn input_to_a_running_turn_is_admitted_on_sqlite_memory() {
    input_to_a_running_turn_is_admitted(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn input_to_a_running_turn_is_admitted_on_sqlite_file() {
    input_to_a_running_turn_is_admitted(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn input_to_a_running_turn_is_admitted_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    input_to_a_running_turn_is_admitted(Dialect::Postgres, Some(url)).await;
}

/// FIG-5221: every kind a session actor's turn ends as is stored from its
/// typed cause and read back through the host's `terminal_of` without a
/// decode error: an answer, a failure and a cancel.
async fn each_turn_terminal_reads_back(dialect: Dialect, postgres_url: Option<String>) {
    use lash_core::facade_support::{TurnAddress, TurnStop, TurnTerminal, TurnWorkDriver};
    use lash_core::runtime::durable::session::cancel_evidence;
    use lash_core_store::store::{RunCommittedOutcome, RunTerminalCause, TurnCommitId};
    use lash_durable::domain::{DomainWrite, TurnWrite};
    let keep = Mutex::default();
    let (stores, database) =
        dialect::open(dialect, postgres_url.as_deref(), SimClock::new(), &keep).await;
    let backend = Backend::for_testing(stores);
    let evidence = cancel_evidence(&TurnCancelRequest {
        session: session(),
        run: run(),
        request_id: "cancel".to_owned(),
        origin: None,
        reason: Some("the host cancelled".to_owned()),
        undelivered: TurnCancelUndeliveredInputPolicy::Defer,
        mode: TurnCancelMode::Immediate,
    });
    let committed = |outcome| RunTerminalCause::Committed {
        commit: TurnCommitId::new(run(), 0),
        turn: run(),
        outcome,
    };
    let ends = [
        (
            "answered",
            committed(RunCommittedOutcome::Finished(
                TurnFinish::AssistantMessage {
                    text: "done".to_owned(),
                },
            )),
            None,
        ),
        (
            "failed",
            committed(RunCommittedOutcome::Stopped(TurnStop::ToolFailure)),
            Some(TurnStop::ToolFailure),
        ),
        (
            "cancelled",
            RunTerminalCause::Cancelled {
                evidence: evidence.clone(),
            },
            Some(TurnStop::Cancelled { evidence }),
        ),
    ];
    for (kind, cause, stop) in ends {
        let session = SessionId::try_from(format!("terminal-{kind}")).unwrap();
        let epoch = admit_running_turn(&backend, database.as_ref(), &session, &run()).await;
        let actor = ActorKey::session(session.as_str()).unwrap();
        let mut tx = database.begin(&actor, epoch).await.unwrap();
        tx.write(DomainWrite::Turn(TurnWrite::Terminal {
            session: session.clone(),
            run: run(),
            cause: Box::new(cause.clone()),
            head_revision: None,
        }));
        database
            .commit(tx, CommitLabel::TURN_COMMIT)
            .await
            .unwrap_or_else(|error| panic!("the {kind} terminal commits: {error}"));
        let end = database.turn_end(&session, &run()).await.unwrap();
        assert_eq!(
            end.as_ref().map(|end| (end.kind().as_str(), &end.cause)),
            Some((kind, &cause)),
            "the {kind} terminal reads back its cause"
        );
        let terminal = TurnWorkDriver::new(backend.clone())
            .await_terminal(&TurnAddress::new(session.clone(), run()))
            .await
            .unwrap_or_else(|error| panic!("the {kind} terminal decodes: {error}"));
        let TurnTerminal::Committed { stop: read } = terminal;
        assert_eq!(read, stop, "the host reads the {kind} terminal");
    }
}

#[tokio::test]
async fn each_turn_terminal_reads_back_through_the_host_on_sqlite_memory() {
    each_turn_terminal_reads_back(Dialect::SqliteMemory, None).await;
}

#[tokio::test]
async fn each_turn_terminal_reads_back_through_the_host_on_sqlite_file() {
    each_turn_terminal_reads_back(Dialect::SqliteFile, None).await;
}

#[tokio::test]
async fn each_turn_terminal_reads_back_through_the_host_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    each_turn_terminal_reads_back(Dialect::Postgres, Some(url)).await;
}
