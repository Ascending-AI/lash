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

use lash_durable::domain::{MailAnswer, MailDomainWrite, ParkEventWrite, TurnWrite};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableProbe, MailTx, Release,
};
use lash_sansio::SavedTurn;
use tokio_util::sync::CancellationToken;

use super::head::{HeadCache, SessionHead};
use super::{phases, turn_cancel};
use crate::{
    ActorContext, AdmittedScope, Backend, Effect, EffectId, HostTurnProtocol, LlmRequest,
    SessionId, TurnId, TurnMachine, TurnMachineConfig, TurnOutcome,
};

pub use lash_durable::domain::{
    ModelPin, TurnCancelAnswer, TurnCancelRequest, TurnRow, UnfinishedPhase,
};

use super::session_close::{
    SESSION_CLOSE_MAIL, SessionCloseError, SessionCloseExit, begin_session_close, run_session_close,
};
use super::session_mail::{SessionMailError, drain_session_mail};
use super::turn_scope::{TurnChildrenStopError, await_turn_children, continue_scope_ends};

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
}

/// One turn an owner runs: its machine and the in-memory work around it. The
/// phase runner polls the machine through it, commits at the turn's labels
/// and hands each effect to the method that answers it; every method answers
/// the machine itself.
#[async_trait::async_trait]
pub trait TurnDrive: Send {
    /// The turn's machine.
    fn machine(&mut self) -> &mut TurnMachine;

    /// Answer an effect that commits nothing of its own: an emit, progress,
    /// a log, a plugin checkpoint, an environment sync or a tool report.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the effect cannot be answered.
    async fn local(&mut self, cx: &ActorContext, effect: Effect) -> Result<(), TurnError>;

    /// Run attempt `attempt` of the pinned model call `id`, bounded by
    /// `limit` (its deadline is the pinned one, never refreshed), and answer
    /// the machine with its result. Deltas it streams go to the session's
    /// live stream; only the completed response is durable.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the call aborts the turn rather than answering it.
    async fn model_call(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        request: Arc<LlmRequest>,
        attempt: u32,
        limit: crate::ExecutionLimit,
    ) -> Result<(), TurnError>;

    /// Restart the session's live stream before a re-sent model call streams:
    /// existing cursors gap and observers reload (the live replay store's
    /// `invalidate_session`), so no one sees an abandoned attempt's partial
    /// text joined to the new attempt's.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the live stream cannot restart; the call is not
    /// sent.
    async fn restart_live_stream(&mut self, cx: &ActorContext) -> Result<(), TurnError>;

    /// The tools the turn's rounds run: the turn's catalog, which pins each
    /// call at admission and gives each member its body (L4, FIG-5174).
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the turn's catalog does not resolve.
    fn tools(
        &mut self,
    ) -> Result<Arc<dyn lash_core_execution::runtime::actor::round::RoundTools>, TurnError>;

    /// Run the code cell of effect `id` from its latest snapshot, or from the
    /// start, to its end, and answer the machine. The cell files its
    /// snapshots under its own execution; `model.done` committed the turn's
    /// `Tools` phase that re-delivers it before it started.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the cell could not reach its end; ownership loss
    /// among them.
    async fn exec_cell(
        &mut self,
        cx: &ActorContext,
        id: EffectId,
        cell: CodeCell,
    ) -> Result<CellExit, TurnError>;

    /// The turn accepted an `Immediate` cancel while its cell ran, and the
    /// phase runner dropped the cell: stop what the cell started that its
    /// future does not own, its tool bodies' cooperative stop among it.
    fn stop_cell(&mut self);

    /// The turn's commit to its session once its machine is done: the next
    /// revision of `head`, the session's head as the owner loaded it, which
    /// `turn.commit` publishes with the turn's terminal. The store refuses
    /// it once the head is elsewhere.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the commit cannot be built.
    async fn finish(
        &mut self,
        cx: &ActorContext,
        done: TurnDone,
        head: &SessionHead,
    ) -> Result<TurnCommit, TurnError>;

    /// The turn's `turn.commit` was acknowledged: publish what the turn held
    /// back for it, its stop's terminal among it (ADR 0122), and everything
    /// queued before it. A drive whose commit was refused, or whose
    /// acknowledgement was lost, is dropped without it and publishes none of
    /// what it held.
    async fn committed(&mut self);
}

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
        let committed = crate::store::RunCommittedOutcome::of_turn_outcome(&outcome);
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
    crate::store::RunTerminalCause::Cancelled { evidence }
}

