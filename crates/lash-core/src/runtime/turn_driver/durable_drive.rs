//! The production turn drive on the durable substrate (ADR 0132 §4; L3,
//! FIG-5172): the turn driver's own handlers answering the effects the phase
//! runner hands it. The phase runner owns every commit; the drive runs what
//! a phase does in memory and builds the turn's head commit.

use std::sync::Arc;

use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
use lash_core_execution::runtime::actor::round::RoundTools;

use super::*;
use crate::runtime::durable::commit_publication::{CommitBase, PublicationMark, PublishedHeads};
use crate::runtime::durable::head::SessionHead;
use crate::runtime::durable::session::{
    CellExit, CellToolCalls, CodeCell, ModelAnswer, ModelCallAttempt, OpenTurn, ParkedTurnState,
    PreparedCall, RestoredTurn, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore,
};
use crate::runtime::turn_loop::DurableTurn;

use super::after_turn::FinishedTurn;

/// One turn the session actor runs through the turn driver.
pub(in crate::runtime) struct RuntimeDrive {
    driver: Box<RuntimeTurnDriver<'static>>,
    machine: TurnMachine,
    observer: TurnObserver,
    /// The run's settlement, which its commit fills from the driver's
    /// admitted sets.
    settlement: crate::store::IngressSettlement,
    /// The turn's before-turn decisions, which every phase commits.
    before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    /// The trace scope the turn's admission retained, which every phase
    /// commits.
    trace_scope: lash_trace::DurableTraceScope,
    live: Arc<dyn crate::LiveReplayStore>,
    /// The revision the turn's activity is published at: the head it opened
    /// at.
    revision: crate::SessionRevision,
    /// Publishes the turn's activity to the live stream: drained once the
    /// turn committed, aborted when the drive is dropped without a commit.
    publisher: tokio::task::JoinHandle<()>,
    /// The head the turn's commit is published against.
    commit: Option<CommitBase>,
    published: Arc<PublishedHeads>,
    /// The turn's commit, from its finish until its publication was
    /// attempted or the drive is dropped without one: a reader on this node
    /// that finds the head moved waits for it (FIG-5605).
    committing: Option<PublicationMark<SessionId, SessionRevision>>,
    /// What the turn's commit carries for after its acknowledgement: the
    /// after-turn callbacks' staged state, and the finalized turn for the
    /// lifecycle observers. Dropped with a commit that was not acknowledged,
    /// the staged state fences its namespaces.
    after_commit: AfterCommit,
    /// The turn's execution bound to the session's attachment store: the
    /// turn's puts are held by it until the drive ends (ADR 0124 §4).
    _attachments: Option<crate::attachments::AttachmentExecutionBinding>,
    effect_phase: Option<TurnPhaseSpan>,
    commit_phase: Option<TurnPhaseSpan>,
    /// The tool calls of the turn's last cell, when `turn.commit` is the
    /// commit that records them: its finish reports them (FIG-5330).
    finishing_cell_calls: Option<CellToolCalls>,
    /// The turn's working execution state as its row holds it (FIG-5887
    /// §6), and the one a phase in flight stages in its place.
    working: Option<lash_core_store::checkpoint_graph::NodeId>,
    staged_working: Option<lash_core_store::checkpoint_graph::NodeId>,
    /// The receipt of the model call the machine waits on that no
    /// derivation has answered yet: the outcome the call just returned, or
    /// the one `model.answer` committed, restored.
    owed: Option<ModelAnswer>,
}

#[derive(Default)]
struct AfterCommit {
    state: Option<crate::plugin::StagedPluginState>,
    finalized: Option<crate::AssembledTurn>,
    panic: Option<(&'static str, String)>,
}

impl Drop for RuntimeDrive {
    fn drop(&mut self) {
        self.publisher.abort();
    }
}

/// What a drive is built from besides the prepared turn.
pub(in crate::runtime) struct DriveParts {
    pub(in crate::runtime) observer: TurnObserver,
    pub(in crate::runtime) settlement: crate::store::IngressSettlement,
    pub(in crate::runtime) live: Arc<dyn crate::LiveReplayStore>,
    pub(in crate::runtime) revision: crate::SessionRevision,
    pub(in crate::runtime) publisher: tokio::task::JoinHandle<()>,
    pub(in crate::runtime) commit: Option<CommitBase>,
    pub(in crate::runtime) published: Arc<PublishedHeads>,
}

impl RuntimeDrive {
    /// A fresh turn: its machine built from the prepared messages.
    pub(in crate::runtime) fn start(
        turn: DurableTurn,
        parts: DriveParts,
    ) -> Result<Self, TurnError> {
        let DurableTurn {
            mut driver,
            window,
            messages,
            before_turn,
            trace_scope,
            attachments,
        } = turn;
        let machine = fresh_machine(&mut driver, window, messages)?;
        Ok(Self::assemble(
            driver,
            machine,
            before_turn,
            attachments,
            trace_scope,
            parts,
        ))
    }

