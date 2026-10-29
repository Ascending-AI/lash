//! The process activation (ADR 0132 §3, §10, §11; S6 of I0; L6, FIG-5175).
//!
//! Each pass opens the owner's transaction, derives at most one event from
//! committed rows, calls the engine's `advance` with the committed state
//! and that event, and commits the new state with the admission of its
//! action in one `process.advance` transaction before the action runs. A
//! pass that did not commit is recomputed by the next one from the same
//! rows, so `advance` is only ever re-called with the same state and event.
//!
//! Events, in order: a cancel not yet delivered (`Cancelled`, once, with its
//! grace); `Started` before the first transition; the event the last
//! transition asked for at once (`KeyPinned`); a settled step; the end of
//! what the process is blocked on,
//! which for a wait is its row's committed winner (a resolution, a timeout
//! or a revocation), never the live state of what it waited for.
//! With none, the steps run their admitted-execution lifecycle
//! ([`Lifecycle`], shared with a round's members): bodies run, retries are
//! recorded and started, parked steps race their waits, and outcomes
//! commit (`step.outcome`). A committed outcome's plugin-state resolutions
//! publish into the process's namespaces, which the activation's
//! [`StepRuntime`] holds; a new activation's lifecycle publishes them again
//! from the rows. While a body runs the actor stays owned; once
//! only parked waits and retry dues are left, it releases as `waiting`
//! until its earliest due time, holding nothing. The activation adds only
//! the engine-event adapter: a step whose call the fold settled is handed
//! to `advance` as `StepSettled`. The one exception is the resolution of a
//! granted tool's call (P6): lash consumes a resolved match itself, admitting
//! the call of the matched tool in the same commit
//! ([`step_admission`](super::step_admission)), and the engine is answered
//! once, by that call.
//!
//! A parked process, one whose state this node cannot decode, and one that
//! never started end without engine code when cancelled. A running or
//! waiting one receives `Cancelled` once, and lash forces its terminal at
//! `grace_until`. A node claims a process in a format set it does not decode
//! only for its pending cancel, and ends it without reading its state.
//!
//! **Formats.** A process is created in its engine's unstarted set; its
//! first transition stamps the engine's state formats, so from then on only
//! a node that decodes them claims it (ADR 0106 §1).
//!
//! **Drain.** On a draining node a pass starts nothing: it waits for the
//! steps already running to commit their outcomes, then releases the actor
//! `ready` under `drain.release`, for a node of the next build to claim.

#[path = "events.rs"]
mod events;
use events::Next;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_durable::domain::{
    CANCEL_MAIL, ExecRootRow, ProcessActorRow, ProcessWrite, RunFacts, RunSeq, ScopeKey, WaitState,
    WaitWrite,
};
use lash_durable::runner::{Activation, Exit, Owned, Wake};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, DurableReads,
    Release, StoreFailure, StoreFailureKind,
};
use tokio_util::sync::CancellationToken;

use super::driver::{Blocked, Driver, Immediate, InFlight, StoredWaitId};
use super::effects::{record_effect, record_omitted_effects, record_ran};
use super::node_trace::NodeCloses;
use super::session_turn::SessionTurns;
use super::step_admission::{Consumed, Handoff};
use super::step_bodies::StepBodies;
use super::succession::{Current, Succession};
use super::terminal::cancelled;
use super::terminal::record_park;
use super::terminal::record_terminal;
use super::waiting::Ending;
use super::{CascadeProgress, end_scope};
use crate::ActorContext;
use crate::AdmittedScope;
use crate::CancelOrigin;
use crate::runtime::actor::activation_loop;
use crate::runtime::actor::round::lifecycle::{Act, Idle, Lifecycle};
use crate::runtime::actor::round::{self, MemberBodies, PolicyView, RoundError, RunFold};
use crate::runtime::actor::waits;
use crate::runtime::process::steps::{ProcessSteps, StepRefusal, StepRuntime};
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::WaitKind;
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::park::ProcessParkReason;
use lash_execution_contracts::process::CapacityVerdict;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::ProcessEngine;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::StepRequest;
use lash_vocabulary::identity::ProcessId;

/// The activation of claimed process actors: one per node, routed by
/// [`lash_durable::ActorDispatch`].
pub struct ProcessActivation {
    /// What a process's terminal and a park are reported through; a node
    /// given none reports neither.
    pub(super) tracing: Option<crate::trace::TraceRuntime>,
    evidence: lash_core_store::execution_limits::EvidenceLimits,
    pub(super) backend: Backend,
    pub(super) steps: Arc<dyn ProcessSteps>,
    probe: Arc<dyn DurableProbe>,
    /// What runs `SessionTurn` processes; a node given none parks them.
    pub(super) session_turns: Option<Arc<dyn SessionTurns>>,
    /// The watched registry whose sinks and change hub hear what this node's
    /// process commits appended; a node given none publishes nothing.
    events: Option<crate::WatchedRegistry>,
}

impl ProcessActivation {
    /// The activation over `backend`, running process steps through `steps`
    /// and reporting to `probe` (`NoProbe` in production).
    #[must_use]
    pub fn new(
        backend: Backend,
        steps: Arc<dyn ProcessSteps>,
        probe: Arc<dyn DurableProbe>,
    ) -> Self {
        Self {
            backend,
            tracing: None,
            evidence: lash_core_store::execution_limits::EvidenceLimits::standard(),
            steps,
            probe,
            session_turns: None,
            events: None,
        }
    }

    /// This activation reporting through `tracing`: its evidence cuts bound
    /// newly started process runs, a process's committed terminal emits its
    /// `lash.process` completion under the trace scope its registration
    /// retained, and a committed park counts in `lash.parked_work.parks`.
    #[must_use]
    pub fn with_tracing(mut self, tracing: crate::trace::TraceRuntime) -> Self {
        self.evidence = tracing.evidence_limits();
        self.tracing = Some(tracing);
        self
    }

    /// This activation running `SessionTurn` processes with `turns`.
    #[must_use]
    pub fn with_session_turns(mut self, turns: Arc<dyn SessionTurns>) -> Self {
        self.session_turns = Some(turns);
        self
    }

