//! The session's mailbox on PostgreSQL: the `session_mail` domain's read and
//! apply (L3s, FIG-5196).
//!
//! The dispatch in `durable/mod.rs` calls [`apply`] inside the fenced owner
//! commit, after the fence; a refusal rolls the whole commit back.

use lash_durable::domain::{
    DomainRefusal, MailBatch, MailBatchKind, MailInput, SessionMailWrite, SessionMailbox,
};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_sansio::{BatchId, InputId, SessionId, TurnId};
use sqlx::PgConnection;

use super::{Committing, SQL, get, sqlx_failure};

fn undecodable(what: &str, detail: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("session mail {what} does not decode: {detail}"),
    })
}

/// Session `session`'s standing: `(live, closing, follow_on_owed)`.
pub(super) async fn standing(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<(bool, bool, bool), DurableError> {
    let row = sqlx::query(SQL.session_mail.standing.sql())
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let meta: i64 = get(&row, 0)?;
    let deleted: i64 = get(&row, 1)?;
    let closing: i64 = get(&row, 2)?;
    let follow_on: i64 = get(&row, 3)?;
    Ok((meta > 0 && deleted == 0, closing > 0, follow_on > 0))
}

/// The session's open mail, read in one snapshot.
pub(super) async fn read(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<SessionMailbox, DurableError> {
    let (live, closing, follow_on_owed) = standing(tx, session).await?;
    let mut mailbox = SessionMailbox {
        live,
        closing,
        follow_on_owed,
        ..SessionMailbox::default()
    };
    if !mailbox.live {
        return Ok(mailbox);
    }
    let bound: Option<String> = sqlx::query_scalar(SQL.session_mail.bound_run.sql())
        .bind(session.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    mailbox.bound_run = bound
        .map(TurnId::parse)
        .transpose()
        .map_err(|error| undecodable("bound run", error))?;
    let inputs = sqlx::query(SQL.session_mail.open_inputs.sql())
        .bind(session.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    for row in &inputs {
        mailbox.inputs.push(decode_input(
            get(row, 0)?,
            get(row, 1)?,
            get(row, 2)?,
            get(row, 3)?,
            &get::<String>(row, 4)?,
            &get::<String>(row, 5)?,
        )?);
    }
    let batches = sqlx::query(SQL.session_mail.open_batches.sql())
        .bind(session.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    for row in &batches {
        mailbox.batches.push(decode_batch(
            get(row, 0)?,
            get(row, 1)?,
            &get::<String>(row, 2)?,
            &get::<String>(row, 3)?,
            &get::<String>(row, 4)?,
        )?);
    }
    Ok(mailbox)
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
pub(super) async fn apply(
    tx: &mut PgConnection,
    _commit: &Committing<'_>,
    write: &SessionMailWrite,
) -> Result<(), DurableError> {
    let SessionMailWrite::Admit {
        session,
        run,
        inputs,
        batches,
    } = write;
    let moved = |item: &str| {
        DurableError::Domain(DomainRefusal::SessionMailMoved {
            session: session.clone(),
            item: item.to_owned(),
        })
    };
    for input in inputs {
        let bound: Option<String> = sqlx::query_scalar(SQL.session_mail.bind_input.sql())
            .bind(session.as_str())
            .bind(input.as_str())
            .bind(run.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_failure)?;
        if bound.is_none() {
            return Err(moved(input.as_str()));
        }
        sqlx::query(SQL.session_mail.record_run_input.sql())
            .bind(session.as_str())
            .bind(input.as_str())
            .bind(run.as_str())
            .execute(&mut *tx)
            .await
            .map_err(sqlx_failure)?;
    }
    for batch in batches {
        let bound: Option<String> = sqlx::query_scalar(SQL.session_mail.bind_batch.sql())
            .bind(session.as_str())
            .bind(batch.as_str())
            .bind(run.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_failure)?;
        if bound.is_none() {
            return Err(moved(batch.as_str()));
        }
    }
    Ok(())
}

/// Arm (`true`) or disarm a cut of every session-mail producer transaction
/// at the session actor's wake: while armed, the wake's `mail_seq` bump
/// raises, so the producer's transaction rolls back before it commits. The
/// mail atomicity law's fault.
#[cfg(any(test, feature = "testing"))]
pub(crate) async fn cut_session_wakes(pool: &sqlx::PgPool, armed: bool) -> sqlx::Result<()> {
    sqlx::raw_sql(if armed {
        "CREATE OR REPLACE FUNCTION lash_test_cut_wake() RETURNS trigger
         LANGUAGE plpgsql AS $$ BEGIN
             RAISE EXCEPTION 'cut before the producer commit';
         END; $$;
         CREATE TRIGGER lash_test_cut_wake BEFORE UPDATE OF mail_seq ON lash_actors
         FOR EACH ROW EXECUTE FUNCTION lash_test_cut_wake();"
    } else {
        "DROP TRIGGER lash_test_cut_wake ON lash_actors;
         DROP FUNCTION lash_test_cut_wake();"
    })
    .execute(pool)
    .await
    .map(|_| ())
}
