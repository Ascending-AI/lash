//! The session activation and the turn's phases (ADR 0132 §4). Owned by V0
//! (FIG-5170), then L3 (FIG-5172).
//!
//! # Contracts
//!
//! - The durable entry is: load rows, [`TurnRestore::restore`] (exactly one
//!   `restore_from_checkpoint`), re-deliver the pending effect, then
//!   the phase runner (`phases::run_phases`). The turn driver becomes this phase runner; it commits at
//!   the catalog's labels (`turn.admit`, `turn.prepare`, `model.start`,
//!   `model.done`, `round.present+model.start`, `turn.commit`) and never
//!   re-executes orchestration to reach a recorded outcome.
//! - The activation calls `session_mail::drain_session_mail` first on every
//!   claim and acts on what it returns.
//! - Extension points that let L3, L4 and L6b work in parallel: the
//!   `model.done` transaction calls `round::admit_round` (S4); the
//!   `round.present+model.start` transaction calls `round::present`; the
//!   `turn.commit` transaction calls `process::end_scope(tx,
//!   ScopeKey::Turn(..), batch)` (S6). L3 owns the transactions; the callee's
//!   owner owns what it writes.
//! - The checkpoint is L3a's bounded encoding, referenced from
//!   [`TurnRow::checkpoint_ref`] by digest.
//! - The ends of scopes (L6b): `turn.commit` and `turn.cancel` call
//!   `turn_scope::end_turn_scope` on their own transaction. After either
//!   commits, and on every claim right after the mail drain, the activation
//!   runs `turn_scope::continue_scope_ends` before it releases; after
//!   `turn.cancel` it then waits, bounded by `stop_grace`, for the turn's
//!   children (`turn_scope::await_turn_children`, G1b). When the
//!   drain returns a `close`, its transaction also calls
//!   `session_close::begin_session_close`; while the session has a close
//!   row that is not its tombstone, the activation admits no turn and runs
//!   `session_close::run_session_close`, releasing as waiting on
//!   `SessionCloseExit::Waiting` (the close's `tombstone` commit itself
//!   releases the actor as terminal).

use std::sync::Arc;

use lash_durable::domain::{MailAnswer, MailDomainWrite, TurnWrite};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, MailTx, Release,
};
use lash_sansio::SavedTurn;
use lash_sansio::llm::types::RecordedRequestTemplate;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use super::head::{HeadCache, SessionHead};
use super::{phases, turn_cancel, turn_trace};
use crate::{
    ActorContext, AdmittedScope, Backend, Effect, EffectId, HostTurnProtocol, LlmRequest,
    SessionId, TurnId, TurnMachine, TurnMachineConfig, TurnOutcome,
};

pub use lash_durable::domain::{
    ModelCall, ModelPin, TurnCancelAnswer, TurnCancelRequest, TurnRow, UnfinishedPhase,
};

use super::session_close::{Closing, SessionCloseError, close_pass};
use super::session_mail::{InputBatching, OneInputPerRun, SessionMailError, drain_session_mail};
use super::turn_scope::{TurnChildrenStopError, await_turn_children, continue_scope_ends};

mod park;

pub use super::model_call::ModelAnswer;
use park::park;
pub(super) use park::resume_refusal;
pub use park::{ParkedTurnState, SessionParkReason, SessionPassFailure};

/// What a session's turns run with: the deployment's protocol, model,
/// environment and code cells. It opens one [`TurnDrive`] per turn an owner
/// runs; nothing it does drives a turn or commits.
#[async_trait::async_trait]
pub trait TurnServices: Send + Sync {
    /// The budgets `session`'s turns run under: a model call's
    /// `model_total` among them.
    fn execution_budgets(&self, session: &SessionId) -> crate::ExecutionBudgets;

    /// Start `row`'s turn from committed state alone: the session's `head`,
    /// as the owner loaded it, and what its admission bound. Nothing it
    /// computes is durable until the phase runner commits.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the turn cannot be prepared.
    async fn start(
        &self,
        cx: &ActorContext,
        row: &TurnRow,
        head: &SessionHead,
    ) -> Result<Box<dyn TurnDrive>, TurnError>;

    /// Take over the turn `restore` names: build what the turn holds in
    /// memory from committed state, and restore its machine from its
    /// checkpoint with [`TurnRestore::restore`] under the configuration it
    /// was built with.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the turn cannot be taken over.
    async fn resume(
        &self,
        cx: &ActorContext,
        restore: TurnRestore<'_>,
    ) -> Result<OpenTurn, TurnError>;

    /// Apply the session-command run `admitted` names (a command, or a
    /// plugin task's operation): its commit settles the command's row, so a
    /// pass that finds the row still open applies it again.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the run could not apply; nothing of it committed.
    async fn apply_commands(
        &self,
        cx: &ActorContext,
        admitted: &AdmittedInputs,
    ) -> Result<(), TurnError>;

    /// Carry `session`, idle and in the session format set of an earlier
    /// build this one carries forward, to this build's (ADR 0106 §2,
    /// FIG-5787): what it holds is restored, recaptured and committed with
    /// this build's session format set, outside a turn. Services that keep
    /// no session state of their own record the format set alone.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the session does not restore under this build or
    /// the commit is refused; nothing of it committed.
    async fn carry_session(
        &self,
        cx: &ActorContext,
        _session: &SessionId,
    ) -> Result<(), TurnError> {
        let mut tx = cx.begin().await?;
        tx.stamp_formats(cx.backend().formats().session().clone());
        cx.commit(tx, CommitLabel::SESSION_COMMAND).await?;
        Ok(())
    }

    /// How many next-turn inputs one of the session's runs takes: the
    /// host's installed queued-work batching. The default takes each input
    /// as its own run.
    fn input_batching(&self) -> &dyn InputBatching {
        &OneInputPerRun
    }

    /// Before each of `cx`'s passes over `session`: announce its committed
    /// head when this node has not published it, should the owner that
    /// committed it have lost the commit's acknowledgement or its life
    /// before it published the commit. Commits nothing.
    async fn announce_head(&self, _cx: &ActorContext, _session: &SessionId) {}

