//! The session's mailbox on SQLite: the `session_mail` domain's read and
//! apply (L3s, FIG-5196).
//!
//! The dispatch in `durable/mod.rs` calls [`apply`] inside the fenced owner
//! commit, after the fence; a refusal rolls the whole commit back.

use lash_durable::domain::{
    DomainRefusal, MailBatch, MailBatchKind, MailInput, SessionMailWrite, SessionMailbox,
};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_sansio::{BatchId, InputId, SessionId, TurnId};
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, SQL};

fn undecodable(what: &str, detail: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("session mail {what} does not decode: {detail}"),
    })
}

fn count(value: i64) -> bool {
    value > 0
}

/// Session `session`'s standing: `(live, closing)`.
fn standing(tx: &Connection, session: &SessionId) -> rusqlite::Result<(bool, bool)> {
    let (meta, deleted, closing) = tx
        .prepare_cached(SQL.session_mail.standing.sql())?
        .query_row([session.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
    Ok((count(meta) && !count(deleted), count(closing)))
}

/// The session's open mail, read in one snapshot.
pub(super) fn read(tx: &Connection, session: &SessionId) -> Answer<SessionMailbox> {
    let (live, closing) = standing(tx, session)?;
    let mut mailbox = SessionMailbox {
        live,
        closing,
        ..SessionMailbox::default()
    };
    if !mailbox.live {
        return Ok(Ok(mailbox));
    }
    let bound: Option<String> = tx
        .prepare_cached(SQL.session_mail.bound_run.sql())?
        .query_row([session.as_str()], |row| row.get(0))
        .optional()?;
    mailbox.bound_run = match bound.map(TurnId::parse).transpose() {
        Ok(run) => run,
        Err(error) => return Ok(Err(undecodable("bound run", error))),
    };
    let inputs = tx
        .prepare_cached(SQL.session_mail.open_inputs.sql())?
        .query_map([session.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (input, seq, source_key, run_spec_hash, state, ingress_json) in inputs {
        match decode_input(input, seq, source_key, run_spec_hash, &state, &ingress_json) {
            Ok(input) => mailbox.inputs.push(input),
            Err(error) => return Ok(Err(error)),
        }
    }
    let batches = tx
        .prepare_cached(SQL.session_mail.open_batches.sql())?
        .query_map([session.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (batch, seq, work_kind, policy, payload_json) in batches {
        match decode_batch(batch, seq, &work_kind, &policy, &payload_json) {
            Ok(batch) => mailbox.batches.push(batch),
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(mailbox))
}

fn decode_input(
    input: String,
    seq: i64,
    source_key: Option<String>,
    run_spec_hash: Option<String>,
    state: &str,
    ingress_json: &str,
) -> Result<MailInput, DurableError> {
    let ingress: lash_core_execution::TurnInputIngress =
        serde_json::from_str(ingress_json).map_err(|error| undecodable("input delivery", error))?;
    let active_turn = match state {
        "pending_active" => ingress.active_turn_id().cloned(),
        _ => None,
    };
    Ok(MailInput {
        input: InputId::parse(input).map_err(|error| undecodable("input id", error))?,
        enqueue_seq: u64::try_from(seq).map_err(|error| undecodable("input order", error))?,
        source_key,
        run_spec_hash,
        active_turn,
    })
}

fn decode_batch(
    batch: String,
    seq: i64,
    work_kind: &str,
    policy: &str,
    payload_json: &str,
) -> Result<MailBatch, DurableError> {
    let payload: lash_core_execution::runtime::QueuedWorkPayload =
        serde_json::from_str(payload_json).map_err(|error| undecodable("batch payload", error))?;
    let kind = match (work_kind, &payload) {
        ("turn", _) => MailBatchKind::Turn,
        (
            "control",
            lash_core_execution::runtime::QueuedWorkPayload::SessionCommand { command },
        ) if matches!(
            **command,
            lash_core_execution::runtime::SessionCommand::RunPluginTask { .. }
        ) =>
        {
            MailBatchKind::Operation
        }
        ("control", _) => MailBatchKind::Control,
        (other, _) => return Err(undecodable("batch kind", other)),
    };
    Ok(MailBatch {
        batch: BatchId::parse(batch).map_err(|error| undecodable("batch id", error))?,
        enqueue_seq: u64::try_from(seq).map_err(|error| undecodable("batch order", error))?,
        kind,
        after_current_turn: policy == "after_current_turn_commit",
    })
}

/// Settle what run `run` of `session` still holds as it ends: a turn whose
/// commit settled its rows holds none, and a turn that ends with no commit
/// (a cancel) leaves its inputs `cancelled` and its batches cancelled, so no
/// row stays bound to an ended run (ADR 0132 §11).
pub(super) fn settle_held(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.prepare_cached(SQL.session_mail.settle_held_inputs.sql())?
        .execute(rusqlite::params![
            session.as_str(),
            run.as_str(),
            lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
            now_ms,
        ])?;
    tx.prepare_cached(SQL.session_mail.settle_held_batches.sql())?
        .execute(rusqlite::params![session.as_str(), run.as_str(), now_ms])?;
    Ok(())
}

/// Apply the owner's session-mail write: bind admitted mail to its run, or
/// mail the session its own next-turn input.
pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &SessionMailWrite,
) -> Answer<()> {
    match write {
        SessionMailWrite::Admit {
            session,
            run,
            inputs,
            batches,
        } => admit(tx, session, run, inputs, batches),
        SessionMailWrite::Enqueue {
            session,
            draft_json,
        } => enqueue(tx, commit, session, draft_json),
    }
}

/// Insert the owner's input as any producer's acceptance does, with the
/// session's wake, in the owner commit. A closing or gone session takes no
/// mail: its close settles what it holds, and the write inserts nothing.
fn enqueue(
    tx: &Connection,
    commit: &Committing<'_>,
    session: &SessionId,
    draft_json: &str,
) -> Answer<()> {
    let (live, closing) = standing(tx, session)?;
    if !live || closing {
        return Ok(Ok(()));
    }
    let refused = |reason: String| {
        Ok(Err(DurableError::Domain(
            DomainRefusal::SessionMailRefused {
                session: session.clone(),
                reason,
            },
        )))
    };
    let draft: lash_core_execution::PendingTurnInputDraft = match serde_json::from_str(draft_json) {
        Ok(draft) => draft,
        Err(error) => return Ok(Err(undecodable("owner input", error))),
    };
    let batch = match lash_core_execution::PendingTurnInputBatch::new(session.clone(), vec![draft])
    {
        Ok(batch) => batch,
        Err(error) => return refused(error.to_string()),
    };
    let now = u64::try_from(commit.now.0).unwrap_or(0);
    match crate::persistence::enqueue_pending_turn_inputs_conn(tx, &batch, now, 0) {
        Ok(_) => Ok(Ok(())),
        Err(lash_core_execution::StoreError::Contended) => {
            Ok(Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Contended,
                message: "the owner's session mail contended".to_owned(),
            })))
        }
        Err(error) => refused(error.to_string()),
    }
}

/// Bind the admitted mail to its run, each row still open and unbound.
fn admit(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    inputs: &[InputId],
    batches: &[BatchId],
) -> Answer<()> {
    let moved = |item: &str| {
        Ok(Err(DurableError::Domain(DomainRefusal::SessionMailMoved {
            session: session.clone(),
            item: item.to_owned(),
        })))
    };
    for input in inputs {
        let bound = tx
            .prepare_cached(SQL.session_mail.bind_input.sql())?
            .query_row(
                rusqlite::params![session.as_str(), input.as_str(), run.as_str()],
                |_| Ok(()),
            )
            .optional()?;
        if bound.is_none() {
            return moved(input.as_str());
        }
        tx.prepare_cached(SQL.session_mail.record_run_input.sql())?
            .execute(rusqlite::params![
                session.as_str(),
                input.as_str(),
                run.as_str()
            ])?;
    }
    for batch in batches {
        let bound = tx
            .prepare_cached(SQL.session_mail.bind_batch.sql())?
            .query_row(
                rusqlite::params![session.as_str(), batch.as_str(), run.as_str()],
                |_| Ok(()),
            )
            .optional()?;
        if bound.is_none() {
            return moved(batch.as_str());
        }
    }
    Ok(Ok(()))
}

/// Arm (`true`) or disarm a cut of every session-mail producer transaction
/// at the session actor's wake: while armed, the wake's `mail_seq` bump
/// aborts, so the producer's transaction rolls back before it commits. The
/// mail atomicity law's fault.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn cut_session_wakes(conn: &Connection, armed: bool) -> rusqlite::Result<()> {
    conn.execute_batch(if armed {
        "CREATE TRIGGER lash_test_cut_wake BEFORE UPDATE OF mail_seq ON actors
         BEGIN SELECT RAISE(ABORT, 'cut before the producer commit'); END;"
    } else {
        "DROP TRIGGER lash_test_cut_wake"
    })
}