    /// A turn taken over from its checkpoint: the machine restored under the
    /// configuration a fresh machine of the prepared turn is built with.
    ///
    /// Everything the turn resumes from is decoded here, before any of its
    /// work is re-delivered: its checkpoint and the driver state parked in
    /// it (the restore), the plugin namespaces its run committed, and the
    /// snapshot of the cell it stopped in. A state this build does not
    /// decode is refused with its typed [`TurnError`], which parks the
    /// session (FIG-5601).
    pub(in crate::runtime) async fn resume(
        cx: &ActorContext,
        turn: DurableTurn,
        parts: DriveParts,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let DurableTurn {
            mut driver,
            window,
            messages,
            before_turn,
            trace_scope,
            attachments,
        } = turn;
        let config = fresh_machine(&mut driver, window, messages)?.into_config();
        let RestoredTurn {
            machine,
            namespaces,
            delivered,
            pending,
            answered,
            row,
        } = restore.restore(config).await?;
        // The steering input the turn's checkpoints delivered before its last phase committed are bound to its run;
        // the turn's commit settles them with the rows its run took.
        driver.pending_turn_inputs.extend(delivered);
        // The namespaces the turn's run committed, the pending
        // checkpoint-callback decisions of an admitted call among them, over
        // the head the run started from, are reinstalled: preparing the turn
        // again served its recorded before-turn decisions and ran no
        // callback, so nothing that committed runs again.
        driver
            .session
            .plugins()
            .restore_run(namespaces)
            .map_err(|error| match error {
                crate::PluginError::StoredDataCorrupt { .. } => TurnError::UndecodableState {
                    state: ParkedTurnState::PluginState,
                    reason: error.to_string(),
                },
                error => TurnError::Exec(format!(
                    "the turn's plugin state did not reinstall: {error}"
                )),
            })?;
        // The protocol records the restored machine already delivered
        // through its progress boundaries reached only the previous owner's
        // draft: each boundary is applied to this one again, its messages
        // and then its records, so the turn's commit holds the history an
        // uncut run commits, in its order.
        for (messages, event_delta) in machine.progressed_boundaries() {
            driver
                .turn_pipeline
                .replay_progress_boundary(&messages, event_delta.to_vec());
        }
        // The tool surface the turn's last sync installed, as its checkpoint
        // records it: the live registry supplies its executors only, so a
        // live source that changed since never reaches the turn's prompts
        // (FIG-6021).
        driver.reinstall_tool_surface().await?;
        // The session's state after the turn's last consumed cell is
        // installed before any model continuation or cell compiles.
        restore_working(cx, &driver, &row).await?;
        if let Some(crate::Effect::ExecCode { id, .. }) = &pending {
            check_cell_snapshot(cx, &driver, &machine, *id).await?;
        }
        let mut drive = Self::assemble(
            driver,
            machine,
            before_turn,
            attachments,
            trace_scope,
            parts,
        );
        drive.working = row.working;
        drive.owed = answered;
        Ok(OpenTurn {
            drive: Box::new(drive),
            pending,
            row,
        })
    }

