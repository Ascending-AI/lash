//! Running one admitted root (FIG-3600): its recorded claim, the head that
//! claim admits it on, and its turns to their terminal commit.
//!
//! An input root claims the accepted next-turn prefix its admission named,
//! as a recorded step keyed by the root, so every redrive of the root, under
//! any admission, replays the same claim and never re-reads pending rows. The
//! claim records the head the root runs on and its turn index; a redrive
//! rebuilds the root from that head, never from the live one its own commit
//! may have moved (FIG-3682). A queued root runs the session's queued work
//! through the queued-run drain.

use std::sync::Arc;

use super::{DriveSinks, RootRun, drive_abort};
use crate::engine::{Admitted, DriveAbort, RootOutcome};
use crate::runtime::LashRuntime;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;
use crate::runtime::logical_turn::{LogicalTurnClaims, LogicalTurnStart};
use crate::runtime::turn_loop::TurnStopwatch;
use crate::{
    RuntimeError, RuntimeErrorCode, ScopedEffectController, SessionError, TurnId, TurnInput,
};
use lash_core_execution::runtime::effect::AdmittedHeadVerdict;

impl LashRuntime {
    /// Claim the input prefix `admitted` names and drive it as the root's
    /// logical turn, holding the session lane the root took before its seal.
    pub(super) async fn run_input_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        head: &crate::InputId,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &TurnInput)>,
        lease: Option<crate::runtime::DriveClaimGuard>,
    ) -> Result<RootRun, DriveAbort> {
        use futures_util::FutureExt;

        let stopwatch = TurnStopwatch::start(self.host.core.clock.as_ref());
        let mut lease = lease;
        // Keep the guard outside the unwinding body: a panicking root
        // releases its lane before the panic resumes, so an immediate
        // successor is not refused as busy.
        let result = std::panic::AssertUnwindSafe(
            self.run_input_root_holding_lease(
                root_controller,
                admitted,
                head,
                sinks,
                live,
                &mut lease,
                stopwatch,
            )
            .boxed(),
        )
        .catch_unwind()
        .await;
        match result {
            Ok(result) => result,
            Err(payload) => {
                if let Some(lease) = lease.as_ref()
                    && let Err(error) = lease.release_if_live().await
                {
                    tracing::warn!(%error, "failed to release session execution lease after root panic");
                }
                std::panic::resume_unwind(payload)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_input_root_holding_lease(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        head: &crate::InputId,
        sinks: &DriveSinks<'_>,
        live: Option<(&crate::InputId, &TurnInput)>,
        lease: &mut Option<crate::runtime::DriveClaimGuard>,
        stopwatch: TurnStopwatch,
    ) -> Result<RootRun, DriveAbort> {
        let root = admitted.root().clone();
        let abort = |error: RuntimeError| drive_abort(Some(&root), error);
        let store = self.drive_store()?;
        let fence = lease
            .as_ref()
            .map(crate::runtime::DriveClaimGuard::fence)
            .ok_or_else(|| {
                abort(RuntimeError::new(
                    RuntimeErrorCode::QueuedWork,
                    "a store-backed root holds the session execution lease",
                ))
            })?;
        // The claim is the root's admission onto a head: its first execution
        // records the head and the turn index, so the resident head is
        // brought current under the lease first; a replay reads both from the
        // journal instead (FIG-3682).
        if let Err(error) = self.refresh_resident_head_under_lease(lease.as_ref()).await {
            self.release_root_lease(lease.as_ref()).await;
            return Err(abort(error));
        }
        let claim_invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                root_controller.execution_scope().clone(),
                // Keyed by the root, never by the admission: a later
                // admission of the same root replays the claim its first
                // execution recorded, so the root drives exactly the rows its
                // journal was written for.
                format!("drive-claim:{root}"),
            )
            .map_err(|error| DriveAbort::Refused(RuntimeError::from(error)))?,
            crate::RuntimeAttribution::for_turn_admission(
                self.state.session_id.clone(),
                root.clone(),
            ),
            format!("{root}.drive-claim"),
        );
        let drive = root_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    claim_invocation,
                    crate::RuntimeEffectCommand::ClaimAcceptedTurnInput {
                        input_id: head.clone(),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::owned_runner(
                    Box::new(RootInputClaimRunner {
                        store: Arc::clone(&store),
                        effect_host: Arc::clone(&self.host.core.control.effect_host),
                        scope: root_controller.admitted_scope().clone(),
                        fence: fence.clone(),
                        owner: fence.owner.clone(),
                        session_id: self.state.session_id.clone(),
                        head: head.clone(),
                        root: root.clone(),
                        max_inputs: self
                            .host
                            .core
                            .durability
                            .queued_work_batching
                            .max_turn_input_claim(),
                        base: crate::store::SessionHeadRef {
                            // Read by the claim body on its first execution.
                            generation: 0,
                            revision: self.state.head_revision,
                            leaf: self.state.session_graph.leaf_node_id.clone(),
                            checkpoint: self.state.checkpoint_ref.clone(),
                        },
                        // Restore safety: state::RESTORED_TURN_INDEX_HEADROOM.
                        turn_index: self.state.turn_index + 1,
                        generation: crate::runtime::turn_loop::generation_fence::current(self),
                        trace: ClaimTrace {
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
            .and_then(crate::RuntimeEffectOutcome::into_accepted_turn_input_drive)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error);
        let claim = match drive {
            Ok(crate::AcceptedTurnInputDrive::Claimed {
                claim,
                base,
                turn_index,
                generation,
            }) => {
                let head_moved = self.state.head_revision != base.revision
                    || self.state.session_graph.leaf_node_id != base.leaf
                    || self.state.checkpoint_ref != base.checkpoint;
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
                                session_id: self.state.session_id.clone(),
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
                let verdict = match verdict {
                    Ok(verdict) => verdict,
                    Err(error) => {
                        self.release_root_lease(lease.as_ref()).await;
                        return Err(abort(error));
                    }
                };
                if let Err(error) = self
                    .adopt_admitted_turn(
                        &store,
                        AdmittedTurn {
                            base: &base,
                            turn_index,
                            generation: generation.as_ref(),
                            root: &root,
                            head,
                        },
                        verdict,
                    )
                    .await
                {
                    self.record_turn_park_after_abort(&error, &root, None).await;
                    self.release_root_lease(lease.as_ref()).await;
                    return Err(abort(error));
                }
                *claim
            }
            Ok(crate::AcceptedTurnInputDrive::Refused { .. }) => {
                self.release_root_lease(lease.as_ref()).await;
                return Ok(RootRun {
                    outcome: RootOutcome::Ceded { root },
                    run: None,
                    driven_inputs: Vec::new(),
                    queued_drain: None,
                });
            }
            Err(error) => {
                // The claim's body may have claimed rows before its outcome
                // was lost. They stay claimed under this generation: the next
                // drive admits the same root first and re-takes them.
                self.release_root_lease(lease.as_ref()).await;
                return Err(abort(error));
            }
        };

        // Drive the claimed rows. Live per-turn state that cannot cross the
        // durable boundary is re-attached from an in-process caller's input.
        let mut driven = claim.materialize_turn_input();
        if let Some((_, live)) = live.filter(|(input_id, _)| {
            claim
                .inputs
                .iter()
                .any(|input| input.input_id == **input_id)
        }) {
            driven.turn_context = live.turn_context.clone();
        }
        driven.trace_turn_id = Some(root.clone());
        let driven_inputs = claim
            .inputs
            .iter()
            .map(|input| input.input_id.clone())
            .collect::<Vec<_>>();
        let drive_claim = claim.clone();
        // A replay carries the first execution's claim token; if another
        // driver reclaimed these rows meanwhile, the commit cedes instead of
        // dropping the settlement and answering them twice.
        self.journaled_drive_claims.insert(claim.claim_id.clone());
        let result = Box::pin(self.drive_logical_turn(
            LogicalTurnStart::Input(driven, None),
            sinks.events,
            sinks.turn_events,
            root_controller.clone(),
            sinks.local_stop.clone(),
            LogicalTurnClaims::new(Vec::new(), vec![claim]),
            lease,
            stopwatch,
        ))
        .await;
        self.journaled_drive_claims.remove(&drive_claim.claim_id);
        self.admitted_turn_index = None;
        let run = self
            .settle_drive_authority(lease.as_ref(), result)
            .await
            .map_err(abort)?;
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
            queued_drain: None,
        })
    }

    /// Run the session's queued work under `admitted`'s root: the unfinished
    /// queued run the root names, or a new one under it.
    pub(super) async fn run_queued_root(
        &mut self,
        root_controller: &ScopedEffectController<'_>,
        admitted: &Admitted,
        sinks: &DriveSinks<'_>,
    ) -> Result<RootRun, DriveAbort> {
        let root = admitted.root().clone();
        let host = Arc::clone(&self.host.core.control.effect_host);
        let options = root_drain_options(root_controller, host.as_ref(), admitted, sinks)?;
        self.queued_run_driven_inputs.clear();
        let drain = Box::pin(self.stream_next_queued_work(options))
            .await
            .map_err(|error| drive_abort(Some(&root), error))?;
        let driven_inputs = std::mem::take(&mut self.queued_run_driven_inputs);
        let mut run = self.root_run_of_drain(root, drain)?;
        if run.run.is_some() {
            // Accepted input the queued run took into its turn — the turn
            // lane composes in `enqueue_seq` order, so input behind earlier
            // queued work rides that work's run (ADR 0101 §5).
            run.driven_inputs = driven_inputs;
        }
        Ok(run)
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

    /// A queued-work root's outcome from how its drain ended.
    fn root_run_of_drain(
        &mut self,
        root: TurnId,
        drain: crate::runtime::turn_loop::QueuedTurnDrain<crate::AssembledTurn>,
    ) -> Result<RootRun, DriveAbort> {
        if let crate::runtime::turn_loop::QueuedTurnDrain::Replayed(receipt) = &drain
            && let Some(run) = self.drive_root.as_mut()
        {
            run.mark_replayed(receipt);
        }
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
                queued_drain: None,
            },
            crate::runtime::turn_loop::QueuedTurnDrain::Replayed(receipt) => RootRun {
                outcome: match &receipt.terminal {
                    Some(crate::store::QueuedRunTerminal::Completed { outcome, .. }) => {
                        RootOutcome::Committed {
                            root,
                            outcome: outcome.clone(),
                        }
                    }
                    _ => RootOutcome::Ceded { root },
                },
                run: None,
                driven_inputs: Vec::new(),
                queued_drain: Some(crate::runtime::turn_loop::QueuedTurnDrain::Replayed(
                    receipt,
                )),
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
                queued_drain: Some(crate::runtime::turn_loop::QueuedTurnDrain::Empty(reason)),
            },
        })
    }

    pub(super) async fn release_root_lease(&self, lease: Option<&crate::runtime::DriveClaimGuard>) {
        if let Some(lease) = lease
            && let Err(error) = lease.release_if_live().await
        {
            tracing::warn!(%error, "failed to release the session execution lease of a root");
        }
    }

    /// Adopt the head a root's claim admitted it on and pin its recorded turn
    /// index for the prepare phase (FIG-3682).
    ///
    /// The recorded inspection decides whether the resident head may be
    /// rebuilt from the claim's base. A live revalidation may only stop a
    /// root whose claim lost its fence while its handler was down; it cannot
    /// select new work or alter the claim's recorded base.
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
            head,
        } = admitted;
        // The root runs only under the executable generation its claim
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
        let verdict = if matches!(verdict, AdmittedHeadVerdict::Ready)
            && head_moved
            && !store
                .committed_turn_exists(turn_id)
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?
        {
            let open = store
                .list_pending_turn_inputs(&self.state.session_id)
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?;
            if open.iter().any(|read| read.input.input_id == *head) {
                AdmittedHeadVerdict::Diverged {
                    live_revision: self.state.head_revision,
                }
            } else {
                AdmittedHeadVerdict::Ceded
            }
        } else {
            verdict
        };
        match verdict {
            AdmittedHeadVerdict::Ready => {}
            AdmittedHeadVerdict::Ceded => {
                return Err(RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    format!(
                        "accepted turn input `{head}` was answered by another driver while root \
                         `{turn_id}` was down, so this root cedes and commits nothing"
                    ),
                ));
            }
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
    head: &'a crate::InputId,
}

