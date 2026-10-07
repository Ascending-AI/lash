//! The production turn drive on the durable substrate (ADR 0132 §4; L3,
//! FIG-5172): the turn driver's own handlers answering the effects the phase
//! runner hands it. The phase runner owns every commit; the drive runs what
//! a phase does in memory and builds the turn's head commit.

use std::sync::Arc;

use lash_core_execution::runtime::actor::round::RoundTools;

use super::*;
use crate::runtime::durable::head::SessionHead;
use crate::runtime::durable::session::{
    CellExit, CodeCell, OpenTurn, RestoredTurn, TurnCommit, TurnDone, TurnDrive, TurnError,
    TurnRestore,
};
use crate::runtime::turn_loop::DurableTurn;

/// One turn the session actor runs through the turn driver.
pub(in crate::runtime) struct RuntimeDrive {
    driver: Box<RuntimeTurnDriver<'static>>,
    machine: TurnMachine,
    observer: TurnObserver,
    /// The run's bound rows, settled by its commit.
    settlement: crate::store::IngressSettlement,
    tools: Option<Arc<dyn RoundTools>>,
    live: Arc<dyn crate::LiveReplayStore>,
    /// Publishes the turn's activity to the live stream: drained once the
    /// turn committed, aborted when the drive is dropped without a commit.
    publisher: tokio::task::JoinHandle<()>,
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
    pub(in crate::runtime) publisher: tokio::task::JoinHandle<()>,
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
            invalid_input,
        } = turn;
        let machine = fresh_machine(&mut driver, messages, &parts.observer, invalid_input)?;
        Ok(Self::assemble(driver, machine, parts))
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
            invalid_input,
        } = turn;
        let config =
            fresh_machine(&mut driver, messages, &parts.observer, invalid_input)?.into_config();
        let RestoredTurn {
            machine,
            pending,
            row,
        } = restore.restore(config).await?;
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
            drive: Box::new(Self::assemble(driver, machine, parts)),
            pending,
            row,
        })
    }

    fn assemble(
        driver: Box<RuntimeTurnDriver<'static>>,
        machine: TurnMachine,
        parts: DriveParts,
    ) -> Self {
        let DriveParts {
            observer,
            settlement,
            live,
            publisher,
        } = parts;
        Self {
            driver,
            machine,
            observer,
            settlement,
            tools: None,
            live,
            publisher,
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
                if let SessionStreamEvent::TokenUsage {
                    usage, cumulative, ..
                } = &event
                {
                    driver.turn_pipeline.state_mut().token_usage = cumulative.clone();
                    driver.latest_prompt_usage = nonzero_usage(usage.clone());
                }
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
            Effect::ReportToolCalls { completed } => {
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
        _attempt: u32,
        _limit: crate::ExecutionLimit,
    ) -> Result<(), TurnError> {
        self.driver
            .protocol_reply
            .mark_model_call(self.machine.messages().iter());
        Box::pin(
            self.driver
                .handle_llm_call_effect(&mut self.machine, id, request, &self.observer),
        )
        .await
        .map_err(runtime)
    }

    async fn restart_live_stream(&mut self, _cx: &ActorContext) -> Result<(), TurnError> {
        self.live
            .invalidate_session(&self.driver.session_id)
            .await
            .map_err(|error| TurnError::Exec(format!("the live stream did not restart: {error}")))
    }

    fn tools(&mut self) -> Result<Arc<dyn RoundTools>, TurnError> {
        if let Some(tools) = &self.tools {
            return Ok(Arc::clone(tools));
        }
        let tools = self.driver.round_tools(&self.observer).map_err(runtime)?;
        self.tools = Some(Arc::clone(&tools));
        Ok(tools)
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
    /// elsewhere.
    async fn finish(
        &mut self,
        _cx: &ActorContext,
        done: TurnDone,
        _head: &SessionHead,
    ) -> Result<TurnCommit, TurnError> {
        let driver = &mut self.driver;
        driver.turn_pipeline.apply_event_delta(done.event_delta);
        driver.turn_pipeline.record_protocol_terminal_output(
            driver.protocol_reply.terminal_output(done.messages.iter()),
        );
        let outcome = done
            .outcome
            .unwrap_or(TurnOutcome::Stopped(TurnStop::Incomplete));
        let failure_evidence = driver.failure_evidence.clone();
        let mut commit = driver
            .turn_pipeline
            .durable_commit(
                done.messages,
                &outcome,
                &failure_evidence,
                Some(&mut driver.session),
            )
            .await
            .map_err(|error| TurnError::Exec(format!("the turn's head commit: {error}")))?;
        if !self.settlement.completed_inputs.is_empty()
            || !self.settlement.completed_batches.is_empty()
        {
            commit.ingress = Some(self.settlement.clone());
        }
        Ok(TurnCommit {
            expected_head: commit.expected_head_revision,
            commit_json: crate::store::encode_session_commit(&commit)
                .map_err(|error| TurnError::Exec(error.to_string()))?,
        })
    }

    /// What the turn held back for its commit is published with it, and the
    /// publisher ends once everything queued reached the live stream.
    async fn committed(&mut self) {
        self.observer.release_terminal();
        self.observer.close();
        if let Err(error) = (&mut self.publisher).await {
            tracing::warn!(%error, "a committed turn's live publisher did not finish");
        }
    }
}

impl RuntimeTurnDriver<'static> {
    /// The turn's round tools: its catalog, under the turn's own opener.
    fn round_tools(&self, observer: &TurnObserver) -> Result<Arc<dyn RoundTools>, RuntimeError> {
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
            .install_recorded_tool_surface(
                &authority.tool_access,
                authority.subagent.as_ref(),
                &surface.tool_definitions,
            )
            .map_err(|error| TurnError::Exec(format!("the tool surface did not install: {error}")))
    }
}