    /// Publish `record`, the sealed model-call record of the receipt
    /// `row`'s model phase holds, on the session's live stream under the id
    /// a drive that settles the receipt publishes it with: what a node that
    /// holds no drive of the turn publishes when the turn's cancel ends its
    /// answered phase (FIG-6274). Services with no live stream publish
    /// nothing. Commits nothing.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the turn's scope cannot be admitted.
    async fn publish_model_call(
        &self,
        _cx: &ActorContext,
        _row: &TurnRow,
        _record: &crate::LlmCallRecord,
    ) -> Result<(), TurnError> {
        Ok(())
    }

    /// The trace scope of `run`, the turn `cx` admits taking inputs whose
    /// retained causes are `inputs`, in admission order: the scope its
    /// `turn.admit` commit retains, under the head input's producer and
    /// linking the others, its span derived from that parent. `None` when
    /// the services trace nothing.
    fn turn_trace_scope(
        &self,
        _cx: &ActorContext,
        _run: &TurnId,
        _inputs: &[lash_trace::TraceCause],
    ) -> Option<lash_trace::DurableTraceScope> {
        None
    }

    /// The trace runtime a turn's `turn_started` and `turn_completed`
    /// records are emitted through, once the commit each reports is
    /// acknowledged. `None` when the services trace nothing.
    fn tracing(&self) -> Option<&crate::trace::TraceRuntime> {
        None
    }
}

/// One admitted model attempt's ordinal, execution bound and cooperative stop.
#[derive(Clone, Debug)]
pub struct ModelCallAttempt {
    /// Ordinal of this send within the pinned call.
    pub ordinal: u32,
    /// Execution bound derived from the call's pinned deadline.
    pub limit: crate::ExecutionLimit,
    /// Stop the live call and retain its sealed attempts before returning.
    pub cancel: tokio_util::sync::CancellationToken,
    /// The typed deadline failure, set before cancelling a live attempt.
    /// When cancelled, `None` means the turn's host requested cancellation.
    pub stop_error: std::sync::Arc<std::sync::Mutex<Option<crate::LlmCallError>>>,
}

mod drive;
pub use drive::TurnDrive;

/// A code cell's source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeCell {
    /// Its language.
    pub language: String,
    /// Its code.
    pub code: String,
}

/// How a code cell's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellExit {
    /// The cell answered the machine.
    Answered,
    /// The cell stopped on calls that wait only on rows (a parked call, a
    /// retry's due time, a timer): its committed snapshot holds it, it has
    /// no answer yet, and the turn suspends until the earliest due, which
    /// the cell noted on the actor's context.
    Suspended,
}

/// One cell's existing ledger references and its transient materialized
/// records. Only the references are written with the consuming turn commit.
#[derive(Clone, Debug, Default)]
pub struct CellToolCalls {
    pub records: Vec<crate::ToolCallRecord>,
    pub ledger: lash_core_execution::runtime::actor::round::graph::ReportView,
}

impl CellToolCalls {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ledger.entries.is_empty()
    }
}

/// A finished machine's last word: what its `Done` effect carried and the
/// outcome it emitted before it.
#[derive(Debug)]
pub struct TurnDone {
    /// The turn's messages.
    pub messages: crate::MessageSequence,
    /// History records the machine appended since its last progress.
    pub event_delta: Vec<crate::SessionHistoryRecord>,
    /// The protocol iteration it finished in.
    pub protocol_iteration: usize,
    /// The outcome it emitted, if it emitted one.
    pub outcome: Option<TurnOutcome>,
    /// The value schema its accepted finish was validated under: the
    /// witness its completion candidate kept. `None` when no finish tool
    /// ended it.
    pub finish_value_schema: Option<crate::JsonSchema>,
}

impl TurnDone {
    /// The run terminal `run`'s commit records: the outcome its turn
    /// committed with, as the store reads every run's end back.
    ///
    /// # Errors
    ///
    /// [`TurnError::Exec`] when `run` is not its own physical turn.
    pub fn run_terminal_cause(
        &self,
        run: &TurnId,
    ) -> Result<crate::store::RunTerminalCause, TurnError> {
        let outcome = self
            .outcome
            .clone()
            .unwrap_or(TurnOutcome::Stopped(crate::TurnStop::Incomplete));
        let committed = crate::store::RunCommittedOutcome::of_turn_outcome(
            &outcome,
            self.finish_value_schema.clone(),
        );
        let commit = crate::store::TurnCommitId::of_physical_turn(run, run).ok_or_else(|| {
            TurnError::Exec(format!("turn {run} is not its own run's physical turn"))
        })?;
        Ok(crate::store::RunTerminalCause::Committed {
            commit,
            turn: run.clone(),
            outcome: committed,
        })
    }
}

/// The run terminal of a cancel the session actor honoured before the turn
/// committed.
#[must_use]
pub fn cancelled_cause(
    evidence: crate::runtime::TurnCancellationEvidence,
) -> crate::store::RunTerminalCause {
    crate::store::RunTerminalCause::Cancelled {
        evidence,
        input_outcome: Default::default(),
    }
}

/// What `turn.commit` writes for a finished turn: the session head's next
/// revision. Its terminal is the finished machine's ([`TurnDone`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCommit {
    /// The head revision the commit replaces.
    pub expected_head: u64,
    /// The session head's commit, encoded by
    /// [`encode_session_commit`](crate::store::encode_session_commit).
    pub commit: crate::store::EncodedSessionCommit,
}

/// The session actor's activation: claim, drain mail, admit and run phases,
/// release.
#[derive(Clone)]
pub struct SessionActivation {
    backend: Backend,
    services: Arc<dyn TurnServices>,
    probe: Arc<dyn DurableProbe>,
}

impl std::fmt::Debug for SessionActivation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionActivation").finish_non_exhaustive()
    }
}

impl SessionActivation {
    /// The activation of sessions over `backend`, running turns with
    /// `services` and reporting to `probe` (`NoProbe` in production).
    #[must_use]
    pub fn new(
        backend: Backend,
        services: Arc<dyn TurnServices>,
        probe: Arc<dyn DurableProbe>,
    ) -> Self {
        Self {
            backend,
            services,
            probe,
        }
    }

