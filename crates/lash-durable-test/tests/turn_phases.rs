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

// Test code.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::runtime::durable::session::{
    AdmittedInputs, SessionActivation, TurnAdmission, TurnCancelRequest, TurnError, TurnServices,
    admit_mail, request_turn_cancel,
};
use lash_core::sansio::{ChatContextProjector, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, ExecResponse, Message, MessageRole, Part, ProtocolTurnOptions,
    TurnMachineConfig, facade_support::TurnFinish, facade_support::TurnOutcome,
    facade_support::shared_parts,
};
use lash_core::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core_execution::{ActorContext, Backend, StoreSet};
use lash_durable::domain::ExecKey;
use lash_durable::runner::Activation;
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DomainWrite, DurableError, DurableStore, FormatSet,
    LeaseConfig, MailTx,
};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::sansio::{ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionBudgets, ExecutionBudgetsConfig, ExecutionLimit, LlmCallError, ProviderAttemptLimits,
    SessionId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId,
};

const FORMATS: &str = "l3";
const SESSION: &str = "l3-session";
const RUN: &str = "l3-turn";
const AGAIN: &str = "again";
const AGAIN_MARKER: &str = "l3-again-";

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
    /// Whether the transcript already held the first answer: which call of
    /// the turn this is.
    second: bool,
    attempt: u32,
    request: String,
    /// Whether the turn's cancel had been requested when it started.
    after_cancel: bool,
}

/// What every node's services saw, kept across nodes.
#[derive(Debug, Default)]
struct Seen {
    calls: Vec<Call>,
    restarts: Vec<u32>,
    cancel_requested: bool,
}

struct L3Services {
    mode: Mode,
    seen: Arc<Mutex<Seen>>,
    backend: Arc<Mutex<Option<Backend>>>,
}

#[async_trait::async_trait]
impl TurnServices for L3Services {
    fn machine_config(&self, session: &SessionId, run: &TurnId) -> TurnMachineConfig {
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
            Mode::Plain | Mode::CancelWhileStreaming => ExecutionBudgets::default(),
        }
    }

    async fn sync_environment(
        &self,
        _cx: &ActorContext,
        _session: &SessionId,
        _run: &TurnId,
    ) -> Result<ExecutionEnvironmentSync, ExecutionEnvironmentSyncFailure> {
        Ok(ExecutionEnvironmentSync {
            system_prompt: Arc::from("l3"),
            tool_specs: Arc::new(Vec::new()),
            projector_turn_inputs: Default::default(),
        })
    }

    async fn call_model(
        &self,
        _cx: &ActorContext,
        request: Arc<LlmRequest>,
        attempt: u32,
        _limit: ExecutionLimit,
    ) -> Result<LlmResponse, LlmCallError> {
        let rendered = serde_json::to_string(&*request).expect("a request encodes");
        let second = rendered.contains(AGAIN_MARKER);
        let first_streaming_call = {
            let mut seen = self.seen.lock_recover();
            let after_cancel = seen.cancel_requested;
            seen.calls.push(Call {
                second,
                attempt,
                request: rendered,
                after_cancel,
            });
            self.mode == Mode::CancelWhileStreaming && !after_cancel
        };
        if first_streaming_call {
            let backend = self
                .backend
                .lock_recover()
                .clone()
                .expect("the backend is built");
            let answer = request_turn_cancel(
                &backend,
                TurnCancelRequest {
                    session: session(),
                    run: run(),
                    request_id: "l3-cancel".to_owned(),
                    origin: None,
                    reason: Some("the host cancelled".to_owned()),
                    undelivered: TurnCancelUndeliveredInputPolicy::Defer,
                    mode: TurnCancelMode::Immediate,
                },
            )
            .await;
            if answer.is_ok() {
                self.seen.lock_recover().cancel_requested = true;
            }
            // The stream never ends on its own: only the cancel stops it.
            return std::future::pending().await;
        }
        let text = if second { "done" } else { AGAIN };
        Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_owned(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        })
    }

    async fn restart_live_stream(
        &self,
        _cx: &ActorContext,
        _session: &SessionId,
    ) -> Result<(), TurnError> {
        let next = self
            .seen
            .lock_recover()
            .calls
            .last()
            .map_or(0, |call| call.attempt);
        self.seen.lock_recover().restarts.push(next);
        Ok(())
    }

    async fn exec_cell(
        &self,
        _cx: &ActorContext,
        _exec: ExecKey,
        _language: &str,
        _code: &str,
        _with: Vec<DomainWrite>,
    ) -> Result<Result<ExecResponse, lash_core::ExecCodeFailure>, TurnError> {
        Err(TurnError::Exec("the L3 scenario runs no cell".to_owned()))
    }
}

