//! Running one admitted run (FIG-3600, FIG-3927): its recorded admission,
//! the head that admission is headed by, and its turns to their terminal
//! commit.
//!
//! A run admits the turn-lane run its admission named, input-headed or
//! queued-headed alike, as a recorded step keyed by the run, so every
//! redrive of the run, under any admission, replays the same admission and
//! never re-reads pending rows. The admission records the head the run executes
//! on and its turn index; a redrive rebuilds the run from that head, never
//! from the live one its own commit may have moved (FIG-3682). A command
//! run applies the session's open command run and admits no turn.

use std::sync::Arc;

use super::{ExecutedRun, ShiftSinks, shift_abort};
use crate::engine::{Admitted, RunOutcome, ShiftAbort, shift_run_scope};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::runtime::logical_turn::{LogicalTurnAdmissions, LogicalTurnStart};
use crate::runtime::turn_loop::TurnStopwatch;
use crate::store::{AdmittedHead, FollowOnRecoveryAnswer, RunAdmissionAnswer, RunAdmissionRefusal};
use crate::{
    RuntimeError, RuntimeErrorCode, ScopedEffectController, SessionError, TurnId, TurnInput,
};
use lash_core_execution::runtime::effect::AdmittedHeadVerdict;

impl LashRuntime {
    /// Admit the turn-lane run `admitted` is headed by and execute it as the
    /// run's logical turn, under the shift fence the run's seal raised.
    /// The admission records `executor`, the execution that executes the run.
    #[allow(
        clippy::too_many_arguments,
        reason = "the run's admission takes the shift fence and its executor from the shift path that runs it, beside the head, sinks and live input the turn needs"
    )]
    pub(super) async fn execute_run(
        &mut self,
        run_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        head: &AdmittedHead,
        sinks: &ShiftSinks<'_>,
        live: Option<(&crate::InputId, &TurnInput)>,
        fence: &crate::store::ShiftFence,
        executor: crate::store::RunExecutor,
    ) -> Result<ExecutedRun, ShiftAbort> {
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let run = admitted.run().clone();
        let abort = |error: RuntimeError| shift_abort(Some(&run), error);
        let store = self.shift_store()?;
        // The admission records the head and the turn index, so the resident
        // head is brought current first; a replay reads both from the journal
        // instead (FIG-3682). The refresh reads the live session outside any
        // recorded step, so its outcome must never decide what the run
        // journals (FIG-4346): a refresh that failed, a deleted session's
        // among them, leaves the shift no head to admit on, and the run
        // issues the same recorded steps headless, whose bodies answer only
        // why ([`execute_headless_run`]).
        if let Err(fault) = self.refresh_resident_head().await {
            return execute_headless_run(
                run_controller,
                admitted,
                head,
                HeadlessRun::Unrefreshed {
                    catalog: self.host.core.session_store_factory(),
                    fault,
                },
            )
            .await
            .map(|outcome| ExecutedRun {
                outcome,
                run: None,
                executed_inputs: Vec::new(),
                empty_drain: None,
            });
        }
        let answer = execute_run_admission(
            run_controller,
            admitted,
            head,
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(AdmitRunRunner {
                    store: store.clone(),
                    effect_host: Arc::clone(&self.host.core.control.effect_host),
                    scope: run_controller.admitted_scope().clone(),
                    fence: fence.clone(),
                    head: head.clone(),
                    run: run.clone(),
                    max_inputs: self
                        .host
                        .core
                        .durability
                        .queued_work_batching
                        .max_turn_input_admission(),
                    policy: self
                        .host
                        .core
                        .durability
                        .queued_work_batching
                        .admission_policy(self.max_context_tokens().map_err(&abort)?),
                    base: crate::store::SessionHeadRef {
                        // Read by the admission body on its first execution.
                        generation: 0,
                        revision: self.state.head_revision,
                        leaf: self.state.session_graph.leaf_node_id.clone(),
                        checkpoint: self.state.checkpoint_ref.clone(),
                    },
                    // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                    turn_index: self.state.turn_index + 1,
                    generation: crate::runtime::turn_loop::generation_fence::current(self),
                    admitted_generation: admitted.admitted_generation().clone(),
                    executor,
                    plugin_host: self
                        .session
                        .as_ref()
                        .map(|session| session.plugins().host().clone()),
                    trace: AdmissionTrace {
                        tracing: self.host.core.tracing.clone(),
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        turn_index: self.state.turn_index + 1,
                        live: None,
                    },
                }),
                None,
            ),
        )
        .await?;
        let admission = match answer {
            Ok(RunAdmissionAnswer::Admitted { admission }) => {
                let live = ResidentHead {
                    revision: self.state.head_revision,
                    leaf: self.state.session_graph.leaf_node_id.clone(),
                    checkpoint: self.state.checkpoint_ref.clone(),
                };
                let verdict = execute_head_inspection(
                    run_controller,
                    admitted,
                    head,
                    lash_core_execution::core_internal::owned_runner_executor(
                        Box::new(InspectAdmittedHeadRunner {
                            store: store.clone(),
                            run: run.clone(),
                            head: head.clone(),
                            base: admission.base.clone(),
                            live,
                        }),
                        None,
                    ),
                )
                .await?
                .map_err(abort)?;
                crate::runtime::turn_loop::generation_fence::admit(
                    self,
                    admission.generation.as_ref(),
                )
                .map_err(abort)?;
                if let Err(error) = self
                    .services
                    .plugins
                    .host()
                    .validate_plugin_admission(&admission.plugins)
                {
                    let error = error.into_turn_failure(RuntimeErrorCode::Plugin);
                    self.record_turn_park_after_abort(&error, &run, None).await;
                    return Err(abort(error));
                }
                let transition = self
                    .record_plugin_transition(run_controller, admitted, &admission)
                    .await?;
                if let Err(error) = self
                    .adopt_admitted_turn(
                        AdmittedTurn {
                            base: &admission.base,
                            turn_index: admission.turn_index,
                            generation: admission.generation.as_ref(),
                            plugins: &admission.plugins,
                            run: &run,
                        },
                        verdict,
                    )
                    .await
                {
                    self.record_turn_park_after_abort(&error, &run, None).await;
                    return Err(abort(error));
                }
                if let Some(record) = transition {
                    let plugins = &self.services.plugins;
                    plugins.adopt_plugin_transition(&record).map_err(|error| {
                        abort(crate::RuntimeEffectControllerError::from(error).into_runtime_error())
                    })?;
                    self.state.authority.plugin_config = record
                        .candidate()
                        .map_err(|error| {
                            abort(
                                crate::RuntimeEffectControllerError::from(error)
                                    .into_runtime_error(),
                            )
                        })?
                        .1;
                }
                *admission
            }
            Ok(RunAdmissionAnswer::Refused { .. }) => {
                return Ok(ExecutedRun {
                    outcome: RunOutcome::Ceded { run },
                    run: None,
                    executed_inputs: Vec::new(),
                    empty_drain: None,
                });
            }
            Err(error) => {
                // The admission's body may have bound rows before its outcome
                // was lost. They stay bound to the run: the next shift admits
                // the same run first and reads its admission back.
                return Err(abort(error));
            }
        };

        // Execute the admitted rows. Live per-turn state that cannot cross the
        // durable boundary is re-attached from an in-process caller's input.
        let executed_inputs = admission.input_ids();
        let inputs = admission.inputs.map(|admitted| *admitted);
        let queued = admission.queued.map(|admitted| *admitted);
        let mut executed = inputs.as_ref().map_or_else(
            || TurnInput::items(Vec::new()),
            crate::AdmittedTurnInputs::materialize_turn_input,
        );
        if let Some((_, live)) =
            live.filter(|(input_id, _)| executed_inputs.iter().any(|id| id == *input_id))
        {
            executed.turn_context = live.turn_context.clone();
        }
        executed.trace_turn_id = Some(run.clone());
        let frames = Box::pin(self.execute_logical_turn(
            LogicalTurnStart::Input(executed),
            sinks.events,
            sinks.turn_events,
            run_controller.clone(),
            sinks.local_stop.clone(),
            LogicalTurnAdmissions::new(queued.into_iter().collect(), inputs.into_iter().collect()),
            Some(fence),
            stopwatch,
        ))
        .await;
        self.admitted_turn_index = None;
        let frames = frames.map_err(abort)?;
        let outcome = match frames.final_turn() {
            Some(turn) => RunOutcome::Committed {
                run,
                kind: crate::store::RunTerminalKind::of_stop(match &turn.outcome {
                    crate::TurnOutcome::Stopped(stop) => Some(stop),
                    _ => None,
                }),
            },
            None => RunOutcome::Ceded { run },
        };
        Ok(ExecutedRun {
            outcome,
            run: Some(frames),
            executed_inputs,
            empty_drain: None,
        })
    }

    /// Apply the session's open command run under `admitted`'s run (ADR 0101
    /// §4): every leading command, each commit fenced by the run's seal,
    /// until the command lane is empty. The run admits no turn. An
    /// administrative compaction runs here, lent the run's controller,
    /// which it rescopes to the command's own scope (FIG-4201), and so do a
    /// host's append, plugin operation and frame open (FIG-4202).
    ///
    /// Once the lane is empty the run ends like any other run: the store
    /// writes its [`CommandsApplied`](crate::store::RunTerminalCause::CommandsApplied)
    /// terminal, which arms its scope close, so the run's journal is
    /// retired (FIG-4202). The end is an idempotent store write: a replay
    /// finds it written, and a run a later admission superseded writes
    /// nothing, leaving the lane to that admission.
    pub(super) async fn execute_commands_run(
        &mut self,
        run_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        fence: &crate::store::ShiftFence,
    ) -> Result<ExecutedRun, ShiftAbort> {
        let run = admitted.run().clone();
        loop {
            match Box::pin(self.drain_next_session_command_fenced(
                fence,
                tokio_util::sync::CancellationToken::new(),
                run_controller,
            ))
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => break,
                // What the drain read live outside its recorded reads never
                // decides what the run journals (FIG-4346): it reads on
                // headless.
                Err(crate::runtime::session_api::CommandDrainStop::Headless(fault)) => {
                    return execute_headless_commands_run(
                        run_controller,
                        admitted,
                        HeadlessRun::Unrefreshed {
                            catalog: self.host.core.session_store_factory(),
                            fault,
                        },
                    )
                    .await
                    .map(|outcome| ExecutedRun {
                        outcome,
                        run: None,
                        executed_inputs: Vec::new(),
                        empty_drain: None,
                    });
                }
                Err(crate::runtime::session_api::CommandDrainStop::Failed(error)) => {
                    self.record_turn_park_after_abort(&error, &run, None).await;
                    return Err(shift_abort(Some(&run), error));
                }
            }
        }
        let store = self.shift_store()?;
        let end = store
            .end_command_run(fence, &run, self.host.core.clock.timestamp_ms())
            .await
            .map_err(|error| {
                ShiftAbort::Retry(crate::runtime::runtime_error_from_store_commit(error))
            })?;
        if end.terminal().is_some()
            && let Some(execution) = self.shift_run.as_mut()
        {
            execution.mark_terminal_written();
        }
        // The outcome is the admission's, never what this execution found:
        // a redelivery finds the lane its first execution already applied,
        // and must answer the same. A lane that made no progress shows in the
        // next admission naming the same leading command, which stops the
        // shift ([`ShiftLoop`](crate::engine::ShiftLoop)).
        Ok(ExecutedRun {
            outcome: RunOutcome::Applied { run },
            run: None,
            executed_inputs: Vec::new(),
            empty_drain: None,
        })
    }

    /// Recover the follow-on the session head owes under `admitted`'s run
    /// (ADR 0101 §3, FIG-3542), from the recovery count `attempts` its shift
    /// admission recorded, under the shift fence its seal raised.
    ///
    /// The run's recorded decision, `shift-follow-on:{run}`, answers
    /// whether the head still owes the follow-on and how it is recovered,
    /// and raises the recovery count inside its body (FIG-4361). The rest of
    /// the run executes the recorded answer, never the head a replay finds:
    /// a follow-on the head owed no longer cedes the run, and one it owed
    /// runs under the recorded fact or commits exhausted.
    ///
    /// The decision records the head the follow-on's turn runs on and that
    /// turn's index, and its body retains that head (FIG-4380). The run
    /// adopts the recorded head and pins the recorded index before its turn,
    /// so a replay after the follow-on's own commit moved the head runs the
    /// turn its journal holds. The resident head is brought current first,
    /// for the decision's first execution to record; a refresh that failed
    /// leaves the shift no head, and the run issues the same step headless
    /// ([`execute_headless_follow_on_run`]).
    pub(super) async fn execute_follow_on_run(
        &mut self,
        run_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        follow_on: &FollowOnWork<'_>,
        sinks: &ShiftSinks<'_>,
        fence: &crate::store::ShiftFence,
    ) -> Result<ExecutedRun, ShiftAbort> {
        let run = admitted.run().clone();
        let store = self.shift_store()?;
        if let Err(fault) = self.refresh_resident_head().await {
            return execute_headless_follow_on_run(
                run_controller,
                admitted,
                follow_on,
                HeadlessRun::Unrefreshed {
                    catalog: self.host.core.session_store_factory(),
                    fault,
                },
            )
            .await
            .map(|outcome| ExecutedRun {
                outcome,
                run: None,
                executed_inputs: Vec::new(),
                empty_drain: None,
            });
        }
        let answer = execute_follow_on_recovery(
            run_controller,
            admitted,
            follow_on,
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(RecoverFollowOnRunner {
                    store,
                    fence: fence.clone(),
                    follow_on: follow_on.turn.clone(),
                    attempts: follow_on.attempts,
                    generation: admitted.admitted_generation().clone(),
                    plugin_host: self
                        .session
                        .as_ref()
                        .map(|session| session.plugins().host().clone()),
                    base: crate::store::SessionHeadRef {
                        // Read by the decision body on its first execution.
                        generation: 0,
                        revision: self.state.head_revision,
                        leaf: self.state.session_graph.leaf_node_id.clone(),
                        checkpoint: self.state.checkpoint_ref.clone(),
                    },
                    // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                    turn_index: self.state.turn_index + 1,
                }),
                None,
            ),
        )
        .await?
        .map_err(|error| shift_abort(Some(&run), error))?;
        let (recovery, base, turn_index, plugins) = match answer {
            FollowOnRecoveryAnswer::Ceded => {
                return Ok(ExecutedRun {
                    outcome: RunOutcome::Ceded { run },
                    run: None,
                    executed_inputs: Vec::new(),
                    empty_drain: Some(
                        crate::runtime::turn_loop::EmptyQueuedDrainReason::AdmissionRefused(
                            crate::AdmissionRefusal::AdmissionRaceLost,
                        ),
                    ),
                });
            }
            FollowOnRecoveryAnswer::Run {
                follow_on,
                base,
                turn_index,
                plugins,
            } => (
                crate::store::FollowOnRecovery::Run(follow_on),
                base,
                turn_index,
                plugins,
            ),
            FollowOnRecoveryAnswer::Exhausted {
                follow_on,
                base,
                turn_index,
                plugins,
            } => (
                crate::store::FollowOnRecovery::Exhausted(follow_on),
                base,
                turn_index,
                plugins,
            ),
        };
        // The recovery is the follow-on's admission by this build, and a
        // run's segment boundary is a plugin adoption point (FIG-4739): every
        // commit of the follow-on's turn writes plugin namespaces in the
        // formats the decision recorded (FIG-4747), on this execution and on
        // every replay of it.
        if let Some(session) = self.session.as_ref() {
            if let Err(error) = session.plugins().host().validate_plugin_admission(&plugins) {
                let error = error.into_turn_failure(RuntimeErrorCode::Plugin);
                self.record_turn_park_after_abort(&error, &run, None).await;
                return Err(shift_abort(Some(&run), error));
            }
            session.plugins().adopt_plugin_admission(plugins);
        }
        // The follow-on's turn runs on the head its decision recorded, at the
        // index it recorded, whatever head this execution refreshed: a replay
        // after the follow-on's own commit finds a head that commit moved
        // (FIG-4380).
        if let Err(error) = self.adopt_recorded_turn(&base, turn_index).await {
            self.record_turn_park_after_abort(&error, &run, None).await;
            return Err(shift_abort(Some(&run), error));
        }
        // The recorded base is read without its pending fact, and the fact
        // the turn runs under is the recorded one: the head's while the head
        // owes the follow-on, since the decision's first execution raised or
        // read it there, and every head write of the turn must carry the
        // fact the head holds.
        let (crate::store::FollowOnRecovery::Run(owed)
        | crate::store::FollowOnRecovery::Exhausted(owed)) = &recovery;
        self.state.pending_follow_on = Some(Box::new(owed.clone()));
        // The follow-on's turn is a physical turn of the logical run that
        // owed it, so it runs under that run's turn scope (FIG-3607
        // contract 4): its effects and process starts are owned by the run
        // whose evidence its final commit writes and whose scope that
        // evidence closes. The recovery run owns only its own steps.
        let host = Arc::clone(&self.host.core.control.effect_host);
        let logical_run = crate::store::PhysicalTurn::split_turn_id(follow_on.turn).0;
        let turn_controller = super::step_controller(
            run_controller,
            host.as_ref(),
            shift_run_scope(admitted.session(), &logical_run),
        )
        .map_err(ShiftAbort::Refused)?;
        let drain = Box::pin(self.execute_recovered_follow_on(
            recovery,
            turn_controller,
            sinks,
            fence.clone(),
        ))
        .await;
        self.admitted_turn_index = None;
        let drain = drain.map_err(|error| shift_abort(Some(&run), error))?;
        Ok(match drain {
            crate::runtime::turn_loop::QueuedTurnDrain::Ran(turn) => ExecutedRun {
                outcome: RunOutcome::Committed {
                    run,
                    kind: crate::store::RunTerminalKind::of_stop(match &turn.outcome {
                        crate::TurnOutcome::Stopped(stop) => Some(stop),
                        _ => None,
                    }),
                },
                run: Some(crate::AgentFrameRun {
                    turns: vec![turn],
                    acceptance: None,
                }),
                executed_inputs: Vec::new(),
                empty_drain: None,
            },
            crate::runtime::turn_loop::QueuedTurnDrain::Empty(reason) => ExecutedRun {
                outcome: RunOutcome::Ceded { run },
                run: None,
                executed_inputs: Vec::new(),
                empty_drain: Some(reason),
            },
        })
    }

    /// Adopt the head a run's admission admitted it on and pin its recorded
    /// turn index for the prepare phase (FIG-3682).
    ///
    /// The recorded inspection alone decides which head the resident session
    /// is rebuilt from: a `Ready` verdict rebuilds it from the admission's
    /// base, whatever the live head is now; an `Advanced` one from the head
    /// the run's own commits published (FIG-4201); an `Overtaken` verdict
    /// ends the run typed `StoreCommitSuperseded`, and a `Diverged` one
    /// parks it.
    ///
    /// A base the store no longer retains parks the run too.
    async fn record_plugin_transition(
        &self,
        controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        admission: &crate::store::RunAdmission,
    ) -> Result<Option<crate::plugin::PluginTransitionRecord>, ShiftAbort> {
        let invocation = run_step_invocation(controller, admitted, "plugin-transition")?;
        let request = crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(invocation.address().clone()),
            owner: crate::RuntimeOwner::Session(admitted.session().clone()),
            base: admission.base.clone(),
            target: admission.plugins.clone(),
        };
        let runner = PluginTransitionRunner {
            host: self.services.plugins.host().clone(),
            store: self.shift_store()?,
            initial: self.state.clone(),
        };
        let outcome = controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::TransitionPlugins {
                        request: Box::new(request),
                    },
                ),
                lash_core_execution::core_internal::owned_runner_executor(Box::new(runner), None),
            )
            .await
            .map_err(|error| shift_abort(Some(admitted.run()), error.into_runtime_error()))?;
        match outcome {
            crate::RuntimeEffectOutcome::TransitionPlugins { record } => {
                record.candidate().map_err(|error| {
                    shift_abort(
                        Some(admitted.run()),
                        crate::RuntimeEffectControllerError::from(error).into_runtime_error(),
                    )
                })?;
                Ok(Some(*record))
            }
            other => Err(shift_abort(
                Some(admitted.run()),
                crate::RuntimeEffectControllerError::wrong_outcome(
                    crate::RuntimeEffectKind::TransitionPlugins,
                    other.kind(),
                )
                .into_runtime_error(),
            )),
        }
    }

    async fn adopt_admitted_turn(
        &mut self,
        admitted: AdmittedTurn<'_>,
        verdict: AdmittedHeadVerdict,
    ) -> Result<(), RuntimeError> {
        let AdmittedTurn {
            base,
            turn_index,
            generation,
            plugins,
            run: turn_id,
        } = admitted;
        // The run executes only under the executable generation its admission
        // recorded (FIG-3571), checked before anything else of it runs.
        crate::runtime::turn_loop::generation_fence::admit(self, generation)?;
        // Every commit of the run writes plugin namespaces in the formats
        // its admission recorded (FIG-4747), on this execution and on every
        // retry of it, whatever the fleet record permits by then.
        if let Some(session) = self.session.as_ref() {
            session.plugins().adopt_plugin_admission(plugins.clone());
        }
        // The verdict is the one `shift-head` recorded, honoured at every
        // position (FIG-4058). Its live check, a head that moved from the
        // admission's base with no commit of this run behind it, is the
        // inspection's body, which runs only when `shift-head` is this
        // attempt's live frontier. A replay is served the recorded verdict:
        // whatever the first attempt did after it is already journaled, so a
        // head that moved since is met by the turn's fenced commit as a typed
        // refusal, never re-decided here at a recorded position.
        let base = match verdict {
            AdmittedHeadVerdict::Ready => base,
            // The run's own commits moved the head: it continues from the
            // head they published, its own frame (FIG-4201).
            AdmittedHeadVerdict::Advanced { ref head } => head,
            // Ordinary head overtaking: another writer committed past the
            // base, so every commit of the run meets the moved head. The
            // run ends with the refusal its commit would meet (FIG-4200).
            AdmittedHeadVerdict::Overtaken { live_revision } => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::StoreCommitSuperseded,
                    format!(
                        "another writer moved the session head from revision {} to {} under \
                         run `{turn_id}` before it committed; the run can never commit on the \
                         head it was admitted on",
                        base.revision, live_revision
                    ),
                ));
            }
            AdmittedHeadVerdict::Diverged { live_revision } => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::EffectReplayDivergence,
                    format!(
                        "the session head at revision {live_revision} is inconsistent with the \
                         revision {} run `{turn_id}` was admitted on; the run is not executed on a \
                         head it was not admitted on",
                        base.revision
                    ),
                ));
            }
        };
        self.adopt_recorded_turn(base, turn_index).await
    }

    /// Adopt `base`, the head a run's recorded step says its turn runs on,
    /// as the resident session, and pin the recorded `turn_index` for the
    /// turn's prepare phase, which then reads no live head (FIG-3682,
    /// FIG-4380).
    ///
    /// A base the store no longer retains parks the run.
    async fn adopt_recorded_turn(
        &mut self,
        base: &crate::store::SessionHeadRef,
        turn_index: u64,
    ) -> Result<(), RuntimeError> {
        let turn_index = usize::try_from(turn_index).map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "admitted turn index exceeds platform range",
            )
        })?;
        self.adopt_admission_base(base)
            .await
            .map_err(|error| match error {
                SessionError::Store {
                    source: source @ crate::StoreError::TurnBaseNotRetained { .. },
                    ..
                } => {
                    RuntimeError::new(RuntimeErrorCode::EffectReplayDivergence, source.to_string())
                }
                error => RuntimeError::new(RuntimeErrorCode::SessionHeadRefresh, error.to_string()),
            })?;
        self.admitted_turn_index = Some(turn_index);
        Ok(())
    }
}