struct InspectAdmittedHeadRunner {
    store: Arc<dyn crate::store::RuntimePersistence>,
    session_id: crate::SessionId,
    root: TurnId,
    head: crate::InputId,
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
                "admitted head inspector was bound to another root or input",
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
            let open = self
                .store
                .list_pending_turn_inputs(&self.session_id)
                .await
                .map_err(store_fault)?;
            if open.iter().any(|read| read.input.input_id == self.head) {
                AdmittedHeadVerdict::Diverged {
                    live_revision: self.live_revision,
                }
            } else {
                AdmittedHeadVerdict::Ceded
            }
        };
        Ok(crate::RuntimeEffectOutcome::InspectAdmittedHead { verdict })
    }
}

/// The drain options a queued-work root runs under: its own drain scope,
/// named by the root.
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
    .with_turn_events(sinks.turn_events)
    .with_admitted_generation(admitted.admitted_generation().clone()))
}

/// Trace attribution for the claim decisions the runner makes.
struct ClaimTrace {
    sink: Option<Arc<dyn lash_trace::TraceSink>>,
    base: lash_trace::TraceContext,
    clock: Arc<dyn crate::Clock>,
    turn_index: usize,
}

/// What the claim probe decided for the head row: either the outcome the
/// journal records (a claim or a refusal) or a race the step retries.
enum RootClaimProbe {
    /// The journaled `ClaimAcceptedTurnInput` outcome.
    Drive(crate::AcceptedTurnInputDrive),
    /// The claim attempt returned no claim yet the head row reads claimable
    /// and held by nobody: claim and read raced, so the step defers to a
    /// later admission instead of journaling a refusal over a row nothing
    /// owns.
    HeadMissedByClaimRace,
}