    /// This activation publishing every process event its commits append
    /// to `watched`'s sinks and change hub, after the commit: the host's
    /// view of a durable process's lifecycle and effects. Publication
    /// resumes from the process's durable publication mark, which each
    /// transition's commit, and the ended process's cascade, move to what
    /// `watched` emitted, so a takeover delivers what a dead owner
    /// committed and never published.
    #[must_use]
    pub fn with_process_events(mut self, watched: crate::WatchedRegistry) -> Self {
        watched.read_durable_marks(Arc::clone(self.backend.durable()) as _);
        self.events = Some(watched);
        self
    }

    /// The fleet format the registry's records are written in.
    pub(super) fn fleet(&self) -> crate::FleetFormat {
        crate::FleetFormatStore::fleet_format(self.backend.process_registry().as_ref())
    }
}

/// How a pass ended.
pub(super) enum Pass {
    /// It committed; run another.
    Again,
    /// Steps run and nothing else is due: wait for what their lifecycle
    /// waits on, for mail or for the next due time.
    Wait(Idle, Option<DurableInstant>),
    /// Nothing runs or is held but a step the host lacked capacity for:
    /// back off, then end the activation without committing, so the next
    /// claim counts it against the activation-loop budget and runs the
    /// step again ([`CapacityVerdict`]).
    Backoff(CapacityVerdict),
    /// A durable quiet phase remains resident until mail, due work or eviction.
    Quiet(Option<DurableInstant>),
    /// The actor was released or ended.
    Released,
}

/// How a wait for the steps ended.
enum Waited {
    /// Something happened: run another pass.
    Woke,
    /// The node's lease lapsed under a step's body: stop, recording
    /// nothing.
    Stopping,
}

/// Every process commit enters the same claim coordinator as its step bodies.
pub(super) struct Claim<'a> {
    owned: &'a Owned,
    pub(super) cx: ActorContext,
}
impl std::ops::Deref for Claim<'_> {
    type Target = Owned;
    fn deref(&self) -> &Owned {
        self.owned
    }
}
impl Claim<'_> {
    pub(super) async fn commit(
        &self,
        tx: ActorTx,
        label: CommitLabel,
    ) -> Result<lash_durable::ActorCommit, DurableError> {
        self.cx.commit(tx, label).await
    }
    async fn drain_release(&self, tx: ActorTx) -> Result<lash_durable::ActorCommit, DurableError> {
        self.cx
            .commit_draining(tx, CommitLabel::DRAIN_RELEASE, Release::Ready)
            .await
    }
}

/// What one activation keeps in memory: never a grant, rebuilt from rows
/// by the next owner.
pub(super) struct Live {
    /// The steps' admitted-execution lifecycle: the bodies it runs and the
    /// outcomes it holds until they commit.
    pub(super) lifecycle: Lifecycle,
    /// What the lifecycle's bodies are built from.
    pub(super) bodies: Arc<StepBodies>,
    /// The actor's park as of the claim.
    pub(super) park: Option<String>,
    /// The claims in a row that committed nothing, as of the claim.
    failed_activations: u32,
    first_pass: bool,
    pub(super) idle_since: Option<std::time::Instant>,
    pub(super) evict: bool,
    /// Whether this activation ended the process for a corrupt refusal.
    ended_refused: bool,
    /// The process's durable publication mark as the last pass read it:
    /// where publication starts past this node's own mark. `None` until a
    /// pass read the row.
    published: Option<u64>,
    /// The hard requirements record of the engine state this pass read,
    /// or of the state its last commit wrote: what every rewrite of
    /// `requires` keeps (FIG-6993).
    pub(super) record: lash_vocabulary::claims::ClaimRequirements,
}

impl Live {
    /// Whether a step body of this activation runs, or holds an outcome
    /// not yet committed.
    pub(super) fn busy(&self) -> bool {
        self.lifecycle.busy()
    }
}

pub(super) fn corrupt(what: &str, error: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
        kind: StoreFailureKind::Corrupt,
        message: format!("{what}: {error}"),
    })
}

pub(crate) fn registry_failure(
    error: &lash_execution_contracts::plugin_error::PluginError,
) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
        kind: StoreFailureKind::Unavailable,
        message: error.to_string(),
    })
}

/// How long an activation waits on a capacity verdict whose `retry_after`
/// is `base`, after `failed_activations` claims in a row without progress:
/// `base` doubled for each, at most 1,024 times `base`.
fn capacity_backoff(base: std::time::Duration, failed_activations: u32) -> std::time::Duration {
    base.saturating_mul(1_u32 << failed_activations.min(10))
}

pub(super) fn millis(at: DurableInstant) -> u64 {
    u64::try_from(at.0).unwrap_or_default()
}

#[async_trait::async_trait]
impl Activation for ProcessActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let Ok(process) = ProcessId::parse(owned.actor().id()) else {
            return Exit::Abandoned;
        };
        let exit = Box::pin(self.run(&owned, &process)).await;
        self.forget_published(&owned, &process).await;
        exit
    }
}

impl ProcessActivation {
    async fn run(&self, owned: &Owned, process: &ProcessId) -> Exit {
        let cx = ActorContext::claimed(
            self.backend.clone(),
            owned,
            AdmittedScope::process(process.clone()),
            CancellationToken::new(),
            Arc::clone(&self.probe),
        );
        let _guard = cx.session_claim_guard();
        let claimed = Claim { owned, cx };
        self.run_claim(&claimed, process).await
    }