    /// The backend it runs over.
    #[must_use]
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// One pass over the session's rows: admit a turn from its mail, run an
    /// unfinished turn to its commit over the head `heads` holds, or, with
    /// nothing to do, carry a session an earlier build left to this build's
    /// formats (asked once per claim, as `carry_checked` records), stay hot
    /// ([`Pass::Idle`]) or release the actor when `release`.
    async fn pass(
        &self,
        cx: &ActorContext,
        session: &SessionId,
        release: bool,
        heads: &mut HeadCache,
        carry_checked: &mut bool,
    ) -> Result<Pass, TurnError> {
        // A turn's scope whose cascade a crash cut short is marked to its end
        // before anything else (L6b).
        continue_scope_ends(cx, session).await?;
        let mut tx = match close_pass(cx, session, self.services.tracing()).await? {
            Closing::Ran(pass) => return Ok(pass),
            Closing::Not(tx) => *tx,
        };
        let open = cx.durable_reads()?.turn(session).await?;
        let Some(row) = open else {
            // No unfinished turn: the mailbox says what runs next (L3s).
            let drain = drain_session_mail(cx, &mut tx, self.services.input_batching()).await?;
            // An idle session an earlier build left is carried to this
            // build's formats, once per claim, before it is let go: the
            // operator's sweep wakes it for this (FIG-5787).
            if drain.admit.is_none() && !*carry_checked {
                *carry_checked = true;
                if cx.holds_carried_session().await? {
                    drop(tx);
                    let carried = self.services.carry_session(cx, session).await;
                    *carry_checked = carried.is_ok();
                    carried?;
                    heads.evict();
                    return Ok(Pass::Again);
                }
            }
            return match drain.admit {
                Some(admitted) if admitted.admission.is_turn() => {
                    // The turn's trace scope is retained by its admission:
                    // every owner that runs the turn reads the scope back
                    // and names its span from it (FIG-5395).
                    let trace = self
                        .services
                        .turn_trace_scope(cx, &admitted.run, &admitted.causes);
                    let inputs = admitted.admission.input_ids().len();
                    let send = admitted.admission.input_ids().first().cloned();
                    let admitted = AdmittedInputs {
                        admission: admitted.admission.with_trace(trace.clone()),
                        ..admitted
                    };
                    let run = admitted.run.clone();
                    admit_turn(cx, &mut tx, admitted).await?;
                    // The turn's rows are this build's session state:
                    // from now only a node that decodes it claims the
                    // session.
                    tx.stamp_formats(cx.backend().formats().session().clone());
                    cx.commit(tx, CommitLabel::TURN_ADMIT).await?;
                    turn_trace::started(
                        self.services.tracing(),
                        session,
                        &run,
                        trace.as_ref(),
                        inputs,
                        send.as_ref(),
                    );
                    Ok(Pass::Again)
                }
                // A command run binds nothing: the commit that applies it
                // settles its row, and until then every drain hands it out
                // again.
                Some(admitted) => {
                    drop(tx);
                    self.services.apply_commands(cx, &admitted).await?;
                    // A command's commit may move the head outside
                    // `turn.commit`: the next turn loads it again.
                    heads.evict();
                    Ok(Pass::Again)
                }
                None if release => {
                    tx.give_up(Release::Idle);
                    cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
                    Ok(Pass::Released)
                }
                None => Ok(Pass::Idle),
            };
        };
        drop(tx);
        // An accepted cancel request ends the turn before anything else
        // runs: the live owner stopped its work for it, or a crash left it.
        // Its stop commits from the turn's last checkpoint.
        if let Some(request) = row.cancel.clone()
            && turn_cancel::stops_before_resuming(&request, &row.phase)
        {
            let stopped = turn_cancel::restore(cx, self.services.as_ref(), &row, heads).await?;
            turn_cancel::finalize(cx, self.services.as_ref(), &row, &request, stopped, heads)
                .await?;
            return await_stop(cx, self.services.as_ref(), session, &row.run).await;
        }
        // The turn's work on this owner runs under its span: a host's
        // `tracing` instrumentation of a provider, a tool or a plugin it
        // calls nests under the turn it serves (FIG-5353).
        let span = tracing::info_span!("lash.turn", session_id = %session, run_id = %row.run);
        let opened = match row.phase {
            UnfinishedPhase::Admitted => match heads.head(cx, session).await {
                Ok(head) => self
                    .services
                    .start(cx, &row, head)
                    .instrument(span.clone())
                    .await
                    .map(|drive| OpenTurn {
                        drive,
                        pending: None,
                        row: row.clone(),
                    }),
                Err(error) => Err(error),
            },
            UnfinishedPhase::Model { .. } | UnfinishedPhase::Tools { .. } => {
                self.services
                    .resume(cx, TurnRestore::new(cx, &row, heads))
                    .instrument(span.clone())
                    .await
            }
        };
        let turn = match opened {
            Ok(turn) => turn,
            // A turn its committed state cannot prepare, on any pass (a
            // plugin build its recorded config refuses, FIG-5246, or a head
            // its creation never recorded, FIG-4553), ends with that refusal
            // as its run's cause: no pass would clear it.
            Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => {
                phases::refuse(cx, self.services.tracing(), &row, refusal).await?;
                return Ok(Pass::Again);
            }
            // The step an after-step request lets finish cannot run on a
            // build that does not decode its state: the request ends the
            // turn without it, as a cancel's restore does, so the refusal
            // never holds the session's one way out (FIG-5601).
            Err(error) if row.cancel.is_some() && resume_refusal(&error).is_some() => {
                if let Some(request) = &row.cancel {
                    turn_cancel::finalize(cx, self.services.as_ref(), &row, request, None, heads)
                        .await?;
                }
                return await_stop(cx, self.services.as_ref(), session, &row.run).await;
            }
            Err(error) => return Err(error),
        };
        let ran = match Box::pin(
            phases::run_phases(cx, self.services.as_ref(), turn, heads).instrument(span),
        )
        .await
        {
            // A refusal no pass would clear, met as the turn finishes (an
            // after-turn callback's), ends the run with it, as one met at
            // preparation does: retrying the pass would meet it again.
            Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => {
                phases::refuse(cx, self.services.tracing(), &row, refusal).await?;
                return Ok(Pass::Again);
            }
            ran => ran?,
        };
        match ran {
            PhaseExit::Committed(_) | PhaseExit::CancelRequested => Ok(Pass::Again),
            PhaseExit::Cancelled => await_stop(cx, self.services.as_ref(), session, &row.run).await,
            PhaseExit::Lost => Ok(Pass::Lost),
            PhaseExit::Released => Ok(Pass::Released),
            PhaseExit::Drained => drain_release(cx).await,
            PhaseExit::Suspended { due } => {
                let next_due = due.into_iter().chain(cx.next_due()).min();
                let mut tx = cx.begin().await?;
                tx.give_up(Release::Waiting { next_due });
                cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
                Ok(Pass::Released)
            }
        }
    }
}

