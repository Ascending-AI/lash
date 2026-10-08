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
//!    and the head open turn batch. A head input takes with it the inputs
//!    behind it that the host's installed drain policy selects
//!    ([`InputBatching`]), among those that share its run spec and lie
//!    before the earliest open turn batch (ADR 0101 §5.2); the default
//!    policy takes the head alone, so each input is its own run.
//!
//! The owner mails its own session too: a frame switch's `turn.commit`
//! mails the switch's task as a next-turn input ([`follow_on_mail`]), so the
//! follow-on is ordinary mail the session's next drain admits (ADR 0101 §3).

use lash_durable::domain::{MailBatchKind, MailInput, SessionMailWrite, SessionMailbox};
use lash_durable::{ActorTx, DomainWrite, DurableError};
use lash_sansio::{SessionId, TurnId};

use super::session::AdmittedInputs;
use crate::ActorContext;
use crate::store::{AdmittedInputIds, AdmittedTurnRows, RunAdmissionRecord};

/// How many next-turn inputs one run takes: the host's queued-work batching
/// (ADR 0101 §5.2), which the drain asks only when more than one input is
/// eligible to run together.
#[async_trait::async_trait]
pub trait InputBatching: Send + Sync {
    /// The admission policy `session`'s next run composes its next-turn
    /// input under, and the most inputs it offers that policy; `None` takes
    /// the head input alone.
    ///
    /// # Errors
    ///
    /// [`SessionMailError`] when the policy cannot be read.
    async fn input_admission(
        &self,
        cx: &ActorContext,
        session: &SessionId,
    ) -> Result<Option<InputAdmission>, SessionMailError>;
}

/// The policy an idle admission of next-turn input composes under.
#[derive(Clone, Debug)]
pub struct InputAdmission {
    /// The most inputs one admission offers the drain policy.
    pub max_inputs: usize,
    /// The policy that selects how many of them run together.
    pub policy: crate::TurnLaneAdmissionPolicy,
}

/// [`InputBatching`] that takes the head input alone: Lash's default drain,
/// for an activation that installs no host policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct OneInputPerRun;

#[async_trait::async_trait]
impl InputBatching for OneInputPerRun {
    async fn input_admission(
        &self,
        _cx: &ActorContext,
        _session: &SessionId,
    ) -> Result<Option<InputAdmission>, SessionMailError> {
        Ok(None)
    }
}

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
    /// The host's input batching could not be read.
    #[error("the session's input batching: {0}")]
    Batching(String),
}

