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
//! transition asked for at once (`KeyPinned`, `Emitted`); a settled step;
//! a signal from the mailbox; the end of what the process is blocked on,
//! which for a wait is its row's committed winner (a resolution, a timeout
//! or a revocation), never the live state of what it waited for.
//! With none, step bodies run (the actor stays owned) or the actor releases
//! as `waiting` until its earliest due time, holding nothing.
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

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core_store::tool_run::{MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole};
use lash_durable::domain::{
    AdmittedId, CANCEL_MAIL, ExecKey, OwnerKey, ParkEventWrite, ProcessActorRow, ProcessWrite,
    RunSeq, SIGNAL_MAIL, ScopeKey, SnapshotRev, SnapshotWrite, WaitState, WaitWrite,
};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, DurableReads,
    Release, StoreFailure, StoreFailureKind,
};
use lash_sansio::{ToolCallAdmission, ToolCallPosition};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::driver::{Blocked, Driver, Immediate, InFlight, Pinned, StoredWaitId};
use super::session_turn::SessionTurns;
use super::terminal::{ProcessParkReason, cancelled, record_park, record_terminal};
use super::{CascadeProgress, end_scope};
use crate::runtime::actor::round::{
    self, AdmittedExecution, BodyOutput, ExecutionDraft, PolicyView, Recovery, RunFold,
};
use crate::runtime::actor::waits::{self, WaitDeadline, WaitKind, WaitSpec};
use crate::runtime::process::engine_state::{
    EngineAction, EngineEvent, EngineState, HostWaitKind, SettledOutcome, StepName,
};
use crate::runtime::process::steps::ProcessSteps;
use crate::{
    ActorContext, AdmittedScope, Backend, CancelOrigin, ProcessEngine, ProcessId, ProcessInput,
    ProcessRecord, ProcessSignal,
};

/// The activation of claimed process actors: one per node, routed by
/// [`lash_durable::ActorDispatch`].
pub struct ProcessActivation {
    pub(super) backend: Backend,
    steps: Arc<dyn ProcessSteps>,
    probe: Arc<dyn DurableProbe>,
    /// What runs `SessionTurn` processes; a node given none parks them.
    pub(super) session_turns: Option<Arc<dyn SessionTurns>>,
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
            steps,
            probe,
            session_turns: None,
        }
    }

    /// This activation running `SessionTurn` processes with `turns`.
    #[must_use]
    pub fn with_session_turns(mut self, turns: Arc<dyn SessionTurns>) -> Self {
        self.session_turns = Some(turns);
        self
    }
}

/// How a pass ended.
pub(super) enum Pass {
    /// It committed; run another.
    Again,
    /// Steps run and nothing else is due: wait for one to finish, for mail
    /// or for the next due time. The fold is of the rows as they stand: the
    /// pass committed nothing after reading them.
    Wait(RunFold, Option<DurableInstant>),
    /// The actor was released or ended.
    Released,
}