    async fn run_claim(&self, owned: &Claim<'_>, process: &ProcessId) -> Exit {
        let (park, failed_activations) = match activation_loop::claimed_row(owned).await {
            Ok(snapshot) => (snapshot.park, snapshot.failed_activations),
            Err(exit) => return exit,
        };
        let steps_cx = self.steps_context(owned, process);
        let bodies = Arc::new(StepBodies {
            steps: Arc::clone(&self.steps),
            runtime: Arc::new(StepRuntime::new(steps_cx.clone())),
            seen: Mutex::default(),
        });
        let mut live = Live {
            lifecycle: Lifecycle::new(
                &steps_cx,
                PolicyView::default(),
                Arc::clone(&bodies) as Arc<dyn MemberBodies>,
                CommitLabel::STEP_OUTCOME,
            ),
            bodies,
            park,
            failed_activations,
            first_pass: true,
            idle_since: None,
            evict: false,
            ended_refused: false,
            published: None,
            record: lash_vocabulary::claims::ClaimRequirements::default(),
        };
        let mut retried_conflict = false;
        let mut acknowledged = (
            owned.acknowledged_commits(),
            steps_cx.acknowledged_commits(),
        );
        loop {
            let passed = Box::pin(self.pass(owned, process, &mut live)).await;
            let progress = (
                owned.acknowledged_commits(),
                steps_cx.acknowledged_commits(),
            );
            if progress != acknowledged {
                acknowledged = progress;
                retried_conflict = false;
            }
            self.publish(process, &live).await;
            match passed {
                Ok(Pass::Again) => {
                    live.idle_since = None;
                    owned.busy();
                }
                Ok(Pass::Quiet(due)) => {
                    let since = *live.idle_since.get_or_insert_with(|| owned.clock().now());
                    let clock = Arc::clone(owned.clock());
                    let sleep = async {
                        if let Some(due) = due {
                            let now = i64::try_from(clock.timestamp_ms()).unwrap_or(i64::MAX);
                            clock
                                .sleep(std::time::Duration::from_millis(
                                    u64::try_from(due.0.saturating_sub(now)).unwrap_or(0),
                                ))
                                .await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    };
                    tokio::select! {
                        wake = owned.wait_idle(since) => match wake {
                            Wake::Yield => live.evict = true,
                            Wake::Hint | Wake::Deadline => { live.idle_since = None; owned.busy(); }
                            _ => {}
                        },
                        () = sleep => { live.idle_since = None; owned.busy(); }
                    }
                }
                Ok(Pass::Wait(idle, due)) => match self.wait(owned, &mut live, &idle, due).await {
                    Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                    Ok(Waited::Stopping) => return Exit::Abandoned,
                    Ok(Waited::Woke) | Err(_) => {}
                },
                Ok(Pass::Backoff(verdict)) => {
                    self.back_off(owned, &live, &verdict).await;
                    return Exit::Abandoned;
                }
                Ok(Pass::Released) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(DurableError::ResolutionDiscovery { message }) => {
                    tracing::debug!(%message, process_id = %process, "discovery will retry without a terminal pin");
                    owned.wait_for_mail().await;
                }
                Err(DurableError::ResolutionRevisionConflict { current, .. }) => {
                    // This claim's own resolution acknowledgement advanced the
                    // projection. Recompute the invocation draft from that exact
                    // postimage; an external conflict requires coherent reload.
                    if steps_cx.process_revision().await.ok() != Some(current) {
                        steps_cx.invalidate_process_projection().await;
                    }
                }
                Err(DurableError::RevisionConflict { .. }) => {
                    // Admitted bodies and held outcomes survive one conflict;
                    // the fresh fold reclassifies them by admission identity.
                    steps_cx.invalidate_process_projection().await;
                    steps_cx.discard_conflicted_attempt().await;
                    if retried_conflict {
                        return Exit::Abandoned;
                    }
                    retried_conflict = true;
                }
                // A refusal as corrupt answers every retry the same: the
                // process ends Failed on the first one, and the next pass
                // runs its cascade.
                Err(DurableError::Store(StoreFailure {
                    code: None,
                    kind: StoreFailureKind::Corrupt,
                    message,
                })) => {
                    live.lifecycle.abandon();
                    match self.end_refused(owned, process, &mut live, message).await {
                        Ok(Pass::Again) => {}
                        Ok(_) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                        // Neither its terminal nor its park committed: the
                        // next claim meets the refusal again.
                        Err(_) => return Exit::Abandoned,
                    }
                }
                // Anything else did not commit, or committed with its
                // answer lost: the next pass reloads the rows and carries on
                // from them.
                Err(error) => {
                    tracing::debug!(%error, process_id = %process, "process activation pass failed; reloading");
                    steps_cx.invalidate_process_projection().await;
                    owned.wait_for_mail().await;
                }
            }
        }
    }
}

impl ProcessActivation {
    /// Wait out a capacity verdict before the activation ends: its
    /// `retry_after`, doubled for each claim in a row that made no progress,
    /// or until mail or the node's drain may need the actor sooner. A poll
    /// tick alone does not end it.
    async fn back_off(&self, owned: &Claim<'_>, live: &Live, verdict: &CapacityVerdict) {
        let delay = capacity_backoff(verdict.retry_after, live.failed_activations);
        tracing::debug!(
            actor = %owned.actor(),
            %verdict,
            delay_ms = delay.as_millis(),
            failed_activations = live.failed_activations,
            "process step met a capacity verdict; backing off"
        );
        let clock = Arc::clone(owned.clock());
        let elapsed = clock.sleep(delay);
        tokio::pin!(elapsed);
        loop {
            tokio::select! {
                () = &mut elapsed => return,
                wake = owned.wait_for_mail() => {
                    if !matches!(wake, Wake::Poll) {
                        return;
                    }
                }
            }
        }
    }

    /// Hand the host every event appended to `process`'s log past its
    /// durable publication mark that this node has not published: only
    /// what committed, each once on this node, in sequence order. Nothing
    /// is published until a pass read the mark.
    async fn publish(&self, process: &ProcessId, live: &Live) {
        let (Some(watched), Some(published)) = (&self.events, live.published) else {
            return;
        };
        watched.publish_committed(process, published).await;
    }

    /// Record in `tx` what this node published of `process` past `row`'s
    /// durable mark, which a later owner resumes publication from. Only a
    /// commit that writes the process's row already (a transition) or that
    /// of an ended process (its cascade) carries it: on PostgreSQL an owner
    /// commit locks its actor row before the process row, and a host's
    /// event append the other way round, so a commit that did not lock the
    /// process row, as a release to `waiting`, must not start to.
    pub(super) fn record_published(
        &self,
        tx: &mut ActorTx,
        process: &ProcessId,
        row: &ProcessActorRow,
    ) {
        let Some(watched) = &self.events else {
            return;
        };
        if let Some(mark) = watched.published_mark(process)
            && mark > row.published_event_sequence
        {
            tx.write(DomainWrite::Process(ProcessWrite::Published {
                process: process.clone(),
                through: mark,
            }));
        }
    }