    fn assemble(
        mut driver: Box<RuntimeTurnDriver<'static>>,
        machine: TurnMachine,
        before_turn: Vec<crate::plugin::RecordedTurnContribution>,
        attachments: Option<crate::attachments::AttachmentExecutionBinding>,
        trace_scope: lash_trace::DurableTraceScope,
        parts: DriveParts,
    ) -> Self {
        let DriveParts {
            observer,
            settlement,
            live,
            revision,
            publisher,
            commit,
            published,
        } = parts;
        let effect_phase = Some(TurnPhaseSpan::begin(
            driver.turn_phase_probe.clone(),
            RuntimeTurnPhase::EffectLoop,
        ));
        // A cell's end rides the commit that consumes its answer (FIG-5999):
        // what runs between the two reads the turn's live state (FIG-6162).
        driver.cell_end = crate::CellEndHandoff::held();
        Self {
            effect_phase,
            commit_phase: None,
            committing: None,
            finishing_cell_calls: None,
            working: None,
            staged_working: None,
            owed: None,
            driver,
            machine,
            observer,
            settlement,
            before_turn,
            trace_scope,
            live,
            revision,
            publisher,
            commit,
            published,
            after_commit: AfterCommit::default(),
            _attachments: attachments,
        }
    }
}

/// Check the snapshot the cell of effect `id`, which the restored `machine`
/// re-delivers, resumes from: the cell's executor decodes it before the cell
/// runs again. A cell with no snapshot starts over and has nothing to check.
///
/// A snapshot the build before this one wrote is carried to this build's
/// formats first, once the fleet permits it (ADR 0106 §2): the carried
/// snapshot and the session's format set commit together before the cell
/// runs again, and a snapshot the carry refuses is one this build does not
/// resume.
async fn check_cell_snapshot(
    cx: &ActorContext,
    driver: &RuntimeTurnDriver<'static>,
    machine: &TurnMachine,
    id: crate::EffectId,
) -> Result<(), TurnError> {
    use lash_durable::domain::{CellId, ExecKey};

    let Some(code_executor) = driver.session.plugins().code_executor() else {
        return Ok(());
    };
    let invocation = driver
        .turn_effect_invocation(machine, id, RuntimeEffectKind::ExecCode)
        .map_err(|error| TurnError::Exec(error.to_string()))?;
    let exec = ExecKey::Cell(
        driver.session_id.clone(),
        driver.turn_id.clone(),
        CellId::new(invocation.effect_replay_key()),
    );
    let Some(cell) = stored_cell(cx, exec).await? else {
        return Ok(());
    };
    let undecodable = |reason| TurnError::UndecodableState {
        state: ParkedTurnState::CellSnapshot,
        reason,
    };
    code_executor
        .check_cell_snapshot(&cell)
        .await
        .map_err(runtime)?
        .map_err(undecodable)?;
    if !cx.carries_sessions().await? {
        return Ok(());
    }
    let Some(carried) = code_executor
        .carried_cell_snapshot(&cell)
        .await
        .map_err(runtime)?
        .map_err(undecodable)?
    else {
        return Ok(());
    };
    let mut tx = cx.begin().await?;
    tx.stage(carried.nodes.into_values());
    for write in carried.writes {
        tx.write(write);
    }
    tx.stamp_formats(cx.backend().formats().session().clone());
    cx.commit(tx, lash_durable::CommitLabel::CELL_SNAPSHOT)
        .await?;
    Ok(())
}

/// Install `row`'s working execution state (FIG-5887 §6), if it holds one,
/// into the session's code executor as the turn's working state.
async fn restore_working(
    cx: &ActorContext,
    driver: &RuntimeTurnDriver<'static>,
    row: &crate::runtime::durable::session::TurnRow,
) -> Result<(), TurnError> {
    let Some(root) = row.working else {
        return Ok(());
    };
    let Some(code_executor) = driver.session.plugins().code_executor() else {
        return Ok(());
    };
    let limits = cx.backend().config().settings().graph_reads.owner();
    let graph = cx
        .durable_reads()?
        .hydrate(
            lash_durable::domain::graph::RootOwner::WorkingExecution {
                session: row.session.clone(),
                run: row.run.clone(),
            },
            Some(root),
            limits,
        )
        .await?;
    let undecodable = |reason: String| TurnError::UndecodableState {
        state: ParkedTurnState::ExecutionState,
        reason,
    };
    let nodes = lash_core_store::checkpoint_graph::closure(&graph, root, &limits)
        .map_err(|error| undecodable(error.to_string()))?;
    code_executor
        .restore_execution_graph(
            crate::plugin::ProtocolSessionContext::new(
                &driver.session_id,
                driver.session.fleet_format(),
            ),
            &crate::plugin::HydratedExecutionGraph { root, nodes },
            crate::plugin::ExecutionGraphPurpose::Working,
        )
        .await
        .map_err(|error| undecodable(error.to_string()))
}

/// `exec`'s root, read whole under one protection: its row and every node
/// it reaches. `None` when the cell has none.
/// Capture references before the cell root and its run facts are retired.
/// Outcomes since the latest snapshot are already committed round material;
/// preserve those exact nodes rather than encoding another output.
async fn cell_calls(
    driver: &RuntimeTurnDriver<'_>,
    machine: &TurnMachine,
    cx: &ActorContext,
    id: crate::EffectId,
) -> Result<CellToolCalls, TurnError> {
    use lash_core_execution::runtime::actor::round::graph::{ReportView, RoundFact};
    use lash_core_execution::runtime::actor::round::{self, PolicyView, SettledOutput};
    use lash_core_store::checkpoint_graph::GraphDecoder;
    use lash_durable::domain::{CellId, ExecKey, OwnerKey};
    let Some(executor) = driver.session.plugins().code_executor() else {
        return Ok(CellToolCalls::default());
    };
    let invocation = driver
        .turn_effect_invocation(machine, id, RuntimeEffectKind::ExecCode)
        .map_err(|error| TurnError::Exec(error.to_string()))?;
    let cell = CellId::new(invocation.effect_replay_key());
    let session = driver.session_id.clone();
    let run = driver.turn_id.clone();
    let exec = ExecKey::Cell(session.clone(), run.clone(), cell.clone());
    let (mut records, mut ledger) = match stored_cell(cx, exec).await? {
        Some(stored) => executor.snapshot_tool_calls(&stored).map_err(|error| {
            TurnError::Exec(format!("the cell call ledger does not read: {error}"))
        })?,
        None => (Vec::new(), ReportView::default()),
    };
    let reads = cx.durable_reads()?;
    let facts = reads.run_facts(&OwnerKey::Cell(session, run, cell)).await?;
    let waits = round::PinnedWaits::read(reads, &facts).await?;
    let fold = round::fold(&facts, &PolicyView::new([]), &waits)
        .map_err(|error| TurnError::Exec(format!("the cell call ledger: {error}")))?;
    let mut decoder = GraphDecoder::new(&facts, facts.read_limits);
    for (run, member) in fold.rounds().flat_map(|round| {
        round
            .members()
            .iter()
            .map(move |member| (round.run(), member))
    }) {
        if member.draft().tool().as_str().starts_with("cell-host:") {
            continue;
        }
        let Some(completed) = member
            .outcome()
            .filter(|outcome| {
                matches!(
                    outcome,
                    SettledOutput::Completed(_) | SettledOutput::Failed(_)
                )
            })
            .and_then(SettledOutput::payload)
            .and_then(round::decode_completed)
        else {
            continue;
        };
        if records
            .iter()
            .any(|record| record.call_id == completed.call_id)
        {
            continue;
        }
        // Only the final attempt belongs in the report, even when earlier
        // failed attempts of this call have not been pruned yet.
        let round::MemberState::Final { start, .. } = member.state() else {
            continue;
        };
        let mut material = None;
        for row in facts.rows.iter().rev().filter(|row| row.run == run) {
            if let RoundFact::Outcome {
                start: settled,
                output,
                ..
            } = RoundFact::load(&mut decoder, row.record)
                .map_err(|error| TurnError::Exec(error.to_string()))?
                && settled == start.0
            {
                material = output.material;
                break;
            }
        }
        let Some(round::graph::MaterialNode::CallPayload(outcome)) = material else {
            return Err(TurnError::Exec(format!(
                "call {} has no typed outcome node",
                completed.call_id
            )));
        };
        let body = round::graph::CallOutcome::body_node(&mut decoder, outcome)
            .map_err(lash_durable::DurableError::Graph)?;
        ledger.entries.push(body);
        records.push(crate::ToolCallRecord {
            call_id: completed.call_id,
            provider_call_id: completed.provider_call_id,
            tool: completed.tool_name,
            args: completed.args,
            output: completed.output,
        });
    }
    Ok(CellToolCalls { records, ledger })
}

impl RuntimeTurnDriver<'_> {
    pub(super) async fn read_cell_calls(
        &self,
        machine: &TurnMachine,
        id: crate::EffectId,
    ) -> Result<CellToolCalls, RuntimeError> {
        cell_calls(self, machine, &self.scoped_effect_controller, id)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::ExecutionStateCaptureFailed,
                    error.to_string(),
                )
            })
    }
}

