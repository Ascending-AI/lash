//! Running one admitted root (FIG-3600, FIG-3927): its recorded admission,
//! the head that admission is headed by, and its turns to their terminal
//! commit.
//!
//! A root admits the turn-lane run its admission named, input-headed or
//! queued-headed alike, as a recorded step keyed by the root, so every
//! redrive of the root, under any admission, replays the same admission and
//! never re-reads pending rows. The admission records the head the root runs
//! on and its turn index; a redrive rebuilds the root from that head, never
//! from the live one its own commit may have moved (FIG-3682). A command
//! root applies the session's open command run and admits no turn.

use std::sync::Arc;

use super::{DriveSinks, RootRun, drive_abort};
use crate::engine::{Admitted, DriveAbort, RootOutcome, drive_root_scope};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::runtime::logical_turn::{LogicalTurnAdmissions, LogicalTurnStart};
use crate::runtime::turn_loop::TurnStopwatch;
use crate::store::{
    AdmittedHead, FollowOnRecoveryAnswer, RootAdmissionAnswer, RootAdmissionRefusal,
};
use crate::{
    RuntimeError, RuntimeErrorCode, ScopedEffectController, SessionError, TurnId, TurnInput,
};
use lash_core_execution::runtime::effect::AdmittedHeadVerdict;

