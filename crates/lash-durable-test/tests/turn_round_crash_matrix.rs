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
//! - **After-step cancel (FIG-5229):** an `AfterStep` cancel requested while
//!   the model streams keeps the call's response: its round is admitted and
//!   its tools run, and the turn ends `Cancelled` at the next model call's
//!   boundary, without calling the model again.
//! - **Cancel in a cell (FIG-5229):** an `Immediate` cancel requested while
//!   a code cell runs stops the cell, and the turn ends `Cancelled` promptly.
//! - **Pending:** a member that parks takes the key of the completion wait
//!   the turn's `model.done` pinned, is never entered again once its park
//!   committed, and settles from the host's resolution of that key; the
//!   model's next call sees that resolution.
//! - **Publication after the commit (FIG-5229):** a turn publishes what it
//!   held for its commit at most once, and only once its `turn.commit` was
//!   acknowledged: a refused commit publishes none of it.
//! - **F1:** a zombie's writes after its reap are refused.

// Test code.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/seed.rs"]
mod seed;
#[path = "support/sim.rs"]
mod sim;

#[path = "support/matrix.rs"]
mod matrix;

use matrix::MatrixTestExt as _;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::{EffectId, Response};
use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::session::{
    AdmittedInputs, CellExit, CodeCell, ComposedCall, ModelPin, OpenTurn, PreparedCall,
    SessionActivation, TurnCancelRequest, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore,
    TurnRow, TurnServices, request_turn_cancel,
};
use lash_core::sansio::{ChatContextProjector, PendingToolCall, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, Message, MessageRole, Part,
    ProtocolTurnOptions, TurnMachine, TurnMachineConfig, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core_execution::runtime::actor::round::{
    self, AdmittedExecution, CompletedCall, Material, MemberBody, MemberPin, MemberResult,
    PolicyView, RoundTools, RunFold, SettledOutput,
};
use lash_core_execution::runtime::actor::waits::{self, WaitDeadline};
use lash_core_execution::{ActorContext, Backend, StoreSet};
use lash_core_store::tool_run::{
    CompletionSource, KnownFailureReason, MaterialOwner, MaterialRole,
};
use lash_durable::domain::{AdmittedId, OwnerKey, RunRecordKind, RunSeq};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableError, DurableStore};
use lash_durable_test::{
    Cut, Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire, WriteKind,
};
use lash_sansio::llm::types::{ProviderRequestBody, ProviderRouteIdentity};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{
    ExecutionBudgets, ExecutionLimit, ExecutionPolicy, ModelToolReturn, SessionId, ToolCallId,
    ToolCallOutput, ToolFailure, ToolFailureClass, ToolId, TurnCancelMode,
    TurnCancelUndeliveredInputPolicy, TurnId,
};

const SESSION: &str = "l4t-session";
const RUN: &str = "l4t-turn";
const RESULTS_MARKER: &str = "l4t-results";
/// The model's answer that runs a code cell.
const CELL: &str = "l4t-cell";
/// How much virtual time an `Immediate` cancel of a running cell may take to
/// end the turn: a few claim polls, nowhere near the cell's own limits.
const PROMPT_CANCEL_MS: u64 = 1_000;

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
    /// A `Once` write that parks on its completion wait, whose key the
    /// outside world resolves a little later.
    Defer,
    /// A `Once` write that parks on its completion wait and hands its key
    /// to the host, who resolves it whenever it decides: an approval.
    Approve,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Write { millis: 0 } => "write_now",
            Self::Write { .. } => "write_slow",
            Self::Flaky => "flaky",
            Self::Hang => "hang",
            Self::Defer => "defer",
            Self::Approve => "approve",
        }
    }

    fn named(name: &str) -> Self {
        match name {
            "write_now" => Self::Write { millis: 0 },
            "write_slow" => Self::Write { millis: 50 },
            "flaky" => Self::Flaky,
            "hang" => Self::Hang,
            "defer" => Self::Defer,
            "approve" => Self::Approve,
            other => panic!("no tool {other}"),
        }
    }

    fn policy(self) -> ExecutionPolicy {
        match self {
            Self::Write { .. } | Self::Hang | Self::Defer | Self::Approve => ExecutionPolicy::Once,
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
    /// The model calls a tool that parks on its completion wait and a quick
    /// `Once` write.
    Pending,
    /// The model's first call requests an `AfterStep` cancel of the turn
    /// while it streams, then answers with a quick `Once` write.
    AfterStepWhileStreaming,
    /// The model answers with a code cell that requests an `Immediate`
    /// cancel of the turn and then runs until it is stopped.
    CancelInCell,
    /// The model calls one tool that parks on an approval the host gives
    /// only once the session released its claim.
    Approval,
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
            Self::Pending => vec![Tool::Defer, Tool::Write { millis: 0 }],
            Self::AfterStepWhileStreaming => vec![Tool::Write { millis: 0 }],
            Self::CancelInCell => Vec::new(),
            Self::Approval => vec![Tool::Approve],
        }
    }
}

