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
//! to `advance` as `StepSettled`. The one exception is a `call_dynamic`
//! call's resolution (P6): lash consumes a resolved match itself, admitting
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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use lash_core_store::tool_run::{AvailableEvidence, CompletionSource};
use lash_durable::domain::{
    CANCEL_MAIL, ExecRootRow, OwnerKey, ProcessActorRow, ProcessWrite, RunSeq, ScopeKey, WaitState,
    WaitWrite,
};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, DurableReads,
    Release, StoreFailure, StoreFailureKind,
};
use tokio_util::sync::CancellationToken;

use super::driver::{Blocked, Driver, Immediate, InFlight, Pinned, StoredWaitId};
use super::effects::{record_effect, record_omitted_effects};
use super::session_turn::SessionTurns;
use super::step_admission::{Admission, Consumed, Handoff, Hold};
use super::succession::{Current, Succession};
use super::terminal::{ProcessParkReason, cancelled, record_park, record_terminal};
use super::{CascadeProgress, end_scope};
use crate::runtime::actor::activation_loop;
use crate::runtime::actor::round::lifecycle::{Act, Idle, Lifecycle};
use crate::runtime::actor::round::{
    self, AdmittedExecution, Material, MemberBodies, MemberBody, PolicyView, RoundError, RunFold,
    SettledOutput,
};
use crate::runtime::actor::waits::{
    self, ParkDeadline, Resolution, WaitDeadline, WaitKind, WaitPurpose, WaitSpec,
};
use crate::runtime::process::engine_state::{EngineAction, EngineEvent, EngineState, StepRequest};
use crate::runtime::process::steps::{ProcessSteps, StepRefusal, StepRuntime};
use crate::{
    ActorContext, AdmittedScope, Backend, CancelOrigin, ProcessEngine, ProcessId, ProcessInput,
    ProcessRecord, ToolCallId,
};

/// The activation of claimed process actors: one per node, routed by
/// [`lash_durable::ActorDispatch`].
pub struct ProcessActivation {
    /// What a process's terminal and a park are reported through; a node
    /// given none reports neither.
    pub(super) tracing: Option<crate::trace::TraceRuntime>,
    trace_limits: lash_trace::TraceLimits,
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
            trace_limits: lash_trace::TraceLimits::standard(),
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
        self.trace_limits = tracing.limits();
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

/// What one activation keeps in memory: never a grant, rebuilt from rows
/// by the next owner.
pub(super) struct Live {
    /// The steps' admitted-execution lifecycle: the bodies it runs and the
    /// outcomes it holds until they commit.
    pub(super) lifecycle: Lifecycle,
    /// What the lifecycle's bodies are built from.
    bodies: Arc<StepBodies>,
    /// The actor's park as of the claim.
    pub(super) park: Option<String>,
    /// The claims in a row that committed nothing, as of the claim.
    failed_activations: u32,
    first_pass: bool,
    /// Whether this activation ended the process for a corrupt refusal.
    ended_refused: bool,
    /// The process's durable publication mark as the last pass read it:
    /// where publication starts past this node's own mark. `None` until a
    /// pass read the row.
    published: Option<u64>,
    /// The `requires` the actor's row holds: claimed, then as written.
    pub(super) requires: Vec<String>,
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
        kind: StoreFailureKind::Corrupt,
        message: format!("{what}: {error}"),
    })
}

pub(super) fn registry_failure(error: &crate::PluginError) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: error.to_string(),
    })
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
        let exit = self.run(&owned, &process).await;
        self.forget_published(&owned, &process).await;
        exit
    }
}

