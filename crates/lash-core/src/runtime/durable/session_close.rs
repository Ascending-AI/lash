//! The session's closing state (ADR 0132 §11, §12). Owned by L6b
//! (FIG-5176).
//!
//! A deletion is session mail ([`request_session_close`]): the session
//! actor, not the caller, closes the session. The session activation drains
//! the mail and enters the closing state on the drain's transaction
//! ([`begin_session_close`]), which also closes the session's scope: from
//! that commit on no process registers under the session or inside it, in
//! any of its turns. Then, on that claim and on every later one,
//! [`run_session_close`] runs the steps that remain, each its own fenced
//! transaction under its `session.close.*` label, so a crash at any step
//! resumes at that step:
//!
//! 1. `cancel`: the open turn is cancelled (L3's cancel path) and its scope
//!    ends ([`end_turn_scope`]);
//! 2. `revoke`: the session's waits are revoked;
//! 3. `end_scope`: the session's `Until` processes are marked for cancel,
//!    batched; the step is recorded with the last batch;
//! 4. `triggers`: once no process is live anywhere in the session's scope
//!    tree, its trigger subscriptions are deleted;
//! 5. `artifacts`: the session's storage is deleted, and that delete's
//!    transaction fences the session's artifact referrers and arms their
//!    `ArtifactCleanup` obligations (ADR 0113), the one outbox kind a close
//!    leaves: it deletes bytes outside the database;
//! 6. `tombstone`: the session's process state is deleted and the close row
//!    becomes its tombstone, and the actor ends.
//!
//! Logical cleanup settles before anything is deleted: no state is deleted
//! while a process of the session is non-terminal. A parent's terminal does
//! not mean its children have stopped, so step 4 reads
//! `live_until_descendants` of the session, which walks below ended
//! processes and through the session's turn scopes, whose children a turn's
//! end marked but which may still be in their grace. While one remains, it
//! pins a `process_terminal` wait on it and releases as waiting; the
//! terminal wakes the session. A process that ended between that read and
//! the wait's commit resolved no wait, so the pin reads the registry after
//! its commit and resolves the wait itself. The deletion steps are idempotent, so a
//! crash between a delete and its step's commit repeats the delete, which
//! finds nothing.

use crate::store::SessionLookup;
use lash_core_execution::runtime::actor::waits;
use lash_durable::domain::{
    SESSION_ACTOR_FORMATS, ScopeKey, SessionCloseRow, SessionCloseStep, SessionCloseWrite,
};
use lash_durable::{
    ActorKey, ActorTx, CommitLabel, DomainWrite, DurableError, FormatSet, MailKind, MailRefusal,
    MailTx, Release,
};

use super::session::{TurnError, cancel_open_turn};
use super::turn_scope::{continue_scope_ends, end_turn_scope, mark_until_children};
use crate::{ActorContext, Backend, SessionId};
use lash_core_execution::runtime::actor::process::{self, CascadeProgress};

/// The mail kind of a close request. The session's mailbox drain
/// (`session_mail`) hands it to the activation as its `close`.
pub const SESSION_CLOSE_MAIL: &str = "session.close";

/// The reason a session close's `cancel` step gives the open turn's cancel.
pub const SESSION_CLOSED: &str = "session_closed";

/// How many of the session's live processes one `triggers` check reads: it
/// waits on the first, and every other one is checked again when it wakes.
const LIVE_PROBE: usize = 1;

/// The answer to a close request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCloseRequested {
    /// The request is the session's mail: the session closes itself.
    Requested,
    /// The catalog never held the id: nothing was closed and the id stays
    /// creatable (ADR 0049).
    Absent,
    /// The session's actor has ended: its close finished before.
    AlreadyClosed,
}

/// Ask `session` to close: its close request as mail, which wakes it.
///
/// A session's actor is created by its first work, so a session that was
/// created but never sent anything has metadata and no actor. Its metadata
/// makes its id one session lifetime (ADR 0049), so its close is its first
/// work: the request creates the actor with the close as its mail.
///
/// # Errors
///
/// The store's refusal other than an absent or ended actor.
pub async fn request_session_close(
    backend: &Backend,
    session: &SessionId,
) -> Result<SessionCloseRequested, DurableError> {
    let actor = session_actor(session);
    loop {
        let mut tx = MailTx::new();
        tx.append(
            actor.clone(),
            MailKind::new(SESSION_CLOSE_MAIL),
            String::new(),
        );
        match backend
            .durable()
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
        {
            Ok(_) => return Ok(SessionCloseRequested::Requested),
            Err(DurableError::MailRefused(MailRefusal::UnknownActor(_))) => {}
            Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_))) => {
                return Ok(SessionCloseRequested::AlreadyClosed);
            }
            Err(error) => return Err(error),
        }
        let lookup = backend
            .session_store_factory()
            .lookup_session(session)
            .await
            .map_err(|error| {
                DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: format!("session {session} was not looked up: {error}"),
                })
            })?;
        match lookup {
            SessionLookup::Absent => return Ok(SessionCloseRequested::Absent),
            SessionLookup::Deleted => return Ok(SessionCloseRequested::AlreadyClosed),
            SessionLookup::Live(_) => {}
        }
        let mut tx = MailTx::new();
        tx.create_actor(actor.clone(), FormatSet::new(SESSION_ACTOR_FORMATS))
            .append(
                actor.clone(),
                MailKind::new(SESSION_CLOSE_MAIL),
                String::new(),
            );
        match backend
            .durable()
            .commit_mail(tx, CommitLabel::MAIL_SESSION)
            .await
        {
            Ok(_) => return Ok(SessionCloseRequested::Requested),
            // A first send created the actor meanwhile: append to it.
            Err(DurableError::MailRefused(MailRefusal::ActorExists(_))) => {}
            Err(error) => return Err(error),
        }
    }
}