/// The first execution of an input root's claim.
///
/// Everything it needs is captured at the drive site; none of it enters the
/// envelope, which names only the head input: the fence and owner change with
/// every lease generation, and the envelope must not.
struct RootInputClaimRunner {
    store: Arc<dyn crate::store::RuntimePersistence>,
    effect_host: Arc<dyn crate::EffectHost>,
    scope: crate::AdmittedScope,
    fence: crate::ClaimAuthority,
    owner: crate::LeaseOwnerIdentity,
    session_id: crate::SessionId,
    head: crate::InputId,
    root: TurnId,
    /// The runtime's turn-input claim bound
    /// ([`QueuedWorkBatchingConfig::max_turn_input_claim`](crate::QueuedWorkBatchingConfig::max_turn_input_claim)).
    max_inputs: usize,
    /// The resident head the root is admitted on, as the drive refreshed it
    /// under the lease. Its generation is read in the body.
    base: crate::store::SessionHeadRef,
    /// The root's turn index: the next one after `base`.
    turn_index: usize,
    /// The executable generation the root is admitted under (FIG-3571).
    generation: Option<crate::ExecutableGeneration>,
    trace: ClaimTrace,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for RootInputClaimRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::ClaimAcceptedTurnInput { input_id } = &envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root claim executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *input_id != self.head {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "root claim executor was bound to `{}` but asked to claim `{input_id}`",
                    self.head
                ),
            ));
        }
        // A store that did not answer is this attempt's fault, never the
        // claim's recorded outcome: the admission re-admits this same root
        // first, so a recorded fault would replay under `drive-claim:{root}`
        // on every later drive and wedge the session. Like admission and the
        // seal, the step runs again; only a claim or a refusal is recorded.
        let root = self.root.clone();
        {
            let scoped = self
                .effect_host
                .scoped(self.scope.clone())
                .map_err(crate::RuntimeEffectControllerError::from)?;
            crate::runtime::LashRuntime::defer_orphaned_turn_inputs_before_drain(
                &self.store,
                &self.fence,
                &root,
                &scoped,
                &self.session_id,
                self.effect_host.as_ref(),
            )
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation()
            })?;
        }
        let drive = match self.claim().await.map_err(|err| {
            let mut fault = crate::RuntimeEffectControllerError::from(
                crate::runtime::runtime_error_from_store_commit(err),
            );
            fault.message = format!("root input claim failed: {}", fault.message);
            fault.retryable_uncommitted_derivation()
        })? {
            RootClaimProbe::Drive(drive) => drive,
            // The claim attempt missed the head yet the read shows it
            // claimable and held by nobody: the two raced, so the step
            // defers to a later admission rather than journal a refusal that
            // would cede the root over a row nothing owns (review of #2290).
            RootClaimProbe::HeadMissedByClaimRace => {
                return Err(crate::RuntimeEffectControllerError::new(
                    RuntimeErrorCode::SessionExecutionLaneBusy,
                    format!("root `{root}` missed its head on a claim race"),
                )
                .retryable_uncommitted_derivation());
            }
        };
        Ok(crate::RuntimeEffectOutcome::ClaimAcceptedTurnInput { drive })
    }
}