impl ProcessActivation {
    async fn run(&self, owned: &Owned, process: &ProcessId) -> Exit {
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
            ended_refused: false,
            published: None,
            requires: owned.requires().to_vec(),
        };
        loop {
            let passed = self.pass(owned, process, &mut live).await;
            self.publish(process, &live).await;
            match passed {
                Ok(Pass::Again) => {}
                Ok(Pass::Wait(idle, due)) => match self.wait(owned, &mut live, &idle, due).await {
                    Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                    Ok(Waited::Stopping) => return Exit::Abandoned,
                    Ok(Waited::Woke) | Err(_) => {}
                },
                Ok(Pass::Released) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                // A refusal as corrupt answers every retry the same: the
                // process ends Failed on the first one, and the next pass
                // runs its cascade.
                Err(DurableError::Store(StoreFailure {
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
                    owned.wait_for_mail().await;
                }
            }
        }
    }
}

impl ProcessActivation {
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
    fn steps_context(&self, owned: &Owned, process: &ProcessId) -> ActorContext {
        ActorContext::claimed(
            self.backend.clone(),
            owned,
            AdmittedScope::process(process.clone()),
            CancellationToken::new(),
            Arc::clone(&self.probe),
        )
    }

    /// Wait for what the steps' lifecycle waits on (a body to finish, a
    /// parked wait to end, its batch window or a retry's due time), for
    /// mail or for `due`.
    async fn wait(
        &self,
        owned: &Owned,
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
        owned: &Owned,
        process: &ProcessId,
        live: &mut Live,
    ) -> Result<Pass, DurableError> {
        let reads: &dyn DurableReads = owned.store().as_ref();
        let mut tx = owned.begin().await?;
        let Some(row) = reads.process(process).await? else {
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
            return self.end_undecodable(owned, tx, process, live).await;
        }
        let record = self
            .backend
            .process_registry()
            .get_process(process)
            .await
            .map_err(|error| registry_failure(&error))?
            .ok_or_else(|| corrupt("a live process actor", "its registry row is gone"))?;
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
                self.load_state(reads, process, &row, engine.as_ref())
                    .await?
            }
            None => Err(ProcessParkReason::UnknownEngine { kind: kind.clone() }),
        };
        // The driver row is decoded with the engine state, before anything
        // of the process runs: one this node does not decode parks the
        // process with its typed reason, and a cancel ends it unread.
        let driver = Driver::decode(row.driver_json.as_deref());
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
        // The composition builds before any step is admitted or resumed: one
        // this node cannot build parks the process and settles nothing, so
        // an admitted step is never answered `Interrupted` for it (D3).
        match self.steps.prepare(&live.bodies.runtime, &record).await {
            Ok(()) => {}
            Err(StepRefusal::CompositionUnavailable { component, reason }) => {
                let reason = ProcessParkReason::CompositionUnavailable {
                    component,
                    message: reason,
                };
                return self.park(owned, tx, &reason).await;
            }
            Err(refusal) => {
                return Err(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: refusal.to_string(),
                }));
            }
        }
        let written = state.format.clone();
        let tx = match self
            .carry_forward(
                owned,
                tx,
                process,
                &row,
                &driver,
                engine.as_ref(),
                &state,
                root.as_ref(),
            )
            .await?
        {
            super::carry::Carry::Ended(pass) => return Ok(pass),
            super::carry::Carry::AsWritten(tx) => *tx,
        };
        let current = Current {
            process,
            row: &row,
            record: &record,
            engine: engine.as_ref(),
            root: root.as_ref(),
        };
        let mut tx = match self.record_successor(owned, tx, &current, &driver).await? {
            Succession::Ended(pass) => return Ok(pass),
            Succession::AsWritten(tx) => *tx,
        };
        let now = owned.store().now().await?;
        if let (Some(origin), Some(until)) = (cancel, driver.grace_until)
            && now.0 >= until
        {
            // The grace ran out: lash ends the process, whatever its steps
            // are doing, and never claims they physically stopped.
            live.lifecycle.abandon();
            record_omitted_effects(&mut tx, process, &driver, self.fleet());
            record_terminal(&mut tx, process, &cancelled(origin, true))?;
            tx.ack_seen();
            self.commit_terminal(owned, tx, process).await?;
            return Ok(Pass::Again);
        }
        let rows = reads.run_facts(&OwnerKey::Process(process.clone())).await?;
        let mut policies = Vec::with_capacity(driver.steps.len());
        for step in driver.steps.values() {
            if let Ok(admission) = self.steps.admit(&record, &step.request, millis(now)).await {
                policies.push((admission.tool, admission.policy));
            }
        }
        let policies = PolicyView::new(policies);
        let waits = round::PinnedWaits::read(reads, &rows).await?;
        let fold = round::fold(&rows, &policies, &waits)
            .map_err(|error| corrupt("a process's run records", error))?;
        live.lifecycle.declare(policies);
        live.bodies.saw(&record, &driver);
        // A draining node advances nothing more: it waits for the steps
        // that run to commit their outcomes, then hands the process to the
        // next build at this committed phase.
        if owned.draining() {
            match live.lifecycle.drain(&fold).await.map_err(steps_failure)? {
                Act::Committed(_) | Act::Refused => return Ok(Pass::Again),
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
        let event = match self
            .next_event(
                &mut tx,
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
                let idle = match live.lifecycle.act(&fold).await.map_err(steps_failure)? {
                    Act::Committed(_) | Act::Refused => return Ok(Pass::Again),
                    Act::Idle(idle) => idle,
                };
                if !idle.suspendable() {
                    return Ok(Pass::Wait(idle, due));
                }
                // Only rows are left to wait on: release as `waiting` until
                // the earliest of them, holding nothing.
                let steps_due = live.lifecycle.due(&idle).await.map_err(steps_failure)?;
                let next_due = due.into_iter().chain(steps_due).min();
                super::waiting::project(&mut tx, process, &record, &driver, &fold, now.0);
                tx.ack_seen().give_up(Release::Waiting { next_due });
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Pass::Released);
            }
        };
        if matches!(event, EngineEvent::Started { .. }) {
            driver.effect_occurrence_cap = self
                .trace_limits
                .process_effect_occurrences
                .min(crate::runtime::process::PROCESS_EFFECT_OCCURRENCE_CAP);
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
            super::waiting::end(&mut tx, process, &record);
            record_omitted_effects(&mut tx, process, &driver, self.fleet());
            record_terminal(
                &mut tx,
                process,
                &crate::ProcessOutcome::from_tool_output(output),
            )?;
            tx.ack_seen();
            self.commit_terminal(owned, tx, process).await?;
            return Ok(Pass::Again);
        }
        let (next, action) = match engine.advance(state, event) {
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
        let mut fresh = Vec::new();
        let applied = self
            .apply(
                &mut tx,
                process,
                &record,
                &mut driver,
                action,
                now,
                &mut fresh,
            )
            .await?;
        if let Some(outcome) = applied {
            super::waiting::end(&mut tx, process, &record);
            record_omitted_effects(&mut tx, process, &driver, self.fleet());
            record_terminal(&mut tx, process, &outcome)?;
            tx.ack_seen();
            self.commit_terminal(owned, tx, process).await?;
            return Ok(Pass::Again);
        }
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: process.clone(),
            expected_rev: row.state_rev,
            driver_json: driver.encode(),
        }));
        self.record_published(&mut tx, process, &row);
        let requires = self
            .rewrite_requires(&mut tx, &record, &driver, &fold, &fresh, live)
            .await?;
        if (row.state_rev == 0 || next.format != written)
            && let Some(formats) = self.backend.formats().process_in(&next.format)
        {
            // The first transition writes the engine's state: from now on
            // only a node that decodes it claims the process. A transition
            // that leaves the state in another format says so too.
            tx.stamp_formats(formats.clone());
        }
        super::state_graph::write_engine_state(&mut tx, process, kind, &next, root.as_ref())?;
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        if let Some(requires) = requires {
            live.requires = requires;
        }
        // The steps this transition admitted run once the next pass folds
        // their rows.
        live.lifecycle.admitted(&fresh);
        Ok(Pass::Again)
    }

    /// The engine state committed with `row`'s revision, and the root row
    /// it replaces, or why this node cannot decode it.
    async fn load_state(
        &self,
        reads: &dyn DurableReads,
        process: &ProcessId,
        row: &ProcessActorRow,
        engine: &dyn ProcessEngine,
    ) -> Result<Result<(EngineState, Option<ExecRootRow>), ProcessParkReason>, DurableError> {
        let format = engine.state_format();
        let carried = self
            .backend
            .state_migration(&format.kind)
            .map(|migration| migration.carries())
            .unwrap_or_default();
        let Some((root, written_by, state)) = super::state_graph::read_engine_state(
            reads,
            process,
            self.backend.config().settings().graph_reads.engine(),
        )
        .await?
        else {
            return Ok(if row.state_rev == 0 {
                Ok((EngineState::empty(format), None))
            } else {
                Err(ProcessParkReason::UndecodableState {
                    kind: format.kind,
                    version: format.version,
                })
            });
        };
        if written_by != engine.kind() {
            return Err(corrupt(
                "a process's engine state",
                format!("engine `{written_by}` wrote it, not `{}`", engine.kind()),
            ));
        }
        if state.format != format && !carried.contains(&state.format) {
            return Ok(Err(ProcessParkReason::UndecodableState {
                kind: state.format.kind,
                version: state.format.version,
            }));
        }
        Ok(Ok((state, Some(root))))
    }

    pub(super) async fn park(
        &self,
        owned: &Owned,
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
        owned: &Owned,
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
            match self.commit_terminal(owned, tx, process).await {
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
        owned: &Owned,
        mut tx: ActorTx,
        process: &ProcessId,
        live: &mut Live,
    ) -> Result<Pass, DurableError> {
        let first_pass = std::mem::replace(&mut live.first_pass, false);
        let cancel = self
            .backend
            .process_registry()
            .get_process(process)
            .await
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
        owned: &Owned,
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
        self.commit_terminal(owned, tx, process).await?;
        Ok(Pass::Again)
    }

    /// One batch of an ended process's cascade, or its release once the
    /// cascade is done.
    async fn cascade(
        &self,
        owned: &Owned,
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

/// What the next event is.
#[expect(
    clippy::large_enum_variant,
    reason = "one per pass, moved straight into the engine call"
)]
enum Next {
    Event(EngineEvent),
    /// A due wait was settled on the transaction: commit it and look again,
    /// since a resolution committed first wins.
    Committed,
    /// Nothing happened; the earliest due time.
    Nothing {
        due: Option<DurableInstant>,
    },
    /// A `call_dynamic` call's resolution committed its match: lash admits
    /// the call of the matched tool in its place, without the engine.
    Matched(Box<Handoff>),
}