/// The invocation of one of a run's recorded shift steps, `{step}:{run}`
/// on the run's scope. Keyed by the run, never by the shift admission: a
/// later admission of the same run replays the step its first execution
/// recorded, so the run executes exactly the rows its journal was written
/// for.
fn run_step_invocation(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    step: &str,
) -> Result<crate::RuntimeEffectInvocation, ShiftAbort> {
    let run = admitted.run();
    Ok(crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            run_controller.execution_scope().clone(),
            format!("{step}:{run}"),
        )
        .map_err(|error| ShiftAbort::Refused(RuntimeError::from(error)))?,
        crate::RuntimeAttribution::for_turn_admission(admitted.session().clone(), run.clone()),
        format!("{run}.{step}"),
    ))
}

/// The run's recorded admission, `shift-admit:{run}`, whose first
/// execution runs `runner`. The outer error is a step the shift could not
/// address; the inner one is the step's answer that is not an admission:
/// its recorded refusal, or a fault its attempt met.
async fn execute_run_admission(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    head: &AdmittedHead,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Result<RunAdmissionAnswer, RuntimeError>, ShiftAbort> {
    let invocation = run_step_invocation(run_controller, admitted, "shift-admit")?;
    Ok(run_controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::AdmitRun { head: head.clone() },
            ),
            runner,
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_run_admission)
        .map_err(crate::RuntimeEffectControllerError::into_runtime_error))
}