/// What one activation keeps in memory: never a grant, rebuilt from rows
/// by the next owner.
pub(super) struct Live {
    /// The context step bodies run under; its token is the steps' cancel.
    steps_cx: ActorContext,
    steps_token: CancellationToken,
    running: JoinSet<(
        StepName,
        AdmittedId,
        Result<Option<BodyOutput>, DurableError>,
    )>,
    /// The step each running body's task runs, to release a body whose
    /// task ended without an output.
    tasks: std::collections::HashMap<tokio::task::Id, StepName>,
    started: BTreeSet<StepName>,
    /// The executions the last committed transition admitted, by step: a
    /// step admitted earlier is rebuilt from the run records' fold.
    fresh: std::collections::BTreeMap<StepName, AdmittedExecution>,
    /// The actor's park as of the claim.
    park: Option<String>,
    /// The claims in a row that committed nothing, as of the claim.
    failed_activations: u32,
    first_pass: bool,
    /// Whether this activation ended the process for a corrupt refusal.
    ended_refused: bool,
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

fn millis(at: DurableInstant) -> u64 {
    u64::try_from(at.0).unwrap_or_default()
}

#[async_trait::async_trait]
impl Activation for ProcessActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let Ok(process) = ProcessId::parse(owned.actor().id()) else {
            return Exit::Abandoned;
        };
        // A failed read leaves the claim as it was: read again at the
        // activation's retry pace while this node holds the actor. A
        // draining node hands it back instead.
        let (park, failed_activations) = loop {
            match owned.store().actor(owned.actor()).await {
                Ok(Some(snapshot)) => break (snapshot.park, snapshot.failed_activations),
                Ok(None) => return Exit::Released,
                Err(_) if owned.draining() => return Exit::Abandoned,
                Err(error) => {
                    tracing::debug!(%error, %process, "process actor read failed; reading again");
                    owned.wait_for_mail().await;
                }
            }
        };
        let steps_token = CancellationToken::new();
        let mut live = Live {
            steps_cx: self.steps_context(&owned, &process, steps_token.clone()),
            steps_token,
            running: JoinSet::new(),
            tasks: std::collections::HashMap::new(),
            started: BTreeSet::new(),
            fresh: std::collections::BTreeMap::new(),
            park,
            failed_activations,
            first_pass: true,
            ended_refused: false,
        };
        loop {
            match self.pass(&owned, &process, &mut live).await {
                Ok(Pass::Again) => {}
                Ok(Pass::Wait(fold, due)) => {
                    if let Err(DurableError::OwnershipLost(_)) =
                        self.wait(&owned, &mut live, &fold, due).await
                    {
                        return Exit::Released;
                    }
                }
                Ok(Pass::Released) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                // A refusal as corrupt answers every retry the same: the
                // process ends Failed on the first one, and the next pass
                // runs its cascade.
                Err(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Corrupt,
                    message,
                })) => {
                    live.running.abort_all();
                    match self.end_refused(&owned, &process, &mut live, message).await {
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
                    tracing::debug!(%error, %process, "process activation pass failed; reloading");
                    owned.wait_for_mail().await;
                }
            }
        }
    }
}

impl ProcessActivation {
    fn steps_context(
        &self,
        owned: &Owned,
        process: &ProcessId,
        token: CancellationToken,
    ) -> ActorContext {
        ActorContext::claimed(
            self.backend.clone(),
            owned,
            AdmittedScope::process(process.clone()),
            token,
            Arc::clone(&self.probe),
        )
    }

