//! The deployment's turn services: the scripted protocol and model, the
//! catalog tools and the code cells a turn runs.
//!
//! Everything else a turn does is the production session activation's: its
//! phases, its round runner and its head commit. A cell session runs
//! entirely on the production turn driver behind the facade ([`super::cells`]). Each session's [`TurnScript`] names what its model answers:
//! the first call is answered with the script's work (tool calls or a
//! TypeScript cell), every call that sees that work's results with a final
//! answer. Every body writes its entry to the world's
//! [`BodyLedger`](super::world::BodyLedger) before it does anything else.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use lash_core::facade_support::{EffectId, Response};
use lash_core::runtime::durable::head::SessionHead;
use lash_core::runtime::durable::session::{
    AdmittedInputs, CellExit, CodeCell, ComposedCall, ModelCallAttempt, ModelPin, OpenTurn,
    PreparedCall, TurnCancelRequest, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore,
    TurnRow, TurnServices, request_turn_cancel,
};
use lash_core::sansio::{ChatContextProjector, PendingToolCall, PendingWork, ProtocolDriverHandle};
use lash_core::{
    DriverAction, DriverContextView, Effect, ExecResponse, LlmOutputPart, LlmRequest, LlmResponse,
    Message, MessageRole, Part, ProtocolTurnOptions, TurnMachine, TurnMachineConfig,
    facade_support::TurnFinish, facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_core_execution::ActorContext;
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, CompletedCall, Material, MemberBody, MemberPin, MemberResult, PolicyView,
    RoundTools, SettledOutput, TraceProposal,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{
    CompletionSource, KnownFailureReason, MaterialOwner, MaterialRole,
};
use lash_durable::domain::OwnerKey;
use lash_sansio::llm::types::{ProviderRequestBody, ProviderRouteIdentity};
use lash_sansio::sansio::ExecutionEnvironmentSync;
use lash_sansio::{
    ExecutionBudgets, ExecutionPolicy, ModelToolReturn, SessionId, ToolCallOutput, ToolFailure,
    ToolFailureClass, ToolId, TurnCancelMode, TurnCancelUndeliveredInputPolicy, TurnId,
};

use super::world::{BodyEntry, World, retry};

/// What a session's model answers its first call with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TurnScript {
    /// A final answer at once.
    Plain,
    /// A round: a slow `Once` write, a flaky `Repeatable` and a quick `Once`
    /// write, so their outcomes commit out of order.
    Round,
    /// One `Once` tool that runs until the turn's cancel, which the host
    /// requests once the tool runs.
    Hang,
    /// A TypeScript cell that calls the `Once` host operation `ext.write`.
    Cell,
    /// [`Self::Cell`], whose operation's first body never returns: the host
    /// kills the node running it there and restarts it.
    CellKilled,
    /// A `Once` tool with a store-local process start.
    Effects,
    /// A turn behind the facade whose every model call composes plugin
    /// prompt sections ([`super::prompts`]).
    Prompt,
    /// A turn behind the facade that the host's compaction command then
    /// summarizes ([`super::compactions`]).
    Compaction,
    /// Two turns behind the facade: the first overflows, and the second's
    /// preparation opens a recovery frame ([`super::compactions`]).
    Pressure,
}

impl TurnScript {
    /// Every script.
    pub const ALL: [Self; 9] = [
        Self::Plain,
        Self::Round,
        Self::Hang,
        Self::Cell,
        Self::CellKilled,
        Self::Effects,
        Self::Prompt,
        Self::Compaction,
        Self::Pressure,
    ];