/// After `run`'s cancel committed, wait for the children it marked (G1b,
/// L6b): the rest of its cascade first, then their terminals, bounded by the
/// stop's grace. A child still running at the grace keeps its cancel, and the
/// stop completes anyway.
pub(super) async fn await_stop(
    cx: &ActorContext,
    services: &dyn TurnServices,
    session: &SessionId,
    run: &TurnId,
) -> Result<Pass, TurnError> {
    continue_scope_ends(cx, session).await?;
    let stop = await_turn_children(
        cx,
        session,
        run,
        services.execution_budgets(session).stop_grace(),
        cx.backend().config().settings().cascade_batch,
    )
    .await?;
    if !stop.may_still_be_running.is_empty() {
        tracing::warn!(
            session_id = %session,
            run_id = %run,
            children = ?stop.may_still_be_running,
            "the cancelled turn's children may still be running past its stop grace"
        );
    }
    Ok(Pass::Again)
}

/// Release the session `ready` for the next build: what a draining node
/// does at a committed phase.
async fn drain_release(cx: &ActorContext) -> Result<Pass, TurnError> {
    cx.drain_release().await?;
    Ok(Pass::Released)
}

/// What one activation pass left.
pub(super) enum Pass {
    /// Run another pass.
    Again,
    /// Nothing to do: the actor stays hot until mail or idle eviction.
    Idle,
    /// The actor is released.
    Released,
    /// Ownership was lost.
    Lost,
}

#[async_trait::async_trait]
impl Activation for SessionActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let Ok(session) = SessionId::try_from(owned.actor().id().to_owned()) else {
            return Exit::Abandoned;
        };
        let cx = ActorContext::claimed(
            self.backend.clone(),
            &owned,
            AdmittedScope::session_operation(session.clone(), "activation"),
            CancellationToken::new(),
            Arc::clone(&self.probe),
        );
        // A session whose claims ended the activation-loop budget's claims
        // in a row with nothing committed (a panic, a read given up, a stop
        // that failed) parks, or ends a pending stop first (FIG-6096,
        // FIG-6099). A session whose claim requirements this node does not
        // serve was claimed for its close or an `Immediate` cancel alone
        // (FIG-6072).
        let cancel_only = owned.purpose() == lash_durable::ClaimPurpose::CancelOnly;
        match park::at_claim(&owned, &cx, &session, self.services.tracing()).await {
            Ok(park::Budget::Within) if !cancel_only => {}
            Ok(_) => {
                return turn_cancel::end_unserved(&owned, &cx, self.services.as_ref(), &session)
                    .await;
            }
            Err(exit) => return exit,
        }
        witness_claim(owned.claimed_on());
        // An owner with nothing to do keeps the actor hot for `idle_evict`,
        // reading its mailbox at every hint or poll, then releases it.
        let idle_evict = self.backend.config().settings().idle_evict;
        // What ended the owner's last wait, until the pass after it.
        let mut woke = None;
        let mut idle_since = None;
        // The owner cache of the session's head, for this claim's epoch.
        let mut heads = HeadCache::default();
        // Whether this claim asked if its idle session is to be carried to
        // this build's formats (FIG-5787).
        let mut carry_checked = false;
        // The passes in a row that failed with an error that does not pass
        // by itself: at the activation-loop budget the session parks
        // (FIG-5230), so a poison session holds no slot.
        let budget = self.backend.config().settings().activation_loop_budget;
        let mut failed_passes = 0_u32;
        loop {
            let release = idle_since.is_some_and(|since: std::time::Instant| {
                owned.clock().now().saturating_duration_since(since) >= idle_evict
            });
            // A draining node starts no pass: every commit before this one
            // is a committed phase the next build resumes from.
            let pass = if owned.draining() {
                drain_release(&cx).await
            } else {
                self.services.announce_head(&cx, &session).await;
                Box::pin(self.pass(&cx, &session, release, &mut heads, &mut carry_checked)).await
            };
            witness_pass(woke.take(), &pass);
            if pass.is_ok() {
                failed_passes = 0;
            }
            match pass {
                Ok(Pass::Again) => idle_since = None,
                Ok(Pass::Idle) => {
                    // Keep the actor hot, but not its decoded head: idle
                    // sessions with no readers reload on their next mail.
                    heads.evict();
                    idle_since.get_or_insert_with(|| owned.clock().now());
                    woke = Some(owned.wait_for_mail().await);
                }
                Ok(Pass::Released | Pass::Lost)
                | Err(TurnError::Durable(DurableError::OwnershipLost(_))) => return Exit::Released,
                // Anything else did not commit, or committed with its answer
                // lost: the next pass reloads the rows, the head among them,
                // and carries on from them. A lost epoch shows at its fenced
                // read.
                Err(error) => {
                    heads.evict();
                    if !passes_by_itself(&error) {
                        failed_passes = failed_passes.saturating_add(1);
                    }
                    // A parked state this build does not decode fails every
                    // pass of this build the same way: the session parks at
                    // once with the typed refusal, its turn open (FIG-5592).
                    let reason = match resume_refusal(&error) {
                        Some(reason) => Some(reason),
                        None => (failed_passes >= budget).then(|| SessionParkReason::PassLoop {
                            failed_passes,
                            error: SessionPassFailure::of(&error),
                        }),
                    };
                    if let Some(reason) = reason {
                        match park(&cx, &session, &reason, self.services.tracing()).await {
                            Ok(park::Parking::Committed) | Err(DurableError::OwnershipLost(_)) => {
                                return Exit::Released;
                            }
                            // A stop is pending, and may be what failed: it
                            // is never parked over. The next claim runs it
                            // first and counts this one (FIG-6099).
                            Ok(park::Parking::Stopping) => return Exit::Abandoned,
                            Err(park_error) => tracing::warn!(
                                session_id = %session,
                                %error,
                                %park_error,
                                "the session's park failed; retrying"
                            ),
                        }
                    }
                    tracing::debug!(%error, "session activation pass failed; reloading");
                    woke = Some(owned.wait_for_mail().await);
                }
            }
        }
    }
}