    /// Wait for a running step to finish (and commit its outcome), for mail
    /// or for `due`.
    ///
    /// The outcome takes its run's next ordinal from `fold`, the rows as
    /// they stand, never from the execution its body ran under: an
    /// ordinal is spent only by a commit that lands, so a failed commit
    /// leaves nothing in memory ahead of the rows (ADR 0132 §5).
    async fn wait(
        &self,
        owned: &Owned,
        live: &mut Live,
        fold: &RunFold,
        due: Option<DurableInstant>,
    ) -> Result<(), DurableError> {
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
            finished = live.running.join_next_with_id(), if !live.running.is_empty() => {
                // The body ended: from now on the rows say what its step
                // needs, whether or not its outcome commits below.
                let task = match &finished {
                    Some(Ok((task, _))) => Some(*task),
                    Some(Err(error)) => Some(error.id()),
                    None => None,
                };
                // A step whose body never started because the node's lease
                // lapsed stays started: this owner is stopping, and the next
                // one recovers the step from the rows.
                let lapsed = matches!(&finished, Some(Ok((_, (_, _, Ok(None))))));
                if let Some(name) = task.and_then(|task| live.tasks.remove(&task))
                    && !lapsed
                {
                    live.started.remove(&name);
                }
                if let Some(Ok((_, (_, id, output)))) = finished
                    && let Some(output) = output?
                {
                    let admitted = fold
                        .admitted(&id)
                        .ok_or_else(|| corrupt("a running process step", "its start has no row"))?;
                    let mut tx = owned.begin().await?;
                    round::settle(&mut tx, &admitted, output, None)
                        .map_err(|refusal| corrupt("a process step's outcome", refusal))?;
                    owned.commit(tx, CommitLabel::STEP_OUTCOME).await?;
                }
            }
            // A draining owner waits for its running steps alone.
            () = owned.wait_for_mail(), if !owned.draining() => {}
            () = sleep, if !owned.draining() => {}
        }
        Ok(())
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
        if row.terminal {
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
        let ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
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
        if let Some(origin) = cancel
            && (live.park.is_some() || loaded.is_err() || row.state_rev == 0)
        {
            return self.end_engine_free(owned, tx, process, live, origin).await;
        }
        if live.park.is_some() {
            // Woken without a cancel or a redrive: stay parked.
            tx.ack_seen().give_up(Release::Parked);
            owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
            return Ok(Pass::Released);
        }
        if live.first_pass
            && live.failed_activations >= self.backend.config().settings().activation_loop_budget
        {
            let reason = ProcessParkReason::ActivationLoop {
                failed_activations: live.failed_activations,
            };
            return self.park(owned, tx, &reason).await;
        }
        live.first_pass = false;
        let (Some(engine), Ok((state, snapshot_rev))) = (engine, loaded) else {
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
        let mut driver = Driver::decode(row.driver_json.as_deref())
            .map_err(|error| corrupt("a process's driver state", error))?;
        let now = owned.store().now().await?;
        if let (Some(origin), Some(until)) = (cancel, driver.grace_until)
            && now.0 >= until
        {
            // The grace ran out: lash ends the process, whatever its steps
            // are doing, and never claims they physically stopped.
            live.running.abort_all();
            record_terminal(&mut tx, process, &cancelled(origin, true))?;
            tx.ack_seen();
            owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
            return Ok(Pass::Again);
        }
        let rows = reads
            .run_records(&OwnerKey::Process(process.clone()))
            .await?;
        let policies = PolicyView::new(driver.steps.values().filter_map(|step| {
            self.steps
                .admit(&record, &step.request, millis(now))
                .ok()
                .map(|admission| (step.request.admitted_tool(kind), admission.policy))
        }));
        let fold = round::fold(&rows, &policies)
            .map_err(|error| corrupt("a process's run records", error))?;
        // A draining node advances nothing more: it waits for the steps
        // that run to commit their outcomes, then hands the process to the
        // next build at this committed phase.
        if owned.draining() {
            if !live.running.is_empty() {
                return Ok(Pass::Wait(fold, None));
            }
            owned.drain_release(tx).await?;
            return Ok(Pass::Released);
        }
        let event = match self
            .next_event(
                &mut tx,
                reads,
                process,
                row.state_rev,
                &mut driver,
                &fold,
                &live.started,
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
            Next::Nothing { due } => {
                self.start_steps(&record, process, &driver, &fold, live);
                if !live.running.is_empty() {
                    return Ok(Pass::Wait(fold, due));
                }
                tx.ack_seen().give_up(Release::Waiting { next_due: due });
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Pass::Released);
            }
        };
        if let EngineEvent::StepSettled { step, .. } = &event {
            // Its body ended; a later step may take its name.
            live.started.remove(step);
        }
        if matches!(event, EngineEvent::Started { .. }) {
            driver.cancel_grace_ms =
                u64::try_from(engine.cancel_grace().as_millis()).unwrap_or(u64::MAX);
        }
        if matches!(event, EngineEvent::Cancelled { .. }) {
            // Steps admitted before the cancel see their token; any the
            // engine asks for now run within the grace under a fresh one.
            live.steps_token.cancel();
            live.steps_token = CancellationToken::new();
            live.steps_cx = self.steps_context(owned, process, live.steps_token.clone());
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
        if next.format != declared {
            let reason = ProcessParkReason::AdvanceRefused {
                message: format!(
                    "engine returned state format {:?}, declared {:?}",
                    next.format, declared
                ),
            };
            return self.park(owned, tx, &reason).await;
        }
        let applied = self.apply(
            &mut tx,
            process,
            &record,
            row.state_rev,
            &mut driver,
            action,
            now,
            &mut live.fresh,
        )?;
        if let Some(outcome) = applied {
            record_terminal(&mut tx, process, &outcome)?;
            tx.ack_seen();
            owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
            return Ok(Pass::Again);
        }
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: process.clone(),
            expected_rev: row.state_rev,
            driver_json: driver.encode(),
        }));
        if row.state_rev == 0
            && let Some(formats) = self.backend.formats().process(kind)
        {
            // The first transition writes the engine's state: from now on
            // only a node that decodes it claims the process.
            tx.stamp_formats(formats.clone());
        }
        tx.write(DomainWrite::Snapshot(SnapshotWrite::Put {
            exec: ExecKey::Process(process.clone()),
            expected: snapshot_rev,
            snapshot_ref: hex_encode(&next.bytes),
            executable_identity: next.format.kind.clone(),
            format_version: next.format.version,
        }));
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        // The fold read before this commit still says what each earlier
        // step needs; a step this transition admitted has no row in it, and
        // starts.
        self.start_steps(&record, process, &driver, &fold, live);
        Ok(Pass::Again)
    }

    /// The engine state committed with `row`'s revision, and the snapshot
    /// revision it replaces, or why this node cannot decode it.
    async fn load_state(
        &self,
        reads: &dyn DurableReads,
        process: &ProcessId,
        row: &ProcessActorRow,
        engine: &dyn ProcessEngine,
    ) -> Result<Result<(EngineState, Option<SnapshotRev>), ProcessParkReason>, DurableError> {
        let format = engine.state_format();
        let snapshot = reads.snapshot(&ExecKey::Process(process.clone())).await?;
        let Some(snapshot) = snapshot else {
            return Ok(if row.state_rev == 0 {
                Ok((EngineState::empty(format), None))
            } else {
                Err(ProcessParkReason::UndecodableState {
                    kind: format.kind,
                    version: format.version,
                })
            });
        };
        if snapshot.executable_identity != format.kind || snapshot.format_version != format.version
        {
            return Ok(Err(ProcessParkReason::UndecodableState {
                kind: snapshot.executable_identity,
                version: snapshot.format_version,
            }));
        }
        let bytes = hex_decode(&snapshot.snapshot_ref)
            .ok_or_else(|| corrupt("a process's engine state", "it is not lowercase hex"))?;
        Ok(Ok((EngineState { format, bytes }, Some(snapshot.rev))))
    }

    pub(super) async fn park(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        reason: &ProcessParkReason,
    ) -> Result<Pass, DurableError> {
        tracing::warn!(actor = %owned.actor(), reason = %reason.encode(), "process parked");
        tx.ack_seen();
        record_park(&mut tx, reason);
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
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
        tracing::warn!(%process, %message, "process commit refused as corrupt; ending it");
        if !live.ended_refused {
            live.ended_refused = true;
            let mut tx = owned.begin().await?;
            record_terminal(&mut tx, process, &commit_refused(&message))?;
            tx.ack_seen();
            match owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await {
                Ok(_) => return Ok(Pass::Again),
                Err(error @ DurableError::OwnershipLost(_)) => return Err(error),
                Err(_) => {}
            }
        }
        let tx = owned.begin().await?;
        self.park(owned, tx, &ProcessParkReason::CommitRefused { message })
            .await
    }

    /// A process claimed in a format set this node does not decode: it was
    /// claimed only for its pending cancel, which ends it from registry
    /// state. Without one (the cancel already ended it, or its mail was
    /// something else) it goes back to idle, read nothing.
    async fn end_undecodable(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        process: &ProcessId,
        live: &mut Live,
    ) -> Result<Pass, DurableError> {
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
            tx.write(DomainWrite::ParkEvent(ParkEventWrite::Ended {
                reason_json: serde_json::json!({ "cancelled": origin }).to_string(),
            }));
        }
        tx.ack_seen();
        owned.commit(tx, CommitLabel::PROCESS_TERMINAL).await?;
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

    /// Start the body of every admitted step this activation is not
    /// running and the fold says to run: newly admitted, or a `Repeatable`
    /// started without an outcome, again at its ordinal.
    fn start_steps(
        &self,
        record: &ProcessRecord,
        process: &ProcessId,
        driver: &Driver,
        fold: &RunFold,
        live: &mut Live,
    ) {
        for (name, step) in &driver.steps {
            if live.started.contains(name) {
                continue;
            }
            let id = step_id(process, step);
            match fold.recovery(&id) {
                None | Some(Recovery::NotStarted | Recovery::RerunAtOrdinal(_)) => {}
                Some(
                    Recovery::Settled(_)
                    | Recovery::Vetoed(_)
                    | Recovery::Interrupt
                    | Recovery::RetryDue { .. }
                    | Recovery::Waiting(_),
                ) => {
                    continue;
                }
            }
            let Some(admitted) = live.fresh.remove(name).or_else(|| fold.admitted(&id)) else {
                continue;
            };
            let body = self.steps.body(record, &step.request, &step.call);
            let cx = live.steps_cx.clone();
            live.started.insert(name.clone());
            let task = {
                let name = name.clone();
                live.running.spawn(async move {
                    let output = round::run_body(&cx, &admitted, body).await;
                    (name, admitted.id().clone(), output)
                })
            };
            live.tasks.insert(task.id(), name.clone());
        }
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
}

