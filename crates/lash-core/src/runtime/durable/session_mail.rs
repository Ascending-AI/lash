//! The session's mailbox, drained on every claim (ADR 0132 §3, §12). Owned
//! by L3s (FIG-5196).
//!
//! Every producer of session work (pending inputs, queued work batches,
//! control intents, turn cancel requests) writes its row and wakes the
//! session in its own transaction (`wake_within` in each dialect). The
//! session activation calls [`drain_session_mail`] first on every claim: it
//! reads the mailbox tables, binds what the session admits under the epoch
//! (the bind commits with the activation's owner commit, or not at all) and
//! returns what the activation must do.
//!
//! What a drain admits, in the order the session owes it:
//!
//! 1. nothing while the session is closing, owes its head a follow-on, or
//!    still has a bound run (the activation resumes those);
//! 2. the head open batch when it is a session command: a command run, or
//!    an operation run for a plugin task, which binds nothing (the commit
//!    that applies it settles it, predicated on the row still being open);
//! 3. otherwise the turn lane's head, the earlier of the head open input
//!    and the head open turn batch, alone (the default drain policy takes
//!    the head alone, so each input is its own run).

use lash_durable::domain::{MailBatchKind, SessionMailWrite, SessionMailbox};
use lash_durable::{ActorTx, DomainWrite, DurableError};
use lash_sansio::{BatchId, InputId, SessionId, TurnId};

use super::session::AdmittedInputs;
use crate::ActorContext;

/// The mail kind a session's close is appended under; its body is empty.
pub const SESSION_CLOSE_MAIL: &str = "session.close";

/// A request to close the session, drained from its mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCloseRequest {
    /// The request, encoded by its owner.
    pub request_json: String,
}

/// What a drain hands the session activation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMailDrain {
    /// Work to admit as the next run.
    pub admit: Option<AdmittedInputs>,
    /// A close of the session to begin.
    pub close: Option<SessionCloseRequest>,
}

/// Which lane a drained run serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMailRunKind {
    /// A turn: bound inputs or turn batches.
    Turn,
    /// One session command, applied by its commit.
    Command,
    /// A plugin task's operation run, applied by its commit.
    Operation,
}

/// What a drain admitted, as [`AdmittedInputs::admission_json`] carries it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMailAdmission {
    /// The lane the run serves.
    pub kind: SessionMailRunKind,
    /// The inputs the run took, bound to it.
    pub inputs: Vec<InputId>,
    /// The batches the run took: bound to a turn run, open for a command or
    /// operation run.
    pub batches: Vec<BatchId>,
}

/// Why a drain failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionMailError {
    /// The store refused.
    #[error(transparent)]
    Durable(#[from] DurableError),
    /// A mailbox row does not decode.
    #[error("session mail does not decode: {0}")]
    Undecodable(String),
}

/// Drain the session's mailbox on `tx`, binding what it admits under the
/// epoch, and return what the activation must do. The whole mailbox is
/// acknowledged: what it signalled is now in the drain's answer or still
/// open in its table for the next drain.
///
/// # Errors
///
/// [`SessionMailError`].
pub async fn drain_session_mail(
    cx: &ActorContext,
    tx: &mut ActorTx,
) -> Result<SessionMailDrain, SessionMailError> {
    let session = SessionId::parse(cx.actor().id())
        .map_err(|error| SessionMailError::Undecodable(format!("session actor id: {error}")))?;
    let mailbox = cx.backend().durable().session_mailbox(&session).await?;
    let close_mail = tx
        .mail()
        .iter()
        .find(|mail| mail.kind.as_str() == SESSION_CLOSE_MAIL)
        .map(|mail| mail.body.clone());
    let close = match close_mail {
        Some(request_json) => Some(SessionCloseRequest { request_json }),
        None if mailbox.closing => Some(SessionCloseRequest {
            request_json: String::new(),
        }),
        None => None,
    };
    let admit = admission(&session, &mailbox)?;
    if let Some((admitted, bind)) = admit.as_ref()
        && let Some(bind) = bind
    {
        tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
            session: session.clone(),
            run: admitted.run.clone(),
            inputs: bind.inputs.clone(),
            batches: bind.batches.clone(),
        }));
    }
    tx.ack_seen();
    Ok(SessionMailDrain {
        admit: admit.map(|(admitted, _)| admitted),
        close,
    })
}

/// The run `mailbox` admits next, and what it binds (`None` for a run that
/// binds nothing).
fn admission(
    session: &SessionId,
    mailbox: &SessionMailbox,
) -> Result<Option<(AdmittedInputs, Option<SessionMailAdmission>)>, SessionMailError> {
    if !mailbox.live || mailbox.closing || mailbox.follow_on_owed || mailbox.bound_run.is_some() {
        return Ok(None);
    }
    let head_batch = mailbox.batches.iter().min_by_key(|batch| batch.enqueue_seq);
    if let Some(batch) = head_batch
        && batch.kind != MailBatchKind::Turn
    {
        let kind = match batch.kind {
            MailBatchKind::Operation => SessionMailRunKind::Operation,
            _ => SessionMailRunKind::Command,
        };
        let admission = SessionMailAdmission {
            kind,
            inputs: Vec::new(),
            batches: vec![batch.batch.clone()],
        };
        return Ok(Some((
            admitted(session, run_of(batch.batch.as_str())?, &admission)?,
            None,
        )));
    }
    let head_input = mailbox.inputs.iter().min_by_key(|input| input.enqueue_seq);
    let turn_batch = |batch: &lash_durable::domain::MailBatch| -> Result<_, SessionMailError> {
        Ok((
            run_of(batch.batch.as_str())?,
            SessionMailAdmission {
                kind: SessionMailRunKind::Turn,
                inputs: Vec::new(),
                batches: vec![batch.batch.clone()],
            },
        ))
    };
    let chosen = match (head_input, head_batch) {
        (Some(input), Some(batch)) if batch.enqueue_seq < input.enqueue_seq => turn_batch(batch)?,
        (Some(input), _) => {
            let run = match input.source_key.as_deref().map(TurnId::parse) {
                Some(Ok(run)) => run,
                _ => run_of(input.input.as_str())?,
            };
            (
                run,
                SessionMailAdmission {
                    kind: SessionMailRunKind::Turn,
                    inputs: vec![input.input.clone()],
                    batches: Vec::new(),
                },
            )
        }
        (None, Some(batch)) => turn_batch(batch)?,
        (None, None) => return Ok(None),
    };
    let (run, bind) = chosen;
    Ok(Some((admitted(session, run, &bind)?, Some(bind))))
}

fn run_of(id: &str) -> Result<TurnId, SessionMailError> {
    TurnId::parse(id)
        .map_err(|error| SessionMailError::Undecodable(format!("run id {id}: {error}")))
}

fn admitted(
    session: &SessionId,
    run: TurnId,
    admission: &SessionMailAdmission,
) -> Result<AdmittedInputs, SessionMailError> {
    let admission_json = serde_json::to_string(admission).map_err(|error| {
        SessionMailError::Undecodable(format!("session {session} admission: {error}"))
    })?;
    Ok(AdmittedInputs {
        run,
        inputs: admission.inputs.clone(),
        admission_json,
    })
}