/// Count, for the perf witness, whether the claim that took the session was
/// rung by a hint or run by the claim poll.
fn witness_claim(claimed_on: lash_durable::runner::Wake) {
    #[cfg(feature = "perf-witness")]
    crate::perf_witness::wakes::record_claim(claimed_on == lash_durable::runner::Wake::Hint);
    #[cfg(not(feature = "perf-witness"))]
    let _ = claimed_on;
}

/// Count, for the perf witness, a pass after a wait a hint or the claim poll
/// ended (FIG-5932), by whether it found work: work found after the poll is
/// mail no hint reached.
fn witness_pass(woke: Option<lash_durable::runner::Wake>, pass: &Result<Pass, TurnError>) {
    #[cfg(feature = "perf-witness")]
    if let Some(wake @ (lash_durable::runner::Wake::Hint | lash_durable::runner::Wake::Poll)) = woke
    {
        crate::perf_witness::wakes::record_pass(
            wake == lash_durable::runner::Wake::Hint,
            matches!(pass, Ok(Pass::Again)),
        );
    }
    #[cfg(not(feature = "perf-witness"))]
    let _ = (woke, pass);
}

/// Whether `error` may pass by itself, so the next pass can succeed with
/// nothing changed: contention, an unreachable store, a lost
/// acknowledgement, or an admission whose mail a producer withdrew since the
/// drain read it (FIG-5262), which the next drain no longer finds. Such a
/// failure does not count toward the session's park.
fn passes_by_itself(error: &TurnError) -> bool {
    matches!(
        error,
        TurnError::Durable(
            DurableError::AckLost { .. }
                | DurableError::Domain(lash_durable::DomainRefusal::SessionMailMoved { .. })
                | DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Contended
                        | lash_durable::StoreFailureKind::Unavailable,
                    ..
                })
        )
    )
}

/// The inputs a drain hands the activation to admit as one turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedInputs {
    /// The run they open.
    pub run: TurnId,
    /// What the run's admission took.
    pub admission: crate::store::RunAdmissionRecord,
    /// The causes the taken inputs retained, in admission order; empty for
    /// a run that takes no input.
    pub causes: Vec<lash_trace::TraceCause>,
}

/// A new model call, prepared for its admission.
pub enum PreparedCall {
    /// Admit the call.
    Admit(Box<ComposedCall>),
    /// The call settles unsent with this error: its prompt failed closed,
    /// or it could not be lowered.
    Unsent(crate::LlmCallError),
    /// The protocol's before-call decision ended the turn instead (a frame
    /// switch or a refused hook): the machine moved on and nothing is
    /// admitted.
    Ended,
}

/// A new model call, ready to admit: the request, its composed prompt and
/// its request template, all of which `model.start` commits.
pub struct ComposedCall {
    /// The request the machine waits on, with the call's sections in it.
    pub request: Arc<LlmRequest>,
    /// The call's composition; `None` when the session has no sections.
    pub prompt: Option<crate::plugin::prompt::ComposedPrompt>,
    /// The template every send of the call fills and sends.
    pub template: RecordedRequestTemplate,
}

/// What a phase row's checkpoint holds: the machine's saved turn, the
/// steering input its checkpoints delivered, which the phase bound to the
/// run, and the turn's before-turn decisions, retained trace scope and
/// recorded run configuration, which a resume serves. It carries no plugin
/// state: the run's changed namespaces are rows of their own (FIG-5301).
/// Encoded by the phase runner, its owner, as its PhaseRoot.
pub struct PhaseCheckpoint {
    /// The machine's checkpoint with the content it names.
    pub saved: SavedTurn<HostTurnProtocol>,
    /// The steering input the turn's checkpoints delivered, with its
    /// application evidence ([`TurnDrive::delivered_inputs`]).
    pub delivered: Vec<crate::AdmittedTurnInputs>,
    /// The turn's before-turn callback decisions
    /// ([`TurnDrive::before_turn`]), which a resume serves instead of
    /// running the callbacks again.
    pub before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    /// The trace scope the turn's admission retained
    /// ([`TurnDrive::trace_scope`]), which a resume reads back (FIG-5363);
    /// before the first phase commit, the turn's admission record holds it
    /// (FIG-5395).
    pub trace: Option<lash_trace::DurableTraceScope>,
    /// The configuration the run resolved before its first durable effect
    /// and the tool surface it installed last ([`TurnDrive::run_config`]),
    /// which a resume and a cancel restore serve instead of resolving again
    /// (FIG-6021). `None` for a drive that resolves no configuration.
    pub config: Option<super::run_config::RecordedRunConfig>,
    /// The first start of the cell the phase holds, which has not answered
    /// ([`TurnDrive::cell_start`]): what a resume closes the cell's span
    /// from, and the terminal that discards it.
    pub cell: Option<CellStart>,
}

/// A code cell's first start, as the `Tools` phase that holds the cell
/// retains it (§3.4): the cell's one span starts there, whichever owner, or
/// how many suspensions later, closes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellStart {
    /// The cell's effect.
    pub effect: EffectId,
    /// The cell's language.
    pub language: String,
    /// Wall-clock epoch milliseconds of the cell's first start.
    pub first_started_at_ms: u64,
}

/// The closing record of a code cell's span, held until the commit that
/// consumes the cell's answer, or the terminal that discards the cell, is
/// acknowledged, and then published once (P1). Dropped unpublished when
/// that commit fails: the owner that commits instead closes the cell.
#[must_use = "a cell's span closes only when its close is published"]
pub struct CellClose(Box<dyn FnOnce() + Send + Sync>);

impl CellClose {
    pub(crate) fn new(publish: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self(Box::new(publish))
    }

