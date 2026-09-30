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
use crate::engine::{Admitted, DriveAbort, RootOutcome};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::runtime::logical_turn::{LogicalTurnAdmissions, LogicalTurnStart};
use crate::runtime::turn_loop::TurnStopwatch;
use crate::store::{AdmittedHead, RootAdmissionAnswer, RootAdmissionRefusal};
use crate::{
    RuntimeError, RuntimeErrorCode, ScopedEffectController, SessionError, TurnId, TurnInput,
};
use lash_core_execution::runtime::effect::AdmittedHeadVerdict;

impl LashRuntime {
    /// Admit the turn-lane run `admitted` is headed by and drive it as the
    /// root's logical turn, under the drive fence the root's seal raised.
    pub(super) async fn run_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        head: &AdmittedHead,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &TurnInput)>,
        fence: &crate::store::DriveFence,
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
            crate::RuntimeEffectLocalExecutor::owned_runner(
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
                        .admission_policy(self.max_context_tokens()),
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
                    crate::RuntimeEffectLocalExecutor::owned_runner(
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
            LogicalTurnStart::Input(driven, None),
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
    /// (ADR 0101 §3, FIG-3542), raising its recovery count from the
    /// `attempts` admission recorded.
    pub(super) async fn run_follow_on_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        follow_on: &TurnId,
        attempts: u32,
        sinks: &DriveSinks<'_>,
    ) -> Result<RootRun, DriveAbort> {
        let root = admitted.root().clone();
        let host = Arc::clone(&self.host.core.control.effect_host);
        let options = root_drain_options(root_controller, host.as_ref(), admitted, sinks)?;
        let drain = Box::pin(self.recover_admitted_follow_on(options, follow_on, attempts))
            .await
            .map_err(|error| drive_abort(Some(&root), error))?;
        self.root_run_of_drain(root, drain)
    }

    /// A follow-on recovery root's outcome from how its drain ended.
    fn root_run_of_drain(
        &self,
        root: TurnId,
        drain: crate::runtime::turn_loop::QueuedTurnDrain<crate::AssembledTurn>,
    ) -> Result<RootRun, DriveAbort> {
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
            crate::runtime::turn_loop::QueuedTurnDrain::Empty(
                crate::runtime::turn_loop::EmptyQueuedDrainReason::ExecutionLaneBusy,
            ) => {
                return Err(DriveAbort::Retry(RuntimeError::new(
                    RuntimeErrorCode::SessionExecutionLaneBusy,
                    format!(
                        "session `{}` cannot run root `{root}` until it acquires its execution lane",
                        self.state.session_id
                    ),
                )));
            }
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
            root: turn_id,
        } = admitted;
        // The root runs only under the executable generation its admission
        // recorded (FIG-3571), checked before anything else of it runs.
        crate::runtime::turn_loop::generation_fence::admit(self, generation)?;
        let turn_index = usize::try_from(turn_index).map_err(|_| {
            RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "admitted turn index exceeds platform range",
            )
        })?;
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
        crate::RuntimeEffectLocalExecutor::owned_runner(
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
            crate::RuntimeEffectLocalExecutor::owned_runner(
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

/// The steps a headless root issues.
enum HeadlessStep {
    /// An input- or queued-headed root's admission and head inspection.
    Root { root: TurnId, head: AdmittedHead },
    /// A command root's read of the session's command lane.
    CommandRun,
}

/// The first execution of a headless root's step ([`run_headless_root`],
/// [`run_headless_commands_root`]): it admits, inspects and reads nothing,
/// and answers why the drive holds no head.
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
                "headless root step executor was bound to another session, root or head",
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
            let published_by_drive = self
                .store
                .load_session_head_meta()
                .await
                .map_err(store_fault)?
                .is_some_and(|head| {
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

/// The drain options a follow-on recovery root runs under: its own drain
/// scope, named by the root.
fn root_drain_options<'a>(
    root_controller: &ScopedEffectController<'a>,
    host: &'a dyn crate::EffectHost,
    admitted: &Admitted,
    sinks: &DriveSinks<'a>,
) -> Result<crate::runtime::QueuedTurnOptions<'a>, DriveAbort> {
    let drain_controller = super::step_controller(
        root_controller,
        host,
        crate::AdmittedScope::queue_drain(admitted.session().clone(), admitted.root().as_str()),
    )
    .map_err(DriveAbort::Refused)?;
    Ok(crate::runtime::QueuedTurnOptions::new(
        sinks.local_stop.immediate_token(),
        crate::runtime::QueuedEffectSource::Scoped(drain_controller),
    )
    .with_local_stop(sinks.local_stop.clone())
    .with_events(sinks.events)
    .with_turn_events(sinks.turn_events))
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
    trace: AdmissionTrace,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for AdmitRootRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
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
        };
        if let Some(admission) = self.store.admit_root(&request).await? {
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