impl RootInputClaimRunner {
    fn emit(&self, name: &str, payload: serde_json::Value) {
        crate::trace::emit_trace(
            &self.trace.sink,
            &self.trace.base,
            lash_trace::TraceContext::default()
                .for_session(self.session_id.clone())
                .for_turn_index(self.trace.turn_index)
                .for_turn(self.root.clone()),
            lash_trace::TraceEvent::Custom {
                name: name.to_string(),
                payload,
            },
            self.trace.clock.as_ref(),
        );
    }

    /// Claim the accepted next-turn prefix headed by the admitted input.
    ///
    /// The store claims the prefix, retains the base, binds the rows to the
    /// root and records the resulting drive in one transaction, and a later
    /// execution of the same root reads that record back instead of claiming
    /// again while the head is undelivered (FIG-3840). A worker that dies
    /// after the commit but before the journal takes the outcome therefore
    /// leaves nothing to recompute: the successor drives exactly the recorded
    /// composition, base and executable generation, never a prefix widened by
    /// inputs that arrived meanwhile.
    ///
    /// A claim that would not reach the head takes nothing, and the head row
    /// is read without mutating it: held means another driver has it; absent
    /// means it was settled, cancelled, or pruned; pending after a missed
    /// claim means the claim raced, so the step asks to run again rather than
    /// record a refusal. Nothing here ever drops, withdraws, or re-admits a
    /// row.
    async fn claim(self) -> Result<RootClaimProbe, crate::StoreError> {
        let probe = |drive| RootClaimProbe::Drive(drive);
        let request = crate::store::RootInputClaimRequest {
            session_id: self.session_id.clone(),
            lease: self.fence.clone(),
            owner: self.owner.clone(),
            root: self.root.clone(),
            head: self.head.clone(),
            max_inputs: self.max_inputs,
            base: self.base.clone(),
            turn_index: self.turn_index as u64,
            generation: self.generation.clone(),
        };
        if let Some(drive) = self.store.claim_root_inputs(&request).await? {
            if let crate::AcceptedTurnInputDrive::Claimed { claim, .. } = &drive {
                self.emit(
                    "turn_input.claimed",
                    serde_json::json!({
                        "claim_id": &claim.claim_id,
                        "input_ids": claim
                            .inputs
                            .iter()
                            .map(|input| input.input_id.clone())
                            .collect::<Vec<_>>(),
                    }),
                );
            }
            return Ok(probe(drive));
        }
        let open = self
            .store
            .list_pending_turn_inputs(&self.session_id)
            .await?;
        let status = open
            .iter()
            .find(|read| read.input.input_id == self.head)
            .map(|read| read.status.clone());
        Ok(probe(match status {
            Some(crate::PendingTurnInputReadStatus::Pending) => {
                return Ok(RootClaimProbe::HeadMissedByClaimRace);
            }
            Some(_) => crate::AcceptedTurnInputDrive::Refused {
                refusal: crate::AcceptedTurnInputRefusal::HeldByLiveClaim,
            },
            None => crate::AcceptedTurnInputDrive::Refused {
                refusal: crate::AcceptedTurnInputRefusal::SettledOrRemoved,
            },
        }))
    }
}
