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
        // instead (FIG-3682).
        self.refresh_resident_head().await.map_err(abort)?;
        let admit_invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                root_controller.execution_scope().clone(),
                // Keyed by the root, never by the drive admission: a later
                // admission of the same root replays the admission its first
                // execution recorded, so the root drives exactly the rows its
                // journal was written for.
                format!("drive-admit:{root}"),
            )
            .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
            crate::RuntimeAttribution::for_turn_admission(
                self.state.session_id.clone(),
                root.clone(),
            ),
            format!("{root}.drive-admit"),
        );
        let answer = root_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    admit_invocation,
                    crate::RuntimeEffectCommand::AdmitRoot { head: head.clone() },
                ),
                crate::RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(AdmitRootRunner {
                        store: Arc::clone(&store),
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
            .await
            .and_then(crate::RuntimeEffectOutcome::into_root_admission)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error);
        let admission = match answer {
            Ok(RootAdmissionAnswer::Admitted { admission }) => {
                let head_moved = self.state.head_revision != admission.base.revision
                    || self.state.session_graph.leaf_node_id != admission.base.leaf
                    || self.state.checkpoint_ref != admission.base.checkpoint;
                let inspection = crate::RuntimeEffectInvocation::new(
                    crate::EffectAddress::new(
                        root_controller.execution_scope().clone(),
                        format!("drive-head:{root}"),
                    )
                    .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
                    crate::RuntimeAttribution::for_turn_admission(
                        self.state.session_id.clone(),
                        root.clone(),
                    ),
                    format!("{root}.drive-head"),
                );
                let verdict = root_controller
                    .execute_effect(
                        crate::RuntimeEffectEnvelope::new(
                            inspection,
                            crate::RuntimeEffectCommand::InspectAdmittedHead {
                                root: root.clone(),
                                head: head.clone(),
                            },
                        ),
                        crate::RuntimeEffectLocalExecutor::owned_runner(
                            Box::new(InspectAdmittedHeadRunner {
                                store: Arc::clone(&store),
                                root: root.clone(),
                                head: head.clone(),
                                head_moved,
                                live_revision: self.state.head_revision,
                            }),
                            None,
                        ),
                    )
                    .await
                    .and_then(|outcome| match outcome {
                        crate::RuntimeEffectOutcome::InspectAdmittedHead { verdict } => Ok(verdict),
                        other => Err(crate::RuntimeEffectControllerError::wrong_outcome(
                            crate::RuntimeEffectKind::InspectAdmittedHead,
                            other.kind(),
                        )),
                    })
                    .map_err(crate::RuntimeEffectControllerError::into_runtime_error);
                let verdict = verdict.map_err(abort)?;
                if let Err(error) = self
                    .adopt_admitted_turn(
                        &store,
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
    /// until the command lane is empty. The root admits no turn.
    pub(super) async fn run_commands_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        fence: &crate::store::DriveFence,
    ) -> Result<RootRun, DriveAbort> {
        let root = admitted.root().clone();
        while Box::pin(self.drain_next_session_command_fenced(
            fence,
            tokio_util::sync::CancellationToken::new(),
            root_controller.controller(),
        ))
        .await
        .map_err(|error| drive_abort(Some(&root), error))?
        .is_some()
        {}
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
    /// The recorded inspection decides whether the resident head may be
    /// rebuilt from the admission's base. A live revalidation may only stop a
    /// root whose admission lost its fence while its handler was down; it
    /// cannot select new work or alter the admission's recorded base.
    ///
    /// A base the store no longer retains parks the root too.
    async fn adopt_admitted_turn(
        &mut self,
        store: &Arc<dyn crate::store::RuntimePersistence>,
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
        let head_moved = self.state.head_revision != base.revision
            || self.state.session_graph.leaf_node_id != base.leaf
            || self.state.checkpoint_ref != base.checkpoint;
        // The head is bound to this root alone (FIG-3927), so a head that
        // moved without this turn's commit diverged from the admission: the
        // root parks rather than drive a head it was not admitted on.
        //
        // A root its own refusal already ended (FIG-4018) is being replayed
        // by the run that met the refusal, which died before it recorded its
        // outcome. That run went past this check, so its replay does too: it
        // retraces the recorded turn to the same refusal, whose end is
        // already written, and records the outcome. Parking here instead
        // would write at a position the journal already recorded, and the
        // run would never finish.
        let verdict = if matches!(verdict, AdmittedHeadVerdict::Ready)
            && head_moved
            && !store
                .committed_turn_exists(turn_id)
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?
            && !store
                .root_terminal(&self.state.session_id, turn_id)
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?
                .is_some_and(|terminal| {
                    matches!(
                        terminal.cause,
                        crate::store::RootTerminalCause::Refused { .. }
                    )
                }) {
            AdmittedHeadVerdict::Diverged {
                live_revision: self.state.head_revision,
            }
        } else {
            verdict
        };
        match verdict {
            AdmittedHeadVerdict::Ready => {}
            AdmittedHeadVerdict::Diverged { live_revision } => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::EffectReplayDivergence,
                    format!(
                        "the session head moved from revision {} to {} under root `{turn_id}` before \
                         it committed; the root is not driven on a head it was not admitted on",
                        base.revision, live_revision
                    ),
                ));
            }
        }
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

struct AdmittedTurn<'a> {
    base: &'a crate::store::SessionHeadRef,
    turn_index: u64,
    generation: Option<&'a crate::ExecutableGeneration>,
    root: &'a TurnId,
}

struct InspectAdmittedHeadRunner {
    store: Arc<dyn crate::store::RuntimePersistence>,
    root: TurnId,
    head: AdmittedHead,
    head_moved: bool,
    live_revision: u64,
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
        let verdict = if !self.head_moved
            || self
                .store
                .committed_turn_exists(&self.root)
                .await
                .map_err(store_fault)?
        {
            AdmittedHeadVerdict::Ready
        } else {
            AdmittedHeadVerdict::Diverged {
                live_revision: self.live_revision,
            }
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
    store: Arc<dyn crate::store::RuntimePersistence>,
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
                session_id,
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
        let session_id = self.fence.session();
        let present = match &self.head {
            AdmittedHead::Input(head) => self
                .store
                .list_pending_turn_inputs(session_id)
                .await?
                .iter()
                .any(|read| read.input.input_id == *head),
            AdmittedHead::Batch(head) => self
                .store
                .list_queued_work(session_id)
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