impl LashRuntime {
    /// Admit the turn-lane run `admitted` is headed by and drive it as the
    /// root's logical turn, under the drive fence the root's seal raised.
    /// The admission records `executor`, the execution that runs the root.
    #[allow(
        clippy::too_many_arguments,
        reason = "the root's admission takes the drive fence and its executor from the drive path that runs it, beside the head, sinks and live input the turn needs"
    )]
    pub(super) async fn run_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        head: &AdmittedHead,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &TurnInput)>,
        fence: &crate::store::DriveFence,
        executor: crate::store::RootExecutor,
    ) -> Result<RootRun, DriveAbort> {
        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let root = admitted.root().clone();
        let abort = |error: RuntimeError| drive_abort(Some(&root), error);
        let store = self.drive_store()?;
        // The admission records the head and the turn index, so the resident
        // head is brought current first; a replay reads both from the journal
        // instead (FIG-3682). The refresh reads the live session outside any
        // recorded step, so its outcome must never decide what the root
        // journals (FIG-4346): a refresh that failed, a deleted session's
        // among them, leaves the drive no head to admit on, and the root
        // issues the same recorded steps headless, whose bodies answer only
        // why ([`run_headless_root`]).
        if let Err(fault) = self.refresh_resident_head().await {
            return run_headless_root(
                root_controller,
                admitted,
                head,
                HeadlessRoot::Unrefreshed {
                    catalog: self.host.core.session_store_factory(),
                    fault,
                },
            )
            .await
            .map(|outcome| RootRun {
                outcome,
                run: None,
                driven_inputs: Vec::new(),
                empty_drain: None,
            });
        }
        let answer = execute_root_admission(
            root_controller,
            admitted,
            head,
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(AdmitRootRunner {
                    store: store.clone(),
                    effect_host: Arc::clone(&self.host.core.control.effect_host),
                    scope: root_controller.admitted_scope().clone(),
                    fence: fence.clone(),
                    head: head.clone(),
                    root: root.clone(),
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
                        sink: self.host.core.tracing.trace_sink.clone(),
                        base: self.host.core.tracing.trace_context.clone(),
                        clock: Arc::clone(&self.host.core.clock),
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        turn_index: self.state.turn_index + 1,
                    },
                }),
                None,
            ),
        )
        .await?;
        let admission = match answer {
            Ok(RootAdmissionAnswer::Admitted { admission }) => {
                let live = ResidentHead {
                    revision: self.state.head_revision,
                    leaf: self.state.session_graph.leaf_node_id.clone(),
                    checkpoint: self.state.checkpoint_ref.clone(),
                };
                let verdict = execute_head_inspection(
                    root_controller,
                    admitted,
                    head,
                    lash_core_execution::core_internal::owned_runner_executor(
                        Box::new(InspectAdmittedHeadRunner {
                            store: store.clone(),
                            root: root.clone(),
                            head: head.clone(),
                            base: admission.base.clone(),
                            live,
                        }),
                        None,
                    ),
                )
                .await?
                .map_err(abort)?;
                if let Err(error) = self
                    .adopt_admitted_turn(
                        AdmittedTurn {
                            base: &admission.base,
                            turn_index: admission.turn_index,
                            generation: admission.generation.as_ref(),
                            plugins: &admission.plugins,
                            root: &root,
                        },
                        verdict,
                    )
                    .await
                {
                    self.record_turn_park_after_abort(&error, &root, None).await;
                    return Err(abort(error));
                }
                *admission
            }
            Ok(RootAdmissionAnswer::Refused { .. }) => {
                return Ok(RootRun {
                    outcome: RootOutcome::Ceded { root },
                    run: None,
                    driven_inputs: Vec::new(),
                    empty_drain: None,
                });
            }
            Err(error) => {
                // The admission's body may have bound rows before its outcome
                // was lost. They stay bound to the root: the next drive admits
                // the same root first and reads its admission back.
                return Err(abort(error));
            }
        };

        // Drive the admitted rows. Live per-turn state that cannot cross the
        // durable boundary is re-attached from an in-process caller's input.
        let driven_inputs = admission.input_ids();
        let inputs = admission.inputs.map(|admitted| *admitted);
        let queued = admission.queued.map(|admitted| *admitted);
        let mut driven = inputs.as_ref().map_or_else(
            || TurnInput::items(Vec::new()),
            crate::AdmittedTurnInputs::materialize_turn_input,
        );
        if let Some((_, live)) =
            live.filter(|(input_id, _)| driven_inputs.iter().any(|id| id == *input_id))
        {
            driven.turn_context = live.turn_context.clone();
        }
        driven.trace_turn_id = Some(root.clone());
        let run = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(driven),
            sinks.events,
            sinks.turn_events,
            root_controller.clone(),
            sinks.local_stop.clone(),
            LogicalTurnAdmissions::new(queued.into_iter().collect(), inputs.into_iter().collect()),
            Some(fence),
            stopwatch,
        ))
        .await;
        self.admitted_turn_index = None;
        let run = run.map_err(abort)?;
        let outcome = match run.final_turn() {
            Some(turn) => RootOutcome::Committed {
                root,
                outcome: turn.outcome.clone(),
            },
            None => RootOutcome::Ceded { root },
        };
        Ok(RootRun {
            outcome,
            run: Some(run),
            driven_inputs,
            empty_drain: None,
        })
    }

    /// Apply the session's open command run under `admitted`'s root (ADR 0101
    /// §4): every leading command, each commit fenced by the root's seal,
    /// until the command lane is empty. The root admits no turn. An
    /// administrative compaction runs here, lent the root's controller,
    /// which it rescopes to the command's own scope (FIG-4201), and so do a
    /// host's append, plugin operation and frame open (FIG-4202).
    ///
    /// Once the lane is empty the root ends like any other root: the store
    /// writes its [`CommandsApplied`](crate::store::RootTerminalCause::CommandsApplied)
    /// terminal, which arms its scope close, so the root's journal is
    /// retired (FIG-4202). The end is an idempotent store write: a replay
    /// finds it written, and a run a later admission superseded writes
    /// nothing, leaving the lane to that admission.
    pub(super) async fn run_commands_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        fence: &crate::store::DriveFence,
    ) -> Result<RootRun, DriveAbort> {
        let root = admitted.root().clone();
        loop {
            match Box::pin(self.drain_next_session_command_fenced(
                fence,
                tokio_util::sync::CancellationToken::new(),
                root_controller,
            ))
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => break,
                // What the drain read live outside its recorded reads never
                // decides what the root journals (FIG-4346): it reads on
                // headless.
                Err(crate::runtime::session_api::CommandDrainStop::Headless(fault)) => {
                    return run_headless_commands_root(
                        root_controller,
                        admitted,
                        HeadlessRoot::Unrefreshed {
                            catalog: self.host.core.session_store_factory(),
                            fault,
                        },
                    )
                    .await
                    .map(|outcome| RootRun {
                        outcome,
                        run: None,
                        driven_inputs: Vec::new(),
                        empty_drain: None,
                    });
                }
                Err(crate::runtime::session_api::CommandDrainStop::Failed(error)) => {
                    self.record_turn_park_after_abort(&error, &root, None).await;
                    return Err(drive_abort(Some(&root), error));
                }
            }
        }
        let store = self.drive_store()?;
        let end = store
            .end_command_root(fence, &root, self.host.core.clock.timestamp_ms())
            .await
            .map_err(|error| {
                DriveAbort::Retry(crate::runtime::runtime_error_from_store_commit(error))
            })?;
        if end.terminal().is_some()
            && let Some(run) = self.drive_root.as_mut()
        {
            run.mark_terminal_written();
        }
        // The outcome is the admission's, never what this execution found:
        // a redelivery finds the lane its first execution already applied,
        // and must answer the same. A lane that made no progress shows in the
        // next admission naming the same leading command, which stops the
        // drive ([`DriveLoop`](crate::engine::DriveLoop)).
        Ok(RootRun {
            outcome: RootOutcome::Applied { root },
            run: None,
            driven_inputs: Vec::new(),
            empty_drain: None,
        })
    }

    /// Recover the follow-on the session head owes under `admitted`'s root
    /// (ADR 0101 §3, FIG-3542), from the recovery count `attempts` its drive
    /// admission recorded, under the drive fence its seal raised.
    ///
    /// The root's recorded decision, `drive-follow-on:{root}`, answers
    /// whether the head still owes the follow-on and how it is recovered,
    /// and raises the recovery count inside its body (FIG-4361). The rest of
    /// the root drives the recorded answer, never the head a replay finds:
    /// a follow-on the head owed no longer cedes the root, and one it owed
    /// runs under the recorded fact or commits exhausted.
    ///
    /// The decision records the head the follow-on's turn runs on and that
    /// turn's index, and its body retains that head (FIG-4380). The root
    /// adopts the recorded head and pins the recorded index before its turn,
    /// so a replay after the follow-on's own commit moved the head runs the
    /// turn its journal holds. The resident head is brought current first,
    /// for the decision's first execution to record; a refresh that failed
    /// leaves the drive no head, and the root issues the same step headless
    /// ([`run_headless_follow_on_root`]).
    pub(super) async fn run_follow_on_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        follow_on: &FollowOnWork<'_>,
        sinks: &DriveSinks<'_>,
        fence: &crate::store::DriveFence,
    ) -> Result<RootRun, DriveAbort> {
        let root = admitted.root().clone();
        let store = self.drive_store()?;
        if let Err(fault) = self.refresh_resident_head().await {
            return run_headless_follow_on_root(
                root_controller,
                admitted,
                follow_on,
                HeadlessRoot::Unrefreshed {
                    catalog: self.host.core.session_store_factory(),
                    fault,
                },
            )
            .await
            .map(|outcome| RootRun {
                outcome,
                run: None,
                driven_inputs: Vec::new(),
                empty_drain: None,
            });
        }
        let answer = execute_follow_on_recovery(
            root_controller,
            admitted,
            follow_on,
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(RecoverFollowOnRunner {
                    store,
                    fence: fence.clone(),
                    follow_on: follow_on.turn.clone(),
                    attempts: follow_on.attempts,
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
        .map_err(|error| drive_abort(Some(&root), error))?;
        let (recovery, base, turn_index) = match answer {
            FollowOnRecoveryAnswer::Ceded => {
                return Ok(RootRun {
                    outcome: RootOutcome::Ceded { root },
                    run: None,
                    driven_inputs: Vec::new(),
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
            } => (
                crate::store::FollowOnRecovery::Run(follow_on),
                base,
                turn_index,
            ),
            FollowOnRecoveryAnswer::Exhausted {
                follow_on,
                base,
                turn_index,
            } => (
                crate::store::FollowOnRecovery::Exhausted(follow_on),
                base,
                turn_index,
            ),
        };
        // The follow-on's turn runs on the head its decision recorded, at the
        // index it recorded, whatever head this execution refreshed: a replay
        // after the follow-on's own commit finds a head that commit moved
        // (FIG-4380).
        if let Err(error) = self.adopt_recorded_turn(&base, turn_index).await {
            self.record_turn_park_after_abort(&error, &root, None).await;
            return Err(drive_abort(Some(&root), error));
        }
        // The recorded base is read without its pending fact, and the fact
        // the turn runs under is the recorded one: the head's while the head
        // owes the follow-on, since the decision's first execution raised or
        // read it there, and every head write of the turn must carry the
        // fact the head holds.
        let (crate::store::FollowOnRecovery::Run(owed)
        | crate::store::FollowOnRecovery::Exhausted(owed)) = &recovery;
        self.state.pending_follow_on = Some(Box::new(owed.clone()));
        // The follow-on's turn is a physical turn of the logical root that
        // owed it, so it runs under that root's turn scope (FIG-3607
        // contract 4): its effects and process starts are owned by the root
        // whose evidence its final commit writes and whose scope that
        // evidence closes. The recovery root owns only its own steps.
        let host = Arc::clone(&self.host.core.control.effect_host);
        let logical_root = crate::store::PhysicalTurn::split_turn_id(follow_on.turn).0;
        let turn_controller = super::step_controller(
            root_controller,
            host.as_ref(),
            drive_root_scope(admitted.session(), &logical_root),
        )
        .map_err(DriveAbort::Refused)?;
        let drain = Box::pin(self.drive_recovered_follow_on(
            recovery,
            turn_controller,
            sinks,
            fence.clone(),
        ))
        .await;
        self.admitted_turn_index = None;
        let drain = drain.map_err(|error| drive_abort(Some(&root), error))?;
        Ok(match drain {
            crate::runtime::turn_loop::QueuedTurnDrain::Ran(turn) => RootRun {
                outcome: RootOutcome::Committed {
                    root,
                    outcome: turn.outcome.clone(),
                },
                run: Some(crate::AgentFrameRun {
                    turns: vec![turn],
                    acceptance: None,
                }),
                driven_inputs: Vec::new(),
                empty_drain: None,
            },
            crate::runtime::turn_loop::QueuedTurnDrain::Empty(reason) => RootRun {
                outcome: RootOutcome::Ceded { root },
                run: None,
                driven_inputs: Vec::new(),
                empty_drain: Some(reason),
            },
        })
    }

    /// Adopt the head a root's admission admitted it on and pin its recorded
    /// turn index for the prepare phase (FIG-3682).
    ///
    /// The recorded inspection alone decides which head the resident session
    /// is rebuilt from: a `Ready` verdict rebuilds it from the admission's
    /// base, whatever the live head is now; an `Advanced` one from the head
    /// the root's own commits published (FIG-4201); an `Overtaken` verdict
    /// ends the root typed `StoreCommitSuperseded`, and a `Diverged` one
    /// parks it.
    ///
    /// A base the store no longer retains parks the root too.
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
            root: turn_id,
        } = admitted;
        // The root runs only under the executable generation its admission
        // recorded (FIG-3571), checked before anything else of it runs.
        crate::runtime::turn_loop::generation_fence::admit(self, generation)?;
        // Every commit of the root writes plugin namespaces in the formats
        // its admission recorded (FIG-4747), on this execution and on every
        // retry of it, whatever the fleet record permits by then.
        if let Some(session) = self.session.as_ref() {
            session.plugins().adopt_plugin_admission(plugins.clone());
        }
        // The verdict is the one `drive-head` recorded, honoured at every
        // position (FIG-4058). Its live check, a head that moved from the
        // admission's base with no commit of this root behind it, is the
        // inspection's body, which runs only when `drive-head` is this
        // attempt's live frontier. A replay is served the recorded verdict:
        // whatever the first attempt did after it is already journaled, so a
        // head that moved since is met by the turn's fenced commit as a typed
        // refusal, never re-decided here at a recorded position.
        let base = match verdict {
            AdmittedHeadVerdict::Ready => base,
            // The root's own commits moved the head: it continues from the
            // head they published, its own frame (FIG-4201).
            AdmittedHeadVerdict::Advanced { ref head } => head,
            // Ordinary head overtaking: another writer committed past the
            // base, so every commit of the root meets the moved head. The
            // root ends with the refusal its commit would meet (FIG-4200).
            AdmittedHeadVerdict::Overtaken { live_revision } => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::StoreCommitSuperseded,
                    format!(
                        "another writer moved the session head from revision {} to {} under \
                         root `{turn_id}` before it committed; the root can never commit on the \
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
                         revision {} root `{turn_id}` was admitted on; the root is not driven on a \
                         head it was not admitted on",
                        base.revision
                    ),
                ));
            }
        };
        self.adopt_recorded_turn(base, turn_index).await
    }

    /// Adopt `base`, the head a root's recorded step says its turn runs on,
    /// as the resident session, and pin the recorded `turn_index` for the
    /// turn's prepare phase, which then reads no live head (FIG-3682,
    /// FIG-4380).
    ///
    /// A base the store no longer retains parks the root.
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