/// The run's recorded head inspection, `shift-head:{run}`, whose first
/// execution runs `runner`: the recorded verdict on the head the run's
/// admission admitted it on.
async fn execute_head_inspection(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    head: &AdmittedHead,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Result<AdmittedHeadVerdict, RuntimeError>, ShiftAbort> {
    let invocation = run_step_invocation(run_controller, admitted, "shift-head")?;
    Ok(run_controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::InspectAdmittedHead {
                    run: admitted.run().clone(),
                    head: head.clone(),
                },
            ),
            runner,
        )
        .await
        .and_then(|outcome| match outcome {
            crate::RuntimeEffectOutcome::InspectAdmittedHead { verdict } => Ok(verdict),
            other => Err(crate::RuntimeEffectControllerError::wrong_outcome(
                crate::RuntimeEffectKind::InspectAdmittedHead,
                other.kind(),
            )),
        })
        .map_err(crate::RuntimeEffectControllerError::into_runtime_error))
}

/// The follow-on a recovery run was admitted for, with the recovery count
/// its shift admission recorded.
pub(super) struct FollowOnWork<'a> {
    pub(super) turn: &'a TurnId,
    pub(super) attempts: u32,
}

/// A follow-on recovery run's recorded decision, `shift-follow-on:{run}`,
/// whose first execution runs `runner` (FIG-4361). The outer error is a step
/// the shift could not address; the inner one is the step's answer that is
/// not a decision: its recorded retirement, or a fault its attempt met.
async fn execute_follow_on_recovery(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    follow_on: &FollowOnWork<'_>,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Result<FollowOnRecoveryAnswer, RuntimeError>, ShiftAbort> {
    let invocation = run_step_invocation(run_controller, admitted, "shift-follow-on")?;
    Ok(run_controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::RecoverFollowOn {
                    follow_on: follow_on.turn.clone(),
                    attempts: follow_on.attempts,
                },
            ),
            runner,
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_follow_on_recovery)
        .map_err(crate::RuntimeEffectControllerError::into_runtime_error))
}