    /// Once the activation ends, drop this node's mark of `process` if the
    /// durable mark covers it; a mark past it stays for the next owner on
    /// this node, or until the next commit records it.
    async fn forget_published(&self, owned: &Owned, process: &ProcessId) {
        let Some(watched) = &self.events else {
            return;
        };
        match owned.store().process(process).await {
            Ok(Some(row)) => {
                watched
                    .forget_published(process, row.published_event_sequence)
                    .await;
            }
            Ok(None) => watched.forget_published(process, u64::MAX).await,
            Err(_) => {}
        }
    }

    /// The context the steps' lifecycle commits and runs bodies under. Its
    /// token is never cancelled: a cancel reaches the steps through the
    /// lifecycle, and the bodies end with the activation that holds them.
    pub(super) fn steps_context(&self, owned: &Claim<'_>, process: &ProcessId) -> ActorContext {
        let _ = process;
        owned.cx.clone()
    }

    /// Wait for what the steps' lifecycle waits on (a body to finish, a
    /// parked wait to end, its batch window or a retry's due time), for
    /// mail or for `due`.
    async fn wait(
        &self,
        owned: &Claim<'_>,
        live: &mut Live,
        idle: &Idle,
        due: Option<DurableInstant>,
    ) -> Result<Waited, DurableError> {
        let clock = Arc::clone(owned.clock());
        let sleep = async {
            match due {
                Some(due) => {
                    let now = i64::try_from(clock.timestamp_ms()).unwrap_or(i64::MAX);
                    let millis = u64::try_from(due.0.saturating_sub(now)).unwrap_or(0);
                    clock.sleep(std::time::Duration::from_millis(millis)).await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            woke = live.lifecycle.wake(idle) => match woke {
                Ok(()) => {}
                // A body found the node's lease lapsed before it started:
                // this owner is stopping, records nothing, and the next one
                // recovers the step from the rows.
                Err(RoundError::Stopped) => return Ok(Waited::Stopping),
                Err(error) => return Err(steps_failure(error)),
            },
            // A draining owner waits for its running steps alone.
            _ = owned.wait_for_mail(), if !owned.draining() => {}
            () = sleep, if !owned.draining() => {}
        }
        Ok(Waited::Woke)
    }

    async fn pass(
        &self,
        owned: &Claim<'_>,
        process: &ProcessId,
        live: &mut Live,
    ) -> Result<Pass, DurableError> {
        let reads: &dyn DurableReads = owned.store().as_ref();
        // Hydrate once per coherent claim. Warm passes refresh only the
        // fenced external frontier; acknowledged own state stays resident.
        let (mut tx, view) = owned.cx.open_process_projection().await?;
        let mut continuation = owned.cx.process_continuation().await?;
        let Some(row) = view.row else {
            // The row is gone: nothing is left to run or to end.
            tx.ack_seen().give_up(Release::Terminal);
            owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
            return Ok(Pass::Released);
        };
        if self.events.is_some() {
            live.published = Some(row.published_event_sequence);
        }
        if row.terminal {
            self.record_published(&mut tx, process, &row);
            return self.cascade(owned, reads, tx, process, &row).await;
        }
        if owned.purpose() == lash_durable::ClaimPurpose::CancelOnly {
            return self
                .end_undecodable(owned, tx, process, live, &view.registry)
                .await;
        }
        let record = continuation.record(process, &view.registry)?;
        owned
            .cx
            .enrich_process(tx.revision(), continuation.clone())
            .await?;
        let cancel = record
            .cancel_request
            .as_deref()
            .map(|request| request.origin);
        // Check the claim before either an engine or a child-turn service
        // runs (FIG-6129). A pending stop goes first, including at the
        // budget: it ends from registry state without entering failing code.
        let at_budget = live.first_pass
            && activation_loop::reached(
                live.failed_activations,
                self.backend.config().settings().activation_loop_budget,
            );
        if let Some(origin) = cancel
            && (live.park.is_some() || row.state_rev == 0 || at_budget)
        {
            return self.end_engine_free(owned, tx, process, live, origin).await;
        }
        if live.park.is_some() {
            // Woken without a cancel or a redrive: stay parked.
            tx.ack_seen().give_up(Release::Parked);
            owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
            return Ok(Pass::Released);
        }
        if at_budget {
            let reason = ProcessParkReason::ActivationLoop {
                failed_activations: live.failed_activations,
            };
            return self.park(owned, tx, &reason).await;
        }
        live.first_pass = false;
        let ProcessInput::Engine { kind, payload, .. } = record.input.as_ref() else {
            // A kernel process (a child session turn) has no engine to
            // advance: its turn is its child session's mail.
            return self
                .session_turn_pass(owned, tx, process, &row, &record, cancel, live)
                .await;
        };
        let engine = self.backend.process_engine(kind).cloned();
        let loaded = match &engine {
            Some(engine) => {
                self.load_state(
                    reads,
                    process,
                    &row,
                    view.root.as_ref(),
                    engine.as_ref(),
                    &mut continuation,
                )
                .await?
            }
            None => Err(ProcessParkReason::UnknownEngine { kind: kind.clone() }),
        };
        // The driver row is decoded with the engine state, before anything
        // of the process runs: one this node does not decode parks the
        // process with its typed reason, and a cancel ends it unread.
        let driver = continuation.driver(row.driver_json.as_deref());
        owned
            .cx
            .enrich_process(tx.revision(), continuation.clone())
            .await?;
        if let Some(origin) = cancel
            && (loaded.is_err() || driver.is_err())
        {
            return self.end_engine_free(owned, tx, process, live, origin).await;
        }
        let (Some(engine), Ok((state, root))) = (engine, loaded) else {
            let reason = match self.backend.process_engine(kind) {
                None => ProcessParkReason::UnknownEngine { kind: kind.clone() },
                Some(engine) => {
                    let format = engine.state_format();
                    ProcessParkReason::UndecodableState {
                        kind: format.kind,
                        version: format.version,
                    }
                }
            };
            return self.park(owned, tx, &reason).await;
        };
        let mut driver = match driver {
            Ok(driver) => driver,
            Err(error) => {
                let reason = ProcessParkReason::UndecodableDriver {
                    message: error.to_string(),
                };
                return self.park(owned, tx, &reason).await;
            }
        };
        let restoring = !continuation.facts_current(tx.revision());
        let (rows, waits) = continuation
            .facts(reads, process, &driver, tx.revision())
            .await?;
        if restoring {
            owned
                .store()
                .begin_process_retained(owned.actor(), owned.epoch(), tx.revision(), 0)
                .await?;
            self.probe.owner_read(
                owned.actor(),
                owned.epoch(),
                lash_durable::OwnerRead::ActiveRunRows,
                rows.rows.len() as u64,
            );
        }
        owned
            .cx
            .enrich_process(tx.revision(), continuation.clone())
            .await?;
        live.record = state.requirements.clone();
        let tx = match self
            .component_format_cut(owned, tx, &record, &row, &driver, live)
            .await?
        {
            Ok(tx) => tx,
            Err(pass) => return Ok(pass),
        };
        // The composition builds before any step is admitted or resumed: one
        // this node cannot build parks the process and settles nothing, so
        // an admitted step is never answered `Interrupted` for it (D3).
        match self.steps.prepare(&live.bodies.runtime, &record).await {
            Ok(()) => {}
            Err(StepRefusal::CompositionUnavailable { reason }) => {
                let reason = ProcessParkReason::CompositionUnavailable { message: reason };
                return self.park(owned, tx, &reason).await;
            }
            Err(StepRefusal::EnvRefused { refusal }) => {
                return self
                    .park(owned, tx, &ProcessParkReason::EnvRefused { refusal })
                    .await;
            }
            Err(refusal) => {
                return Err(DurableError::Store(StoreFailure {
                    code: None,
                    kind: StoreFailureKind::Unavailable,
                    message: refusal.to_string(),
                }));
            }
        }
        let written = state.format.clone();
        let current = Current {
            process,
            row: &row,
            record: &record,
            engine: engine.as_ref(),
            root: root.as_ref(),
            requirements: &state.requirements,
        };
        let mut tx = match self.record_successor(owned, tx, &current, &driver).await? {
            Succession::Ended(pass) => return Ok(pass),
            Succession::AsWritten(tx) => *tx,
        };
        // The store's clock as the open read it: what this pass decides at.
        let now = tx.opened_at();
        if let (Some(origin), Some(until)) = (cancel, driver.grace_until)
            && now.0 >= until
        {
            // The grace ran out: lash ends the process, whatever its steps
            // are doing, and never claims they physically stopped. The waits
            // it still lists were cut short by the cancel.
            live.lifecycle.abandon();
            super::waiting::end(&mut tx, process, &record, Ending::Cancelled);
            record_omitted_effects(&mut tx, process, &driver, self.fleet());
            record_terminal(&mut tx, process, &cancelled(origin, true))?;
            tx.ack_seen();
            self.commit_terminal(owned, tx, process, NodeCloses::default())
                .await?;
            return Ok(Pass::Again);
        }
        let mut policies = Vec::with_capacity(driver.steps.len());
        let mut link_failures = BTreeMap::new();
        for step in driver.steps.values() {
            if let Ok(admission) = self
                .steps
                .admit(
                    &live.bodies.runtime,
                    &record,
                    &step.request,
                    step.target.as_ref(),
                    millis(now),
                )
                .await
            {
                policies.push((admission.tool, admission.policy));
                if let Some(output) = admission.settled {
                    link_failures.insert(step.call.clone(), (&step.request, output));
                }
            }
        }
        if owned.cx.process_revision().await? != tx.revision() {
            return Ok(Pass::Again);
        }
        let policies = PolicyView::new(policies);
        let fold = round::fold(&rows, &policies, &waits)
            .map_err(|error| corrupt("a process's run records", error))?;
        live.lifecycle
            .publish_committed(&fold)
            .map_err(steps_failure)?;
        let environment = self
            .steps
            .requirements(
                &live.bodies.runtime,
                &record,
                &super::step_admission::step_requests(&driver),
            )
            .await?
            .unwrap_or_default();
        let work = super::step_admission::outstanding_work(
            process,
            environment.clone(),
            &live.record,
            &driver,
            Some(&fold),
        );
        live.bodies.saw(&record, &driver, work);
        // A previously admitted call can drift on takeover or before a retry.
        // Settle at its existing ordinal; do not turn the drift into a policy
        // veto, restart a body, or write a new x_start. Admitted waits and
        // bodies already running under this owner's match keep their outcomes.
        // A started call whose policy does not rerun an interrupted attempt
        // (Once) still interrupts: outside work may already have happened.
        let mut settled_links = false;
        for view in fold.rounds() {
            for member in view.members() {
                if member.outcome().is_some()
                    || !member.draft().policy().reruns_interrupted()
                    || matches!(member.state(), round::MemberState::Waiting { .. })
                    || live.lifecycle.holds_execution(&view.id_of(member))
                {
                    continue;
                }
                if let Some((request, output)) = link_failures.remove(member.call()) {
                    let output =
                        self.steps
                            .admitted_output(process, request, member.call(), output);
                    round::settle(&mut tx, &view.execution(member), output, Vec::new(), None)
                        .map_err(|error| corrupt("a drifted process step", error))?;
                    settled_links = true;
                }
            }
        }
        if settled_links {
            live.bodies.stage_commit(&mut tx, owned.epoch())?;
            let appended = RunFacts::appended(tx.domain(), tx.graph(), owned.epoch());
            let committed = owned.commit(tx, CommitLabel::STEP_OUTCOME).await?;
            continuation
                .committed(reads, committed.revision, appended, &driver)
                .await?;
            owned
                .cx
                .enrich_process(committed.revision, continuation.clone())
                .await?;
            return Ok(Pass::Again);
        }
        live.lifecycle.declare(policies);
        live.lifecycle.observe(&fold);
        // A draining node advances nothing more: it waits for the steps
        // that run to commit their outcomes, then hands the process to the
        // next build at this committed phase.
        if owned.draining() {
            match live
                .lifecycle
                .act_in(&fold, now, &tx, false)
                .await
                .map_err(steps_failure)?
            {
                Act::Committed(committed) => {
                    continuation
                        .committed(reads, committed.revision, committed.appended, &driver)
                        .await?;
                    owned
                        .cx
                        .enrich_process(committed.revision, continuation.clone())
                        .await?;
                    return Ok(Pass::Again);
                }
                Act::Refused => return Ok(Pass::Again),
                Act::Idle(idle) if !idle.suspendable() => return Ok(Pass::Wait(idle, None)),
                Act::Idle(_) => {}
            }
            owned.drain_release(tx).await?;
            return Ok(Pass::Released);
        }
        // A successor pair the host asked for is applied once the run is at
        // rest, before anything else of it moves.
        let mut tx = match self
            .apply_recorded_successor(owned, tx, &current, &driver, &state, &fold, live)
            .await?
        {
            Succession::Ended(pass) => return Ok(pass),
            Succession::AsWritten(tx) => *tx,
        };
        // What this pass may hand the engine: the commit that consumes a
        // step's outcome closes its occurrence's node.
        let held = record.trace_nodes.then(|| driver.steps.clone());
        let event = match self
            .next_event(
                &mut tx,
                engine.as_ref(),
                process,
                reads,
                row.state_rev,
                &mut driver,
                &fold,
                record
                    .cancel_request
                    .as_deref()
                    .map(|request| (request.origin, request.requested_at_ms)),
                payload,
                now,
            )
            .await?
        {
            Next::Event(event) => event,
            Next::Committed => {
                owned.commit(tx, CommitLabel::WAIT_TIMEOUT).await?;
                return Ok(Pass::Again);
            }
            Next::Matched(handoff) => {
                let consumed = Consumed {
                    process,
                    row: &row,
                    record: &record,
                    fold: &fold,
                    now,
                };
                return self
                    .hand_off(owned, tx, &consumed, driver, live, *handoff)
                    .await;
            }
            // Nothing for the engine: the steps' lifecycle acts. A commit of
            // its own leaves this transaction unwritten; the next pass reads
            // what it committed.
            Next::Nothing { due } => {
                let idle = match live
                    .lifecycle
                    .act_in(&fold, now, &tx, true)
                    .await
                    .map_err(steps_failure)?
                {
                    Act::Committed(committed) => {
                        continuation
                            .committed(reads, committed.revision, committed.appended, &driver)
                            .await?;
                        owned
                            .cx
                            .enrich_process(committed.revision, continuation.clone())
                            .await?;
                        return Ok(Pass::Again);
                    }
                    Act::Refused => return Ok(Pass::Again),
                    Act::Idle(idle) => idle,
                };
                if !idle.suspendable() {
                    return Ok(Pass::Wait(idle, due));
                }
                if let Some(verdict) = idle.capacity() {
                    return Ok(Pass::Backoff(verdict.clone()));
                }
                // Only durable waits remain. Retain the acknowledged view
                // while quiet until the host's idle or resident-count limit.
                let steps_due = live.lifecycle.due(&idle).await.map_err(steps_failure)?;
                let next_due = due.into_iter().chain(steps_due).min();
                super::waiting::project(&mut tx, process, &record, &driver, &fold, now.0);
                if live.evict
                    || live.idle_since.is_some_and(|since| {
                        owned.clock().now().saturating_duration_since(since)
                            >= self.backend.owner_retention().idle
                    })
                {
                    tx.ack_seen().give_up(Release::Waiting { next_due });
                    owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                    return Ok(Pass::Released);
                }
                if !tx.domain().is_empty() || tx.woken() {
                    tx.ack_seen();
                    owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                }
                return Ok(Pass::Quiet(next_due));
            }
        };
        let nodes = match (&event, &held) {
            (EngineEvent::StepSettled { step, outcome, .. }, Some(held)) => held
                .get(step)
                .map(|consumed| NodeCloses::consumed(&record, &fold, consumed, outcome))
                .unwrap_or_default(),
            _ => NodeCloses::default(),
        };
        if matches!(event, EngineEvent::Started { .. }) {
            driver.effect_occurrence_cap = self.evidence.process_effect_occurrences();
            driver.cancel_grace_ms =
                u64::try_from(engine.cancel_grace().as_millis()).unwrap_or(u64::MAX);
        }
        if matches!(event, EngineEvent::Cancelled { .. }) {
            // Steps admitted before the cancel see their token; any the
            // engine asks for now run within the grace.
            live.lifecycle.cancel_runs_before(RunSeq(driver.next_run));
        }
        // A tool panic is a host terminal, not an event the engine may
        // recover from by resuming its guest or scheduling another step.
        if let EngineEvent::StepSettled { outcome, .. } = &event
            && let Some(output) = outcome
                .payload()
                .and_then(|payload| serde_json::from_str::<crate::ToolCallOutput>(payload).ok())
            && output.tool_panic_stop().is_some()
        {
            super::waiting::end(&mut tx, process, &record, Ending::Resumed);
            record_omitted_effects(&mut tx, process, &driver, self.fleet());
            record_terminal(
                &mut tx,
                process,
                &lash_execution_contracts::process::ProcessOutcome::from_tool_output(output),
            )?;
            tx.ack_seen();
            let nodes = nodes.and(NodeCloses::open(&record, &fold, &driver));
            self.commit_terminal(owned, tx, process, nodes).await?;
            return Ok(Pass::Again);
        }
        let ending = Ending::of(&event);
        let (next, action) = match engine.advance(Arc::unwrap_or_clone(state), event) {
            Ok(transition) => transition,
            Err(error) => {
                let reason = ProcessParkReason::AdvanceRefused {
                    message: error.into_plugin_error().to_string(),
                };
                return self.park(owned, tx, &reason).await;
            }
        };
        let declared = engine.state_format();
        if next.format != declared
            && next.format != written
            && self.backend.formats().process_in(&next.format).is_none()
        {
            let reason = ProcessParkReason::AdvanceRefused {
                message: format!(
                    "engine returned state format {:?}, declared {:?}",
                    next.format, declared
                ),
            };
            return self.park(owned, tx, &reason).await;
        }
        driver.status = self
            .backend
            .status_projection(kind)
            .and_then(|projection| projection.status(&next))
            .map(|status| {
                Box::new(lash_execution_contracts::process::EngineStatus {
                    format: next.format.clone(),
                    status,
                })
            });
        let mut fresh = Vec::new();
        let applied = self
            .apply(
                &live.bodies.runtime,
                &mut tx,
                process,
                &record,
                &mut driver,
                action,
                ending,
                now,
                &mut fresh,
            )
            .await?;
        if owned.cx.process_revision().await? != tx.revision() {
            return Ok(Pass::Again);
        }
        if tx.release() == Some(Release::Parked) {
            owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
            return Ok(Pass::Released);
        }
        if let Some(outcome) = applied {
            super::waiting::end(&mut tx, process, &record, ending);
            record_omitted_effects(&mut tx, process, &driver, self.fleet());
            record_terminal(&mut tx, process, &outcome)?;
            tx.ack_seen();
            let nodes = nodes.and(NodeCloses::open(&record, &fold, &driver));
            self.commit_terminal(owned, tx, process, nodes).await?;
            return Ok(Pass::Again);
        }
        let driver_json = driver.encode();
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: process.clone(),
            expected_rev: row.state_rev,
            driver_json: driver_json.clone(),
        }));
        self.record_published(&mut tx, process, &row);
        // The state this commit writes states the record its `requires`
        // keeps, in the same commit (FIG-6993).
        live.record = next.requirements.clone();
        self.rewrite_requires(&mut tx, &record, &driver, &fold, &fresh, live)
            .await?;
        if (row.state_rev == 0 || next.format != written)
            && let Some(formats) = self.backend.formats().process_in(&next.format)
        {
            // The first transition writes the engine's state: from now on
            // only a node that decodes it claims the process. A transition
            // that leaves the state in another format says so too.
            tx.stamp_formats(formats.clone());
        }
        // The facts this transition appends, before its engine state is
        // staged beside them: the next pass folds them without a read.
        let appended = RunFacts::appended(tx.domain(), tx.graph(), owned.epoch());
        let written =
            super::state_graph::write_engine_state(&mut tx, process, kind, &next, root.as_ref())?;
        let committed = owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        nodes.publish(self.tracing.as_ref(), &record, millis(now));
        continuation
            .committed(reads, committed.revision, appended, &driver)
            .await?;
        continuation.wrote(written, kind, next, driver_json, driver);
        owned
            .cx
            .enrich_process(committed.revision, continuation)
            .await?;
        // The steps this transition admitted run once the next pass folds
        // their rows.
        live.lifecycle.admitted(&fresh);
        Ok(Pass::Again)
    }