/// The invocation of one of a root's recorded drive steps, `{step}:{root}`
/// on the root's scope. Keyed by the root, never by the drive admission: a
/// later admission of the same root replays the step its first execution
/// recorded, so the root drives exactly the rows its journal was written
/// for.
fn root_step_invocation(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    step: &str,
) -> Result<crate::RuntimeEffectInvocation, DriveAbort> {
    let root = admitted.root();
    Ok(crate::RuntimeEffectInvocation::new(
        crate::EffectAddress::new(
            root_controller.execution_scope().clone(),
            format!("{step}:{root}"),
        )
        .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
        crate::RuntimeAttribution::for_turn_admission(admitted.session().clone(), root.clone()),
        format!("{root}.{step}"),
    ))
}

/// The root's recorded admission, `drive-admit:{root}`, whose first
/// execution runs `runner`. The outer error is a step the drive could not
/// address; the inner one is the step's answer that is not an admission:
/// its recorded refusal, or a fault its attempt met.
async fn execute_root_admission(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    head: &AdmittedHead,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Result<RootAdmissionAnswer, RuntimeError>, DriveAbort> {
    let invocation = root_step_invocation(root_controller, admitted, "drive-admit")?;
    Ok(root_controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::AdmitRoot { head: head.clone() },
            ),
            runner,
        )
        .await
        .and_then(crate::RuntimeEffectOutcome::into_root_admission)
        .map_err(crate::RuntimeEffectControllerError::into_runtime_error))
}