/// The first execution of a follow-on recovery run's decision: the shift's
/// one live read of the fact the head owes, and the fenced write that raises
/// its recovery count before the follow-on's first effect (ADR 0101 §3).
///
/// The decision is taken on the count `attempts` the shift admission
/// recorded. A head that owes the follow-on at a count already past it was
/// raised by an earlier execution of this step whose answer was not
/// recorded, which this one continues without raising again. Once the raised
/// count would pass the recovery bound the fact froze for its logical run,
/// the answer is `Exhausted` and the fact is not raised: the bound of the
/// host executing the run never decides it. The recorded fact carries that
/// bound.
///
/// A run or an exhaustion records `base`, the resident head the shift
/// refreshed, and `turn_index`, the next one after it, and retains the base
/// under the fence before the raise, as a run's admission retains its own
/// (FIG-3682, FIG-4380): the run's turn runs on that head at that index on
/// every execution. A store that did not answer is this attempt's fault and
/// the step runs again; a retired session is recorded (FIG-3630).
struct RecoverFollowOnRunner {
    store: crate::store::SessionStore,
    fence: crate::store::ShiftFence,
    follow_on: TurnId,
    attempts: u32,
    /// The generation of the build this recovery runs on, which holds the
    /// follow-on's run from the raise on (FIG-4739).
    generation: crate::engine::BuildGeneration,
    /// The plugins the follow-on's turn runs, whose composition and writer
    /// formats the decision records (FIG-4747). `None` for a runtime with
    /// no session.
    plugin_host: Option<crate::plugin::PluginHost>,
    /// The resident head the follow-on's turn runs on, as the shift
    /// refreshed it. Its generation is read in the body.
    base: crate::store::SessionHeadRef,
    /// The follow-on's turn index: the next one after `base`.
    turn_index: usize,
}