    /// The engine state committed with `row`'s revision under `root`, the
    /// root row the pass's view read, which the state's next write
    /// replaces, or why this node cannot decode it.
    async fn load_state(
        &self,
        reads: &dyn DurableReads,
        process: &ProcessId,
        row: &ProcessActorRow,
        root: Option<&ExecRootRow>,
        engine: &dyn ProcessEngine,
        continuation: &mut super::continuation::Continuation,
    ) -> Result<Result<(Arc<EngineState>, Option<ExecRootRow>), ProcessParkReason>, DurableError>
    {
        let format = engine.state_format();
        let Some(root) = root else {
            return Ok(if row.state_rev == 0 {
                Ok((Arc::new(EngineState::empty(format)), None))
            } else {
                Err(ProcessParkReason::UndecodableState {
                    kind: format.kind,
                    version: format.version,
                })
            });
        };
        let (written_by, state) = continuation
            .engine(
                reads,
                process,
                root,
                self.backend.config().settings().graph_reads.engine(),
            )
            .await?;
        if written_by != engine.kind() {
            return Err(corrupt(
                "a process's engine state",
                format!("engine `{written_by}` wrote it, not `{}`", engine.kind()),
            ));
        }
        if state.format != format && self.backend.formats().process_in(&state.format).is_none() {
            return Ok(Err(ProcessParkReason::UndecodableState {
                kind: state.format.kind.clone(),
                version: state.format.version,
            }));
        }
        Ok(Ok((state, Some(root.clone()))))
    }

