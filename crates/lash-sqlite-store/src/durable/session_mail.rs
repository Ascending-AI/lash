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

/// The session's open mail, read in one snapshot.
pub(super) fn read(tx: &Connection, session: &SessionId) -> Answer<SessionMailbox> {
    let (meta, deleted, closing, follow_on) = tx
        .prepare_cached(SQL.session_mail.standing.sql())?
        .query_row([session.as_str()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut mailbox = SessionMailbox {
        live: count(meta) && !count(deleted),
        closing: count(closing),
        follow_on_owed: count(follow_on),
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

/// Bind the admitted mail to its run, each row still open and unbound.
pub(super) fn apply(
    tx: &Connection,
    _commit: &Committing<'_>,
    write: &SessionMailWrite,
) -> Answer<()> {
    let SessionMailWrite::Admit {
        session,
        run,
        inputs,
        batches,
    } = write;
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