impl RecoverFollowOnRunner {
    async fn decide(&self) -> Result<FollowOnRecoveryAnswer, crate::StoreError> {
        let Some(owed) = self
            .store
            .load_pending_follow_on()
            .await?
            .filter(|owed| owed.is_turn(&self.follow_on))
        else {
            return Ok(FollowOnRecoveryAnswer::Ceded);
        };
        let basis = crate::store::PendingFollowOn {
            attempts: self.attempts,
            ..owed.clone()
        };
        let recovery = basis.recovery()?;
        let base = crate::store::SessionHeadRef {
            generation: self.store.read_session_state_version().await?,
            ..self.base.clone()
        };
        self.store.retain_admission_base(&self.fence, &base).await?;
        let turn_index = self.turn_index as u64;
        // Chosen from the fleet record as it stands now, and recorded with
        // the decision: nothing of the follow-on's turn ran under an earlier
        // execution of this step whose answer was lost.
        let plugins = match &self.plugin_host {
            Some(host) => host.admit_plugins(self.store.store().as_ref()).await?,
            None => crate::store::plugin_writers::PluginAdmission::default(),
        };
        let raised_earlier = owed.attempts > basis.attempts;
        Ok(match recovery {
            crate::store::FollowOnRecovery::Run(_) => FollowOnRecoveryAnswer::Run {
                follow_on: if raised_earlier {
                    owed
                } else {
                    self.store
                        .raise_pending_follow_on_attempts(
                            &self.fence,
                            &owed.follow_on_turn_id,
                            &self.generation,
                        )
                        .await?
                },
                base,
                turn_index,
                plugins,
            },
            crate::store::FollowOnRecovery::Exhausted(_) => FollowOnRecoveryAnswer::Exhausted {
                // The head's fact: an earlier execution of this step may have
                // raised it past `basis`.
                follow_on: owed,
                base,
                turn_index,
                plugins,
            },
        })
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RecoverFollowOnRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::RecoverFollowOn {
            follow_on,
            attempts,
        } = &envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "follow-on recovery executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *follow_on != self.follow_on || *attempts != self.attempts {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "follow-on recovery executor was bound to another follow-on or recovery count",
            ));
        }
        let answer = self
            .decide()
            .await
            .map_err(|error| super::admission::store_fault("follow-on recovery failed", error))?;
        Ok(crate::RuntimeEffectOutcome::RecoverFollowOn {
            answer: Box::new(answer),
        })
    }
}

/// Why a sealed run's shift holds no current head of its session.
#[derive(Clone)]
pub(super) enum HeadlessRun {
    /// The engine could not open the session at all: its close or
    /// tombstone already committed (ADR 0049).
    Retired,
    /// The shift opened the session, but its resident head could not be
    /// brought current: `fault`, which a deleted session's refresh meets too.
    /// `catalog` is the deployment's session catalog, which answers whether
    /// the session was deleted.
    Unrefreshed {
        catalog: Arc<dyn crate::DeploymentStore>,
        fault: RuntimeError,
    },
}

impl HeadlessRun {
    /// How an attempt ends whose journal holds work of `admitted`'s run
    /// past its headless steps, which cannot replay without the session's
    /// head: `what` the journal holds. A live fault that journals nothing:
    /// the refresh's, or, for a retired session, one naming the retirement.
    /// The engine retries it; a session that stays retired is released by the
    /// engine's park reconcile, whose park writer answers a deleted session
    /// `TargetGone`.
    fn past_its_steps(self, admitted: &Admitted, what: &str) -> ShiftAbort {
        ShiftAbort::Retry(match self {
            Self::Unrefreshed { fault, .. } => fault,
            Self::Retired => RuntimeError::new(
                RuntimeErrorCode::SessionHeadRefresh,
                format!(
                    "run `{}` of session `{}` recorded {what} before its session retired; the \
                     work its journal holds after them cannot replay without the session",
                    admitted.run(),
                    admitted.session()
                ),
            ),
        })
    }
}

/// Run a sealed input- or queued-headed run whose shift holds no current
/// head of its session (FIG-4346).
///
/// The run still issues its recorded admission and head inspection, in the
/// order and under the envelopes a run with a head issues them, so a
/// replay follows the journal an earlier attempt of the run recorded, and
/// nothing the shift read outside those steps decides what it journals
/// (ADR 0105 §1). Their bodies admit and inspect nothing, since there is no
/// head to do it on: [`HeadlessRunStepRunner`] answers only why.
///
/// - A step that recorded the session's retirement ends the run with the
///   typed `SessionDeleted` refusal (ADR 0049), recorded where the journal
///   held nothing more, so every replay answers the same.
/// - A step that recorded a refusal to admit cedes the run, as it does with
///   a head.
/// - A recorded admission and head inspection mean an earlier attempt ran
///   the run's turn after them, and that turn cannot replay without the
///   session's head ([`HeadlessRun::past_its_steps`]).
pub(super) async fn execute_headless_run(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    head: &AdmittedHead,
    headless: HeadlessRun,
) -> Result<RunOutcome, ShiftAbort> {
    let run = admitted.run().clone();
    let runner = || {
        lash_core_execution::core_internal::owned_runner_executor(
            Box::new(HeadlessRunStepRunner {
                session: admitted.session().clone(),
                step: HeadlessStep::Run {
                    run: run.clone(),
                    head: head.clone(),
                },
                headless: headless.clone(),
            }),
            None,
        )
    };
    match execute_run_admission(run_controller, admitted, head, runner()).await? {
        Ok(RunAdmissionAnswer::Admitted { .. }) => {}
        Ok(RunAdmissionAnswer::Refused { .. }) => return Ok(RunOutcome::Ceded { run }),
        Err(error) => return Err(shift_abort(Some(&run), error)),
    }
    if let Err(error) = execute_head_inspection(run_controller, admitted, head, runner()).await? {
        return Err(shift_abort(Some(&run), error));
    }
    Err(headless.past_its_steps(admitted, "its admission and head inspection"))
}

