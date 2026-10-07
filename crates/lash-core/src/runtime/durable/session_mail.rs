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
//! 1. nothing while the session is closing or still has a bound run (the
//!    activation resumes it);
//! 2. the head open batch when it is a session command: a command run, or
//!    an operation run for a plugin task, which binds nothing (the commit
//!    that applies it settles it, predicated on the row still being open);
//! 3. otherwise the turn lane's head, the earlier of the head open input
//!    and the head open turn batch, alone (the default drain policy takes
//!    the head alone, so each input is its own run).
//!
//! The owner mails its own session too: a frame switch's `turn.commit`
//! mails the switch's task as a next-turn input ([`follow_on_mail`]), so the
//! follow-on is ordinary mail the session's next drain admits (ADR 0101 §3).

use lash_durable::domain::{MailBatchKind, SessionMailWrite, SessionMailbox};
use lash_durable::{ActorTx, DomainWrite, DurableError};
use lash_sansio::{SessionId, TurnId};

use super::session::AdmittedInputs;
use crate::ActorContext;
use crate::store::{AdmittedInputIds, AdmittedTurnRows, RunAdmissionRecord};

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
    let admit = admission(&mailbox)?;
    // A turn binds what it took; a command or operation run binds nothing.
    if let Some(admitted) = admit.as_ref()
        && admitted.admission.is_turn()
    {
        tx.write(DomainWrite::SessionMail(SessionMailWrite::Admit {
            session: session.clone(),
            run: admitted.run.clone(),
            inputs: admitted.admission.input_ids().to_vec(),
            batches: admitted.admission.batch_ids(),
        }));
    }
    tx.ack_seen();
    Ok(SessionMailDrain { admit, close })
}

/// The run `mailbox` admits next, with what its admission took.
fn admission(mailbox: &SessionMailbox) -> Result<Option<AdmittedInputs>, SessionMailError> {
    if !mailbox.live || mailbox.closing || mailbox.bound_run.is_some() {
        return Ok(None);
    }
    let head_batch = mailbox.batches.iter().min_by_key(|batch| batch.enqueue_seq);
    if let Some(batch) = head_batch
        && batch.kind != MailBatchKind::Turn
    {
        let admission = match batch.kind {
            MailBatchKind::Operation => RunAdmissionRecord::Operation {
                batch: batch.batch.clone(),
            },
            _ => RunAdmissionRecord::Command {
                batch: batch.batch.clone(),
            },
        };
        return Ok(Some(AdmittedInputs {
            run: run_of(batch.batch.as_str())?,
            admission,
        }));
    }
    let head_input = mailbox.inputs.iter().min_by_key(|input| input.enqueue_seq);
    let turn_batch = |batch: &lash_durable::domain::MailBatch| -> Result<_, SessionMailError> {
        Ok(AdmittedInputs {
            run: run_of(batch.batch.as_str())?,
            admission: RunAdmissionRecord::Turn {
                took: AdmittedTurnRows::Batch {
                    id: batch.batch.clone(),
                },
            },
        })
    };
    Ok(Some(match (head_input, head_batch) {
        (Some(input), Some(batch)) if batch.enqueue_seq < input.enqueue_seq => turn_batch(batch)?,
        (Some(input), _) => {
            let run = match input.source_key.as_deref().map(TurnId::parse) {
                Some(Ok(run)) => run,
                _ => run_of(input.input.as_str())?,
            };
            let ids = AdmittedInputIds::new(vec![input.input.clone()])
                .map_err(|error| SessionMailError::Undecodable(error.to_string()))?;
            AdmittedInputs {
                run,
                admission: RunAdmissionRecord::Turn {
                    took: AdmittedTurnRows::Inputs { ids },
                },
            }
        }
        (None, Some(batch)) => turn_batch(batch)?,
        (None, None) => return Ok(None),
    }))
}

fn run_of(id: &str) -> Result<TurnId, SessionMailError> {
    TurnId::parse(id)
        .map_err(|error| SessionMailError::Undecodable(format!("run id {id}: {error}")))
}

/// The run a frame switch's follow-on starts: named by the frame the switch
/// opened, so every retry of the switch's commit mails the same input, and
/// a frame's task runs once. The input is keyed by it as a host's keyed
/// send is, so a host attaches to the follow-on by this id.
#[must_use]
pub fn frame_task_run(frame_key: &crate::FrameKey) -> TurnId {
    TurnId::prefixed("frame-task:", frame_key.as_str())
}

/// The mail a turn's `turn.commit` sends its own session for `outcome`: a
/// frame switch's task, as the session's next-turn input under its
/// follow-on's run ([`frame_task_run`]), so the switch, its new frame and
/// its follow-on commit together (ADR 0101 §3). Any other outcome mails
/// nothing.
///
/// # Errors
///
/// [`SessionMailError::Undecodable`] when the input does not encode.
pub fn follow_on_mail(
    session: &SessionId,
    outcome: &crate::TurnOutcome,
) -> Result<Option<SessionMailWrite>, SessionMailError> {
    let crate::TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = outcome
    else {
        return Ok(None);
    };
    let run = frame_task_run(frame_key);
    let draft = crate::PendingTurnInputDraft::new(
        session.clone(),
        crate::TurnInputIngress::next_turn(),
        crate::TurnInput::text(task.clone()).durable_projection(),
    )
    .with_input_id(crate::PendingTurnInputDraft::keyed_input_id(
        session,
        run.as_str(),
    ))
    .with_source_key(run.as_str());
    let draft_json = serde_json::to_string(&draft).map_err(|error| {
        SessionMailError::Undecodable(format!(
            "the follow-on of frame {}: {error}",
            frame_key.as_str()
        ))
    })?;
    Ok(Some(SessionMailWrite::Enqueue {
        session: session.clone(),
        draft_json,
    }))
}