async fn stored_cell(
    cx: &ActorContext,
    exec: lash_durable::domain::ExecKey,
) -> Result<Option<crate::plugin::StoredCell>, TurnError> {
    let reads = cx.durable_reads()?;
    let Some(row) = reads.exec_root(&exec).await? else {
        return Ok(None);
    };
    let graph = reads
        .hydrate(
            lash_durable::domain::graph::RootOwner::Exec(exec.clone()),
            Some(row.root),
            cx.backend().config().settings().graph_reads.owner(),
        )
        .await?;
    Ok(Some(crate::plugin::StoredCell {
        exec,
        row,
        graph,
        read_limits: cx.backend().config().settings().graph_reads.execution(),
    }))
}

/// The machine a prepared turn starts: ended at once when its recorded model
/// selection is refused.
fn fresh_machine(
    driver: &mut RuntimeTurnDriver<'static>,
    window: crate::runtime::durable::head::TurnWindow,
    messages: crate::MessageSequence,
) -> Result<TurnMachine, TurnError> {
    driver.protocol_reply.mark_run_start(messages.iter());
    let refused = driver.validate_recorded_selection().err();
    let mut machine = driver.turn_machine(window, messages);
    if let Some(event) = refused {
        machine.fail_turn(*event);
    }
    Ok(machine)
}

fn runtime(error: RuntimeError) -> TurnError {
    TurnError::Runtime(error)
}

/// A cell's abort as its turn's error. A worker that refuses the state the
/// cell resumes from, after the restore's check read it, is the same typed
/// refusal the restore gives: the turn parks on its cell snapshot.
fn cell_abort(error: RuntimeError) -> TurnError {
    match &error.cause {
        Some(crate::RuntimeErrorCause::CellSnapshotUndecodable { refusal }) => {
            TurnError::UndecodableState {
                state: ParkedTurnState::CellSnapshot,
                reason: refusal.to_string(),
            }
        }
        _ => TurnError::Runtime(error),
    }
}

#[async_trait::async_trait]
impl TurnDrive for RuntimeDrive {
    fn machine(&mut self) -> &mut TurnMachine {
        &mut self.machine
    }

    fn model_unsent(&mut self, id: crate::EffectId, error: crate::LlmCallError) {
        let mut failure = crate::llm::transport::LlmTransportError::new(error.message.clone())
            .with_kind(error.kind)
            .with_terminal_reason(error.terminal_reason)
            .with_retry_verdict(crate::llm::transport::TransportRetryVerdict::NotRetryable);
        failure.code = error.code.clone();
        failure.partial_response = error.partial_response.clone();
        let call_id = crate::LlmCallId(format!(
            "{}:turn:{}:llm:{}",
            self.driver.session_id,
            self.driver.turn_id,
            self.machine.protocol_iteration(),
        ));
        let record = crate::provider::synthetic_terminal_call_record(
            call_id,
            crate::AttemptOutcome::Failed,
            &failure,
            false,
            crate::ProtocolPosition::NoResponse,
            Vec::new(),
        );
        self.driver.record_model_call(&self.observer, record);
        self.machine.handle_response(Response::LlmComplete {
            id,
            result: Err(error),
            text_streamed: false,
        });
    }