/// Run a sealed command run whose shift holds no current head of its
/// session (FIG-4346), from its next read of the session's command lane.
///
/// A command run's recorded steps are its reads of the lane
/// (`session-command-run:{ordinal}`, numbered by `run_controller`); every
/// command but an administrative compaction settles and commits off the
/// journal. The headless run issues the reads in that order, and their
/// bodies read nothing ([`HeadlessRunStepRunner`]):
///
/// - a read that recorded the session's retirement ends the run with the
///   typed `SessionDeleted` refusal (ADR 0049);
/// - a recorded empty lane is the run's end, `Applied`, as it is with a
///   head;
/// - a recorded run of commands that journal nothing is followed by the next
///   read, as it is with a head;
/// - a recorded compaction journaled its apply after the read, which cannot
///   replay without the session's head ([`HeadlessRun::past_its_steps`]).
pub(super) async fn execute_headless_commands_run(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    headless: HeadlessRun,
) -> Result<RunOutcome, ShiftAbort> {
    let run = admitted.run().clone();
    loop {
        let batches = crate::runtime::session_api::execute_session_command_run_read(
            run_controller,
            admitted.session(),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(HeadlessRunStepRunner {
                    session: admitted.session().clone(),
                    step: HeadlessStep::CommandRun,
                    headless: headless.clone(),
                }),
                None,
            ),
        )
        .await
        .map_err(|error| shift_abort(Some(&run), error))?;
        if batches.is_empty() {
            return Ok(RunOutcome::Applied { run });
        }
        let run = crate::AdmittedQueuedWork {
            session_id: admitted.session().clone(),
            batches,
        };
        let journals_nothing = run.session_commands().is_some_and(|commands| {
            !matches!(
                commands.as_slice(),
                [(_, crate::SessionCommand::CompactContext { .. })]
            )
        });
        if !journals_nothing {
            return Err(headless.past_its_steps(admitted, "a compaction's read"));
        }
    }
}

/// Run a sealed follow-on recovery run whose shift holds no current head of
/// its session (FIG-4361).
///
/// The run still issues its recorded decision, `shift-follow-on`, under
/// the envelope a run with a head issues it, and its body decides nothing
/// ([`HeadlessRunStepRunner`]):
///
/// - a decision that recorded the session's retirement ends the run with
///   the typed `SessionDeleted` refusal (ADR 0049);
/// - a recorded `Ceded` cedes the run, as it does with a head;
/// - a recorded run or exhaustion means an earlier attempt ran the
///   follow-on's turn after it, which cannot replay without the session's
///   head ([`HeadlessRun::past_its_steps`]).
pub(super) async fn execute_headless_follow_on_run(
    run_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    follow_on: &FollowOnWork<'_>,
    headless: HeadlessRun,
) -> Result<RunOutcome, ShiftAbort> {
    let run = admitted.run().clone();
    let runner = lash_core_execution::core_internal::owned_runner_executor(
        Box::new(HeadlessRunStepRunner {
            session: admitted.session().clone(),
            step: HeadlessStep::FollowOn {
                follow_on: follow_on.turn.clone(),
                attempts: follow_on.attempts,
            },
            headless: headless.clone(),
        }),
        None,
    );
    match execute_follow_on_recovery(run_controller, admitted, follow_on, runner).await? {
        Ok(FollowOnRecoveryAnswer::Ceded) => Ok(RunOutcome::Ceded { run }),
        Ok(FollowOnRecoveryAnswer::Run { .. } | FollowOnRecoveryAnswer::Exhausted { .. }) => {
            Err(headless.past_its_steps(admitted, "its follow-on recovery"))
        }
        Err(error) => Err(shift_abort(Some(&run), error)),
    }
}

/// The steps a headless run issues.
enum HeadlessStep {
    /// An input- or queued-headed run's admission and head inspection.
    Run { run: TurnId, head: AdmittedHead },
    /// A command run's read of the session's command lane.
    CommandRun,
    /// A follow-on recovery run's decision.
    FollowOn { follow_on: TurnId, attempts: u32 },
}

/// The first execution of a headless run's step ([`execute_headless_run`],
/// [`execute_headless_commands_run`], [`execute_headless_follow_on_run`]): it
/// admits, inspects, reads and decides nothing, and answers why the shift
/// holds no head.
///
/// A retired session is a settled fact the step records, as the shift's own
/// admission and seal record it (FIG-3630, FIG-3881): the engine could not
/// open the session at all, or the catalog, read inside the step, holds the
/// session's deletion tombstone. Any other refresh fault is this attempt's,
/// never recorded, and the engine runs the step again.
struct HeadlessRunStepRunner {
    session: crate::SessionId,
    step: HeadlessStep,
    headless: HeadlessRun,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for HeadlessRunStepRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let bound = match (&self.step, &envelope.command) {
            (
                HeadlessStep::Run { head: bound, .. },
                crate::RuntimeEffectCommand::AdmitRun { head },
            ) => head == bound,
            (
                HeadlessStep::Run {
                    run: bound_run,
                    head: bound_head,
                },
                crate::RuntimeEffectCommand::InspectAdmittedHead { run, head },
            ) => run == bound_run && head == bound_head,
            (
                HeadlessStep::CommandRun,
                crate::RuntimeEffectCommand::ReadSessionCommandRun { session },
            ) => *session == self.session,
            (
                HeadlessStep::FollowOn {
                    follow_on: bound_follow_on,
                    attempts: bound_attempts,
                },
                crate::RuntimeEffectCommand::RecoverFollowOn {
                    follow_on,
                    attempts,
                },
            ) => follow_on == bound_follow_on && attempts == bound_attempts,
            (_, other) => {
                return Err(crate::RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                    format!(
                        "headless run step executor cannot execute {} command",
                        other.kind().as_str()
                    ),
                ));
            }
        };
        if !bound {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "headless run step executor was bound to another session, run, head or follow-on",
            ));
        }
        let retirement = |error: crate::StoreError| {
            super::admission::store_fault("session retired under its run", error)
        };
        match self.headless {
            HeadlessRun::Retired => Err(retirement(crate::StoreError::SessionDeleted {
                session_id: self.session,
            })),
            HeadlessRun::Unrefreshed { catalog, fault } => {
                match catalog.lookup_session(&self.session).await {
                    Ok(crate::store::SessionLookup::Deleted) => {
                        Err(retirement(crate::StoreError::SessionDeleted {
                            session_id: self.session,
                        }))
                    }
                    _ => Err(crate::RuntimeEffectControllerError::from(fault)
                        .retryable_uncommitted_derivation()),
                }
            }
        }
    }
}

struct AdmittedTurn<'a> {
    base: &'a crate::store::SessionHeadRef,
    turn_index: u64,
    generation: Option<&'a crate::ExecutableGeneration>,
    plugins: &'a crate::store::plugin_writers::PluginAdmission,
    run: &'a TurnId,
}

/// The resident head an attempt refreshed before its run's inspection.
struct ResidentHead {
    revision: u64,
    leaf: Option<crate::NodeId>,
    checkpoint: Option<crate::store::BlobRef>,
}