/// What `turn.commit` writes for a finished turn: the session head's next
/// revision. Its terminal is the finished machine's ([`TurnDone`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnCommit {
    /// The head revision the commit replaces.
    pub expected_head: u64,
    /// The session head's commit, encoded by
    /// [`encode_session_commit`](crate::store::encode_session_commit).
    pub commit_json: String,
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
    /// nothing to do, stay hot ([`Pass::Idle`]) or release the actor when
    /// `release`.
    async fn pass(
        &self,
        cx: &ActorContext,
        session: &SessionId,
        release: bool,
        heads: &mut HeadCache,
    ) -> Result<Pass, TurnError> {
        // A turn's scope whose cascade a crash cut short is marked to its end
        // before anything else (L6b).
        continue_scope_ends(cx, session).await?;
        let mut tx = cx.begin().await?;
        if let Some(seq) = tx
            .mail()
            .iter()
            .filter(|mail| mail.kind.as_str() == SESSION_CLOSE_MAIL)
            .map(|mail| mail.seq)
            .max()
        {
            begin_session_close(&mut tx, session);
            tx.ack_through(seq);
            cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN).await?;
            return Ok(Pass::Again);
        }
        // A closing session admits nothing: it runs its close (L6b).
        if cx
            .durable_reads()?
            .session_close(session)
            .await?
            .is_some_and(|close| !close.is_tombstone())
        {
            drop(tx);
            return match run_session_close(cx, session)
                .await
                .map_err(|error| TurnError::Close(Box::new(error)))?
            {
                Some(SessionCloseExit::Closed) => Ok(Pass::Released),
                Some(SessionCloseExit::Waiting) => {
                    let mut tx = cx.begin().await?;
                    tx.give_up(Release::Waiting {
                        next_due: cx.next_due(),
                    });
                    cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
                    Ok(Pass::Released)
                }
                None => Ok(Pass::Again),
            };
        }
        let open = cx.durable_reads()?.turn(session).await?;
        let Some(row) = open else {
            // No unfinished turn: the mailbox says what runs next (L3s).
            let drain = drain_session_mail(cx, &mut tx).await?;
            return match drain.admit {
                Some(admitted) if admitted.admission.is_turn() => {
                    admit_turn(cx, &mut tx, admitted).await?;
                    // The turn's rows are this build's session state:
                    // from now only a node that decodes it claims the
                    // session.
                    tx.stamp_formats(cx.backend().formats().session().clone());
                    cx.commit(tx, CommitLabel::TURN_ADMIT).await?;
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
        if let Some(request) = row.cancel.clone() {
            turn_cancel::finalize(cx, &row, &request).await?;
            // The stop then waits for the children its cancel marked (G1b,
            // L6b): the rest of its cascade first, then their terminals,
            // bounded by the stop's grace. A child still running at the
            // grace keeps its cancel, and the stop completes anyway.
            continue_scope_ends(cx, session).await?;
            let stop = await_turn_children(
                cx,
                session,
                &row.run,
                self.services.execution_budgets(session).stop_grace(),
                self.backend.config().settings().cascade_batch,
            )
            .await?;
            if !stop.may_still_be_running.is_empty() {
                tracing::warn!(
                    %session,
                    run = %row.run,
                    children = ?stop.may_still_be_running,
                    "the cancelled turn's children may still be running past its stop grace"
                );
            }
            return Ok(Pass::Again);
        }
        let opened = match row.phase {
            UnfinishedPhase::Admitted => {
                let head = heads.head(cx, session).await?;
                self.services
                    .start(cx, &row, head)
                    .await
                    .map(|drive| OpenTurn {
                        drive,
                        pending: None,
                        row: row.clone(),
                    })
            }
            UnfinishedPhase::Model { .. } | UnfinishedPhase::Tools { .. } => {
                self.services
                    .resume(cx, TurnRestore::new(cx, &row, heads))
                    .await
            }
        };
        let turn = match opened {
            Ok(turn) => turn,
            // A turn its committed state cannot prepare, on any pass (a
            // plugin build its recorded config refuses, FIG-5246), ends
            // with that refusal as its run's cause: no pass would clear it.
            Err(TurnError::Runtime(refusal)) if refusal.is_terminal() => {
                phases::refuse(cx, &row, refusal).await?;
                return Ok(Pass::Again);
            }
            Err(error) => return Err(error),
        };
        match phases::run_phases(cx, self.services.as_ref(), turn, heads).await? {
            PhaseExit::Committed(_) | PhaseExit::CancelRequested => Ok(Pass::Again),
            PhaseExit::Lost => Ok(Pass::Lost),
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

/// Release the session `ready` for the next build: what a draining node
/// does at a committed phase.
async fn drain_release(cx: &ActorContext) -> Result<Pass, TurnError> {
    cx.drain_release().await?;
    Ok(Pass::Released)
}

/// What one activation pass left.
enum Pass {
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
        // A claim never takes a session in a set its node does not decode;
        // one adopted in another set goes back unread.
        if owned.purpose() == lash_durable::ClaimPurpose::CancelOnly {
            let mut tx = match cx.begin().await {
                Ok(tx) => tx,
                Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                Err(_) => return Exit::Abandoned,
            };
            tx.give_up(Release::Idle);
            return match cx.commit(tx, CommitLabel::SESSION_RELEASE).await {
                Ok(_) | Err(DurableError::OwnershipLost(_)) => Exit::Released,
                Err(_) => Exit::Abandoned,
            };
        }
        // An owner with nothing to do keeps the actor hot for `idle_evict`,
        // reading its mailbox at every hint or poll, then releases it.
        let idle_evict = self.backend.config().settings().idle_evict;
        let mut idle_since = None;
        // The owner cache of the session's head, for this claim's epoch.
        let mut heads = HeadCache::default();
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
                self.pass(&cx, &session, release, &mut heads).await
            };
            if pass.is_ok() {
                failed_passes = 0;
            }
            match pass {
                Ok(Pass::Again) => idle_since = None,
                Ok(Pass::Idle) => {
                    idle_since.get_or_insert_with(|| owned.clock().now());
                    owned.wait_for_mail().await;
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
                    if failed_passes >= budget {
                        let reason = SessionParkReason::PassLoop {
                            failed_passes,
                            error: error.to_string(),
                        };
                        match park(&cx, &reason).await {
                            Ok(()) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                            Err(park_error) => tracing::warn!(
                                %session,
                                %error,
                                %park_error,
                                "the session's park failed; retrying"
                            ),
                        }
                    }
                    tracing::debug!(%error, "session activation pass failed; reloading");
                    owned.wait_for_mail().await;
                }
            }
        }
    }
}

/// Whether `error` may pass by itself, so the next pass can succeed with
/// nothing changed: contention, an unreachable store or a lost
/// acknowledgement. Such a failure does not count toward the session's
/// park.
fn passes_by_itself(error: &TurnError) -> bool {
    matches!(
        error,
        TurnError::Durable(
            DurableError::AckLost { .. }
                | DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Contended
                        | lash_durable::StoreFailureKind::Unavailable,
                    ..
                })
        )
    )
}