    async fn local(&mut self, _cx: &ActorContext, effect: Effect) -> Result<(), TurnError> {
        let driver = &mut self.driver;
        let machine = &mut self.machine;
        let observer = &self.observer;
        match effect {
            Effect::Emit(event) => {
                // A finished machine's `Error` is its stop's terminal: it
                // publishes after the commit (ADR 0122).
                if machine.is_done() && matches!(event, SessionStreamEvent::Error(_)) {
                    observer.hold_terminal();
                }
                driver.emit_recorded(observer, event);
                Ok(())
            }
            Effect::Progress {
                messages,
                event_delta,
                protocol_iteration,
            } => {
                Box::pin(driver.apply_progress_boundary(messages, event_delta, protocol_iteration))
                    .await
                    .map_err(runtime)
            }
            Effect::Log { event } => {
                driver.handle_log_event(event);
                Ok(())
            }
            Effect::Checkpoint { id, checkpoint } => {
                Box::pin(driver.handle_checkpoint_effect(machine, id, checkpoint, observer))
                    .await
                    .map_err(runtime)
            }
            Effect::SyncExecutionEnvironment { id } => {
                Box::pin(driver.handle_execution_environment_sync_effect(machine, id, observer))
                    .await
                    .map_err(runtime)
            }
            Effect::ReportToolCalls { completed, .. } => {
                Box::pin(driver.report_undispatched_turn_tool_calls(
                    completed,
                    machine.protocol_iteration(),
                    observer,
                ))
                .await
                .map_err(runtime)
            }
            other => Err(TurnError::Exec(format!(
                "{other:?} is a phase's effect, not a local one"
            ))),
        }
    }

    async fn model_call(
        &mut self,
        _cx: &ActorContext,
        id: crate::EffectId,
        request: Arc<LlmRequest>,
        admitted: &lash_sansio::llm::types::AdmittedSend,
        attempt: ModelCallAttempt,
    ) -> Result<(), TurnError> {
        let ModelCallAttempt {
            ordinal: attempt,
            cancel,
            stop_error,
            ..
        } = attempt;
        self.driver.model_stop_error = Arc::clone(&stop_error);
        self.driver
            .protocol_reply
            .mark_model_call(self.machine.messages().iter());
        let stop = self.driver.children_stop.clone();
        let owed = {
            let call = Box::pin(self.driver.handle_llm_call_effect(
                &mut self.machine,
                id,
                request,
                admitted,
                attempt,
                &self.observer,
            ));
            tokio::pin!(call);
            // A cancelled call settles before the turn's stop, which
            // publishes the sealed call ahead of its terminal.
            tokio::select! {
                biased;
                result = &mut call => result,
                () = cancel.cancelled() => {
                    stop.cancel();
                    call.await
                }
            }
        }
        .map_err(runtime)?;
        // A provider panic re-raises from what the live outcome held: its
        // receipt keeps no provider text (FIG-4087).
        if let Some(ModelAnswer {
            result: Err(error), ..
        }) = &owed
            && error.code
                == Some(crate::FailureCode::lash(
                    crate::TurnFailureCode::ProviderPanicked,
                ))
        {
            self.driver.provider_panic = Some(error.message.clone());
        }
        self.owed = owed;
        Ok(())
    }

    fn owed_answer(&mut self) -> &mut Option<ModelAnswer> {
        &mut self.owed
    }

    async fn derive_answer(
        &mut self,
        _cx: &ActorContext,
        id: crate::EffectId,
    ) -> Result<(), TurnError> {
        let Some(answer) = self.owed.clone() else {
            return Err(TurnError::Exec(format!(
                "model call {id:?} owes no derivation"
            )));
        };
        Box::pin(
            self.driver
                .derive_llm_answer(&mut self.machine, id, answer, &self.observer),
        )
        .await
        .map_err(runtime)?;
        // A derivation that aborted still owes its receipt, which the next
        // attempt restores from the rows.
        self.owed = None;
        Ok(())
    }

    async fn settle_cancelled_answer(
        &mut self,
        _cx: &ActorContext,
        id: crate::EffectId,
    ) -> Result<(), TurnError> {
        let Some(answer) = self.owed.take() else {
            return Ok(());
        };
        Box::pin(self.driver.settle_cancelled_llm_answer(
            &mut self.machine,
            id,
            answer,
            &self.observer,
        ))
        .await
        .map_err(runtime)
    }

    async fn prepare_call(
        &mut self,
        _cx: &ActorContext,
        id: crate::EffectId,
        call: u32,
        request: Arc<LlmRequest>,
    ) -> Result<PreparedCall, TurnError> {
        let prepared = Box::pin(self.driver.prepare_call(
            &mut self.machine,
            id,
            call,
            request,
            &self.observer,
        ))
        .await
        .map_err(runtime)?;
        if let PreparedCall::Unsent(error) = &prepared
            && error.code
                == Some(crate::FailureCode::lash(
                    crate::TurnFailureCode::ProviderPanicked,
                ))
        {
            self.driver.provider_panic = Some(error.message.clone());
        }
        Ok(prepared)
    }

    fn delivered_inputs(&self) -> Vec<crate::AdmittedTurnInputs> {
        self.driver
            .pending_turn_inputs
            .iter()
            .filter(|admitted| {
                matches!(
                    admitted.mode,
                    crate::TurnInputAdmissionMode::ActiveTurn { .. }
                )
            })
            .cloned()
            .collect()
    }

    fn before_turn(&self) -> Vec<crate::plugin::RecordedTurnContribution> {
        self.before_turn.clone()
    }