impl ProcessActivation {
    #[expect(
        clippy::too_many_arguments,
        reason = "the rows one event is derived from"
    )]
    async fn next_event(
        &self,
        tx: &mut ActorTx,
        process: &ProcessId,
        reads: &dyn DurableReads,
        state_rev: u64,
        driver: &mut Driver,
        fold: &RunFold,
        cancel: Option<(CancelOrigin, u64)>,
        payload: &serde_json::Value,
        now: DurableInstant,
    ) -> Result<Next, DurableError> {
        if let Some((origin, requested_at_ms)) = cancel
            && driver.grace_until.is_none()
        {
            // The grace runs from the committed request, so a transition
            // recomputed after a failed commit sees the same event.
            let grace_until = DurableInstant(i64::try_from(requested_at_ms).unwrap_or(i64::MAX))
                .after_millis(i64::try_from(driver.cancel_grace_ms).unwrap_or(i64::MAX));
            driver.grace_until = Some(grace_until.0);
            if let Some(seq) = tx
                .mail()
                .iter()
                .filter(|mail| mail.kind.as_str() == CANCEL_MAIL)
                .map(|mail| mail.seq)
                .max()
            {
                tx.ack_through(seq);
            }
            return Ok(Next::Event(EngineEvent::Cancelled {
                origin,
                grace_until,
            }));
        }
        if state_rev == 0 {
            return Ok(Next::Event(EngineEvent::Started {
                payload: payload.clone(),
            }));
        }
        if let Some(immediate) = driver.immediate.take() {
            return Ok(Next::Event(match immediate {
                Immediate::KeyPinned { name } => {
                    let pinned = driver.keys.get(&name).ok_or_else(|| {
                        corrupt("a process's pinned key", "the driver names no such key")
                    })?;
                    EngineEvent::KeyPinned {
                        name,
                        key: waits::PinnedKey::new(pinned.key.clone()),
                    }
                }
            }));
        }
        if let Some((step, outcome)) = settled_step(driver, fold) {
            // A `call_dynamic` call's committed resolution is consumed by
            // lash (P6): a resolved match hands off to its target, and a
            // refused or unavailable one answers the engine, typed, from
            // the recorded decision.
            let matched = step
                .request
                .resolves()
                .then(|| crate::runtime::process::steps::resolved_match(&outcome))
                .flatten();
            if let Some(matched) = matched {
                if let Some(decision) =
                    crate::runtime::process::steps::dynamic_decision(process, &matched)
                {
                    record_effect(tx, process, driver, &step, &decision, self.fleet());
                    return Ok(Next::Event(EngineEvent::StepSettled {
                        call_id: Some(step.call.clone()),
                        step: step.request.step().clone(),
                        outcome: decision,
                    }));
                }
                if let Some(target) =
                    crate::runtime::process::steps::dynamic_target(&step.request, matched)
                {
                    return Ok(Next::Matched(Box::new(Handoff {
                        resolution: step,
                        target,
                    })));
                }
                return Err(corrupt(
                    "a call_dynamic resolution",
                    "its input is not a call path with its arguments",
                ));
            }
            let outcome = self.steps.engine_output(process, &step.request, outcome);
            record_effect(tx, process, driver, &step, &outcome, self.fleet());
            return Ok(Next::Event(EngineEvent::StepSettled {
                call_id: match &step.request {
                    StepRequest::Tool { .. } => Some(step.call.clone()),
                    StepRequest::Engine { .. } => None,
                },
                step: step.request.step().clone(),
                outcome,
            }));
        }
        let mut due = driver
            .grace_until
            .map(DurableInstant)
            .into_iter()
            .collect::<Vec<_>>();
        match driver.blocked.clone() {
            None | Some(Blocked::Idle) => {}
            Some(Blocked::Sleep { until, .. }) => {
                if now.0 >= until {
                    driver.blocked = None;
                    return Ok(Next::Event(EngineEvent::Woke));
                }
                due.push(DurableInstant(until));
            }
            Some(Blocked::External { name, wait, .. }) => {
                let row = reads
                    .wait(&wait.0)
                    .await?
                    .ok_or_else(|| corrupt("a pinned key's wait", "its row is gone"))?;
                match row.lifecycle.state() {
                    WaitState::Resolved => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ExternalResolved {
                            name,
                            resolution: resolution(&row)?,
                        }));
                    }
                    WaitState::TimedOut | WaitState::Revoked => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ExternalTimedOut { name }));
                    }
                    WaitState::Pending => match row.purpose.deadline() {
                        Some(deadline) if now >= deadline => {
                            tx.write(DomainWrite::Wait(WaitWrite::Due { id: wait.0 }));
                            return Ok(Next::Committed);
                        }
                        Some(deadline) => due.push(deadline),
                        None => {}
                    },
                }
            }
            Some(Blocked::Process {
                process: target,
                wait,
                ..
            }) => {
                // The wait row's committed winner is the answer, never the
                // target's live registry row: a timeout that committed first
                // stays a timeout, and a resolution outlives the target's
                // pruning.
                let mut row = process_wait(reads, &wait).await?;
                if row.lifecycle.state() == WaitState::Pending {
                    // A target that ended before the wait was pinned
                    // resolved no wait: resolve it from the target's
                    // recorded end, first winner, and read the winner.
                    waits::resolve_ended_terminal(
                        &self.backend,
                        &waits::WaitRef::new(wait.0, WaitKind::ProcessTerminal),
                    )
                    .await?;
                    row = process_wait(reads, &wait).await?;
                }
                match row.lifecycle.state() {
                    WaitState::Resolved => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ProcessEnded {
                            process: target,
                            outcome: waits::process_outcome(resolution(&row)?)?,
                        }));
                    }
                    WaitState::TimedOut | WaitState::Revoked => {
                        driver.blocked = None;
                        return Ok(Next::Event(EngineEvent::ProcessWaitTimedOut {
                            process: target,
                        }));
                    }
                    WaitState::Pending => match row.purpose.deadline() {
                        Some(deadline) if now >= deadline => {
                            tx.write(DomainWrite::Wait(WaitWrite::Due { id: wait.0 }));
                            return Ok(Next::Committed);
                        }
                        Some(deadline) => due.push(deadline),
                        None => {}
                    },
                }
            }
        }
        Ok(Next::Nothing {
            due: due.into_iter().min(),
        })
    }

    /// Record `action`'s admission on `tx` and its effect on `driver`.
    /// Answers the terminal to commit instead when the action ends the
    /// process or its admission is refused.
    #[expect(
        clippy::too_many_arguments,
        reason = "the transition and the rows its admission is recorded against"
    )]
    async fn apply(
        &self,
        tx: &mut ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        driver: &mut Driver,
        action: EngineAction,
        now: DurableInstant,
        fresh: &mut Vec<AdmittedExecution>,
    ) -> Result<Option<crate::ProcessOutcome>, DurableError> {
        let ProcessInput::Engine { .. } = record.input.as_ref() else {
            return Err(corrupt("an advanced process", "it runs no engine"));
        };
        // An engine wait is bounded by the engine's own bound, fixed now.
        let deadline = |bound| ParkDeadline::admitted(bound, now).deadline();
        driver.blocked = None;
        match action {
            EngineAction::Steps {
                steps: requests,
                wake,
            } => {
                // A recorded match is lash's to record, at the commit that
                // consumes its resolution: an engine asks for the
                // `call_dynamic` effect, never for a match.
                if requests.iter().any(|request| request.matched().is_some()) {
                    return Ok(Some(refused(
                        "a step names a recorded match, which only lash records".to_owned(),
                    )));
                }
                match self
                    .admit_steps(
                        tx,
                        process,
                        record,
                        driver,
                        requests,
                        now,
                        fresh,
                        Hold::Counted,
                    )
                    .await?
                {
                    Admission::Admitted => {}
                    Admission::Ended(outcome) => return Ok(Some(outcome)),
                    Admission::Refused(refusal) => {
                        return Ok(Some(refused(refusal.to_string())));
                    }
                }
                driver.blocked = wake.map(|until| Blocked::Sleep {
                    until: until.0,
                    site: None,
                });
            }
            EngineAction::PinKey { name, bound } => {
                let (wait, key) = waits::pin(
                    tx,
                    WaitSpec {
                        scope: ScopeKey::Process(process.clone()),
                        purpose: WaitPurpose::EngineKey {
                            name: name.0.clone(),
                            deadline: deadline(bound).map(WaitDeadline::at),
                        },
                    },
                );
                let key = key.ok_or_else(|| corrupt("a pinned key", "no key was minted"))?;
                driver.keys.insert(
                    name.clone(),
                    Pinned {
                        wait: StoredWaitId(wait.id()),
                        key: key.as_str().to_owned(),
                    },
                );
                driver.immediate = Some(Immediate::KeyPinned { name });
            }
            EngineAction::AwaitExternal { name, site } => {
                let Some(pinned) = driver.keys.get(&name) else {
                    return Ok(Some(refused(format!("key `{}` was never pinned", name.0))));
                };
                driver.blocked = Some(Blocked::External {
                    name,
                    wait: pinned.wait,
                    site,
                });
            }
            EngineAction::AwaitProcess {
                process: target,
                bound,
                site,
            } => {
                let (wait, _) = waits::pin(
                    tx,
                    WaitSpec {
                        scope: ScopeKey::Process(process.clone()),
                        purpose: WaitPurpose::ProcessTerminal {
                            process: target.clone(),
                            deadline: deadline(bound).map(WaitDeadline::at),
                        },
                    },
                );
                driver.blocked = Some(Blocked::Process {
                    process: target,
                    wait: StoredWaitId(wait.id()),
                    site,
                });
            }
            EngineAction::Sleep { until, site } => {
                driver.blocked = Some(Blocked::Sleep {
                    until: until.0,
                    site,
                });
            }
            EngineAction::Idle => driver.blocked = Some(Blocked::Idle),
            EngineAction::Terminal(outcome) => return Ok(Some(outcome)),
        }
        super::waiting::settle(tx, process, record, driver);
        Ok(None)
    }
}