/// Drain the session's mailbox on `tx`, binding what it admits under the
/// epoch, and return what the activation must do. A run of next-turn input
/// takes as many inputs as `batching` selects. The whole mailbox is
/// acknowledged: what it signalled is now in the drain's answer or still
/// open in its table for the next drain.
///
/// # Errors
///
/// [`SessionMailError`].
pub async fn drain_session_mail(
    cx: &ActorContext,
    tx: &mut ActorTx,
    batching: &dyn InputBatching,
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
    let admit = match admission(&mailbox)? {
        Some(Head::Run(admitted)) => Some(admitted),
        Some(Head::Input { run, inputs }) => Some(AdmittedInputs {
            run,
            admission: RunAdmissionRecord::Turn {
                took: AdmittedTurnRows::Inputs {
                    ids: composed_inputs(cx, tx, &session, inputs, batching).await?,
                },
                trace: None,
            },
        }),
        None => None,
    };
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

/// What a mailbox admits next: a run whose admission is decided, or the
/// head input with the inputs that may run with it, which the host's drain
/// policy composes.
enum Head {
    Run(AdmittedInputs),
    Input {
        /// The run the head input opens.
        run: TurnId,
        /// The head input and the open inputs that may share its run, in
        /// ingress order: those of its run spec, before the earliest open
        /// turn batch, which a composition never passes (ADR 0101 §5.2).
        inputs: Vec<MailInput>,
    },
}

/// The inputs of `inputs` the run takes: the head alone, unless the host's
/// policy selects more of them.
async fn composed_inputs(
    cx: &ActorContext,
    tx: &ActorTx,
    session: &SessionId,
    inputs: Vec<MailInput>,
    batching: &dyn InputBatching,
) -> Result<AdmittedInputIds, SessionMailError> {
    let undecodable = |error: crate::StoreError| SessionMailError::Undecodable(error.to_string());
    let head = inputs.first().map(|input| vec![input.input.clone()]);
    let mut ids = head.unwrap_or_default();
    if inputs.len() > 1
        && let Some(InputAdmission { max_inputs, policy }) =
            batching.input_admission(cx, session).await?
    {
        // The policy weighs each input's payload and age, which the mailbox
        // does not carry: the session's pending rows do.
        let mut pending = cx
            .backend()
            .session_store_factory()
            .list_pending_turn_inputs(session)
            .await
            .map_err(undecodable)?
            .into_iter()
            .map(|read| (read.input.input_id.clone(), read.input))
            .collect::<std::collections::BTreeMap<_, _>>();
        let rows = inputs
            .iter()
            .take(max_inputs.max(1))
            .map_while(|input| pending.remove(&input.input))
            .collect::<Vec<_>>();
        let now = u64::try_from(tx.opened_at().0).unwrap_or(0);
        if let Some(composed) =
            crate::store::plan_next_turn_input_admission(session, rows, max_inputs, &policy, now)
            && !composed.inputs.is_empty()
        {
            ids = composed.input_ids();
        }
    }
    AdmittedInputIds::new(ids).map_err(|error| SessionMailError::Undecodable(error.to_string()))
}

/// The run `mailbox` admits next, with what its admission took.
fn admission(mailbox: &SessionMailbox) -> Result<Option<Head>, SessionMailError> {
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
        return Ok(Some(Head::Run(AdmittedInputs {
            run: run_of(batch.batch.as_str())?,
            admission,
        })));
    }
    let head_input = mailbox.inputs.iter().min_by_key(|input| input.enqueue_seq);
    let turn_batch = |batch: &lash_durable::domain::MailBatch| -> Result<_, SessionMailError> {
        Ok(AdmittedInputs {
            run: run_of(batch.batch.as_str())?,
            admission: RunAdmissionRecord::Turn {
                took: AdmittedTurnRows::Batch {
                    id: batch.batch.clone(),
                },
                trace: None,
            },
        })
    };
    Ok(Some(match (head_input, head_batch) {
        (Some(input), Some(batch)) if batch.enqueue_seq < input.enqueue_seq => {
            Head::Run(turn_batch(batch)?)
        }
        (Some(head), batch) => {
            let run = head
                .run()
                .ok_or_else(|| SessionMailError::Undecodable(format!("run id {}", head.input)))?;
            let stop = batch.map(|batch| batch.enqueue_seq);
            let mut inputs = mailbox
                .inputs
                .iter()
                .filter(|input| input.enqueue_seq >= head.enqueue_seq)
                .collect::<Vec<_>>();
            inputs.sort_by_key(|input| input.enqueue_seq);
            let inputs = inputs
                .into_iter()
                .take_while(|input| {
                    (input.input == head.input || (!is_frame_task(head) && !is_frame_task(input)))
                        && input.run_spec_hash == head.run_spec_hash
                        && stop.is_none_or(|stop| input.enqueue_seq < stop)
                })
                .cloned()
                .collect();
            Head::Input { run, inputs }
        }
        (None, Some(batch)) => Head::Run(turn_batch(batch)?),
        (None, None) => return Ok(None),
    }))
}

fn is_frame_task(input: &MailInput) -> bool {
    input
        .source_key
        .as_deref()
        .is_some_and(|key| key.starts_with("frame-task:"))
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
/// nothing. `agent_frame_switches` is the switching run's admitted depth;
/// the mailed input retains its successor depth through owner changes.
///
/// # Errors
///
/// [`SessionMailError::Undecodable`] when the input does not encode.
pub fn follow_on_mail(
    session: &SessionId,
    outcome: &crate::TurnOutcome,
    agent_frame_switches: u32,
) -> Result<Option<SessionMailWrite>, SessionMailError> {
    let crate::TurnOutcome::AgentFrameSwitch {
        frame_key, task, ..
    } = outcome
    else {
        return Ok(None);
    };
    let run = frame_task_run(frame_key);
    let mut input = crate::TurnInput::text(task.clone());
    input.agent_frame_switches = agent_frame_switches.saturating_add(1);
    let draft = crate::PendingTurnInputDraft::new(
        session.clone(),
        crate::TurnInputIngress::next_turn(),
        input.durable_projection(),
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

/// Read the chain depth from the immutable inputs this run admitted. A wake
/// has no input and starts a new chain. Checkpoint steering never changes the
/// chain the run started, and an owner change reads the same persisted depth.
pub(super) async fn agent_frame_switches(
    cx: &ActorContext,
    row: &super::session::TurnRow,
) -> Result<u32, super::session::TurnError> {
    let store = cx.backend().session_store_factory();
    let mut switches = 0;
    for id in row.admission.input_ids() {
        let read = store
            .pending_turn_input(&row.session, id)
            .await
            .map_err(|error| super::session::TurnError::Runtime(error.runtime_error()))?
            .ok_or_else(|| {
                super::session::TurnError::Exec(format!(
                    "run {} took input {id}, which is no longer pending",
                    row.run
                ))
            })?;
        switches = switches.max(read.input.input.agent_frame_switches);
    }
    Ok(switches)
}