/// The root's recorded head inspection, `drive-head:{root}`, whose first
/// execution runs `runner`: the recorded verdict on the head the root's
/// admission admitted it on.
async fn execute_head_inspection(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    head: &AdmittedHead,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Result<AdmittedHeadVerdict, RuntimeError>, DriveAbort> {
    let invocation = root_step_invocation(root_controller, admitted, "drive-head")?;
    Ok(root_controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::InspectAdmittedHead {
                    root: admitted.root().clone(),
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

/// The follow-on a recovery root was admitted for, with the recovery count
/// its drive admission recorded.
pub(super) struct FollowOnWork<'a> {
    pub(super) turn: &'a TurnId,
    pub(super) attempts: u32,
}

/// A follow-on recovery root's recorded decision, `drive-follow-on:{root}`,
/// whose first execution runs `runner` (FIG-4361). The outer error is a step
/// the drive could not address; the inner one is the step's answer that is
/// not a decision: its recorded retirement, or a fault its attempt met.
async fn execute_follow_on_recovery(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    follow_on: &FollowOnWork<'_>,
    runner: crate::RuntimeEffectLocalExecutor<'_>,
) -> Result<Result<FollowOnRecoveryAnswer, RuntimeError>, DriveAbort> {
    let invocation = root_step_invocation(root_controller, admitted, "drive-follow-on")?;
    Ok(root_controller
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

/// The first execution of a follow-on recovery root's decision: the drive's
/// one live read of the fact the head owes, and the fenced write that raises
/// its recovery count before the follow-on's first effect (ADR 0101 §3).
///
/// The decision is taken on the count `attempts` the drive admission
/// recorded. A head that owes the follow-on at a count already past it was
/// raised by an earlier execution of this step whose answer was not
/// recorded, which this one continues without raising again. Once the raised
/// count would pass the recovery bound the fact froze for its logical run,
/// the answer is `Exhausted` and the fact is not raised: the bound of the
/// host driving the root never decides it. The recorded fact carries that
/// bound.
///
/// A run or an exhaustion records `base`, the resident head the drive
/// refreshed, and `turn_index`, the next one after it, and retains the base
/// under the fence before the raise, as a root's admission retains its own
/// (FIG-3682, FIG-4380): the root's turn runs on that head at that index on
/// every execution. A store that did not answer is this attempt's fault and
/// the step runs again; a retired session is recorded (FIG-3630).
struct RecoverFollowOnRunner {
    store: crate::store::SessionStore,
    fence: crate::store::DriveFence,
    follow_on: TurnId,
    attempts: u32,
    /// The resident head the follow-on's turn runs on, as the drive
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
        let raised_earlier = owed.attempts > basis.attempts;
        Ok(match recovery {
            crate::store::FollowOnRecovery::Run(_) => FollowOnRecoveryAnswer::Run {
                follow_on: if raised_earlier {
                    owed
                } else {
                    self.store
                        .raise_pending_follow_on_attempts(&self.fence, &owed.follow_on_turn_id)
                        .await?
                },
                base,
                turn_index,
            },
            crate::store::FollowOnRecovery::Exhausted(_) => FollowOnRecoveryAnswer::Exhausted {
                // The head's fact: an earlier execution of this step may have
                // raised it past `basis`.
                follow_on: owed,
                base,
                turn_index,
            },
        })
    }
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RecoverFollowOnRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
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

/// Why a sealed root's drive holds no current head of its session.
#[derive(Clone)]
pub(super) enum HeadlessRoot {
    /// The engine could not open the session at all: its close or
    /// tombstone already committed (ADR 0049).
    Retired,
    /// The drive opened the session, but its resident head could not be
    /// brought current: `fault`, which a deleted session's refresh meets too.
    /// `catalog` is the deployment's session catalog, which answers whether
    /// the session was deleted.
    Unrefreshed {
        catalog: Arc<dyn crate::DeploymentStore>,
        fault: RuntimeError,
    },
}

impl HeadlessRoot {
    /// How an attempt ends whose journal holds work of `admitted`'s root
    /// past its headless steps, which cannot replay without the session's
    /// head: `what` the journal holds. A live fault that journals nothing:
    /// the refresh's, or, for a retired session, one naming the retirement.
    /// The engine retries it; a session that stays retired is released by the
    /// engine's park reconcile, whose park writer answers a deleted session
    /// `TargetGone`.
    fn past_its_steps(self, admitted: &Admitted, what: &str) -> DriveAbort {
        DriveAbort::Retry(match self {
            Self::Unrefreshed { fault, .. } => fault,
            Self::Retired => RuntimeError::new(
                RuntimeErrorCode::SessionHeadRefresh,
                format!(
                    "root `{}` of session `{}` recorded {what} before its session retired; the \
                     work its journal holds after them cannot replay without the session",
                    admitted.root(),
                    admitted.session()
                ),
            ),
        })
    }
}

/// Run a sealed input- or queued-headed root whose drive holds no current
/// head of its session (FIG-4346).
///
/// The root still issues its recorded admission and head inspection, in the
/// order and under the envelopes a root with a head issues them, so a
/// replay follows the journal an earlier attempt of the root recorded, and
/// nothing the drive read outside those steps decides what it journals
/// (ADR 0105 §1). Their bodies admit and inspect nothing, since there is no
/// head to do it on: [`HeadlessRootStepRunner`] answers only why.
///
/// - A step that recorded the session's retirement ends the root with the
///   typed `SessionDeleted` refusal (ADR 0049), recorded where the journal
///   held nothing more, so every replay answers the same.
/// - A step that recorded a refusal to admit cedes the root, as it does with
///   a head.
/// - A recorded admission and head inspection mean an earlier attempt ran
///   the root's turn after them, and that turn cannot replay without the
///   session's head ([`HeadlessRoot::past_its_steps`]).
pub(super) async fn run_headless_root(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    head: &AdmittedHead,
    headless: HeadlessRoot,
) -> Result<RootOutcome, DriveAbort> {
    let root = admitted.root().clone();
    let runner = || {
        lash_core_execution::core_internal::owned_runner_executor(
            Box::new(HeadlessRootStepRunner {
                session: admitted.session().clone(),
                step: HeadlessStep::Root {
                    root: root.clone(),
                    head: head.clone(),
                },
                headless: headless.clone(),
            }),
            None,
        )
    };
    match execute_root_admission(root_controller, admitted, head, runner()).await? {
        Ok(RootAdmissionAnswer::Admitted { .. }) => {}
        Ok(RootAdmissionAnswer::Refused { .. }) => return Ok(RootOutcome::Ceded { root }),
        Err(error) => return Err(drive_abort(Some(&root), error)),
    }
    if let Err(error) = execute_head_inspection(root_controller, admitted, head, runner()).await? {
        return Err(drive_abort(Some(&root), error));
    }
    Err(headless.past_its_steps(admitted, "its admission and head inspection"))
}

/// Run a sealed command root whose drive holds no current head of its
/// session (FIG-4346), from its next read of the session's command lane.
///
/// A command root's recorded steps are its reads of the lane
/// (`session-command-run:{ordinal}`, numbered by `root_controller`); every
/// command but an administrative compaction settles and commits off the
/// journal. The headless root issues the reads in that order, and their
/// bodies read nothing ([`HeadlessRootStepRunner`]):
///
/// - a read that recorded the session's retirement ends the root with the
///   typed `SessionDeleted` refusal (ADR 0049);
/// - a recorded empty lane is the root's end, `Applied`, as it is with a
///   head;
/// - a recorded run of commands that journal nothing is followed by the next
///   read, as it is with a head;
/// - a recorded compaction journaled its apply after the read, which cannot
///   replay without the session's head ([`HeadlessRoot::past_its_steps`]).
pub(super) async fn run_headless_commands_root(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    headless: HeadlessRoot,
) -> Result<RootOutcome, DriveAbort> {
    let root = admitted.root().clone();
    loop {
        let batches = crate::runtime::session_api::execute_session_command_run_read(
            root_controller,
            admitted.session(),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(HeadlessRootStepRunner {
                    session: admitted.session().clone(),
                    step: HeadlessStep::CommandRun,
                    headless: headless.clone(),
                }),
                None,
            ),
        )
        .await
        .map_err(|error| drive_abort(Some(&root), error))?;
        if batches.is_empty() {
            return Ok(RootOutcome::Applied { root });
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

/// Run a sealed follow-on recovery root whose drive holds no current head of
/// its session (FIG-4361).
///
/// The root still issues its recorded decision, `drive-follow-on`, under
/// the envelope a root with a head issues it, and its body decides nothing
/// ([`HeadlessRootStepRunner`]):
///
/// - a decision that recorded the session's retirement ends the root with
///   the typed `SessionDeleted` refusal (ADR 0049);
/// - a recorded `Ceded` cedes the root, as it does with a head;
/// - a recorded run or exhaustion means an earlier attempt ran the
///   follow-on's turn after it, which cannot replay without the session's
///   head ([`HeadlessRoot::past_its_steps`]).
pub(super) async fn run_headless_follow_on_root(
    root_controller: &ScopedEffectController<'_>,
    admitted: &Admitted,
    follow_on: &FollowOnWork<'_>,
    headless: HeadlessRoot,
) -> Result<RootOutcome, DriveAbort> {
    let root = admitted.root().clone();
    let runner = lash_core_execution::core_internal::owned_runner_executor(
        Box::new(HeadlessRootStepRunner {
            session: admitted.session().clone(),
            step: HeadlessStep::FollowOn {
                follow_on: follow_on.turn.clone(),
                attempts: follow_on.attempts,
            },
            headless: headless.clone(),
        }),
        None,
    );
    match execute_follow_on_recovery(root_controller, admitted, follow_on, runner).await? {
        Ok(FollowOnRecoveryAnswer::Ceded) => Ok(RootOutcome::Ceded { root }),
        Ok(FollowOnRecoveryAnswer::Run { .. } | FollowOnRecoveryAnswer::Exhausted { .. }) => {
            Err(headless.past_its_steps(admitted, "its follow-on recovery"))
        }
        Err(error) => Err(drive_abort(Some(&root), error)),
    }
}

/// The steps a headless root issues.
enum HeadlessStep {
    /// An input- or queued-headed root's admission and head inspection.
    Root { root: TurnId, head: AdmittedHead },
    /// A command root's read of the session's command lane.
    CommandRun,
    /// A follow-on recovery root's decision.
    FollowOn { follow_on: TurnId, attempts: u32 },
}

/// The first execution of a headless root's step ([`run_headless_root`],
/// [`run_headless_commands_root`], [`run_headless_follow_on_root`]): it
/// admits, inspects, reads and decides nothing, and answers why the drive
/// holds no head.
///
/// A retired session is a settled fact the step records, as the drive's own
/// admission and seal record it (FIG-3630, FIG-3881): the engine could not
/// open the session at all, or the catalog, read inside the step, holds the
/// session's deletion tombstone. Any other refresh fault is this attempt's,
/// never recorded, and the engine runs the step again.
struct HeadlessRootStepRunner {
    session: crate::SessionId,
    step: HeadlessStep,
    headless: HeadlessRoot,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for HeadlessRootStepRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let bound = match (&self.step, &envelope.command) {
            (
                HeadlessStep::Root { head: bound, .. },
                crate::RuntimeEffectCommand::AdmitRoot { head },
            ) => head == bound,
            (
                HeadlessStep::Root {
                    root: bound_root,
                    head: bound_head,
                },
                crate::RuntimeEffectCommand::InspectAdmittedHead { root, head },
            ) => root == bound_root && head == bound_head,
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
                        "headless root step executor cannot execute {} command",
                        other.kind().as_str()
                    ),
                ));
            }
        };
        if !bound {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "headless root step executor was bound to another session, root, head or follow-on",
            ));
        }
        let retirement = |error: crate::StoreError| {
            super::admission::store_fault("session retired under its root", error)
        };
        match self.headless {
            HeadlessRoot::Retired => Err(retirement(crate::StoreError::SessionDeleted {
                session_id: self.session,
            })),
            HeadlessRoot::Unrefreshed { catalog, fault } => {
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
    root: &'a TurnId,
}

/// The resident head an attempt refreshed before its root's inspection.
struct ResidentHead {
    revision: u64,
    leaf: Option<crate::NodeId>,
    checkpoint: Option<crate::store::BlobRef>,
}

/// The body of a root's `drive-head` step: the drive's one live head check.
///
/// It runs only when the step is not recorded yet, so at the attempt's live
/// frontier before any turn effect, and decides from the resident head this
/// attempt refreshed. A head that moved from the admission's base with no
/// final commit of the root behind it is decided by its components
/// (FIG-4200): a higher revision that a fenced commit published is the
/// root's own, `Advanced`, and the root continues from it (FIG-4201); a
/// higher revision a lane-less write published is another writer
/// overtaking the head, `Overtaken`, and the root ends typed; a lower
/// revision, or the same revision with another leaf or checkpoint, is an
/// inconsistent head, `Diverged`, and the root parks before it drives a
/// head it was not admitted on. A replay serves the recorded verdict and
/// never runs it.
///
/// A fenced commit that lands while the root is unfinished is the root's
/// own: the store refuses a fence an admission superseded, and every
/// admission sealed while the root is unfinished resumes it.
struct InspectAdmittedHeadRunner {
    store: crate::store::SessionStore,
    root: TurnId,
    head: AdmittedHead,
    /// The head the root's admission recorded.
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
    /// of the root behind it. `published_by_drive` is whether a fenced commit
    /// published the live head.
    fn moved_head_verdict(&self, published_by_drive: bool) -> AdmittedHeadVerdict {
        let live_revision = self.live.revision;
        if live_revision <= self.base.revision {
            return AdmittedHeadVerdict::Diverged { live_revision };
        }
        if published_by_drive {
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
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::InspectAdmittedHead { root, head } = &envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "admitted head inspector received another command",
            ));
        };
        if *root != self.root || *head != self.head {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "admitted head inspector was bound to another root or head",
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
                .committed_turn_exists(&self.root)
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
            let published_by_drive = meta.is_some_and(|head| {
                head.published_by_drive
                    && head.head_revision == self.live.revision
                    && head.leaf_node_id == self.live.leaf
                    && head.checkpoint_ref == self.live.checkpoint
            });
            self.moved_head_verdict(published_by_drive)
        };
        Ok(crate::RuntimeEffectOutcome::InspectAdmittedHead { verdict })
    }
}

/// Trace attribution for the admission decisions the runner makes.
struct AdmissionTrace {
    sink: Option<Arc<dyn lash_trace::TraceSink>>,
    base: lash_trace::TraceContext,
    clock: Arc<dyn crate::Clock>,
    turn_index: usize,
}

/// What the admission probe decided for the head row: either the outcome
/// the journal records (an admission or a refusal) or a race the step
/// retries.
enum RootAdmissionProbe {
    /// The journaled `AdmitRoot` outcome.
    Answer(RootAdmissionAnswer),
    /// The admission took nothing yet the head row is still in the store:
    /// composition and read raced, so the step defers to a later admission
    /// instead of journaling a refusal over a row that is still there.
    HeadMissedByRace,
}

/// The first execution of a root's admission.
///
/// Everything it needs is captured at the drive site; none of it enters the
/// envelope, which names only the head: the fence changes with every drive
/// epoch, and the envelope must not.
struct AdmitRootRunner {
    store: crate::store::SessionStore,
    effect_host: Arc<dyn crate::EffectHost>,
    scope: crate::AdmittedScope,
    fence: crate::store::DriveFence,
    head: AdmittedHead,
    root: TurnId,
    /// The runtime's turn-input admission bound
    /// ([`QueuedWorkBatchingConfig::max_turn_input_admission`](crate::QueuedWorkBatchingConfig::max_turn_input_admission)).
    max_inputs: usize,
    /// The turn-lane composition policy a queued-work head is admitted
    /// under.
    policy: crate::TurnLaneAdmissionPolicy,
    /// The resident head the root is admitted on, as the drive refreshed it.
    /// Its generation is read in the body.
    base: crate::store::SessionHeadRef,
    /// The root's turn index: the next one after `base`.
    turn_index: usize,
    /// The executable generation the root is admitted under (FIG-3571).
    generation: Option<crate::ExecutableGeneration>,
    admitted_generation: crate::engine::BuildGeneration,
    /// The execution that runs the root, which the admission records
    /// (FIG-4403).
    executor: crate::store::RootExecutor,
    /// The plugins the root runs, whose composition and writer formats the
    /// admission records (FIG-4747). `None` for a runtime with no session.
    plugin_host: Option<crate::plugin::PluginHost>,
    trace: AdmissionTrace,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for AdmitRootRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::AdmitRoot { head } = &envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root admission executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *head != self.head {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root admission executor was bound to `{:?}` but asked to admit `{head:?}`",
                    self.head
                ),
            ));
        }
        // A store that did not answer is this attempt's fault, never the
        // admission's recorded outcome: the drive re-admits this same root
        // first, so a recorded fault would replay under `drive-admit:{root}`
        // on every later drive and wedge the session. Like the drive's
        // admission and the seal, the step runs again; only an admission or
        // a refusal is recorded.
        //
        // Every permanent store refusal is an admission outcome (FIG-4629).
        // The retry marker rejects terminal causes, so the journal records
        // the typed refusal and the root answers its sender.
        let root = self.root.clone();
        self.activate_turn_cancel_binding().await.map_err(|error| {
            crate::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation()
        })?;
        let answer = match self.admit().await.map_err(|err| {
            let mut fault = crate::RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(err),
            );
            fault.message = format!("root admission failed: {}", fault.message);
            fault.retryable_uncommitted_derivation()
        })? {
            RootAdmissionProbe::Answer(answer) => answer,
            // The admission missed the head yet the head row is still there:
            // the two raced, so the step defers to a later admission rather
            // than journal a refusal that would cede the root over a row that
            // is still to be answered (review of #2290).
            RootAdmissionProbe::HeadMissedByRace => {
                return Err(crate::RuntimeEffectControllerError::new(
                    RuntimeErrorCode::SessionExecutionLaneBusy,
                    format!("root `{root}` missed its head on an admission race"),
                )
                .retryable_uncommitted_derivation());
            }
        };
        Ok(crate::RuntimeEffectOutcome::AdmitRoot { answer })
    }
}