/// A process ended because the store refused its commit as corrupt.
fn commit_refused(message: &str) -> crate::ProcessOutcome {
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::Internal,
            "process_commit_refused",
            message,
        ),
    ))
}

/// A step's declaration that could not be read now: the pass commits
/// nothing, and the next one asks again.
pub(super) fn unavailable(reason: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: reason,
    })
}

pub(super) fn refused(message: String) -> crate::ProcessOutcome {
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::InvalidRequest,
            "process_action_refused",
            message,
        ),
    ))
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

/// The bodies of a process's steps, from the host's [`ProcessSteps`]: what
/// the lifecycle runs, for the process record and the in-flight steps as
/// the last pass read them.
pub(super) struct StepBodies {
    steps: Arc<dyn ProcessSteps>,
    /// What step bodies run under: the process actor's claimed context, and
    /// the process's namespaces, which this activation's committed step
    /// outcomes publish into.
    runtime: Arc<StepRuntime>,
    seen: Mutex<SeenSteps>,
}

/// The process and its in-flight steps' requests, by call, as a pass read
/// them.
#[derive(Default)]
struct SeenSteps {
    record: Option<Arc<ProcessRecord>>,
    requests: BTreeMap<ToolCallId, StepRequest>,
}

impl StepBodies {
    /// Note `record` and `driver`'s in-flight steps, as this pass read them.
    fn saw(&self, record: &ProcessRecord, driver: &Driver) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.record = Some(Arc::new(record.clone()));
        seen.requests = driver
            .steps
            .values()
            .map(|step| (step.call.clone(), step.request.clone()))
            .collect();
    }

    /// The process and the request of the step `call` names.
    fn step(&self, call: &ToolCallId) -> Option<(Arc<ProcessRecord>, StepRequest)> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        Some((
            Arc::clone(seen.record.as_ref()?),
            seen.requests.get(call)?.clone(),
        ))
    }
}

fn unknown_step() -> SettledOutput {
    SettledOutput::Cancelled {
        evidence: AvailableEvidence::default(),
    }
}

impl MemberBodies for StepBodies {
    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.step(execution.call()) {
            Some((record, step)) => self.steps.body(&self.runtime, &record, &step, execution),
            // The lifecycle runs only admitted steps; a body for any other
            // is never asked for. Answer as a stop rather than run anything.
            None => Box::new(|_| Box::pin(async { unknown_step().into() })),
        }
    }

    fn stop_grace(&self) -> std::time::Duration {
        self.steps.stop_grace()
    }

    fn resolved(
        &self,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        match self.step(execution.call()) {
            Some((record, step)) => self
                .steps
                .resolved(&record, &step, execution, parked, resolution),
            None => unknown_step(),
        }
    }

    fn publish_state(
        &self,
        state: &[crate::plugin::StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.runtime.publish_state(state)
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