/// The body of a run's `shift-head` step: the shift's one live head check.
///
/// It runs only when the step is not recorded yet, so at the attempt's live
/// frontier before any turn effect, and decides from the resident head this
/// attempt refreshed. A head that moved from the admission's base with no
/// final commit of the run behind it is decided by its components
/// (FIG-4200): a higher revision that a fenced commit published is the
/// run's own, `Advanced`, and the run continues from it (FIG-4201); a
/// higher revision a lane-less write published is another writer
/// overtaking the head, `Overtaken`, and the run ends typed; a lower
/// revision, or the same revision with another leaf or checkpoint, is an
/// inconsistent head, `Diverged`, and the run parks before it executes a
/// head it was not admitted on. A replay serves the recorded verdict and
/// never runs it.
///
/// A fenced commit that lands while the run is unfinished is the run's
/// own: the store refuses a fence an admission superseded, and every
/// admission sealed while the run is unfinished resumes it.
struct InspectAdmittedHeadRunner {
    store: crate::store::SessionStore,
    run: TurnId,
    head: AdmittedHead,
    /// The head the run's admission recorded.
    base: crate::store::SessionHeadRef,
    live: ResidentHead,
}

impl InspectAdmittedHeadRunner {
    /// Whether the live head is the admission's base.
    fn head_is_base(&self) -> bool {
        self.live.revision == self.base.revision
            && self.live.leaf == self.base.leaf
            && self.live.checkpoint == self.base.checkpoint
    }

    /// The verdict on a head that moved from the base with no final commit
    /// of the run behind it. `published_by_shift` is whether a fenced commit
    /// published the live head.
    fn moved_head_verdict(&self, published_by_shift: bool) -> AdmittedHeadVerdict {
        let live_revision = self.live.revision;
        if live_revision <= self.base.revision {
            return AdmittedHeadVerdict::Diverged { live_revision };
        }
        if published_by_shift {
            return AdmittedHeadVerdict::Advanced {
                head: crate::store::SessionHeadRef {
                    generation: self.base.generation,
                    revision: live_revision,
                    leaf: self.live.leaf.clone(),
                    checkpoint: self.live.checkpoint.clone(),
                },
            };
        }
        AdmittedHeadVerdict::Overtaken { live_revision }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for InspectAdmittedHeadRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::InspectAdmittedHead { run, head } = &envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "admitted head inspector received another command",
            ));
        };
        if *run != self.run || *head != self.head {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "admitted head inspector was bound to another run or head",
            ));
        }
        let store_fault = |error| {
            crate::RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(error),
            )
            .retryable_uncommitted_derivation()
        };
        let verdict = if self.head_is_base()
            || self
                .store
                .committed_turn_exists(&self.run)
                .await
                .map_err(store_fault)?
        {
            AdmittedHeadVerdict::Ready
        } else {
            // Whether a fenced commit published the live head the attempt
            // refreshed: read from the head's own row, and only when it is
            // still that head.
            let meta = self
                .store
                .load_session_head_meta()
                .await
                .map_err(store_fault)?;
            let published_by_shift = meta.is_some_and(|head| {
                head.published_by_shift
                    && head.head_revision == self.live.revision
                    && head.leaf_node_id == self.live.leaf
                    && head.checkpoint_ref == self.live.checkpoint
            });
            self.moved_head_verdict(published_by_shift)
        };
        Ok(crate::RuntimeEffectOutcome::InspectAdmittedHead { verdict })
    }
}

/// Trace attribution for the admission decisions the runner makes.
struct AdmissionTrace {
    tracing: crate::trace::TraceRuntime,
    turn_index: usize,
    /// The admission body's live step, bound when the body really runs.
    live: Option<Arc<crate::trace::LiveStep>>,
}

/// What the admission probe decided for the head row: either the outcome
/// the journal records (an admission or a refusal) or a race the step
/// retries.
enum RunAdmissionProbe {
    /// The journaled `AdmitRun` outcome.
    Answer(RunAdmissionAnswer),
    /// The admission took nothing yet the head row is still in the store:
    /// composition and read raced, so the step defers to a later admission
    /// instead of journaling a refusal over a row that is still there.
    HeadMissedByRace,
}

/// The first execution of a run's admission.
///
/// Everything it needs is captured at the shift site; none of it enters the
/// envelope, which names only the head: the fence changes with every shift
/// epoch, and the envelope must not.
struct AdmitRunRunner {
    store: crate::store::SessionStore,
    effect_host: Arc<dyn crate::EffectHost>,
    scope: crate::AdmittedScope,
    fence: crate::store::ShiftFence,
    head: AdmittedHead,
    run: TurnId,
    /// The runtime's turn-input admission bound
    /// ([`QueuedWorkBatchingConfig::max_turn_input_admission`](crate::QueuedWorkBatchingConfig::max_turn_input_admission)).
    max_inputs: usize,
    /// The turn-lane composition policy a queued-work head is admitted
    /// under.
    policy: crate::TurnLaneAdmissionPolicy,
    /// The resident head the run is admitted on, as the shift refreshed it.
    /// Its generation is read in the body.
    base: crate::store::SessionHeadRef,
    /// The run's turn index: the next one after `base`.
    turn_index: usize,
    /// The executable generation the run is admitted under (FIG-3571).
    generation: Option<crate::ExecutableGeneration>,
    admitted_generation: crate::engine::BuildGeneration,
    /// The execution that executes the run, which the admission records
    /// (FIG-4403).
    executor: crate::store::RunExecutor,
    /// The plugins the run executes, whose composition and writer formats the
    /// admission records (FIG-4747). `None` for a runtime with no session.
    plugin_host: Option<crate::plugin::PluginHost>,
    trace: AdmissionTrace,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for AdmitRunRunner {
    fn bind_live_step(&mut self, live: Arc<crate::trace::LiveStep>) {
        self.trace.live = Some(live);
    }

    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::AdmitRun { head } = &envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "run admission executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *head != self.head {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "run admission executor was bound to `{:?}` but asked to admit `{head:?}`",
                    self.head
                ),
            ));
        }
        // A store that did not answer is this attempt's fault, never the
        // admission's recorded outcome: the shift re-admits this same run
        // first, so a recorded fault would replay under `shift-admit:{run}`
        // on every later shift and wedge the session. Like the shift's
        // admission and the seal, the step runs again; only an admission or
        // a refusal is recorded.
        //
        // Every permanent store refusal is an admission outcome (FIG-4629).
        // The retry marker rejects terminal causes, so the journal records
        // the typed refusal and the run answers its sender.
        let run = self.run.clone();
        self.activate_turn_cancel_binding().await.map_err(|error| {
            crate::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation()
        })?;
        let answer = match self.admit().await.map_err(|err| {
            let mut fault = crate::RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(err),
            );
            fault.message = format!("run admission failed: {}", fault.message);
            fault.retryable_uncommitted_derivation()
        })? {
            RunAdmissionProbe::Answer(answer) => answer,
            // The admission missed the head yet the head row is still there:
            // the two raced, so the step defers to a later admission rather
            // than journal a refusal that would cede the run over a row that
            // is still to be answered (review of #2290).
            RunAdmissionProbe::HeadMissedByRace => {
                return Err(crate::RuntimeEffectControllerError::new(
                    RuntimeErrorCode::SessionExecutionLaneBusy,
                    format!("run `{run}` missed its head on an admission race"),
                )
                .retryable_uncommitted_derivation());
            }
        };
        Ok(crate::RuntimeEffectOutcome::AdmitRun { answer })
    }
}