    pub(super) async fn park(
        &self,
        owned: &Claim<'_>,
        mut tx: ActorTx,
        reason: &ProcessParkReason,
    ) -> Result<Pass, DurableError> {
        tracing::warn!(actor = %owned.actor(), reason = %reason.encode(), "process parked");
        // A cancel-loop park keeps its cancel mail pending: it keeps the
        // process claimable for its cancel by a node that only ends it, once
        // a redrive readies it (FIG-6099).
        if !matches!(reason, ProcessParkReason::CancelLoop { .. }) {
            tx.ack_seen();
        }
        record_park(&mut tx, reason);
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        if let Some(tracing) = &self.tracing {
            crate::operational_metrics::record_work_parked(
                tracing.metrics(),
                Some(&lash_trace::EmissionPermit::new_transition()),
                "process",
                reason.code(),
            );
        }
        Ok(Pass::Released)
    }

    /// End `process` Failed because the store refused its commit as
    /// corrupt; park it when its terminal is refused too, or when a refusal
    /// recurs after it ended.
    async fn end_refused(
        &self,
        owned: &Claim<'_>,
        process: &ProcessId,
        live: &mut Live,
        message: String,
    ) -> Result<Pass, DurableError> {
        tracing::warn!(process_id = %process, %message, "process commit refused as corrupt; ending it");
        if !live.ended_refused {
            live.ended_refused = true;
            let mut tx = owned.begin().await?;
            record_terminal(&mut tx, process, &commit_refused(&message))?;
            tx.ack_seen();
            match self
                .commit_terminal(owned, tx, process, NodeCloses::default())
                .await
            {
                Ok(()) => return Ok(Pass::Again),
                Err(error @ DurableError::OwnershipLost(_)) => return Err(error),
                Err(_) => {}
            }
        }
        let tx = owned.begin().await?;
        self.park(owned, tx, &ProcessParkReason::CommitRefused { message })
            .await
    }