/// The L3 scenario on SQLite in memory, fresh for every matrix cell.
struct L3 {
    mode: Mode,
    seen: Arc<Mutex<Seen>>,
    tripwire: Arc<Tripwire>,
    backend: Arc<Mutex<Option<Backend>>>,
}

impl L3 {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            seen: Arc::default(),
            tripwire: Arc::default(),
            backend: Arc::default(),
        }
    }
}

#[async_trait::async_trait]
impl Scenario for L3 {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
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
            Arc::new(L3Services {
                mode: self.mode,
                seen: Arc::clone(&self.seen),
                backend: Arc::clone(&self.backend),
            }),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let admission = TurnAdmission {
            base_head: 0,
            messages: vec![Message {
                id: "l3-input".to_owned(),
                role: MessageRole::User,
                parts: shared_parts(vec![Part::text(
                    "l3-input.p0".to_owned(),
                    "think twice".to_owned(),
                    None,
                )]),
                origin: None,
                reply_marker: None,
            }],
        };
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

        match self.mode {
            Mode::Plain | Mode::ShortDeadline => {
                // Atomic turn progress: one head advance, with its terminal.
                let commits = committed(CommitLabel::TURN_COMMIT);
                if commits != 1 {
                    violations.push(format!("the turn committed {commits} times"));
                }
            }
            Mode::CancelWhileStreaming => {
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
            }
        }

        if let Some(cut) = cut {
            violations.extend(cut_laws(self.mode, cut, &seen));
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// The laws of one cut: a node killed after `model.start` committed.
fn cut_laws(mode: Mode, cut: &Cut, seen: &Seen) -> Vec<String> {
    let mut violations = Vec::new();
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
        Mode::CancelWhileStreaming => {}
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

async fn prove(mode: Mode, labels: &[CommitLabel]) {
    let report = matrix().run(|| L3::new(mode)).await;
    eprintln!(
        "L3 {mode:?}: {} cells over labels {:?}",
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

/// A turn of two model calls holds the re-send, atomic-progress, F1 and
/// NR-4 laws at every commit label.
#[tokio::test]
async fn a_turn_of_two_model_calls_resumes_at_every_label_without_replay() {
    prove(
        Mode::Plain,
        &[
            CommitLabel::TURN_ADMIT,
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
        ],
    )
    .await;
}

/// L-C1: a call whose node died after pinning it is not sent again once its
/// deadline passed; the deadline is not refreshed on resume.
#[tokio::test]
async fn a_model_call_whose_pinned_deadline_passed_is_never_sent_again() {
    prove(Mode::ShortDeadline, &[CommitLabel::MODEL_START]).await;
}

/// A cancel requested while the model streams ends the turn `Cancelled`, at
/// every cut, with no model call after the request.
#[tokio::test]
async fn a_cancel_while_streaming_ends_the_turn_and_a_crash_mid_cancel_finalizes_it() {
    prove(
        Mode::CancelWhileStreaming,
        &[CommitLabel::MODEL_START, CommitLabel::TURN_CANCEL],
    )
    .await;
}