impl AdmitRunRunner {
    /// The shift's fenced activation gate: the session's turn-cancellation
    /// binding is recorded on first use and checked on every later run, so
    /// a reopened host with a different physical authority is refused before
    /// the run admits anything or does any other session work.
    async fn activate_turn_cancel_binding(&self) -> Result<(), RuntimeError> {
        let scoped = self.effect_host.scoped(self.scope.clone())?;
        let binding = self.effect_host.turn_control_binding(&scoped).await?;
        let binding_id = binding.binding_id();
        let session_id = self.fence.session();
        self.store
            .validate_turn_cancellation_binding(
                &self.fence,
                binding_id,
                &crate::runtime::effect::executor::admitted_turn_cancel_scope(
                    &crate::TurnAddress::new(session_id, &self.run),
                    scoped.execution_scope(),
                    binding_id,
                ),
            )
            .await
            .map_err(crate::runtime::runtime_error_from_store_commit)
    }

    /// Observes one decision of the admission body. `payload` is built only
    /// when the record will be emitted.
    fn emit(&self, name: &str, payload: impl FnOnce() -> serde_json::Value) {
        let (Some(live), tracing) = (&self.trace.live, &self.trace.tracing) else {
            return;
        };
        if !tracing.is_observed() {
            return;
        }
        tracing
            .body(
                Some(crate::trace::turn_trace_scope(
                    self.fence.session(),
                    &self.run,
                    tracing.clock().timestamp_ms(),
                )),
                live,
            )
            .observe(|| {
                (
                    lash_trace::TraceContext::default()
                        .for_session(self.fence.session().clone())
                        .for_turn_index(self.trace.turn_index)
                        .for_turn(self.run.clone()),
                    lash_trace::TraceEvent::Custom {
                        name: name.to_string(),
                        payload: payload(),
                    },
                )
            });
    }

    /// Admit the turn-lane run headed by the admitted head.
    ///
    /// The store composes the run from open rows, binds them to the run,
    /// retains the base and records the admission in one transaction, and a
    /// later execution of the same run reads that record back instead of
    /// composing again (FIG-3840, FIG-3927). A worker that dies after the
    /// commit but before the journal takes the outcome therefore leaves
    /// nothing to recompute: the successor executes exactly the recorded
    /// composition, base and executable generation, never a run widened by
    /// rows that arrived meanwhile.
    ///
    /// A composition that would not reach the head takes nothing, and the
    /// head row is read without mutating it: absent means it was settled,
    /// cancelled, or pruned, and the run cedes; still present means the
    /// admission raced, so the step asks to run again rather than record a
    /// refusal. Nothing here ever drops, withdraws, or re-admits a row.
    async fn admit(self) -> Result<RunAdmissionProbe, crate::StoreError> {
        // The admission is the adoption point (FIG-4747): this build's
        // composition, and each plugin's writer chosen from the fleet record
        // as it stands now. The store records the first admission's choice
        // and answers it to every later one, so the record is read here and
        // nowhere after.
        let plugins = match &self.plugin_host {
            Some(host) => host.admit_plugins(self.store.store().as_ref()).await?,
            None => crate::store::plugin_writers::PluginAdmission::default(),
        };
        let request = crate::store::AdmitRunRequest {
            fence: self.fence.clone(),
            run: self.run.clone(),
            head: self.head.clone(),
            max_inputs: self.max_inputs,
            policy: self.policy.clone(),
            base: self.base.clone(),
            turn_index: self.turn_index as u64,
            generation: self.generation.clone(),
            admitted_generation: self.admitted_generation.clone(),
            executor: self.executor.clone(),
            plugins,
            // No admission candidate is proposed here, so the run is
            // admitted unanchored. The store still retains the run's scope:
            // the cause of the rows it admits and its start.
            trace_anchor: lash_trace::TraceAnchor::Untraced,
        };
        let admission = match self.store.admit_run(&request).await {
            // The record decides (FIG-4765): the run is run by the executor
            // its admission names, so this execution cedes it. The recorded
            // executor never changes, so the refusal is the step's outcome.
            Err(crate::StoreError::RunHeldByAnotherExecutor { .. }) => {
                return Ok(RunAdmissionProbe::Answer(RunAdmissionAnswer::Refused {
                    refusal: RunAdmissionRefusal::HeldByAnotherExecutor,
                }));
            }
            answer => answer?,
        };
        if let Some(admission) = admission {
            let causes = admission
                .queued
                .as_ref()
                .map(|queued| queued.materialize_queued_checkpoint_work().turn_causes)
                .unwrap_or_default();
            self.emit("ingress.admitted", || {
                crate::runtime::turn_loop::ingress_admitted_trace_payload(
                    &self.run,
                    crate::store::RUN_ADMISSION_STEP,
                    crate::AdmissionBoundary::Idle,
                    admission.inputs.as_deref(),
                    admission.queued.as_deref(),
                    &causes,
                )
            });
            return Ok(RunAdmissionProbe::Answer(RunAdmissionAnswer::Admitted {
                admission: Box::new(admission),
            }));
        }
        let present = match &self.head {
            AdmittedHead::Input(head) => self
                .store
                .list_pending_turn_inputs()
                .await?
                .iter()
                .any(|read| read.input.input_id == *head),
            AdmittedHead::Batch(head) => self
                .store
                .list_queued_work()
                .await?
                .iter()
                .any(|batch| batch.batch_id == *head),
        };
        Ok(if present {
            RunAdmissionProbe::HeadMissedByRace
        } else {
            RunAdmissionProbe::Answer(RunAdmissionAnswer::Refused {
                refusal: RunAdmissionRefusal::HeadGone,
            })
        })
    }
}

struct PluginTransitionRunner {
    host: crate::PluginHost,
    store: crate::store::SessionStore,
    initial: crate::RuntimeSessionState,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for PluginTransitionRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::TransitionPlugins { request } = envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "plugin transition requires its recorded request",
            ));
        };
        let state = if request.base.revision == 0 {
            self.initial
        } else {
            crate::store::load_session_window_state(
                &self.store,
                crate::store::WindowSelector::Admitted(request.base.clone()),
            )
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(
                    crate::runtime::runtime_error_from_store_commit(error),
                )
                .retryable_uncommitted_derivation()
            })?
            .ok_or_else(|| {
                crate::runtime::runtime_error_from_store_commit(
                    crate::StoreError::TurnBaseNotRetained {
                        revision: request.base.revision,
                    },
                )
            })?
            .state
        };
        let plugins = state.plugin_state().cloned().unwrap_or_default();
        let record =
            self.host
                .transition_plugins(*request, &plugins, &state.authority.plugin_config);
        Ok(crate::RuntimeEffectOutcome::TransitionPlugins {
            record: Box::new(record),
        })
    }
}