    /// Publish the close: its commit was acknowledged.
    pub fn publish(self) {
        (self.0)();
    }
}

/// A cell its turn's terminal stops before it answered
/// ([`TurnDrive::discarded_cell`]).
#[derive(Debug)]
pub struct DiscardedCell {
    /// The close of the cell's span, `discarded`.
    pub close: CellClose,
    /// The calls the cell admitted that have no outcome, with their recorded trace names or typed identities.
    pub open: Vec<(
        lash_core_execution::runtime::actor::round::ExecutionDraft,
        lash_trace::TraceToolName,
    )>,
}

impl std::fmt::Debug for CellClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellClose").finish_non_exhaustive()
    }
}

impl PhaseCheckpoint {
    /// Stages this checkpoint in `tx` as a phase advance does, and returns
    /// the PhaseRoot a row's phase names.
    ///
    /// # Errors
    ///
    /// The account of a checkpoint that does not encode.
    #[cfg(any(test, feature = "testing"))]
    pub fn stage_for_test(
        &self,
        tx: &mut lash_durable::ActorTx,
    ) -> Result<lash_durable::domain::checkpoint_graph::NodeId, String> {
        Ok(super::phase_root::StagedPhase::of(self, &[])?
            .staged_in(tx)
            .0)
    }

    /// Stages in `tx` the PhaseRoot `root` of `graph` with its checkpoint
    /// text made `edit` of it, as another build would have written it, and
    /// returns the new root.
    ///
    /// # Errors
    ///
    /// The account of a root that does not read.
    #[cfg(any(test, feature = "testing"))]
    pub fn restage_text_for_test(
        graph: &lash_durable::domain::HydratedGraph,
        root: lash_durable::domain::checkpoint_graph::NodeId,
        edit: fn(&str) -> String,
        tx: &mut lash_durable::ActorTx,
    ) -> Result<lash_durable::domain::checkpoint_graph::NodeId, String> {
        super::phase_root::restaged_text(graph, root, edit, tx)
    }
}

/// What a resumed turn's preparation serves from its checkpoint instead of
/// deciding again: its before-turn callback decisions (ADR 0133 §6), the
/// trace scope its admission retained, and the run's recorded
/// configuration (FIG-6021), so no before-turn callback runs, no admission
/// is proposed and no configuration is resolved once a phase has committed.
pub struct RecordedPreparation {
    /// The before-turn callback decisions.
    pub before_turn: Vec<crate::plugin::RecordedTurnContribution>,
    /// The retained trace scope; `None` for a drive that retains none.
    pub trace: Option<lash_trace::DurableTraceScope>,
    /// The run's resolved shape and installed tool surface.
    pub config: super::run_config::RecordedRunConfig,
    /// The first start of the cell the phase holds, if any.
    pub cell: Option<CellStart>,
}

/// A turn restored from its rows: the machine, the effect it re-delivers,
/// the plugin namespaces its run changed, and its row.
pub struct RestoredTurn {
    /// The machine, restored from the checkpoint.
    pub machine: TurnMachine,
    /// The namespaces the turn's run changed, as its rows record them,
    /// which the resumed drive reinstalls over the head.
    pub namespaces: Vec<lash_durable::domain::TurnNamespace>,
    /// The steering input the turn's checkpoints delivered before its last
    /// phase committed, which the resumed drive settles with its commit.
    pub delivered: Vec<crate::AdmittedTurnInputs>,
    /// The effect the checkpoint re-delivers, if it is waiting on one.
    pub pending: Option<Effect>,
    /// The pinned call's receipt, which no derivation has answered, if
    /// `model.answer` committed one beside the phase.
    pub answered: Option<ModelAnswer>,
    /// The turn's row.
    pub row: TurnRow,
}

/// A turn an owner runs: its drive, the effect a restore re-delivers, and
/// its row.
pub struct OpenTurn {
    /// The turn's drive, holding its machine.
    pub drive: Box<dyn TurnDrive>,
    /// The effect the checkpoint re-delivers, if it is waiting on one.
    pub pending: Option<Effect>,
    /// The turn's row.
    pub row: TurnRow,
}

/// How a phase run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseExit {
    /// The turn committed, ending its run as this kind.
    Committed(crate::store::RunTerminalKind),
    /// Nothing is runnable: release as `waiting` until `due` or mail.
    Suspended {
        /// The earliest due time.
        due: Option<DurableInstant>,
    },
    /// Its last boundary also gave up the actor; no release write remains.
    Released,
    /// Ownership was lost: drop everything held for the actor.
    Lost,
    /// The turn stopped for its session's requested close, or for a cancel
    /// it accepted once its machine had finished, before `turn.commit`: its
    /// in-memory work stopped, and the next pass ends it from its rows.
    CancelRequested,
    /// The turn stopped between steps for the cancel request it accepted,
    /// and its stop committed with its terminal: the turn is over, and its
    /// cancel's children are still to be awaited.
    Cancelled,
    /// The node is draining: the turn stopped at a committed phase, before
    /// starting its next model call or cell, and the actor is released for
    /// the next build to resume from its rows (ADR 0106 §1).
    Drained,
}

/// Why a turn was not admitted; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnAdmitRefusal {
    /// The session already has an unfinished turn.
    #[error("session {0} already has an unfinished turn")]
    OpenTurnExists(SessionId),
}

/// The paid answer of receipt `root`, which `row`'s model phase names.
///
/// # Errors
///
/// [`TurnRestoreError`] when the receipt does not read or decode.
pub(super) async fn paid_answer(
    cx: &ActorContext,
    row: &TurnRow,
    root: lash_durable::domain::checkpoint_graph::NodeId,
) -> Result<ModelAnswer, TurnRestoreError> {
    let limits = cx.backend().config().settings().graph_reads.owner();
    let graph = cx
        .durable_reads()?
        .hydrate(
            lash_durable::domain::graph::RootOwner::ModelAnswer {
                session: row.session.clone(),
                run: row.run.clone(),
            },
            Some(root),
            limits,
        )
        .await?;
    lash_core_execution::runtime::actor::model_receipt::ModelReceipt::<ModelAnswer>::load(
        &graph, root, limits,
    )
    .map(|receipt| receipt.answer)
    .map_err(|reason| TurnRestoreError::Undecodable {
        run: row.run.clone(),
        reason,
    })
}

