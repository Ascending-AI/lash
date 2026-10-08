//! The production turn drive on the durable substrate (ADR 0132 §4; L3,
//! FIG-5172): the turn driver's own handlers answering the effects the phase
//! runner hands it. The phase runner owns every commit; the drive runs what
//! a phase does in memory and builds the turn's head commit.

use std::sync::Arc;

use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
use lash_core_execution::runtime::actor::round::RoundTools;

use super::*;
use crate::runtime::durable::commit_publication::{CommitBase, PublishedHeads};
use crate::runtime::durable::head::SessionHead;
use crate::runtime::durable::session::{
    CellExit, CodeCell, OpenTurn, PreparedCall, RestoredTurn, TurnCommit, TurnDone, TurnDrive,
    TurnError, TurnRestore,
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
    /// How many of the driver's queued-work sets the run's own admission
    /// took: those after them are what the turn's checkpoints delivered.
    opening_work: usize,
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
}

#[derive(Default)]
struct AfterCommit {
    state: Option<crate::plugin::StagedPluginState>,
    finalized: Option<crate::AssembledTurn>,
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
            invalid_input,
            attachments,
        } = turn;
        let machine = fresh_machine(&mut driver, messages, &parts.observer, invalid_input)?;
        let opening_work = driver.pending_queued.len();
        Ok(Self::assemble(
            driver,
            machine,
            opening_work,
            before_turn,
            attachments,
            trace_scope,
            parts,
        ))
    }

    /// A turn taken over from its checkpoint: the machine restored under the
    /// configuration a fresh machine of the prepared turn is built with.
    pub(in crate::runtime) async fn resume(
        turn: DurableTurn,
        parts: DriveParts,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError> {
        let DurableTurn {
            mut driver,
            messages,
            before_turn,
            trace_scope,
            invalid_input,
            attachments,
        } = turn;
        let config =
            fresh_machine(&mut driver, messages, &parts.observer, invalid_input)?.into_config();
        let RestoredTurn {
            machine,
            namespaces,
            delivered,
            delivered_work,
            pending,
            row,
        } = restore.restore(config).await?;
        // The steering input and queued turn work the turn's checkpoints
        // delivered before its last phase committed are bound to its run;
        // the turn's commit settles them with the rows its run took.
        driver.pending_turn_inputs.extend(delivered);
        let opening_work = driver.pending_queued.len();
        driver.pending_queued.extend(delivered_work);
        // The namespaces the turn's run committed, the pending
        // checkpoint-callback decisions of an admitted call among them, over
        // the head the run started from, are reinstalled: preparing the turn
        // again served its recorded before-turn decisions and ran no
        // callback, so nothing that committed runs again.
        driver
            .session
            .plugins()
            .restore_run(namespaces)
            .map_err(|error| {
                TurnError::Exec(format!(
                    "the turn's plugin state did not reinstall: {error}"
                ))
            })?;
        // The records the restored machine already delivered through its
        // progress boundaries reached only the previous owner's draft: they
        // join this one, so the turn's commit holds the history an uncut run
        // commits. The draft holds the history the turn started from, which
        // the machine's leads with.
        let started = driver.turn_pipeline.active_events().len();
        driver.turn_pipeline.apply_event_delta(
            machine
                .progressed_events()
                .get(started..)
                .unwrap_or_default()
                .to_vec(),
        );
        // The tool surface a sync before the checkpoint recorded: pinned
        // again from the session's live registry.
        driver.reinstall_tool_surface()?;
        Ok(OpenTurn {
            drive: Box::new(Self::assemble(
                driver,
                machine,
                opening_work,
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
        opening_work: usize,
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
            driver,
            machine,
            observer,
            settlement,
            opening_work,
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

/// The machine a prepared turn starts: ended at once when its input did not
/// normalize or its recorded model selection is refused.
fn fresh_machine(
    driver: &mut RuntimeTurnDriver<'static>,
    messages: crate::MessageSequence,
    observer: &TurnObserver,
    invalid_input: Option<String>,
) -> Result<TurnMachine, TurnError> {
    driver.protocol_reply.mark_run_start(messages.iter());
    let refused = driver.validate_recorded_selection().err();
    let mut machine = driver.turn_machine(messages);
    if let Some(message) = invalid_input {
        driver.emit_recorded(
            observer,
            make_error_event(
                crate::TurnFailureKind::InputValidation,
                Some(crate::TurnFailureCode::InvalidTurnInput.into()),
                message.clone(),
                Some(message),
            ),
        );
        machine.finish_with_outcome(TurnOutcome::Stopped(TurnStop::InvalidInput));
    } else if let Some(event) = refused {
        machine.fail_turn(*event);
    }
    Ok(machine)
}

fn runtime(error: RuntimeError) -> TurnError {
    TurnError::Runtime(error)
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
                if machine.is_done() && matches!(event, SessionStreamEvent::Error { .. }) {
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
        body: &lash_sansio::llm::types::ProviderRequestBody,
        attempt: u32,
        _limit: crate::ExecutionLimit,
    ) -> Result<(), TurnError> {
        self.driver
            .protocol_reply
            .mark_model_call(self.machine.messages().iter());
        Box::pin(self.driver.handle_llm_call_effect(
            &mut self.machine,
            id,
            request,
            body,
            attempt,
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
        Box::pin(
            self.driver
                .prepare_call(&mut self.machine, id, call, request, &self.observer),
        )
        .await
        .map_err(runtime)
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

    fn delivered_work(&self) -> Vec<crate::AdmittedQueuedWork> {
        self.driver
            .pending_queued
            .get(self.opening_work..)
            .unwrap_or_default()
            .to_vec()
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
                tracing::warn!(%session, %error, "the live replay of a re-sent call's earlier attempts did not read");
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
        self.driver.recorded_assembly.note_code_execution();
        Box::pin(self.driver.handle_exec_code_effect(
            &mut self.machine,
            id,
            cell.language,
            cell.code,
            &self.observer,
        ))
        .await
        .map_err(runtime)
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
            Some(FinishedTurn::read(cx, &driver.session_id, &driver.turn_id, &outcome).await?)
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
        self.after_commit = AfterCommit {
            state: after_turn.map(|after_turn| after_turn.state),
            finalized: finished
                .filter(|_| observed)
                .map(|finished| finished.finalized(driver.turn_pipeline.state().to_snapshot())),
        };
        // The commit settles every input and every queued work batch the
        // turn executed, the inputs with the application evidence their
        // delivery recorded: those its run took and those its checkpoints
        // delivered.
        let mut settlement = self.settlement.clone();
        settlement.completed_inputs = driver
            .pending_turn_inputs
            .iter()
            .map(crate::AdmittedTurnInputs::completion)
            .collect();
        settlement.completed_batches = driver
            .pending_queued
            .iter()
            .map(crate::AdmittedQueuedWork::completion)
            .collect();
        if !settlement.completed_inputs.is_empty() || !settlement.completed_batches.is_empty() {
            commit.ingress = Some(settlement);
        }
        Ok(TurnCommit {
            expected_head: commit.expected_head_revision,
            commit_json: crate::store::encode_session_commit(&commit)
                .map_err(|error| TurnError::Exec(error.to_string()))?,
        })
    }

    /// The after-turn callbacks' state publishes from the acknowledged
    /// commit. What the turn held back for its commit is published with it,
    /// and the publisher ends once everything queued reached the live
    /// stream; then the commit itself, which settles the turn's provisional
    /// activity; then the lifecycle observers see the finalized turn.
    async fn committed(&mut self) {
        self.commit_phase.take();
        let _delivery = TurnPhaseSpan::begin(
            self.driver.turn_phase_probe.clone(),
            RuntimeTurnPhase::PostCommitDelivery,
        );
        let AfterCommit { state, finalized } = std::mem::take(&mut self.after_commit);
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
    fn reinstall_tool_surface(&mut self) -> Result<(), TurnError> {
        let surface = self
            .prepare_execution_environment()
            .map_err(|error| TurnError::Exec(format!("the tool surface did not pin: {error}")))?;
        let authority = &self.turn_pipeline.state().authority;
        self.session
            .install_recorded_tool_surface(&authority.tool_access, &surface.tool_definitions)
            .map_err(|error| TurnError::Exec(format!("the tool surface did not install: {error}")))
    }
}