    /// A process claimed in a format set this node does not decode, or with
    /// a claim requirement it does not serve: it was claimed only for its
    /// pending cancel, which ends it from registry state. Without one (the
    /// cancel already ended it, or its mail was something else) it goes
    /// back to idle, read nothing.
    ///
    /// A claim that finds the activation-loop budget's claims in a row
    /// ended with nothing committed parks it `CancelLoop` instead, its
    /// cancel kept (FIG-6099): ending it failed every time.
    async fn end_undecodable(
        &self,
        owned: &Claim<'_>,
        mut tx: ActorTx,
        process: &ProcessId,
        live: &mut Live,
        registry: &lash_durable::domain::ProcessRegistryRow,
    ) -> Result<Pass, DurableError> {
        let first_pass = std::mem::replace(&mut live.first_pass, false);
        let cancel = lash_execution_contracts::process::registry_transitions::stored_record(
            process, registry,
        )
        .map_err(|error| registry_failure(&error))?
        .and_then(|record| {
            record
                .cancel_request
                .as_deref()
                .map(|request| request.origin)
        });
        match cancel {
            Some(_)
                if first_pass
                    && activation_loop::reached(
                        live.failed_activations,
                        self.backend.config().settings().activation_loop_budget,
                    ) =>
            {
                let reason = ProcessParkReason::CancelLoop {
                    failed_activations: live.failed_activations,
                };
                self.park(owned, tx, &reason).await
            }
            Some(origin) => self.end_engine_free(owned, tx, process, live, origin).await,
            None => {
                tx.give_up(Release::Idle);
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                Ok(Pass::Released)
            }
        }
    }