/// Why a session actor parked: the park feed's typed reason. A parked
/// session runs nothing until an operator redrives it or a control wake (a
/// turn cancel) readies it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionParkReason {
    /// Its passes failed `failed_passes` times in a row within one claim,
    /// each with an error a retry does not clear (an undecodable checkpoint
    /// among them): the activation-loop budget (FIG-5230).
    PassLoop {
        /// The passes in a row that failed.
        failed_passes: u32,
        /// The last pass's error.
        error: String,
    },
}

impl SessionParkReason {
    /// The park feed's encoding.
    #[expect(
        clippy::expect_used,
        reason = "a park reason is plain data whose encoding cannot fail"
    )]
    #[must_use]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a park reason encodes")
    }
}

/// Park the session with `reason`: its park-feed entry and its release as
/// `parked`, one commit. Its unread mail stays for the claim that follows a
/// redrive.
async fn park(cx: &ActorContext, reason: &SessionParkReason) -> Result<(), DurableError> {
    tracing::warn!(actor = %cx.actor(), reason = %reason.encode(), "session parked");
    let mut tx = cx.begin().await?;
    tx.write(DomainWrite::ParkEvent(ParkEventWrite::Park {
        reason_json: reason.encode(),
    }));
    tx.give_up(Release::Parked);
    cx.commit(tx, CommitLabel::SESSION_RELEASE).await.map(drop)
}

/// The inputs a drain hands the activation to admit as one turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedInputs {
    /// The run they open.
    pub run: TurnId,
    /// What the run's admission took.
    pub admission: crate::store::RunAdmissionRecord,
}

/// A turn restored from its rows: the machine, the effect it re-delivers,
/// and its row.
pub struct RestoredTurn {
    /// The machine, restored from the checkpoint.
    pub machine: TurnMachine,
    /// The effect the checkpoint re-delivers, if it is waiting on one.
    pub pending: Option<Effect>,
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
    /// Ownership was lost: drop everything held for the actor.
    Lost,
    /// The turn accepted a cancel request it honours here: its in-memory
    /// work stopped, and the next pass finalizes it from its row.
    CancelRequested,
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
        turn_deadline: None,
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
}

impl<'a> TurnRestore<'a> {
    /// The restore of `row`'s turn on `cx`, over the head `heads` holds:
    /// what the activation hands [`TurnServices::resume`] when it takes the
    /// turn over.
    #[must_use]
    pub fn new(cx: &'a ActorContext, row: &'a TurnRow, heads: &'a mut HeadCache) -> Self {
        Self { cx, row, heads }
    }

    /// The turn's row.
    #[must_use]
    pub fn row(&self) -> &TurnRow {
        self.row
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
        self,
        config: TurnMachineConfig,
    ) -> Result<RestoredTurn, TurnRestoreError> {
        let row = self.row;
        let stored = row
            .phase
            .checkpoint()
            .ok_or_else(|| TurnRestoreError::NoCheckpoint(row.run.clone()))?;
        let saved: SavedTurn<HostTurnProtocol> =
            serde_json::from_str(stored).map_err(|error| TurnRestoreError::Undecodable {
                run: row.run.clone(),
                reason: error.to_string(),
            })?;
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
        self.cx.probe().checkpoint_restored(&row.session, &row.run);
        let mut machine = TurnMachine::restore_from_checkpoint(config, saved, window)?;
        let pending = next_work(&mut machine);
        Ok(RestoredTurn {
            machine,
            pending,
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