    /// The script's name, the prefix of its sessions' ids.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Round => "round",
            Self::Hang => "hang",
            Self::Cell => "cell",
            Self::CellKilled => "cellkilled",
            Self::Effects => "effects",
            Self::Prompt => "prompt",
            Self::Compaction => "compaction",
            Self::Pressure => "pressure",
        }
    }

    /// The script a session runs, named by its id's prefix.
    #[must_use]
    pub fn of(session: &SessionId) -> Option<Self> {
        let prefix = session.as_str().split('-').next()?;
        Self::ALL.into_iter().find(|script| script.name() == prefix)
    }

    /// A session of this script, distinct by `tag`.
    #[must_use]
    pub fn session(self, tag: &str) -> SessionId {
        session_id(&format!("{}-{tag}", self.name()))
    }
}

/// A session id the simulator spelled.
#[must_use]
pub fn session_id(id: &str) -> SessionId {
    #[expect(
        clippy::expect_used,
        reason = "the simulator's session ids are non-empty literals"
    )]
    SessionId::try_from(id.to_owned()).expect("a simulator session id")
}

/// A turn id the simulator spelled.
#[must_use]
pub fn turn_id(id: &str) -> TurnId {
    #[expect(
        clippy::expect_used,
        reason = "the simulator's turn ids are non-empty literals"
    )]
    TurnId::try_from(id.to_owned()).expect("a simulator turn id")
}

/// What marks a round's results in the transcript.
const RESULTS_MARKER: &str = "sim-results";
/// The `Once` tool a cell calls ([`super::cells`]).
pub const EXT_WRITE: &str = "ext_write";
/// The final answer's text.
pub const FINAL: &str = "done";

/// A catalog tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// A `Once` write that completes after the seed's latency.
    WriteSlow,
    /// A `Once` write that completes at once.
    WriteNow,
    /// A `Repeatable` write whose first attempt reports a known failure.
    Flaky,
    /// A `Once` write that runs until the turn's cancel stops it.
    Hang,
    /// A `Once` call that starts a process: a store-local effect.
    Spawn,
}

impl Tool {
    const ALL: [Self; 5] = [
        Self::WriteSlow,
        Self::WriteNow,
        Self::Flaky,
        Self::Hang,
        Self::Spawn,
    ];

    /// The tool's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WriteSlow => "write_slow",
            Self::WriteNow => "write_now",
            Self::Flaky => "flaky",
            Self::Hang => "hang",
            Self::Spawn => "spawn",
        }
    }

    fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// The policy the catalog declares.
    #[must_use]
    pub fn policy(self) -> ExecutionPolicy {
        match self {
            Self::WriteSlow | Self::WriteNow | Self::Hang | Self::Spawn => ExecutionPolicy::Once,
            Self::Flaky => {
                ExecutionPolicy::repeatable(NonZeroU32::MIN.saturating_add(2), 100, 1_000)
            }
        }
    }
}

/// The catalog tools whose results a model request carries, in the order
/// it presents them.
fn presented(rendered: &str) -> String {
    let mut found: Vec<(usize, &str)> = Tool::ALL
        .into_iter()
        .filter_map(|tool| {
            rendered
                .find(&format!("{}=", tool.name()))
                .map(|at| (at, tool.name()))
        })
        .collect();
    found.sort_unstable();
    found
        .into_iter()
        .map(|(_, name)| name)
        .collect::<Vec<_>>()
        .join(",")
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

fn result_message(id: String, text: String) -> Vec<DriverAction> {
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

/// The scripted protocol: an answer with tool calls starts their round, and
/// any other answer finishes the turn with it. A round's result joins the
/// transcript and the model is called again.
#[derive(Debug)]
struct ScriptedProtocol;

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
        result_message(
            format!("{RESULTS_MARKER}-{}", ctx.protocol_iteration()),
            format!("{RESULTS_MARKER}: {}", rendered.join("; ")),
        )
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
        result_message(format!("sim-cell-{}", ctx.protocol_iteration()), text)
    }
}