    fn trace_scope(&self) -> Option<lash_trace::DurableTraceScope> {
        Some(self.trace_scope.clone())
    }

    fn run_config(&self) -> Option<crate::runtime::durable::run_config::RecordedRunConfig> {
        Some(self.driver.run_config.clone())
    }

    fn run_config_stored(&mut self, stored: &crate::runtime::durable::run_config::RunConfigStored) {
        self.driver.run_config.stored(stored);
    }

    fn run_changes(&self) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
        self.driver.session.plugins().run_changes()
    }

    fn run_changes_committed(&self, written: &[lash_durable::domain::TurnNamespaceWrite]) {
        self.driver.session.plugins().run_changes_committed(written);
    }

    async fn consume_cell(
        &mut self,
        _cx: &ActorContext,
        tx: &mut lash_durable::ActorTx,
        id: crate::EffectId,
        working: bool,
    ) -> Result<(), TurnError> {
        let Some(code_executor) = self.driver.session.plugins().code_executor() else {
            return Ok(());
        };
        let session = self.driver.session_id.clone();
        if working {
            let capture = code_executor
                .capture_execution_graph(
                    crate::plugin::ProtocolSessionContext::new(
                        &session,
                        self.driver.session.fleet_format(),
                    ),
                    crate::plugin::ExecutionGraphPurpose::Working,
                )
                .await
                .map_err(|error| TurnError::Exec(error.to_string()))?;
            let staged = match capture {
                crate::plugin::ExecutionGraphCapture::Replace { root, nodes } => {
                    tx.stage(nodes.into_values());
                    Some(root)
                }
                crate::plugin::ExecutionGraphCapture::Clear => None,
            };
            if staged != self.working {
                tx.write(lash_durable::DomainWrite::Turn(
                    lash_durable::domain::TurnWrite::Working {
                        session: session.clone(),
                        run: self.driver.turn_id.clone(),
                        expected: self.working,
                        working: staged,
                    },
                ));
            }
            self.staged_working = staged;
        }
        // The cell's root is consumed with its answer: the turn holds its
        // outcome in its report and its state as its working state, and the
        // store reclaims the cell's records and admissions with the root.
        // The cell named its execution and the root when it ended: the one
        // that records its end, or, for an end held in memory, its last
        // park (FIG-5999). The turn derives neither again: by this commit
        // its machine stands past the step the cell's key was derived in.
        // A cell the turn stopped in named none, and its root stays until
        // the turn ends.
        if let Some((_, ended)) = self
            .driver
            .answered_cell_end
            .as_ref()
            .filter(|(cell, _)| *cell == id)
        {
            tx.write(lash_durable::DomainWrite::ExecRoot(
                lash_durable::domain::graph::ExecRootWrite::Retire {
                    exec: ended.exec.clone(),
                    expected: ended.root,
                },
            ));
        }
        Ok(())
    }

    fn hold_response(&mut self, held: crate::HeldResponse) -> bool {
        let carries = self
            .driver
            .session
            .plugins()
            .code_executor()
            .is_some_and(|executor| executor.carries_held_response());
        if carries {
            self.driver.held_response = Some(held);
        }
        carries
    }

    fn answered_cell_end_held(&self, id: crate::EffectId) -> bool {
        self.driver
            .answered_cell_end
            .as_ref()
            .is_some_and(|(cell, ended)| *cell == id && ended.held)
    }

    async fn phase_committed(&mut self) {
        let Some(staged) = self.staged_working.take() else {
            return;
        };
        self.working = Some(staged);
        if let Some(code_executor) = self.driver.session.plugins().code_executor() {
            code_executor.acknowledge_execution_state_capture().await;
        }
    }

    fn live_stream_cursor(&self) -> String {
        self.live
            .current_cursor(&self.driver.session_id, self.revision)
            .as_str()
            .to_owned()
    }

    /// The call's earlier attempts streamed into the session's live replay
    /// (on this node or a dead owner's) after the cursor the call pinned
    /// before its first attempt: a replay from there names their prose and
    /// reasoning, which one `ModelAttemptReset` retracts before the re-sent
    /// attempt streams under its own key. A replay that gaps from there
    /// (retention dropped some of it, or the stream restarted since), or
    /// that cannot be read, restarts the stream with a gap: nothing
    /// published after the pin can prove what was dropped before it
    /// (FIG-5399).
    async fn restart_live_stream(
        &mut self,
        _cx: &ActorContext,
        id: crate::EffectId,
        pin: &crate::runtime::durable::session::ModelPin,
    ) -> Result<(), TurnError> {
        let invocation = self
            .driver
            .turn_effect_invocation(&self.machine, id, RuntimeEffectKind::LlmCall)
            .map_err(|error| TurnError::Exec(format!("the model call's identity: {error}")))?;
        let base = invocation.effect_replay_key().to_owned();
        let session = &self.driver.session_id;
        let replayed = match crate::SessionCursor::from_store_token(pin.stream_from.as_str()) {
            Ok(from) => self
                .live
                .replay_after_cursor(&from)
                .await
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        let reset = match replayed {
            Ok(crate::LiveReplayOutcome::Replayed(events)) => Some(
                super::abandoned_stream::attempt_reset(&events, &self.driver.turn_id, &base),
            ),
            Ok(crate::LiveReplayOutcome::Gap(_)) => None,
            Err(error) => {
                tracing::warn!(session_id = %session, %error, "the live replay of a re-sent call's earlier attempts did not read");
                None
            }
        };
        let Some(reset) = reset else {
            return self
                .live
                .invalidate_session(session)
                .await
                .map_err(|error| {
                    TurnError::Exec(format!("the live stream did not restart: {error}"))
                });
        };
        let id = crate::TurnActivityId::observed(
            format!(
                "{}:reset",
                super::abandoned_stream::model_stream_key(&base, pin.attempt)
            ),
            0,
        );
        let activity = crate::TurnActivity {
            correlation_id: id.clone(),
            id,
            event: reset,
        };
        self.live
            .publish(
                session,
                self.revision,
                vec![crate::LiveReplayEventDraft::new(
                    Some(&self.driver.turn_id),
                    crate::SessionObservationEventPayload::TurnActivity(activity),
                )],
            )
            .await
            .map(drop)
            .map_err(|error| TurnError::Exec(format!("the live stream did not restart: {error}")))
    }

    fn tools(&mut self) -> Result<Arc<dyn RoundTools>, TurnError> {
        self.driver
            .round_tools(&self.observer, self.machine.protocol_iteration())
            .map_err(runtime)
    }

    async fn exec_cell(
        &mut self,
        _cx: &ActorContext,
        id: crate::EffectId,
        cell: CodeCell,
    ) -> Result<CellExit, TurnError> {
        Box::pin(self.driver.handle_exec_code_effect(
            &mut self.machine,
            id,
            cell.language,
            cell.code,
            &self.observer,
        ))
        .await
        .map_err(cell_abort)
    }

    fn answered_cell_calls(&mut self) -> CellToolCalls {
        std::mem::take(&mut self.driver.answered_cell_calls)
    }

    async fn stopped_cell_calls(
        &mut self,
        cx: &ActorContext,
        id: crate::EffectId,
    ) -> Result<CellToolCalls, TurnError> {
        cell_calls(&self.driver, &self.machine, cx, id).await
    }

    fn finishing_cell_calls(&mut self, calls: &CellToolCalls) {
        self.finishing_cell_calls = Some(calls.clone());
    }

    fn stop_cell(&mut self) {
        self.driver.children_stop.cancel();
    }

    /// The commit is built over the head the runtime opened at; the store
    /// refuses it once the session's head, `head` among its readers, is
    /// elsewhere. The turn's after-turn callbacks run first, once its
    /// outcome is known (FIG-5283): what they decide commits with it.
    async fn finish(
        &mut self,
        cx: &ActorContext,
        done: TurnDone,
        _head: &SessionHead,
    ) -> Result<TurnCommit, TurnError> {
        self.effect_phase.take();
        self.commit_phase = Some(TurnPhaseSpan::begin(
            self.driver.turn_phase_probe.clone(),
            RuntimeTurnPhase::CommittedTurn,
        ));
        let driver = &mut self.driver;
        driver.apply_final_event_delta(done.event_delta, done.protocol_iteration);
        driver.turn_pipeline.record_protocol_terminal_output(
            driver.protocol_reply.terminal_output(done.messages.iter()),
        );
        let outcome = done
            .outcome
            .unwrap_or(TurnOutcome::Stopped(TurnStop::Incomplete));
        let plugins = Arc::clone(driver.session.plugins());
        let observed = plugins.has_runtime_event_hooks();
        let finished = if plugins.has_after_turn_hooks() || observed {
            let unrecorded = self.finishing_cell_calls.take();
            Some(
                FinishedTurn::read(
                    cx,
                    &driver.session_id,
                    &driver.turn_id,
                    &outcome,
                    unrecorded,
                    &plugins.transcript_decoders(),
                )
                .await?,
            )
        } else {
            None
        };
        let after_turn = match &finished {
            Some(finished) => Box::pin(driver.run_after_turn(finished, &self.observer))
                .await
                .map_err(runtime)?,
            None => None,
        };
        let failure_evidence = driver.failure_evidence.clone();
        // The checkpoint owns usage, including calls a previous owner
        // completed. Project it once at the commit, preserving the head's
        // usage when this turn made no call and its last prompt when the
        // last call reported zero usage.
        if let Some(last) = self.machine.last_call_usage() {
            let state = driver.turn_pipeline.state_mut();
            state.token_usage = self.machine.cumulative_usage().clone();
            if let Some(prompt) = nonzero_usage(last.clone()) {
                state.last_prompt_usage = Some(prompt);
            }
        }
        let mut commit = driver
            .turn_pipeline
            .durable_commit(
                done.messages,
                &outcome,
                &failure_evidence,
                Some(&mut driver.session),
                after_turn.as_ref(),
            )
            .await
            .map_err(|error| TurnError::Exec(format!("the turn's head commit: {error}")))?;
        let panic = match &outcome {
            TurnOutcome::Stopped(TurnStop::ToolPanicked { message, .. }) => {
                Some(("tool_panicked", message.clone()))
            }
            TurnOutcome::Stopped(TurnStop::ProviderError { .. }) => driver
                .provider_panic
                .take()
                .map(|message| ("provider_panicked", message)),
            _ => None,
        };
        self.after_commit = AfterCommit {
            panic,
            state: after_turn.map(|after_turn| after_turn.state),
            finalized: finished
                .filter(|_| observed)
                .map(|finished| finished.finalized(driver.turn_pipeline.state().to_snapshot())),
        };
        // The commit settles every input the turn executed, with the
        // application evidence its delivery recorded: those its run took and
        // those its checkpoints delivered.
        let mut settlement = self.settlement.clone();
        settlement.completed_inputs = driver
            .pending_turn_inputs
            .iter()
            .map(crate::AdmittedTurnInputs::completion)
            .collect();
        if !settlement.completed_inputs.is_empty() {
            commit.ingress = Some(settlement);
        }
        // The turn's activity reaches the live stream before its commit is
        // durable: a follower that reads the run ended from the store finds
        // everything the run published already there, and waits for no
        // observation of the commit (FIG-5507). The turn's outcome stays held
        // for the commit (ADR 0122, FIG-5800).
        self.observer.published().await;
        self.committing = self
            .commit
            .as_ref()
            .map(|commit| self.published.committing(commit));
        Ok(TurnCommit {
            expected_head: commit.expected_head_revision,
            commit: crate::store::encode_session_commit(&commit)
                .map_err(|error| TurnError::Exec(error.to_string()))?,
        })
    }

    /// The after-turn callbacks' state publishes from the acknowledged
    /// commit. What the outcome held back for its commit is published with it,
    /// and the publisher ends once that reached the live stream; then the
    /// commit itself, which settles the turn's provisional activity; then
    /// the lifecycle observers see the finalized turn.
    async fn committed(&mut self) {
        self.commit_phase.take();
        let _delivery = TurnPhaseSpan::begin(
            self.driver.turn_phase_probe.clone(),
            RuntimeTurnPhase::PostCommitDelivery,
        );
        let AfterCommit {
            state,
            finalized,
            panic,
        } = std::mem::take(&mut self.after_commit);
        let plugins = Arc::clone(self.driver.session.plugins());
        if let Some(state) = state
            && let Err(error) = plugins.publish_committed_state(state.resolutions())
        {
            tracing::warn!(
                %error,
                "a committed turn's after-turn state did not publish; its namespaces are fenced"
            );
        }
        self.observer.release_terminal();
        self.observer.close();
        if let Err(error) = (&mut self.publisher).await {
            tracing::warn!(%error, "a committed turn's live publisher did not finish");
        }
        if let Some(commit) = &self.commit {
            commit
                .publish(
                    self.live.as_ref(),
                    &self.published,
                    Some(&self.driver.turn_id),
                )
                .await;
        }
        self.committing.take();
        // The lifecycle observers see the turn once it is final: they have
        // no veto, and what they fail with is reported, not committed.
        if let Some(turn) = finalized
            && let Err(error) = plugins
                .dispatch(self.driver.turn_phase_probe.as_ref())
                .emit_runtime_event(crate::PluginLifecycleEvent::TurnFinalized(Arc::new(turn)))
                .await
        {
            tracing::warn!(%error, "a finalized turn's lifecycle observers failed");
        }
        // Both the call's typed outcome and its terminal turn are durable.
        // A redrive reads that terminal instead of invoking host code again.
        if let Some((code, message)) = panic
            && crate::panic_containment::is_loud()
        {
            panic!("{code}: {message}");
        }
    }
}