impl AdmitRootRunner {
    /// The drive's fenced activation gate: the session's turn-cancellation
    /// binding is recorded on first use and checked on every later root, so
    /// a reopened host with a different physical authority is refused before
    /// the root admits anything or does any other session work.
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
                    &crate::TurnAddress::new(session_id, &self.root),
                    scoped.execution_scope(),
                    binding_id,
                ),
            )
            .await
            .map_err(crate::runtime::runtime_error_from_store_commit)
    }

    fn emit(&self, name: &str, payload: serde_json::Value) {
        crate::trace::emit_trace(
            &self.trace.sink,
            &self.trace.base,
            lash_trace::TraceContext::default()
                .for_session(self.fence.session().clone())
                .for_turn_index(self.trace.turn_index)
                .for_turn(self.root.clone()),
            lash_trace::TraceEvent::Custom {
                name: name.to_string(),
                payload,
            },
            self.trace.clock.as_ref(),
        );
    }

    /// Admit the turn-lane run headed by the admitted head.
    ///
    /// The store composes the run from open rows, binds them to the root,
    /// retains the base and records the admission in one transaction, and a
    /// later execution of the same root reads that record back instead of
    /// composing again (FIG-3840, FIG-3927). A worker that dies after the
    /// commit but before the journal takes the outcome therefore leaves
    /// nothing to recompute: the successor drives exactly the recorded
    /// composition, base and executable generation, never a run widened by
    /// rows that arrived meanwhile.
    ///
    /// A composition that would not reach the head takes nothing, and the
    /// head row is read without mutating it: absent means it was settled,
    /// cancelled, or pruned, and the root cedes; still present means the
    /// admission raced, so the step asks to run again rather than record a
    /// refusal. Nothing here ever drops, withdraws, or re-admits a row.
    async fn admit(self) -> Result<RootAdmissionProbe, crate::StoreError> {
        // The admission is the adoption point (FIG-4747): this build's
        // composition, and each plugin's writer chosen from the fleet record
        // as it stands now. The store records the first admission's choice
        // and answers it to every later one, so the record is read here and
        // nowhere after.
        let plugins = match &self.plugin_host {
            Some(host) => host.admit_plugins(self.store.store().as_ref()).await?,
            None => crate::store::plugin_writers::PluginAdmission::default(),
        };
        let request = crate::store::AdmitRootRequest {
            fence: self.fence.clone(),
            root: self.root.clone(),
            head: self.head.clone(),
            max_inputs: self.max_inputs,
            policy: self.policy.clone(),
            base: self.base.clone(),
            turn_index: self.turn_index as u64,
            generation: self.generation.clone(),
            admitted_generation: self.admitted_generation.clone(),
            executor: self.executor.clone(),
            plugins,
        };
        let admission = match self.store.admit_root(&request).await {
            // The record decides (FIG-4765): the root is run by the executor
            // its admission names, so this execution cedes it. The recorded
            // executor never changes, so the refusal is the step's outcome.
            Err(crate::StoreError::RootHeldByAnotherExecutor { .. }) => {
                return Ok(RootAdmissionProbe::Answer(RootAdmissionAnswer::Refused {
                    refusal: RootAdmissionRefusal::HeldByAnotherExecutor,
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
            self.emit(
                "ingress.admitted",
                crate::runtime::turn_loop::ingress_admitted_trace_payload(
                    &self.root,
                    crate::store::ROOT_ADMISSION_STEP,
                    crate::AdmissionBoundary::Idle,
                    admission.inputs.as_deref(),
                    admission.queued.as_deref(),
                    &causes,
                ),
            );
            return Ok(RootAdmissionProbe::Answer(RootAdmissionAnswer::Admitted {
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
            RootAdmissionProbe::HeadMissedByRace
        } else {
            RootAdmissionProbe::Answer(RootAdmissionAnswer::Refused {
                refusal: RootAdmissionRefusal::HeadGone,
            })
        })
    }
}
