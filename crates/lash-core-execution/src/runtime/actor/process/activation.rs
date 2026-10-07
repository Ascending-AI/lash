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
//! With none, the steps run their admitted-execution lifecycle
//! ([`Lifecycle`], shared with a round's members): bodies run, retries are
//! recorded and started, parked steps race their waits, and outcomes
//! commit (`step.outcome`). While a body runs the actor stays owned; once
//! only parked waits and retry dues are left, it releases as `waiting`
//! until its earliest due time, holding nothing. The activation adds only
//! the engine-event adapter: a step whose call the fold settled is handed
//! to `advance` as `StepSettled`.
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

use lash_core_store::tool_run::{
    AttemptOutcome, AvailableEvidence, CompletionSource, MaterialLocation, MaterialOwner,
    MaterialPayload, MaterialRole,
};
use lash_durable::domain::{
    CANCEL_MAIL, ExecKey, OwnerKey, ParkEventWrite, ProcessActorRow, ProcessWrite, RunSeq,
    SIGNAL_MAIL, ScopeKey, SnapshotRev, SnapshotWrite, WaitState, WaitWrite,
};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, DurableReads,
    Release, StoreFailure, StoreFailureKind,
};
use lash_sansio::{ToolCallAdmission, ToolCallPosition};
use tokio_util::sync::CancellationToken;

use super::driver::{Blocked, Driver, Immediate, InFlight, Pinned, StoredWaitId};
use super::session_turn::SessionTurns;
use super::terminal::{ProcessParkReason, cancelled, record_park, record_terminal};
use super::{CascadeProgress, end_scope};
use crate::runtime::actor::round::lifecycle::{Act, Idle, Lifecycle};
use crate::runtime::actor::round::{
    self, AdmittedExecution, BodyOutput, ExecutionDraft, MemberBodies, MemberBody, PolicyView,
    RoundDraft, RoundError, RunFold,
};
use crate::runtime::actor::waits::{self, Resolution, WaitDeadline, WaitKind, WaitSpec};
use crate::runtime::process::engine_state::{
    EngineAction, EngineEvent, EngineState, HostWaitKind, SettledOutcome, StepRequest,
};
use crate::runtime::process::steps::ProcessSteps;
use crate::{
    ActorContext, AdmittedScope, Backend, CancelOrigin, ProcessEngine, ProcessId, ProcessInput,
    ProcessRecord, ProcessSignal, ToolCallId,
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
    lifecycle: Lifecycle,
    /// What the lifecycle's bodies are built from.
    bodies: Arc<StepBodies>,
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
        let bodies = Arc::new(StepBodies {
            steps: Arc::clone(&self.steps),
            seen: Mutex::default(),
        });
        let mut live = Live {
            lifecycle: Lifecycle::new(
                &self.steps_context(&owned, &process),
                PolicyView::default(),
                Arc::clone(&bodies) as Arc<dyn MemberBodies>,
                CommitLabel::STEP_OUTCOME,
            ),
            bodies,
            park,
            failed_activations,
            first_pass: true,
            ended_refused: false,
        };
        loop {
            match self.pass(&owned, &process, &mut live).await {
                Ok(Pass::Again) => {}
                Ok(Pass::Wait(idle, due)) => match self.wait(&owned, &mut live, &idle, due).await {
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
            () = owned.wait_for_mail(), if !owned.draining() => {}
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
            live.lifecycle.abandon();
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
        let event = match self
            .next_event(
                &mut tx,
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
                tx.ack_seen().give_up(Release::Waiting { next_due });
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Pass::Released);
            }
        };
        if matches!(event, EngineEvent::Started { .. }) {
            driver.cancel_grace_ms =
                u64::try_from(engine.cancel_grace().as_millis()).unwrap_or(u64::MAX);
        }
        if matches!(event, EngineEvent::Cancelled { .. }) {
            // Steps admitted before the cancel see their token; any the
            // engine asks for now run within the grace.
            live.lifecycle.cancel_runs_before(RunSeq(driver.next_run));
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
        let mut fresh = Vec::new();
        let applied = self.apply(
            &mut tx,
            process,
            &record,
            row.state_rev,
            &mut driver,
            action,
            now,
            &mut fresh,
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
        // The steps this transition admitted run once the next pass folds
        // their rows.
        live.lifecycle.admitted(&fresh);
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
                Immediate::Emitted => EngineEvent::Emitted,
            }));
        }
        if let Some(event) = settled_event(driver, fold)? {
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
    fn apply(
        &self,
        tx: &mut ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        state_rev: u64,
        driver: &mut Driver,
        action: EngineAction,
        now: DurableInstant,
        fresh: &mut Vec<AdmittedExecution>,
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
                        admitted.wait,
                    ));
                    driver.steps.insert(step.request.step().clone(), step);
                }
                if drafts.is_empty() {
                    return Ok(Some(refused("a Steps action names no step".to_owned())));
                }
                // A step that may park has its completion wait pinned with
                // its admission, as a round member has.
                let admitted = round::admit_round(
                    tx,
                    &ScopeKey::Process(process.clone()),
                    RoundDraft {
                        owner: OwnerKey::Process(process.clone()),
                        run: RunSeq(run),
                        members: drafts,
                    },
                )
                .map_err(|refusal| corrupt("a process step's admission", refusal))?;
                fresh.extend(admitted.members().iter().cloned());
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

/// The engine-event adapter: the first in-flight step whose call the fold
/// settled, as its `StepSettled` event. Its lifecycle recorded the outcome;
/// the step leaves the driver with the transition that hands it over.
fn settled_event(driver: &mut Driver, fold: &RunFold) -> Result<Option<EngineEvent>, DurableError> {
    for (name, step) in &driver.steps {
        let Some(outcome) = fold
            .round(RunSeq(step.run))
            .and_then(|view| view.members().get(usize::try_from(step.member).ok()?))
            .and_then(round::RoundMember::outcome)
        else {
            continue;
        };
        let payload = round::outcome_material(outcome)
            .and_then(|material| fold.material(material))
            .map(str::to_owned);
        let outcome = SettledOutcome::new(outcome.clone(), payload)
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

fn unknown_step() -> BodyOutput {
    BodyOutput::from(AttemptOutcome::Cancelled {
        evidence: AvailableEvidence::default(),
    })
}

impl MemberBodies for StepBodies {
    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.step(execution.call()) {
            Some((record, step)) => {
                let body = self.steps.body(&record, &step, execution);
                Box::new(move |token| Box::pin(async move { body(token).await.into() }))
            }
            // The lifecycle runs only admitted steps; a body for any other
            // is never asked for. Answer as a stop rather than run anything.
            None => Box::new(|_| Box::pin(async { unknown_step().into() })),
        }
    }

    fn resolved(
        &self,
        execution: &AdmittedExecution,
        source: &CompletionSource,
        metadata: Option<&str>,
        resolution: Resolution,
    ) -> BodyOutput {
        match self.step(execution.call()) {
            Some((record, step)) => self
                .steps
                .resolved(&record, &step, execution, source, metadata, resolution),
            None => unknown_step(),
        }
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