impl RuntimeTurnDriver<'static> {
    /// The turn's round tools: its catalog, under the turn's own opener.
    fn round_tools(
        &self,
        observer: &TurnObserver,
        protocol_iteration: usize,
    ) -> Result<Arc<dyn RoundTools>, RuntimeError> {
        let context = self
            .execution_context(
                observer,
                Arc::new(crate::ChronologicalProjection::default()),
            )
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::ToolCatalogResolutionFailed,
                    error.to_string(),
                )
            })?;
        context
            .with_tracing(self.execution_tracing(protocol_iteration))
            .round_tools(crate::EffectOpener::turn(
                self.session_id.clone(),
                self.turn_id.clone(),
            ))
            .map_err(RuntimeEffectControllerError::into_runtime_error)
    }

    /// The machine a fresh turn starts from `messages`, in `window`.
    fn turn_machine(
        &mut self,
        window: crate::runtime::durable::head::TurnWindow,
        messages: crate::MessageSequence,
    ) -> TurnMachine {
        self.prepare_machine(window, messages, 0)
    }

    /// Install the tool surface the turn's checkpoint records as the one its
    /// last sync installed, over the session's live executors.
    async fn reinstall_tool_surface(&mut self) -> Result<(), TurnError> {
        let recorded = match self.run_config.surface() {
            Some(recorded) => Arc::clone(recorded),
            // A checkpoint taken before the run's first sync records none:
            // the restored machine syncs before it offers a tool.
            None => return Ok(()),
        };
        self.session
            .install_recorded_tool_surface(recorded)
            .map_err(|error| {
                TurnError::Exec(format!("the tool surface did not install: {error}"))
            })?;
        self.surface_installed = true;
        Ok(())
    }
}

#[cfg(test)]
#[path = "sequence_sharing_law.rs"]
mod sequence_sharing_law;