/// Why a turn could not be restored from its rows.
#[derive(Debug, thiserror::Error)]
pub enum TurnRestoreError {
    /// The row names no checkpoint to restore from.
    #[error("turn {0} has no checkpoint")]
    NoCheckpoint(TurnId),
    /// The checkpoint does not restore.
    #[error(transparent)]
    Checkpoint(#[from] lash_sansio::TurnCheckpointRestoreError),
    /// The committed window the checkpoint pins cannot be read.
    #[error("turn {run}'s window cannot be read: {source}")]
    Window {
        /// The turn.
        run: TurnId,
        /// Why.
        #[source]
        source: Box<TurnError>,
    },
    /// The stored checkpoint does not decode.
    #[error("turn {run} checkpoint does not decode: {reason}")]
    Undecodable {
        /// The turn.
        run: TurnId,
        /// Why.
        reason: String,
    },
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
}

/// Why a phase run failed.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// The runtime failed.
    #[error(transparent)]
    Runtime(#[from] crate::RuntimeError),
    /// The turn could not be admitted.
    #[error(transparent)]
    Admit(#[from] TurnAdmitRefusal),
    /// The session's mailbox could not be drained.
    #[error(transparent)]
    Mail(#[from] SessionMailError),
    /// The session's close stopped at a step; the next claim resumes it.
    #[error(transparent)]
    Close(Box<SessionCloseError>),
    /// The turn could not be restored.
    #[error(transparent)]
    Restore(#[from] TurnRestoreError),
    /// A cancelled turn's stop could not wait for its children.
    #[error(transparent)]
    Stop(#[from] TurnChildrenStopError),
    /// A pinned model call was re-delivered with another request: its
    /// checkpoint no longer re-yields the call it pinned.
    #[error("the pinned model request {pinned} was re-delivered as {redelivered}")]
    ModelPinBroken {
        /// The pinned request's reference.
        pinned: String,
        /// The re-delivered request's reference.
        redelivered: String,
    },
    /// A state the turn resumes from is in a format this build does not
    /// decode. The turn commits nothing for it: its session parks with the
    /// refusal ([`SessionParkReason::UndecodableState`]).
    #[error("the turn's {state:?} is in a format this build does not decode: {reason}")]
    UndecodableState {
        /// Which state.
        state: ParkedTurnState,
        /// The decoder's account.
        reason: String,
    },
    /// An effect the turn yielded could not run, or its lane does not run it
    /// on this path yet.
    #[error("{0}")]
    Exec(String),
}

/// Admit `inputs` as the session's turn on `tx`: the turn row, the bound
/// inputs and the turn deadline (`turn.admit`).
///
/// The store refuses the commit with `OpenTurnExists` when the session has
/// an unfinished turn; binding the inputs' rows is L3s's (FIG-5196) drain,
/// and a host turn deadline is L3's.
///
/// # Errors
///
/// [`TurnAdmitRefusal`]; nothing is recorded.
pub async fn admit_turn(
    cx: &ActorContext,
    tx: &mut ActorTx,
    inputs: AdmittedInputs,
) -> Result<TurnRow, TurnAdmitRefusal> {
    let session = session_of(cx);
    tx.write(DomainWrite::Turn(TurnWrite::Admit {
        session: session.clone(),
        run: inputs.run.clone(),
        admission: inputs.admission.clone(),
        turn_deadline: None,
    }));
    Ok(TurnRow {
        session,
        run: inputs.run,
        admission: inputs.admission,
        phase: UnfinishedPhase::Admitted,
        iteration: 0,
        model_calls: 0,
        tool_calls: 0,
        turn_deadline: None,
        working: None,
        written_epoch: cx.epoch(),
        cancel: None,
    })
}

#[expect(
    clippy::expect_used,
    reason = "a session actor's id is a session id by construction"
)]
fn session_of(cx: &ActorContext) -> SessionId {
    SessionId::try_from(cx.actor().id().to_owned()).expect("a session actor names its session")
}

/// The restore of one turn from its checkpoint, handed to
/// [`TurnServices::resume`]: it runs at most once, so a takeover performs
/// exactly one `restore_from_checkpoint`, reported to the context's probe.
pub struct TurnRestore<'a> {
    cx: &'a ActorContext,
    row: &'a TurnRow,
    heads: &'a mut HeadCache,
    /// The phase checkpoint, once read, with the nodes its content is
    /// stored as.
    phase: Option<(PhaseCheckpoint, super::phase_root::PhaseStored)>,
}

impl<'a> TurnRestore<'a> {
    /// The restore of `row`'s turn on `cx`, over the head `heads` holds:
    /// what the activation hands [`TurnServices::resume`] when it takes the
    /// turn over.
    #[must_use]
    pub fn new(cx: &'a ActorContext, row: &'a TurnRow, heads: &'a mut HeadCache) -> Self {
        Self {
            cx,
            row,
            heads,
            phase: None,
        }
    }

    /// The turn's phase checkpoint, read from its PhaseRoot under one
    /// protection the first time it is asked for.
    async fn phase(&mut self) -> Result<&PhaseCheckpoint, TurnRestoreError> {
        let phase = match self.phase.take() {
            Some(phase) => phase,
            None => {
                let row = self.row;
                let root = row
                    .phase
                    .checkpoint()
                    .ok_or_else(|| TurnRestoreError::NoCheckpoint(row.run.clone()))?;
                let graph = self
                    .cx
                    .durable_reads()?
                    .hydrate(
                        lash_durable::domain::graph::RootOwner::TurnPhase {
                            session: row.session.clone(),
                            run: row.run.clone(),
                        },
                        Some(root),
                        self.cx.backend().config().settings().graph_reads.owner(),
                    )
                    .await?;
                super::phase_root::load(
                    &graph,
                    root,
                    self.cx.backend().config().settings().graph_reads.owner(),
                )
                .map_err(|reason| TurnRestoreError::Undecodable {
                    run: row.run.clone(),
                    reason,
                })?
            }
        };
        Ok(&self.phase.insert(phase).0)
    }

    /// The turn's row.
    #[must_use]
    pub fn row(&self) -> &TurnRow {
        self.row
    }

    /// What the turn's checkpoint records for preparing the resumed turn:
    /// its before-turn callback decisions, its retained trace scope and its
    /// run's recorded configuration, so no before-turn callback runs, no
    /// admission is proposed and no configuration is resolved once a phase
    /// has committed (ADR 0133 §6, FIG-5363, FIG-6021).
    ///
    /// # Errors
    ///
    /// [`TurnRestoreError`] when the row holds no checkpoint, it does not
    /// decode, or it records no run configuration.
    pub async fn recorded_preparation(&mut self) -> Result<RecordedPreparation, TurnRestoreError> {
        let run = self.row.run.clone();
        let phase = self.phase().await?;
        let config = phase
            .config
            .clone()
            .ok_or_else(|| TurnRestoreError::Undecodable {
                run,
                reason: "the turn checkpoint records no run configuration".to_owned(),
            })?;
        Ok(RecordedPreparation {
            before_turn: phase.before_turn.clone(),
            trace: phase.trace.clone(),
            config,
            cell: phase.cell.clone(),
        })
    }

    /// Restore the turn's machine under `config`, the configuration it was
    /// built with, over the committed window its checkpoint pins, and the
    /// effect its checkpoint re-delivers. The window is the head the owner
    /// cached when the checkpoint pins that head, and is read back from the
    /// session store at the pin only when the head has moved past it.
    ///
    /// # Errors
    ///
    /// [`TurnRestoreError`].
    pub async fn restore(
        mut self,
        config: TurnMachineConfig,
    ) -> Result<RestoredTurn, TurnRestoreError> {
        let row = self.row;
        self.phase().await?;
        let Some((
            PhaseCheckpoint {
                saved, delivered, ..
            },
            stored,
        )) = self.phase.take()
        else {
            return Err(TurnRestoreError::NoCheckpoint(row.run.clone()));
        };
        // A paid answer restores with its receipt, which the resumed turn
        // derives; a call in flight restores to be sent again.
        let answered = match &row.phase {
            UnfinishedPhase::Model {
                call: ModelCall::Answered { receipt },
                ..
            } => Some(paid_answer(self.cx, row, *receipt).await?),
            UnfinishedPhase::Admitted
            | UnfinishedPhase::Model {
                call: ModelCall::Requested,
                ..
            }
            | UnfinishedPhase::Tools { .. } => None,
        };
        let window = match saved.checkpoint.window_pin() {
            Some(pin) => Some(
                self.heads
                    .window_at(self.cx, &row.session, pin)
                    .await
                    .map_err(|source| TurnRestoreError::Window {
                        run: row.run.clone(),
                        source: Box::new(source),
                    })?,
            ),
            None => None,
        };
        let namespaces = self
            .cx
            .durable_reads()?
            .turn_namespaces(&row.session, &row.run)
            .await?;
        self.cx.probe().checkpoint_restored(&row.session, &row.run);
        let mut machine = TurnMachine::restore_from_checkpoint(config, saved, window)?;
        // The turn's next phase builds on the content this one stored.
        stored.restored(&mut machine);
        let pending = next_work(&mut machine);
        Ok(RestoredTurn {
            machine,
            namespaces,
            delivered,
            pending,
            answered,
            row: row.clone(),
        })
    }
}

/// The machine's next effect that needs work from the host, passing over
/// what only reports: emits, progress and logs. Their content is the
/// machine's own state, committed with its next checkpoint.
fn next_work(machine: &mut TurnMachine) -> Option<Effect> {
    loop {
        match machine.poll_effect()? {
            Effect::Emit(_) | Effect::Progress { .. } | Effect::Log { .. } => {}
            effect => return Some(effect),
        }
    }
}

/// Request a cancel of one of a session's turns, from outside the session
/// actor: one mailbox transaction records it on the turn's cancel-request
/// row and control-wakes the session (`mail.session`). The owner sees it on
/// the turn's row at its next fenced read, through the wake hint or its
/// poll; the next owner sees it after a crash.
///
/// # Errors
///
/// The store's refusal; nothing was written.
pub async fn request_turn_cancel(
    backend: &Backend,
    request: TurnCancelRequest,
) -> Result<TurnCancelAnswer, DurableError> {
    let mut tx = MailTx::new();
    tx.write(MailDomainWrite::RequestTurnCancel(request));
    let mut commit = backend.commit_mail(tx, CommitLabel::MAIL_SESSION).await?;
    match commit.answers.pop() {
        Some(MailAnswer::RequestTurnCancel(answer)) if commit.answers.is_empty() => Ok(answer),
        other => Err(DurableError::Store(lash_durable::StoreFailure {
            kind: lash_durable::StoreFailureKind::Corrupt,
            message: format!("a turn cancel request was answered with {other:?}"),
        })),
    }
}

/// Cancel the session's unfinished turn from inside the session actor, in
/// the caller's transaction: its `Cancelled` terminal, with `cause` as its
/// typed cause. The caller commits `tx` under its own label.
///
/// It touches no in-memory state: the session activation runs one thing at
/// a time, so no phase of the turn runs while the caller holds `tx`.
///
/// # Errors
///
/// [`TurnError::Durable`] when the turn row cannot be read.
pub async fn cancel_open_turn(
    cx: &ActorContext,
    tx: &mut ActorTx,
    cause: &crate::runtime::TurnCancellationEvidence,
) -> Result<Option<TurnRow>, TurnError> {
    let Some(row) = cx.durable_reads()?.turn(&session_of(cx)).await? else {
        return Ok(None);
    };
    tx.write(DomainWrite::Turn(TurnWrite::Terminal {
        session: row.session.clone(),
        run: row.run.clone(),
        cause: Box::new(cancelled_cause(cause.clone())),
        head_revision: None,
    }));
    Ok(Some(row))
}

/// The evidence of the cancel `request` the turn accepted.
#[must_use]
pub fn cancel_evidence(request: &TurnCancelRequest) -> crate::runtime::TurnCancellationEvidence {
    crate::runtime::TurnCancellationEvidence {
        request_id: request.request_id.clone(),
        origin: request.origin.clone(),
        reason: request.reason.clone(),
        undelivered: request.undelivered,
        mode: request.mode,
        honoured_after_step: None,
    }
}