/// The machine every turn of the deployment runs under.
fn machine_config(session: &SessionId, run: &TurnId) -> TurnMachineConfig {
    TurnMachineConfig {
        model_tool_calls: lash_core::sansio::ModelToolCalls::fixture(),
        protocol_driver: Arc::new(ScriptedProtocol),
        projector: Arc::new(ChatContextProjector),
        model: lash_sansio::llm_profile::LlmProfileConfig::new(
            lash_sansio::llm_profile::RecordedLlmProfile::mint(
                lash_sansio::llm_profile::LlmProfileKey::new("lash-sim-model"),
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
        no_progress_budget: lash_core::NoProgressBudget::bounded(12),
        attachment_acceptance: Default::default(),
        generation: lash_core::GenerationOptions::default(),
        session_id: session.clone(),
        agent_frame_id: "lash-sim-frame".to_string(),
        turn_id: run.clone(),
        emit_llm_trace: false,
        writer_formats: lash_core::build_newest_writer_formats(),
        termination: ProtocolTurnOptions::default(),
    }
}

fn commit_budget() -> lash_core::facade_support::CommitBudget {
    lash_core::facade_support::CommitBudget::bounded(1024 * 1024, 512)
}

/// The deployment's turn services.
#[derive(Clone)]
pub struct SimServices {
    world: Arc<World>,
    /// How long [`Tool::WriteSlow`] takes: the seed's latency.
    slow: Duration,
}

impl SimServices {
    /// Services that write to `world`'s ledger, with [`Tool::WriteSlow`]
    /// taking `slow`.
    #[must_use]
    pub fn new(world: Arc<World>, slow: Duration) -> Self {
        Self { world, slow }
    }

    fn drive(&self, row: &TurnRow, machine: TurnMachine) -> Box<dyn TurnDrive> {
        Box::new(SimDrive {
            services: self.clone(),
            session: row.session.clone(),
            run: row.run.clone(),
            machine,
        })
    }
}

#[async_trait::async_trait]
impl TurnServices for SimServices {
    fn execution_budgets(&self, _session: &SessionId) -> ExecutionBudgets {
        ExecutionBudgets::default()
    }

    async fn start(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError> {
        if let Some(cells) = self.cells(&row.session)? {
            return cells.start(cx, row, head).await;
        }
        // The turn starts from the session head's window, with the inputs
        // its admission took.
        let backend = self.world.backend().map_err(TurnError::Exec)?;
        let window = head.window()?;
        let messages = window.then(admitted_messages(&backend, row).await?);
        let machine = TurnMachine::in_window(
            machine_config(&row.session, &row.run),
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
        if let Some(cells) = self.cells(&restore.row().session)? {
            return cells.resume(cx, restore).await;
        }
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
        cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError> {
        let session = session_id(cx.actor().id());
        match self.cells(&session)? {
            Some(cells) => cells.apply_commands(cx, admitted).await,
            None => Err(TurnError::Exec(format!(
                "the scripted session {session} runs no commands"
            ))),
        }
    }

    /// A [`TurnScript::Round`] turn is traced, so its admission's export is
    /// recorded under `turn.traced`, which the matrix cuts (FIG-5457).
    fn propose_turn_trace(&self, cx: &ActorContext, run: &TurnId) -> Option<TraceProposal> {
        let session = session_id(cx.actor().id());
        if TurnScript::of(&session) != Some(TurnScript::Round) {
            return None;
        }
        let anchor = lash::tracing::TraceAnchor::Context(told_carrier()?);
        let scope = lash::tracing::TraceScopeId::admission(lash::tracing::TraceScopeOwner::Turn {
            session_id: session,
            turn_id: run.clone(),
        });
        Some(TraceProposal {
            scope: lash::tracing::DurableTraceScope {
                scope,
                cause: lash::tracing::TraceCause::Root,
                anchor: anchor.clone(),
                started_at_ms: 0,
            },
            candidate: Box::new(Untold(anchor)),
        })
    }
}

impl SimServices {
    /// The services a session behind the facade (a cell or a prompt session)
    /// runs with, or `None` for a scripted one.
    fn cells(&self, session: &SessionId) -> Result<Option<Arc<dyn TurnServices>>, TurnError> {
        match TurnScript::of(session) {
            Some(TurnScript::Cell | TurnScript::CellKilled) => Ok(Some(super::cells::services(
                &super::cells::cell_core(&self.world).map_err(TurnError::Exec)?,
            ))),
            Some(TurnScript::Prompt) => Ok(Some(super::cells::services(
                &super::prompts::prompt_core(&self.world).map_err(TurnError::Exec)?,
            ))),
            Some(TurnScript::Compaction | TurnScript::Pressure) => {
                Ok(Some(super::cells::services(
                    &super::compactions::compaction_core(&self.world).map_err(TurnError::Exec)?,
                )))
            }
            _ => Ok(None),
        }
    }
}

/// The messages `row`'s turn starts with: one user message per input its
/// admission took, read back from the session's store.
async fn admitted_messages(
    backend: &lash_core_execution::Backend,
    row: &TurnRow,
) -> Result<Vec<Message>, TurnError> {
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
        let id = format!("input-{input}");
        messages.push(Message {
            id: id.clone(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(format!("{id}.p0"), text, None)]),
            origin: None,
            reply_marker: None,
        });
    }
    Ok(messages)
}

/// One turn's drive: its machine, answered by the deployment's services.
struct SimDrive {
    services: SimServices,
    session: SessionId,
    run: TurnId,
    machine: TurnMachine,
}

impl SimDrive {
    fn script(&self) -> Result<TurnScript, TurnError> {
        TurnScript::of(&self.session)
            .ok_or_else(|| TurnError::Exec(format!("session {} names no script", self.session)))
    }
}

#[async_trait::async_trait]
impl TurnDrive for SimDrive {
    fn machine(&mut self) -> &mut TurnMachine {
        &mut self.machine
    }

    fn tools(&mut self) -> Result<Arc<dyn RoundTools>, TurnError> {
        Ok(Arc::new(Catalog {
            services: self.services.clone(),
            session: self.session.clone(),
            run: self.run.clone(),
        }))
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

    /// A scripted session composes no sections: its request is lowered as
    /// its canonical encoding.
    async fn prepare_call(
        &mut self,
        _cx: &ActorContext,
        _id: EffectId,
        _call: u32,
        request: Arc<LlmRequest>,
    ) -> Result<PreparedCall, TurnError> {
        let route = ProviderRouteIdentity {
            provider: "sim".into(),
            endpoint: "https://sim.test/v1".into(),
            model: request.model.wire_model().into(),
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
        _attempt: ModelCallAttempt,
    ) -> Result<(), TurnError> {
        let rendered =
            serde_json::to_string(&*request).map_err(|error| TurnError::Exec(error.to_string()))?;
        let answered = rendered.contains(RESULTS_MARKER);
        self.services.world.note(format!(
            "model.call {} {}",
            self.session,
            if answered { "final" } else { "first" }
        ));
        if rendered.contains(RESULTS_MARKER) {
            self.services.world.note(format!(
                "model.results {} {}",
                self.session,
                presented(&rendered)
            ));
        }
        let tools: &[Tool] = match self.script()? {
            _ if answered => &[],
            TurnScript::Plain
            | TurnScript::Cell
            | TurnScript::CellKilled
            | TurnScript::Prompt
            | TurnScript::Compaction
            | TurnScript::Pressure => &[],
            TurnScript::Round => &[Tool::WriteSlow, Tool::Flaky, Tool::WriteNow],
            TurnScript::Hang => &[Tool::Hang],
            TurnScript::Effects => &[Tool::Spawn],
        };
        let parts = if !tools.is_empty() {
            tools
                .iter()
                .enumerate()
                .map(|(index, tool)| LlmOutputPart::ToolCall {
                    call_id: format!("provider-{index}"),
                    tool_name: tool.name().to_owned(),
                    input_json: format!("{{\"slot\":{index}}}"),
                    replay: None,
                })
                .collect()
        } else {
            let text = FINAL.to_owned();
            vec![LlmOutputPart::Text {
                text,
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

/// A body's journal-local output, owned by its turn.
fn turn_output(opener: &EffectOpener, text: String) -> Material {
    Material::journal_local(
        MaterialOwner::Run {
            opener: opener.clone(),
        },
        MaterialRole::AttemptOutput,
        text,
    )
}

fn unencodable() -> MemberResult {
    MemberResult::from(SettledOutput::Interrupted)
}

/// A body whose effect was refused: its known failure, which a `Once` call
/// records as its outcome.
fn refused(opener: &EffectOpener, error: &str) -> MemberResult {
    MemberResult::from(SettledOutput::Failed(
        turn_output(opener, format!("refused: {error}"))
            .failure(KnownFailureReason::Reported, None),
    ))
}

/// The deployment's catalog: each tool's policy, body and answer.
struct Catalog {
    services: SimServices,
    session: SessionId,
    run: TurnId,
}

/// A traced turn's or round call's trace admission candidate: an anchor
/// exported nowhere.
struct Untold(lash::tracing::TraceAnchor);

/// The carrier every traced scope of the matrix is anchored by.
fn told_carrier() -> Option<lash::tracing::TraceCarrier> {
    Some(lash::tracing::TraceCarrier::new(
        lash::tracing::W3cTraceId::from_bytes(1_u128.to_be_bytes()).ok()?,
        lash::tracing::W3cSpanId::from_bytes(1_u64.to_be_bytes()).ok()?,
        lash::tracing::W3cTraceFlags::from_byte(lash::tracing::W3cTraceFlags::SAMPLED),
        lash::tracing::W3cTraceState::default(),
    ))
}

impl lash::tracing::TraceAdmissionCandidate for Untold {
    fn anchor(&self) -> lash::tracing::TraceAnchor {
        self.0.clone()
    }

    fn settle(self: Box<Self>, _outcome: lash::tracing::TraceCandidateOutcome) {}
}

impl RoundTools for Catalog {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    /// A [`TurnScript::Round`] turn's calls are traced, so its admission's
    /// exports are recorded under `round.traced`, which the matrix cuts
    /// (FIG-5452).
    fn propose_trace(&self, call: &PendingToolCall) -> Option<TraceProposal> {
        if TurnScript::of(&self.session) != Some(TurnScript::Round) {
            return None;
        }
        let anchor = lash::tracing::TraceAnchor::Context(told_carrier()?);
        let mut scope = lash_core_execution::trace::tool_trace_scope(
            &EffectOpener::turn(self.session.clone(), self.run.clone()),
            None,
            &call.call_id,
            0,
        );
        scope.anchor = anchor.clone();
        Some(TraceProposal {
            scope,
            candidate: Box::new(Untold(anchor)),
        })
    }

    fn pin(&self, call: &PendingToolCall, now_ms: u64) -> MemberPin {
        let tool = Tool::named(&call.tool_name).unwrap_or(Tool::WriteNow);
        // No tool of this catalog parks; each body is bounded as its host
        // would bound a store write.
        MemberPin::admitted(
            ToolId::new(tool.name()),
            tool.policy(),
            lash_sansio::ToolBounds {
                execution: std::time::Duration::from_secs(120),
                park: None,
            },
            now_ms,
        )
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

    fn policies(&self) -> PolicyView {
        PolicyView::new(
            Tool::ALL
                .into_iter()
                .map(|tool| (ToolId::new(tool.name()), tool.policy())),
        )
    }

    fn body(&self, call: &PendingToolCall, execution: &AdmittedExecution) -> MemberBody {
        let world = Arc::clone(&self.services.world);
        let slow = self.services.slow;
        let tool = Tool::named(&call.tool_name);
        let call = call.call_id.clone();
        let attempt = execution.attempt();
        let policy = execution.policy();
        let session = self.session.clone();
        let run = self.run.clone();
        let owner = OwnerKey::Turn(session.clone(), run.clone());
        let opener = EffectOpener::turn(session.clone(), run.clone());
        Box::new(move |token| {
            Box::pin(async move {
                let Some(tool) = tool else {
                    return unencodable();
                };
                let admitted = world.admitted(&owner, &call).await;
                world.ledger().enter(
                    &owner,
                    &call,
                    BodyEntry {
                        tool: tool.name().to_owned(),
                        policy,
                        attempt,
                        at_ms: world.now_ms(),
                        admitted,
                    },
                );
                let mut effects = Vec::new();
                match tool {
                    Tool::WriteSlow => world.sleep(slow).await,
                    // The failing attempt finishes well after the others,
                    // so its retry commits as a batch of its own.
                    Tool::Flaky if attempt == 1 => world.sleep(slow * 4).await,
                    Tool::Hang => {
                        request_cancel(&world, session, run);
                        token.cancelled().await;
                        return MemberResult::from(SettledOutput::Cancelled {
                            evidence: Default::default(),
                        });
                    }
                    Tool::Spawn => match super::effects::spawn(&world, &call).await {
                        Ok(staged) => effects = staged,
                        Err(error) => {
                            world.note(format!("effect.refused {error}"));
                            return refused(&opener, &error);
                        }
                    },
                    Tool::WriteNow | Tool::Flaky => {}
                }
                let output = turn_output(&opener, format!("{}#{attempt}", tool.name()));
                MemberResult {
                    output: if tool == Tool::Flaky && attempt == 1 {
                        SettledOutput::Failed(output.failure(KnownFailureReason::Reported, None))
                    } else {
                        SettledOutput::Completed(output)
                    },
                    store_local: effects,
                    terminal: None,
                }
            })
        })
    }

    fn completed(&self, call: &PendingToolCall, output: &SettledOutput) -> CompletedCall {
        let output = match output {
            SettledOutput::Completed(material) => ToolCallOutput::success(material.payload()),
            other => ToolCallOutput::failure(ToolFailure::runtime(
                ToolFailureClass::Execution,
                "sim_unsettled",
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

/// The host's turn cancel, requested once the hanging tool runs: a request
/// of its own, retried while the store fails transiently.
fn request_cancel(world: &Arc<World>, session: SessionId, run: TurnId) {
    let noted = Arc::clone(world);
    world.spawn(async move {
        let request = TurnCancelRequest {
            session,
            run,
            request_id: "lash-sim-cancel".to_owned(),
            origin: None,
            reason: Some("the host cancelled".to_owned()),
            undelivered: TurnCancelUndeliveredInputPolicy::Defer,
            mode: TurnCancelMode::Immediate,
        };
        let answer = retry(&noted, |host| {
            let request = request.clone();
            async move { request_turn_cancel(&host, request).await }
        })
        .await;
        if answer.is_ok() {
            noted.note("turn.cancel.requested");
        }
    });
}

/// How long a killed node stays down before its supervisor restarts it:
/// past a failover, so another node reaps it first.
const RESTART_AFTER: Duration = Duration::from_secs(30);

/// The host kills the node that owns `session`'s actor, as a crash there
/// would, and its supervisor restarts it once another node took over.
pub(super) fn kill_owner(world: &Arc<World>, session: &SessionId) {
    let Some(nodes) = world.nodes() else {
        return;
    };
    let Ok(actor) = lash_durable::ActorKey::session(session.as_str()) else {
        return;
    };
    let killer = Arc::clone(world);
    world.spawn(async move {
        let owner = match nodes.database().actor(&actor).await {
            Ok(Some(snapshot)) => snapshot.owner.map(|owner| owner.node.as_str().to_owned()),
            _ => None,
        };
        let Some(node) = owner else {
            return;
        };
        killer.note(format!("killed {node}"));
        nodes.kill(&node);
        killer.sleep(RESTART_AFTER).await;
        nodes.restart(&node);
    });
}