/// The outside world: every body entry, per call and attempt. It survives
/// every node, so a write a crash cannot undo is visible to the laws.
#[derive(Debug, Default)]
struct ExternalWorld {
    writes: Mutex<BTreeMap<ToolCallId, Vec<u32>>>,
}

/// How long the outside world takes to resolve a key it was handed.
const RESOLVE_AFTER_MS: u64 = 20;

/// The answer the outside world resolves a parked call with.
fn host_answer(call: &ToolCallId) -> serde_json::Value {
    serde_json::json!({ "answered": call.as_str() })
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
        if ids.is_empty() && response_text(&llm_response) == CELL {
            return vec![DriverAction::Start(PendingWork::Exec {
                language: "typescript".to_owned(),
                code: "for (;;) {}".to_owned(),
                driver_state: lash_core::ProtocolDriverState::new("l4t", serde_json::Value::Null),
            })];
        }
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
    /// The virtual time the turn's cancel was requested at.
    requested_at_ms: Option<u64>,
    /// Model calls that answered after requesting an `AfterStep` cancel.
    answered_after_request: usize,
    /// How many times a drive published what its turn held for its commit.
    published: usize,
    /// The completion key each approval handed the host, by call.
    approvals: Vec<(ToolCallId, String)>,
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

    fn clock(&self) -> Arc<SimClock> {
        self.clock.lock_recover().clone().expect("a clock")
    }

    /// Request the turn's cancel in `mode` from outside the actor, as a host
    /// would.
    async fn request_cancel(&self, mode: TurnCancelMode) {
        let answer = request_turn_cancel(
            &self.backend(),
            TurnCancelRequest {
                session: session(),
                run: run(),
                request_id: "l4t-cancel".to_owned(),
                origin: None,
                reason: Some("the host cancelled".to_owned()),
                undelivered: TurnCancelUndeliveredInputPolicy::Defer,
                mode,
            },
        )
        .await;
        if answer.is_ok() {
            let now = self.clock().logical_ms();
            let mut seen = self.seen.lock_recover();
            seen.cancel_requested = true;
            seen.requested_at_ms.get_or_insert(now);
        }
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
            provider: "round".into(),
            endpoint: "https://round.test/v1".into(),
            model: "scripted".into(),
        };
        let body = ProviderRequestBody::of_request(route, &request)
            .map_err(|error| TurnError::Exec(error.to_string()))?;
        Ok(PreparedCall::Admit(Box::new(ComposedCall {
            request,
            prompt: None,
            body,
        })))
    }

    async fn model_call(
        &mut self,
        _cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        _body: &ProviderRequestBody,
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
        let requests_after_step = self.services.mode == Mode::AfterStepWhileStreaming
            && !second
            && !self.services.seen.lock_recover().cancel_requested;
        if requests_after_step {
            self.services
                .request_cancel(TurnCancelMode::AfterStep)
                .await;
            // Outlive the owner's cancel watch waking on the request: an
            // after-step request must not stop the call.
            let clock = self.services.clock();
            lash_core_ids::clock::Clock::sleep(&*clock, Duration::from_millis(50)).await;
        }
        let parts = if second {
            vec![LlmOutputPart::Text {
                text: "done".to_owned(),
                response_meta: None,
            }]
        } else if self.services.mode == Mode::CancelInCell {
            vec![LlmOutputPart::Text {
                text: CELL.to_owned(),
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
        if requests_after_step {
            self.services.seen.lock_recover().answered_after_request += 1;
        }
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
        Ok(Arc::new(Catalog {
            services: self.services.clone(),
        }))
    }

    async fn exec_cell(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _cell: CodeCell,
    ) -> Result<CellExit, TurnError> {
        if self.services.mode != Mode::CancelInCell {
            return Err(TurnError::Exec("the L4 scenario runs no cell".to_owned()));
        }
        self.services
            .request_cancel(TurnCancelMode::Immediate)
            .await;
        // The cell never ends on its own: only the cancel stops it.
        std::future::pending().await
    }

    async fn finish(
        &mut self,
        _cx: &ActorContext,
        done: TurnDone,
        head: &SessionHead,
    ) -> Result<TurnCommit, TurnError> {
        head.commit(&self.run, done, commit_budget()).await
    }

    /// The scenario's cell holds nothing outside its future.
    fn stop_cell(&mut self) {}

    /// The turn publishes what it held for its commit.
    async fn committed(&mut self) {
        self.services.seen.lock_recover().published += 1;
    }
}

fn commit_budget() -> lash_core::facade_support::CommitBudget {
    lash_core::facade_support::CommitBudget::bounded(1024 * 1024, 512)
}

/// A member's output, owned by the turn: its journal-local material.
fn output_material(text: &str) -> Material {
    Material::journal_local(
        MaterialOwner::Run {
            opener: lash_core_store::effect_opener::EffectOpener::turn(session(), run()),
        },
        MaterialRole::AttemptOutput,
        text.to_owned(),
    )
}

/// The scenario's catalog: each tool's policy, body and answer.
struct Catalog {
    services: L4Services,
}

impl RoundTools for Catalog {
    fn pin(&self, call: &PendingToolCall, now_ms: u64) -> MemberPin {
        let tool = Tool::named(&call.tool_name);
        let budget = ExecutionBudgets::default().tool_default();
        let limit = ExecutionLimit::starting_at(now_ms, budget, budget);
        MemberPin {
            tool: ToolId::new(tool.name()),
            policy: tool.policy(),
            limit,
            wait: matches!(tool, Tool::Defer | Tool::Approve).then(|| {
                WaitDeadline::at_instant(lash_durable::DurableInstant(
                    i64::try_from(limit.expires_at).unwrap(),
                ))
            }),
        }
    }

    fn policies(&self) -> PolicyView {
        PolicyView::new(
            [
                Tool::Write { millis: 0 },
                Tool::Write { millis: 50 },
                Tool::Flaky,
                Tool::Hang,
                Tool::Defer,
                Tool::Approve,
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
        let pinned = execution.draft().pinned_wait();
        Box::new(move |token| {
            Box::pin(async move {
                match tool {
                    Tool::Approve => {
                        let pinned = pinned.expect("a parking member's wait is pinned");
                        let key = waits::host_key(&pinned.wait())
                            .expect("a tool completion wait has a host key");
                        world.write(&call, attempt);
                        services
                            .seen
                            .lock_recover()
                            .approvals
                            .push((call.clone(), key.as_str().to_owned()));
                        return MemberResult::from(SettledOutput::Waiting(
                            output_material("parked").parked(pinned.id.to_hex()),
                        ));
                    }
                    Tool::Defer => {
                        let pinned = pinned.expect("a parking member's wait is pinned");
                        let backend = services.backend();
                        let key = waits::host_key(&pinned.wait())
                            .expect("a tool completion wait has a host key");
                        world.write(&call, attempt);
                        let answer = host_answer(&call);
                        tokio::spawn(async move {
                            lash_core_ids::clock::Clock::sleep(
                                &*clock,
                                Duration::from_millis(RESOLVE_AFTER_MS),
                            )
                            .await;
                            let _ = waits::resolve_host(
                                &backend,
                                key.as_str(),
                                waits::Resolution::Ok(answer),
                            )
                            .await;
                        });
                        return MemberResult::from(SettledOutput::Waiting(
                            output_material("parked").parked(pinned.id.to_hex()),
                        ));
                    }
                    Tool::Write { millis } if millis > 0 => {
                        lash_core_ids::clock::Clock::sleep(&*clock, Duration::from_millis(millis))
                            .await;
                    }
                    Tool::Hang => {
                        world.write(&call, attempt);
                        services.request_cancel(TurnCancelMode::Immediate).await;
                        token.cancelled().await;
                        return MemberResult::from(SettledOutput::Cancelled {
                            evidence: Default::default(),
                        });
                    }
                    Tool::Write { .. } | Tool::Flaky => {}
                }
                world.write(&call, attempt);
                let output = output_material(&format!("{}#{attempt}", tool.name()));
                MemberResult::from(if tool == Tool::Flaky && attempt == 1 {
                    SettledOutput::Failed(output.failure(KnownFailureReason::Reported, None))
                } else {
                    SettledOutput::Completed(output)
                })
            })
        })
    }

    fn resolved(
        &self,
        _call: &PendingToolCall,
        _execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: waits::Resolution,
    ) -> SettledOutput {
        assert_eq!(
            parked.payload(),
            "parked",
            "the park's material rides its row"
        );
        let text = match resolution {
            waits::Resolution::Ok(value) => value.to_string(),
            other => format!("{other:?}"),
        };
        SettledOutput::Completed(output_material(&text))
    }

    fn completed(&self, call: &PendingToolCall, output: &SettledOutput) -> CompletedCall {
        let output = match output {
            SettledOutput::Completed(material) => ToolCallOutput::success(material.payload()),
            other => ToolCallOutput::failure(ToolFailure::runtime(
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
        let stores = sim::memory(clock).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let stores: Arc<dyn StoreSet> = Arc::new(stores);
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
        seed::send_turn(&backend, &session(), &run(), "use the tools").await?;
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
            Mode::Pending | Mode::Approval => {
                violations.extend(pending_laws(
                    self.mode,
                    &fold,
                    &self.world,
                    &self.tripwire,
                    &seen,
                ));
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
                    matches!(member.outcome(), Some(SettledOutput::Cancelled { .. }))
                });
                if commits + cancels != 1 || (stopped && cancels != 1) {
                    violations.push(format!(
                        "cancel: {commits} head commits and {cancels} cancel terminals"
                    ));
                }
            }
            Mode::AfterStepWhileStreaming => {
                violations.extend(after_step_laws(
                    &fold,
                    &self.world,
                    &seen,
                    cut,
                    committed(CommitLabel::TURN_COMMIT),
                    committed(CommitLabel::TURN_CANCEL),
                ));
            }
            Mode::CancelInCell => {
                let commits = committed(CommitLabel::TURN_COMMIT);
                let cancels = committed(CommitLabel::TURN_CANCEL);
                if commits != 0 || cancels != 1 {
                    violations.push(format!(
                        "cancel in a cell: {commits} head commits and {cancels} cancel terminals"
                    ));
                }
                if seen.called_after_cancel {
                    violations.push(
                        "cancel in a cell: a model call started after the request".to_owned(),
                    );
                }
                // Promptly: the uncut turn's cancel commits within a few
                // claim polls of the request, not at any limit of the cell's.
                let ended = trace
                    .iter()
                    .find(|write| {
                        write.point.label == CommitLabel::TURN_CANCEL && write.committed()
                    })
                    .map(|write| write.at_ms);
                let prompt = match (seen.requested_at_ms, ended) {
                    (Some(at), Some(ended)) => ended.saturating_sub(at) <= PROMPT_CANCEL_MS,
                    _ => false,
                };
                if cut.is_none() && !prompt {
                    violations.push(format!(
                        "cancel in a cell: requested at {:?} ms, the turn ended at {ended:?} ms",
                        seen.requested_at_ms
                    ));
                }
            }
        }

        violations.extend(publication_laws(
            &seen,
            cut,
            committed(CommitLabel::TURN_COMMIT),
        ));
        if let Some(cut) = cut {
            violations.extend(zombie_laws(cut, &trace));
        }
        violations
    }
}

/// A turn publishes what it held for its commit only once its `turn.commit`
/// was acknowledged: at most once, never for a commit the store refused, and
/// exactly once when the turn committed uncut or after a refused attempt.
/// A commit whose acknowledgement its owner never received publishes
/// nothing.
fn publication_laws(seen: &Seen, cut: Option<&Cut>, commits: usize) -> Vec<String> {
    let refused_commit = cut.is_some_and(|cut| {
        cut.point.label == CommitLabel::TURN_COMMIT && cut.fault == Fault::FailBefore
    });
    let must_publish = commits == 1 && (cut.is_none() || refused_commit);
    if seen.published > commits || (must_publish && seen.published != 1) {
        vec![format!(
            "publication: the turn published its held terminal {} times over {commits} commits",
            seen.published
        )]
    } else {
        Vec::new()
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
            Tool::Write { .. } | Tool::Hang | Tool::Defer | Tool::Approve => {
                if entries > 1 || writes.len() > 1 {
                    violations.push(format!(
                        "F2: Once {call} was entered {entries} times, wrote {writes:?}"
                    ));
                }
                match outcome {
                    SettledOutput::Completed(_) if writes.len() == 1 => {}
                    SettledOutput::Interrupted => {}
                    other => violations.push(format!(
                        "F2: Once {call} settled {other:?} after writing {writes:?}"
                    )),
                }
            }
            Tool::Flaky => {
                if !matches!(outcome, SettledOutput::Completed(_)) {
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

/// F2 and Pending over the parking round: the parked `Once` is entered at
/// most once and settles from its key's resolution, or `Interrupted` when
/// its owner died before its park committed; the model's next call sees
/// what it settled with.
fn pending_laws(
    mode: Mode,
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
    for (tool, member) in mode.tools().iter().zip(view.members()) {
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
        if entries > 1 || writes.len() > 1 {
            violations.push(format!(
                "F2: Once {call} was entered {entries} times, wrote {writes:?}"
            ));
        }
        let answered = host_answer(call).to_string();
        match (tool, member.outcome()) {
            (Tool::Defer | Tool::Approve, Some(SettledOutput::Completed(material)))
                if !writes.is_empty() && material.payload() == answered =>
            {
                let seen_answer = seen.requests.iter().any(|request| {
                    request.contains(RESULTS_MARKER) && request.contains("answered")
                });
                if !seen_answer && !seen.requests.is_empty() {
                    violations.push(format!("Pending: the model never saw {call}'s resolution"));
                }
            }
            (_, Some(SettledOutput::Interrupted)) => {}
            (Tool::Write { .. }, Some(SettledOutput::Completed(_))) if writes.len() == 1 => {}
            (_, other) => violations.push(format!(
                "Pending: {call} settled {other:?} after writing {writes:?}"
            )),
        }
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
            Some(SettledOutput::Cancelled { .. } | SettledOutput::Interrupted) => {}
            other => violations.push(format!(
                "cancel: {} settled {other:?} after the turn's cancel",
                member.call()
            )),
        }
    }
    violations
}

/// An `AfterStep` cancel requested while the model streams lets the call
/// answer, and calls the model no more. Uncut, the call's round is admitted
/// and its write runs and completes before the turn ends `Cancelled`; a cut
/// before the round committed may lose the answer with its node.
fn after_step_laws(
    fold: &RunFold,
    world: &ExternalWorld,
    seen: &Seen,
    cut: Option<&Cut>,
    commits: usize,
    cancels: usize,
) -> Vec<String> {
    let mut violations = Vec::new();
    if commits != 0 || cancels != 1 {
        violations.push(format!(
            "after-step: {commits} head commits and {cancels} cancel terminals"
        ));
    }
    if seen.called_after_cancel {
        violations.push("after-step: a model call started after the request".to_owned());
    }
    if cut.is_some() {
        return violations;
    }
    if seen.answered_after_request != 1 {
        violations.push(format!(
            "after-step: {} calls answered after requesting the cancel",
            seen.answered_after_request
        ));
    }
    let Some((run, _)) = declared(fold) else {
        violations.push("after-step: the answered call's round was never admitted".to_owned());
        return violations;
    };
    for member in fold.round(run).unwrap().members() {
        let writes = world.writes(member.call());
        match member.outcome() {
            Some(SettledOutput::Completed(_)) if writes.len() == 1 => {}
            other => violations.push(format!(
                "after-step: {} settled {other:?} after writing {writes:?}",
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
    let report = matrix().run_test(|| L4::new(mode)).await;
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

/// A turn whose `turn.commit` the store refuses publishes none of what it
/// held for it; the attempt that commits publishes it once (FIG-5229).
#[tokio::test]
async fn a_refused_turn_commit_publishes_no_terminal_event() {
    let report = matrix()
        .labels(&[CommitLabel::TURN_COMMIT])
        .run_test(|| L4::new(Mode::Mixed))
        .await;
    report.assert_held();
    assert!(
        report.cells.iter().any(|cell| {
            cell.point.label == CommitLabel::TURN_COMMIT && cell.fault == Fault::FailBefore
        }),
        "the matrix never refused the turn's commit"
    );
}

/// An `AfterStep` cancel requested while the model streams keeps the
/// response: its round runs, and the turn ends `Cancelled` before the next
/// model call, at every cut (FIG-5229).
#[tokio::test]
async fn an_after_step_cancel_while_the_model_streams_keeps_the_response_and_its_tools() {
    prove(
        Mode::AfterStepWhileStreaming,
        &[
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::TURN_CANCEL,
        ],
    )
    .await;
}

/// An `Immediate` cancel requested while a code cell runs stops the cell,
/// and the turn ends `Cancelled` promptly, at every cut (FIG-5229).
#[tokio::test]
async fn an_immediate_cancel_during_a_long_cell_ends_the_turn_cancelled_promptly() {
    prove(
        Mode::CancelInCell,
        &[CommitLabel::MODEL_DONE, CommitLabel::TURN_CANCEL],
    )
    .await;
}

/// A turn whose model calls a tool that parks on its completion wait, beside
/// a quick write: the parked `Once` is entered at most once, its key's
/// resolution settles it, and the model's next call sees that resolution,
/// at every commit label of the turn and its round.
#[tokio::test]
async fn a_parked_member_settles_from_its_key_at_every_label() {
    prove(
        Mode::Pending,
        &[
            CommitLabel::MODEL_DONE,
            CommitLabel::ROUND_OUTCOME,
            CommitLabel::ROUND_PRESENT_MODEL_START,
            CommitLabel::TURN_COMMIT,
        ],
    )
    .await;
}

/// A14 (FIG-5226): a turn whose only member parked on an approval has
/// nothing runnable. Its session releases its claim as `waiting`, holding
/// no slot, with the approval's deadline as its due time; the host's
/// approval wakes it, the next claim resumes the round from its fold, and
/// the turn commits once, with the parked body never entered again.
#[tokio::test]
async fn a_turn_whose_only_member_is_a_parked_approval_releases_its_claim_and_resumes_on_resolution()
 {
    const HORIZON_MS: u64 = 600_000;
    let scenario = L4::new(Mode::Approval);
    let clock = SimClock::new();
    let database = scenario.database(Arc::clone(&clock)).await;
    let nodes = Arc::new(SimNodes::new(
        database,
        Arc::clone(&clock),
        lash_durable_test::Script::new(),
        scenario.config(),
        scenario.activation(),
    ));
    scenario.start(&nodes).await.expect("the turn is sent");

    let mut opened = false;
    let parked = loop {
        let snapshot = nodes
            .database()
            .actor(&actor())
            .await
            .unwrap()
            .expect("the session's actor");
        if snapshot.state == ActorState::Waiting {
            break snapshot;
        }
        let open = nodes.database().turn(&session()).await.unwrap().is_some();
        opened |= open;
        assert!(
            open || !opened,
            "the session never released its claim while its only member was parked: \
             it held it until the approval's wait timed out and the turn ended ({:?})",
            snapshot.state
        );
        assert!(
            clock.logical_ms() < HORIZON_MS,
            "the session still holds its claim ({:?}) with only a parked approval",
            snapshot.state
        );
        assert!(nodes.step().await.is_some(), "stalled before the release");
    };
    assert!(parked.owner.is_none(), "a waiting session holds no slot");
    assert!(
        parked.next_due.is_some(),
        "the session released without its approval's deadline as its due"
    );
    assert!(
        nodes.database().turn(&session()).await.unwrap().is_some(),
        "the turn is still open while its approval is pending"
    );

    let (call, key) = scenario
        .seen
        .lock_recover()
        .approvals
        .first()
        .cloned()
        .expect("the approval handed out its key");
    let backend = scenario.backend.lock_recover().clone().unwrap();
    let answer = waits::resolve_host(&backend, &key, waits::Resolution::Ok(host_answer(&call)))
        .await
        .unwrap();
    assert_eq!(answer, lash_durable::domain::ResolveAnswer::Resolved);

    while !scenario.done(&nodes).await {
        assert!(
            clock.logical_ms() < HORIZON_MS,
            "the approved turn did not resume and commit"
        );
        assert!(nodes.step().await.is_some(), "stalled after the approval");
    }
    nodes.quiesce().await;
    let violations = scenario.check(&nodes, None).await;
    assert!(violations.is_empty(), "{violations:#?}");
}