/// The identity of an in-flight step's admitted execution: its owner, its
/// run and its member's first start.
fn step_id(process: &ProcessId, step: &InFlight) -> AdmittedId {
    AdmittedId {
        owner: OwnerKey::Process(process.clone()),
        run: RunSeq(step.run),
        ordinal: round::member_ordinal(step.member),
    }
}

#[expect(
    clippy::expect_used,
    reason = "a step's request is plain JSON whose material reference always encodes"
)]
fn step_request_material(
    process: &ProcessId,
    step: &InFlight,
) -> lash_core_store::tool_run::MaterialRef {
    MaterialPayload::new(
        MaterialOwner::Process {
            process_id: process.clone(),
        },
        MaterialRole::PreparedRequest,
        None,
        step.request.input().to_string(),
    )
    .reference(MaterialLocation::JournalLocal)
    .expect("a step request's material encodes")
}

impl ProcessActivation {
    #[expect(
        clippy::too_many_arguments,
        reason = "the rows one event is derived from"
    )]
    async fn next_event(
        &self,
        tx: &mut ActorTx,
        reads: &dyn DurableReads,
        process: &ProcessId,
        state_rev: u64,
        driver: &mut Driver,
        fold: &RunFold,
        running: &BTreeSet<StepName>,
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
                Immediate::Emitted => EngineEvent::Emitted,
            }));
        }
        if let Some(event) = settled_step(tx, process, driver, fold, running)? {
            return Ok(Next::Event(event));
        }
        if let Some(mail) = tx
            .mail()
            .iter()
            .find(|mail| mail.kind.as_str() == SIGNAL_MAIL)
            .cloned()
        {
            let signal: ProcessSignal = serde_json::from_str(&mail.body)
                .map_err(|error| corrupt("a process signal", error))?;
            tx.ack_through(mail.seq);
            return Ok(Next::Event(EngineEvent::Signal(signal)));
        }
        let mut due = driver
            .grace_until
            .map(DurableInstant)
            .into_iter()
            .collect::<Vec<_>>();
        match driver.blocked.clone() {
            None | Some(Blocked::Idle) => {}
            Some(Blocked::Sleep { until }) => {
                if now.0 >= until {
                    driver.blocked = None;
                    return Ok(Next::Event(EngineEvent::Woke));
                }
                due.push(DurableInstant(until));
            }
            Some(Blocked::External { name, wait }) => {
                let row = reads
                    .wait(&wait.0)
                    .await?
                    .ok_or_else(|| corrupt("a pinned key's wait", "its row is gone"))?;
                match row.state {
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
                    WaitState::Pending => match row.deadline {
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
            }) => {
                // The wait row's committed winner is the answer, never the
                // target's live registry row: a timeout that committed first
                // stays a timeout, and a resolution outlives the target's
                // pruning.
                let mut row = process_wait(reads, &wait).await?;
                if row.state == WaitState::Pending {
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
                match row.state {
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
                    WaitState::Pending => match row.deadline {
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
    fn apply(
        &self,
        tx: &mut ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        state_rev: u64,
        driver: &mut Driver,
        action: EngineAction,
        now: DurableInstant,
        fresh: &mut std::collections::BTreeMap<StepName, AdmittedExecution>,
    ) -> Result<Option<crate::ProcessOutcome>, DurableError> {
        let ProcessInput::Engine { kind: engine, .. } = record.input.as_ref() else {
            return Err(corrupt("an advanced process", "it runs no engine"));
        };
        let budgets = lash_sansio::ExecutionBudgets::default();
        let deadline = |requested: Option<std::time::Duration>| {
            WaitDeadline::resolve(
                requested,
                budgets.wait_default(),
                budgets.wait_ceiling(),
                now,
            )
        };
        driver.blocked = None;
        match action {
            EngineAction::Steps(requests) => {
                let run = driver.next_run;
                driver.next_run += 1;
                let admission = ToolCallAdmission::process("", process.clone());
                let mut drafts = Vec::with_capacity(requests.len());
                let mut names = Vec::with_capacity(requests.len());
                for (member, request) in (0_u64..).zip(requests) {
                    if driver.steps.contains_key(request.step()) {
                        return Ok(Some(refused(format!(
                            "step `{}` is already in flight",
                            request.step().0
                        ))));
                    }
                    let admitted = match self.steps.admit(record, &request, millis(now)) {
                        Ok(admitted) => admitted,
                        Err(refusal) => return Ok(Some(refused(refusal.to_string()))),
                    };
                    let call = admission.call_id(&[ToolCallPosition::ProcessStep {
                        step: &request.step().0,
                        run,
                    }]);
                    let step = InFlight {
                        run,
                        member,
                        call,
                        policy: admitted.policy,
                        limit_expires_at: admitted.limit.expires_at,
                        limit_max_slice_ms: u64::try_from(admitted.limit.max_slice.as_millis())
                            .unwrap_or(u64::MAX),
                        request,
                    };
                    drafts.push(ExecutionDraft::new(
                        step.call.clone(),
                        step.request.admitted_tool(engine),
                        step_request_material(process, &step),
                        step.policy,
                        step.limit(),
                        None,
                    ));
                    names.push(step.request.step().clone());
                    driver.steps.insert(step.request.step().clone(), step);
                }
                if drafts.is_empty() {
                    return Ok(Some(refused("a Steps action names no step".to_owned())));
                }
                let admitted =
                    round::admit(tx, &OwnerKey::Process(process.clone()), RunSeq(run), drafts)
                        .map_err(|refusal| corrupt("a process step's admission", refusal))?;
                fresh.clear();
                fresh.extend(names.into_iter().zip(admitted));
            }
            EngineAction::PinKey {
                name,
                kind,
                deadline: requested,
            } => {
                let deadline = match deadline(requested) {
                    Ok(deadline) => deadline,
                    Err(refusal) => return Ok(Some(refused(refusal.to_string()))),
                };
                let kind = match kind {
                    HostWaitKind::ToolCompletion => WaitKind::ToolCompletion,
                    HostWaitKind::Custom => WaitKind::Custom,
                };
                let (wait, key) = waits::pin(
                    tx,
                    WaitSpec {
                        kind,
                        scope: ScopeKey::Process(process.clone()),
                        target_process: None,
                        deadline: Some(deadline),
                    },
                )
                .map_err(|refusal| corrupt("a pinned key", refusal))?;
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
            EngineAction::AwaitExternal { name } => {
                let Some(pinned) = driver.keys.get(&name) else {
                    return Ok(Some(refused(format!("key `{}` was never pinned", name.0))));
                };
                driver.blocked = Some(Blocked::External {
                    name,
                    wait: pinned.wait,
                });
            }
            EngineAction::AwaitProcess {
                process: target,
                deadline: requested,
            } => {
                let deadline = match deadline(requested) {
                    Ok(deadline) => deadline,
                    Err(refusal) => return Ok(Some(refused(refusal.to_string()))),
                };
                let (wait, _) = waits::pin(
                    tx,
                    WaitSpec {
                        kind: WaitKind::ProcessTerminal,
                        scope: ScopeKey::Process(process.clone()),
                        target_process: Some(target.clone()),
                        deadline: Some(deadline),
                    },
                )
                .map_err(|refusal| corrupt("a process wait", refusal))?;
                driver.blocked = Some(Blocked::Process {
                    process: target,
                    wait: StoredWaitId(wait.id()),
                });
            }
            EngineAction::Sleep { until } => {
                driver.blocked = Some(Blocked::Sleep { until: until.0 });
            }
            EngineAction::Idle => driver.blocked = Some(Blocked::Idle),
            EngineAction::Emit {
                event_type,
                payload,
            } => {
                tx.write(DomainWrite::Process(ProcessWrite::Emit {
                    process: process.clone(),
                    event_type: event_type.name.clone(),
                    payload_json: payload.to_string(),
                    replay_key: format!("process:{process}:emit:{state_rev}"),
                }));
                driver.immediate = Some(Immediate::Emitted);
            }
            EngineAction::Terminal(outcome) => return Ok(Some(outcome)),
        }
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

/// A process ended because lash refused what its engine asked for.
fn refused(message: String) -> crate::ProcessOutcome {
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
        crate::ToolFailure::runtime(
            crate::ToolFailureClass::InvalidRequest,
            "process_action_refused",
            message,
        ),
    ))
}

/// The first in-flight step the fold settled, as its event; a started
/// `Once` without an outcome is recorded `Interrupted` on `tx` first.
fn settled_step(
    tx: &mut ActorTx,
    process: &ProcessId,
    driver: &mut Driver,
    fold: &RunFold,
    running: &BTreeSet<StepName>,
) -> Result<Option<EngineEvent>, DurableError> {
    for (name, step) in &driver.steps {
        let id = step_id(process, step);
        let outcome = match fold.recovery(&id) {
            Some(Recovery::Settled(outcome) | Recovery::Vetoed(outcome)) => outcome.clone(),
            // A started `Once` step without an outcome was interrupted only
            // if no body of it runs: one this activation started is still
            // running, and its outcome commits when it ends.
            Some(Recovery::Interrupt) if !running.contains(name) => {
                round::settle_interrupted(tx, fold, &id)
                    .map_err(|refusal| corrupt("an interrupted process step", refusal))?;
                lash_core_store::tool_run::AttemptOutcome::Interrupted
            }
            _ => continue,
        };
        let payload = round::outcome_material(&outcome)
            .and_then(|material| fold.material(material))
            .map(str::to_owned);
        let outcome = SettledOutcome::new(outcome, payload)
            .map_err(|refusal| corrupt("a process step's outcome", refusal))?;
        let name = name.clone();
        driver.steps.remove(&name);
        return Ok(Some(EngineEvent::StepSettled {
            step: name,
            outcome,
        }));
    }
    Ok(None)
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
    let stored = row
        .resolution_ref
        .as_deref()
        .ok_or_else(|| corrupt("a resolved wait", "it holds no resolution"))?;
    serde_json::from_str(stored).map_err(|error| corrupt("a wait's resolution", error))
}

/// An engine state's bytes as `snapshot_ref` carries them inline: lowercase
/// hex.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            let pair = text.get(at..at + 2)?;
            pair.bytes()
                .all(|digit| digit.is_ascii_digit() || (b'a'..=b'f').contains(&digit))
                .then(|| u8::from_str_radix(pair, 16).ok())
                .flatten()
        })
        .collect()
}