    /// End a cancelled process without calling its engine: it is parked,
    /// undecodable here, or never started.
    pub(super) async fn end_engine_free(
        &self,
        owned: &Claim<'_>,
        mut tx: ActorTx,
        process: &ProcessId,
        live: &mut Live,
        origin: CancelOrigin,
    ) -> Result<Pass, DurableError> {
        let outcome = cancelled(origin, false);
        record_terminal(&mut tx, process, &outcome)?;
        if live.park.take().is_some() {
            tx.write(DomainWrite::ActorPark { reason_json: None });
        }
        tx.ack_seen();
        self.commit_terminal(owned, tx, process, NodeCloses::default())
            .await?;
        Ok(Pass::Again)
    }

    /// One batch of an ended process's cascade, or its release once the
    /// cascade is done.
    async fn cascade(
        &self,
        owned: &Claim<'_>,
        reads: &dyn DurableReads,
        mut tx: ActorTx,
        process: &ProcessId,
        row: &ProcessActorRow,
    ) -> Result<Pass, DurableError> {
        tx.ack_seen();
        let Some(cursor) = row.cascade_cursor.as_deref() else {
            tx.give_up(Release::Terminal);
            owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
            return Ok(Pass::Released);
        };
        let after = (!cursor.is_empty())
            .then(|| ProcessId::parse(cursor))
            .transpose()
            .map_err(|error| corrupt("a process's cascade cursor", error))?;
        let progress = end_scope(
            reads,
            &mut tx,
            &ScopeKey::Process(process.clone()),
            after.as_ref(),
            self.backend.config().settings().cascade_batch,
            CancelOrigin::ParentEnded,
            process.as_str(),
        )
        .await?;
        let done = progress == CascadeProgress::Done;
        if done {
            tx.give_up(Release::Terminal);
        }
        owned.commit(tx, CommitLabel::CASCADE_BATCH).await?;
        Ok(if done { Pass::Released } else { Pass::Again })
    }
}

/// A process ended because the store refused its commit as corrupt.
fn commit_refused(message: &str) -> lash_execution_contracts::process::ProcessOutcome {
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "process_commit_refused",
            message,
        )),
    )
}

/// A step's declaration that could not be read now: the pass commits
/// nothing, and the next one asks again.
pub(super) fn unavailable(reason: String) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
        kind: StoreFailureKind::Unavailable,
        message: reason,
    })
}

pub(super) fn refused(message: String) -> lash_execution_contracts::process::ProcessOutcome {
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
            crate::ToolFailureClass::InvalidRequest,
            "process_action_refused",
            message,
        )),
    )
}

/// The engine-event adapter: the first in-flight step whose call the fold
/// settled, with its payload as the fold checked it, for its `StepSettled`
/// event. Its lifecycle recorded the outcome; the step leaves the driver
/// with the transition that hands it over.
fn settled_step(driver: &mut Driver, fold: &RunFold) -> Option<(InFlight, SettledOutput)> {
    let (name, outcome) = driver.steps.iter().find_map(|(name, step)| {
        let outcome = fold
            .round(RunSeq(step.run))?
            .members()
            .get(usize::try_from(step.member).ok()?)?
            .outcome()?;
        Some((name.clone(), outcome.clone()))
    })?;
    let step = driver.steps.remove(&name)?;
    Some((step, outcome))
}

/// A steps' lifecycle failure as the activation reports it: a store
/// failure as it is, anything else as corrupt rows.
fn steps_failure(error: RoundError) -> DurableError {
    match error {
        RoundError::Durable(error) => error,
        other => corrupt("a process's steps", other),
    }
}

/// A resolved wait's resolution.
/// The row of a process's wait on another process's terminal.
async fn process_wait(
    reads: &dyn DurableReads,
    wait: &StoredWaitId,
) -> Result<lash_durable::domain::WaitRow, DurableError> {
    reads
        .wait(&wait.0)
        .await?
        .ok_or_else(|| corrupt("a process wait", "its row is gone"))
}

fn resolution(row: &lash_durable::domain::WaitRow) -> Result<waits::Resolution, DurableError> {
    let lash_durable::domain::WaitLifecycle::Resolved {
        resolution_ref: stored,
        ..
    } = &row.lifecycle
    else {
        return Err(corrupt("a resolved wait", "it holds no resolution"));
    };
    serde_json::from_str(stored).map_err(|error| corrupt("a wait's resolution", error))
}
