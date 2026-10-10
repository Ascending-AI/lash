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
    CellExit, CellToolCalls, CodeCell, ModelCallAttempt, OpenTurn, ParkedTurnState, PreparedCall,
    RestoredTurn, TurnCommit, TurnDone, TurnDrive, TurnError, TurnRestore,
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
            messages,
            before_turn,
            trace_scope,
            attachments,
        } = turn;
        let machine = fresh_machine(&mut driver, messages)?;
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
            messages,
            before_turn,
            trace_scope,
            attachments,
        } = turn;
        let config = fresh_machine(&mut driver, messages)?.into_config();
        let RestoredTurn {
            machine,
            namespaces,
            delivered,
            pending,
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
        // The tool surface a sync before the checkpoint recorded: pinned
        // again from the session's live registry.
        driver.reinstall_tool_surface().await?;
        if let Some(crate::Effect::ExecCode { id, .. }) = &pending {
            check_cell_snapshot(cx, &driver, &machine, *id).await?;
        }
        Ok(OpenTurn {
            drive: Box::new(Self::assemble(
                driver,
                machine,
                before_turn,
                attachments,
                trace_scope,
                parts,
            )),
            pending,
            row,
        })
    }

    fn assemble(
        driver: Box<RuntimeTurnDriver<'static>>,
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
        Self {
            effect_phase,
            commit_phase: None,
            committing: None,
            finishing_cell_calls: None,
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
    let Some(snapshot) = cx.durable_reads()?.snapshot(&exec).await? else {
        return Ok(());
    };
    let undecodable = |reason| TurnError::UndecodableState {
        state: ParkedTurnState::CellSnapshot,
        reason,
    };
    code_executor
        .check_cell_snapshot(&snapshot.snapshot_ref)
        .await
        .map_err(runtime)?
        .map_err(undecodable)?;
    if !cx.carries_sessions().await? {
        return Ok(());
    }
    let Some(carried) = code_executor
        .carried_cell_snapshot(&snapshot.snapshot_ref)
        .await
        .map_err(runtime)?
        .map_err(undecodable)?
    else {
        return Ok(());
    };
    let mut tx = cx.begin().await?;
    tx.write(lash_durable::DomainWrite::Snapshot(
        lash_durable::domain::SnapshotWrite::Put {
            exec,
            expected: Some(snapshot.rev),
            snapshot_ref: carried.snapshot,
            executable_identity: carried.executable_identity,
            format_version: carried.format_version,
        },
    ));
    tx.stamp_formats(cx.backend().formats().session().clone());
    cx.commit(tx, lash_durable::CommitLabel::CELL_SNAPSHOT)
        .await?;
    Ok(())
}

/// The machine a prepared turn starts: ended at once when its recorded model
/// selection is refused.
fn fresh_machine(
    driver: &mut RuntimeTurnDriver<'static>,
    messages: crate::MessageSequence,
) -> Result<TurnMachine, TurnError> {
    driver.protocol_reply.mark_run_start(messages.iter());
    let refused = driver.validate_recorded_selection().err();
    let mut machine = driver.turn_machine(messages);
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
            ..
        } = attempt;
        self.driver
            .protocol_reply
            .mark_model_call(self.machine.messages().iter());
        let stop = self.driver.children_stop.clone();
        let call = Box::pin(self.driver.handle_llm_call_effect(
            &mut self.machine,
            id,
            request,
            admitted,
            attempt,
            &self.observer,
        ));
        tokio::pin!(call);
        let result = tokio::select! {
            biased;
            result = &mut call => result,
            () = cancel.cancelled() => {
                stop.cancel();
                call.await
            }
        }
        .map_err(runtime);
        if cancel.is_cancelled() {
            // Publish the sealed model call before the turn's cancel terminal:
            // dropping this drive otherwise aborts its live publisher.
            self.observer.close();
            let _ = (&mut self.publisher).await;
        }
        result
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

    fn run_changes(&self) -> Vec<lash_durable::domain::TurnNamespaceWrite> {
        self.driver.session.plugins().run_changes()
    }

    fn run_changes_committed(&self, written: &[lash_durable::domain::TurnNamespaceWrite]) {
        self.driver.session.plugins().run_changes_committed(written);
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
        use lash_core_execution::runtime::actor::round::{self, PolicyView, SettledOutput};
        use lash_durable::domain::{CellId, ExecKey, OwnerKey};

        let Some(code_executor) = self.driver.session.plugins().code_executor() else {
            return Ok(CellToolCalls::default());
        };
        // The cell's snapshot and records are filed under its replay key,
        // which the restored machine derives again.
        let invocation = self
            .driver
            .turn_effect_invocation(&self.machine, id, RuntimeEffectKind::ExecCode)
            .map_err(|error| TurnError::Exec(error.to_string()))?;
        let cell = CellId::new(invocation.effect_replay_key());
        let session = self.driver.session_id.clone();
        let run = self.driver.turn_id.clone();
        let reads = cx.durable_reads()?;
        let exec = ExecKey::Cell(session.clone(), run.clone(), cell.clone());
        let mut records = match reads.snapshot(&exec).await? {
            Some(snapshot) => code_executor
                .snapshot_tool_calls(&snapshot.snapshot_ref)
                .map_err(|error| {
                    TurnError::Exec(format!(
                        "the stopped cell's snapshot does not read: {error}"
                    ))
                })?,
            None => Vec::new(),
        };
        // A call that settled after the cell's latest snapshot is in no
        // ledger yet: its outcome is still among the cell's own records,
        // which only the next snapshot prunes. A member without a completed
        // answer never finished, and is not reported.
        let rows = reads
            .run_records(&OwnerKey::Cell(session, run, cell))
            .await?;
        let waits = round::PinnedWaits::read(reads, &rows).await?;
        let fold = round::fold(&rows, &PolicyView::new([]), &waits)
            .map_err(|error| TurnError::Exec(format!("the stopped cell's records: {error}")))?;
        for member in fold.rounds().flat_map(|round| round.members()) {
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
            records.push(crate::ToolCallRecord {
                call_id: completed.call_id,
                provider_call_id: completed.provider_call_id,
                tool: completed.tool_name,
                args: completed.args,
                output: completed.output,
            });
        }
        Ok(self.driver.bounded_cell_calls(records))
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
        driver.turn_pipeline.apply_event_delta(done.event_delta);
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
        // Code outputs live inside protocol records, outside the message
        // parts the boundary can inspect. Acquire their session holds in the
        // same commit before the execution's staging holds may end (ADR 0124).
        let mut attachments = std::mem::take(&mut commit.committed_attachment_ids);
        attachments.extend(
            driver
                .recorded_assembly
                .retained_outputs
                .iter()
                .map(|retained| retained.reference.id.clone()),
        );
        attachments.sort();
        attachments.dedup();
        commit = commit.with_committed_attachments(attachments);
        let panic = match &outcome {
            TurnOutcome::Stopped(TurnStop::ToolPanicked { message, .. }) => {
                Some(("tool_panicked", message.clone()))
            }
            TurnOutcome::Stopped(TurnStop::ProviderError) => driver
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
            commit_json: crate::store::encode_session_commit(&commit)
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

    /// The machine a fresh turn starts from `messages`.
    fn turn_machine(&mut self, messages: crate::MessageSequence) -> TurnMachine {
        self.prepare_machine(messages, 0)
    }

    /// Pin the session's live tool surface again, as the turn's sync did.
    async fn reinstall_tool_surface(&mut self) -> Result<(), TurnError> {
        let surface = self
            .prepare_execution_environment()
            .await
            .map_err(|error| match error {
                crate::PluginError::Runtime(error) => TurnError::Runtime(error),
                error => TurnError::Exec(format!("the tool surface did not pin: {error}")),
            })?;
        let authority = &self.turn_pipeline.state().authority;
        self.session
            .install_recorded_tool_surface(&authority.tool_access, &surface.tool_definitions)
            .map_err(|error| TurnError::Exec(format!("the tool surface did not install: {error}")))
    }
}