/// The session's actor key. A session id is never blank, so it always makes
/// one.
#[expect(
    clippy::expect_used,
    reason = "a parsed session id is never blank, the only refused actor id"
)]
fn session_actor(session: &SessionId) -> ActorKey {
    ActorKey::session(session.as_str()).expect("a session id is a valid actor id")
}

/// Enter the closing state on `tx`, the transaction that drained the close
/// request, and close the session's scope in it: a start under the session
/// or inside it that commits later is refused. A session already closing
/// keeps where its close got to.
pub fn begin_session_close(tx: &mut ActorTx, session: &SessionId) {
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Begin {
        session: session.clone(),
    }));
    process::close_scope(tx, &ScopeKey::Session(session.clone()));
}

/// Where a close run stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCloseExit {
    /// The tombstone is written and the actor ended in that commit.
    Closed,
    /// A process of the session is still ending: its terminal wakes the
    /// session, which runs the close again. Release as waiting.
    Waiting,
}

/// Why a close step did not commit. Every step before it stands, and the
/// next claim resumes at it.
#[derive(Debug, thiserror::Error)]
pub enum SessionCloseError {
    /// The store refused: [`DurableError::OwnershipLost`] once the session is
    /// someone else's.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// The open turn's cancel failed.
    #[error("the open turn's cancel: {0}")]
    Turn(#[from] TurnError),
    /// The trigger store did not delete the session's subscriptions.
    #[error("trigger subscriptions: {0}")]
    Triggers(crate::PluginError),
    /// The session's storage delete stopped.
    #[error("storage: {0}")]
    Storage(Box<crate::store::MaintenanceFailure<crate::store::SessionBlobReclaimReport>>),
    /// The process registry did not delete the session's process state.
    #[error("process state: {0}")]
    Process(crate::PluginError),
}

/// Run what remains of `session`'s close, from the step after the stored
/// one. `None` when the session is not closing.
///
/// # Errors
///
/// [`SessionCloseError`]; the steps before the failed one stand.
pub async fn run_session_close(
    cx: &ActorContext,
    session: &SessionId,
) -> Result<Option<SessionCloseExit>, SessionCloseError> {
    let backend = cx.backend();
    let Some(row) = backend.durable().session_close(session).await? else {
        return Ok(None);
    };
    let scope = ScopeKey::Session(session.clone());
    let mut next = row.next();
    while let Some(step) = next {
        match step {
            SessionCloseStep::Cancel => {
                let mut tx = cx.begin().await?;
                let cause = crate::runtime::TurnCancellationEvidence {
                    reason: Some(SESSION_CLOSED.to_owned()),
                    ..crate::runtime::TurnCancellationEvidence::internal(SESSION_CLOSED)
                };
                if let Some(turn) = cancel_open_turn(cx, &mut tx, &cause).await? {
                    end_turn_scope(cx, &mut tx, session, &turn.run).await?;
                }
                commit_step(cx, tx, session, step).await?;
                continue_scope_ends(cx, session).await?;
            }
            SessionCloseStep::Revoke => {
                let mut tx = cx.begin().await?;
                waits::revoke_scope(&mut tx, &scope);
                commit_step(cx, tx, session, step).await?;
            }
            SessionCloseStep::EndScope => loop {
                let mut tx = cx.begin().await?;
                if mark_until_children(cx, &mut tx, &scope).await? == CascadeProgress::Done {
                    commit_step(cx, tx, session, step).await?;
                    break;
                }
                cx.commit(tx, step.label()).await?;
            },
            SessionCloseStep::Triggers => {
                if let Some(live) = backend
                    .durable()
                    .live_until_descendants(&scope, LIVE_PROBE)
                    .await?
                    .into_iter()
                    .next()
                {
                    // A process that ended after the read resolves the wait
                    // here, which wakes the session.
                    waits::pin_process_terminal(cx, scope.clone(), &live, None).await?;
                    return Ok(Some(SessionCloseExit::Waiting));
                }
                backend
                    .trigger_store()
                    .delete_session_subscriptions(session)
                    .await
                    .map_err(SessionCloseError::Triggers)?;
                commit_step(cx, cx.begin().await?, session, step).await?;
            }
            SessionCloseStep::Artifacts => {
                backend
                    .session_store_factory()
                    .delete_session(session)
                    .await
                    .map_err(|failure| SessionCloseError::Storage(Box::new(failure)))?;
                commit_step(cx, cx.begin().await?, session, step).await?;
            }
            SessionCloseStep::Tombstone => {
                backend
                    .process_registry()
                    .delete_session_process_state(session)
                    .await
                    .map_err(SessionCloseError::Process)?;
                let mut tx = cx.begin().await?;
                // The waits the triggers step pinned on ending processes.
                waits::revoke_scope(&mut tx, &scope);
                tx.ack_seen().give_up(Release::Terminal);
                commit_step(cx, tx, session, step).await?;
                return Ok(Some(SessionCloseExit::Closed));
            }
        }
        next = step.next();
    }
    Ok(Some(SessionCloseExit::Closed))
}

/// Record `step` as done on `tx` and commit it under the step's label.
async fn commit_step(
    cx: &ActorContext,
    mut tx: ActorTx,
    session: &SessionId,
    step: SessionCloseStep,
) -> Result<(), DurableError> {
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: session.clone(),
        step,
    }));
    cx.commit(tx, step.label()).await?;
    Ok(())
}

/// The session's close as its row stands; `None` when it never began.
///
/// # Errors
///
/// The store's refusal.
pub async fn session_close_state(
    backend: &Backend,
    session: &SessionId,
) -> Result<Option<SessionCloseRow>, DurableError> {
    backend.durable().session_close(session).await
}
