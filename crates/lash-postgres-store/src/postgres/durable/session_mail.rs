//! The session's mailbox on PostgreSQL: the `session_mail` domain's read and
//! apply (L3s, FIG-5196).
//!
//! The dispatch in `durable/mod.rs` calls [`apply`] inside the fenced owner
//! commit, after the fence; a refusal rolls the whole commit back.

use lash_durable::domain::{
    DomainRefusal, MailBatch, MailBatchKind, MailInput, SessionMailWrite, SessionMailbox,
    queued_input_run,
};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_sansio::{BatchId, InputId, SessionId, TurnId};
use sqlx::PgConnection;

use super::{Committing, SQL, get, sqlx_failure};

impl super::PostgresDurableStore {
    /// Session `session`'s write authority, held to the commit: its actor
    /// row, then its history lock. Every session writer takes the two in
    /// this order, as an owner commit does, so no writer holds history
    /// while it waits for the actor row, and racing producers queue instead
    /// of deadlocking (FIG-5423, FIG-5429).
    ///
    /// A session with no actor yet has no row to lock: history alone orders
    /// its writers, and the one that creates the actor holds history. An
    /// actor created while this transaction waited for history would leave
    /// it holding history without the row, so the history lock is taken in
    /// a savepoint, and released to take the new row first. Actor rows are
    /// never deleted, so this backs off at most once.
    pub(crate) async fn lock_session_writes(
        tx: &mut PgConnection,
        session: &SessionId,
    ) -> Result<(), crate::StoreError> {
        use crate::support::store_sqlx_error;
        let connection = crate::connection_sql::connection_sql();
        let actor = session_actor(session)?;
        loop {
            let held: Option<i64> = sqlx::query_scalar(SQL.postgres.lock_session_writes.sql())
                .bind(actor.as_str())
                .bind(session.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            if held.is_some() {
                return Ok(());
            }
            sqlx::query(connection.savepoint_session_history.sql())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            sqlx::query(connection.lock_xact_session_history.sql())
                .bind(session.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            let created = sqlx::query(SQL.actor.epoch_of.sql())
                .bind(actor.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?
                .is_some();
            if created {
                sqlx::query(connection.rollback_to_session_history.sql())
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(store_sqlx_error)?;
            }
            sqlx::query(connection.release_session_history.sql())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            if !created {
                return Ok(());
            }
        }
    }

    /// The session's actor row alone: an owner commit's first lock, as a
    /// law's owner takes it.
    #[cfg(test)]
    pub(crate) async fn lock_session_actor(
        tx: &mut PgConnection,
        session: &SessionId,
    ) -> Result<(), crate::StoreError> {
        let actor = session_actor(session)?;
        sqlx::query(SQL.postgres.lock_actors.sql())
            .bind([actor.as_str()].as_slice())
            .execute(crate::observed_sql::executor(tx))
            .await
            .map_err(crate::support::store_sqlx_error)?;
        Ok(())
    }
}

fn session_actor(session: &SessionId) -> Result<lash_durable::ActorKey, crate::StoreError> {
    lash_durable::ActorKey::session(session.as_str())
        .map_err(|error| crate::StoreError::Backend(error.to_string()))
}

fn undecodable(what: &str, detail: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("session mail {what} does not decode: {detail}"),
    })
}

/// Session `session`'s standing: `(live, closing)`.
pub(super) async fn standing(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<(bool, bool), DurableError> {
    let row = sqlx::query(SQL.session_mail.standing.sql())
        .bind(session.as_str())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let meta: i64 = get(&row, 0)?;
    let deleted: i64 = get(&row, 1)?;
    let closing: i64 = get(&row, 2)?;
    Ok((meta > 0 && deleted == 0, closing > 0))
}

/// The session's open mail, read in one snapshot.
pub(super) async fn read(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<SessionMailbox, DurableError> {
    let (live, closing) = standing(tx, session).await?;
    let mut mailbox = SessionMailbox {
        live,
        closing,
        ..SessionMailbox::default()
    };
    if !mailbox.live {
        return Ok(mailbox);
    }
    let bound: Option<String> = sqlx::query_scalar(SQL.session_mail.bound_run.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    mailbox.bound_run = bound
        .map(TurnId::parse)
        .transpose()
        .map_err(|error| undecodable("bound run", error))?;
    let inputs = sqlx::query(SQL.session_mail.open_inputs.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
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
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    for row in &batches {
        mailbox.batches.push(decode_batch(
            get(row, 0)?,
            get(row, 1)?,
            &get::<String>(row, 2)?,
            &get::<String>(row, 3)?,
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
    policy: &str,
    payload_json: &str,
) -> Result<MailBatch, DurableError> {
    let payload: lash_core_execution::runtime::QueuedWorkPayload =
        serde_json::from_str(payload_json).map_err(|error| undecodable("batch payload", error))?;
    let lash_core_execution::runtime::QueuedWorkPayload::SessionCommand { command } = &payload;
    let kind = if matches!(
        **command,
        lash_core_execution::runtime::SessionCommand::RunPluginTask { .. }
    ) {
        MailBatchKind::Operation
    } else {
        MailBatchKind::Control
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
pub(super) async fn settle_held(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    now_ms: i64,
) -> Result<(), DurableError> {
    sqlx::query(SQL.session_mail.settle_held_inputs.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
        .bind(now_ms)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    sqlx::query(SQL.session_mail.settle_held_batches.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .bind(now_ms)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

/// Withdraw the input session `session` would admit as run `run`, while it
/// is still open and unbound session mail: into its `cancelled` tombstone at
/// `now_ms`, so no run ever takes it (FIG-5262). `None` when no open input
/// names the run, or an admission bound it first: the withdraw's `UPDATE`
/// waits on the row an admission holds and then finds it bound.
pub(super) async fn withdraw_queued(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    now_ms: i64,
) -> Result<Option<InputId>, DurableError> {
    let candidates: Vec<(String, Option<String>)> =
        sqlx::query_as(SQL.session_mail.open_inputs_named.sql())
            .bind(session.as_str())
            .bind(run.as_str())
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    for (input, source_key) in candidates {
        let input = InputId::parse(input).map_err(|error| undecodable("input id", error))?;
        if queued_input_run(&input, source_key.as_deref()).as_ref() != Some(run) {
            continue;
        }
        let withdrawn: Option<String> = sqlx::query_scalar(SQL.session_mail.withdraw_input.sql())
            .bind(session.as_str())
            .bind(input.as_str())
            .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
            .bind(now_ms)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        return Ok(withdrawn.map(|_| input));
    }
    Ok(None)
}

/// Apply the owner's session-mail write: bind admitted mail to its run, or
/// mail the session its own next-turn input.
pub(super) async fn apply(
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    write: &SessionMailWrite,
) -> Result<(), DurableError> {
    match write {
        SessionMailWrite::Admit {
            session,
            run,
            inputs,
            batches,
        } => admit(tx, session, run, inputs, batches).await,
        SessionMailWrite::Enqueue {
            session,
            draft_json,
        } => enqueue(tx, commit, session, draft_json).await,
    }
}

/// Insert the owner's input as any producer's acceptance does, with the
/// session's wake, in the owner commit. A closing or gone session takes no
/// mail: its close settles what it holds, and the write inserts nothing.
async fn enqueue(
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    session: &SessionId,
    draft_json: &str,
) -> Result<(), DurableError> {
    let refused = |reason: String| {
        DurableError::Domain(DomainRefusal::SessionMailRefused {
            session: session.clone(),
            reason,
        })
    };
    let (live, closing) = standing(tx, session).await?;
    if !live || closing {
        return Ok(());
    }
    let draft: lash_core_execution::PendingTurnInputDraft =
        serde_json::from_str(draft_json).map_err(|error| undecodable("owner input", error))?;
    let batch = lash_core_execution::PendingTurnInputBatch::new(session.clone(), vec![draft])
        .map_err(|error| refused(error.to_string()))?;
    let now = u64::try_from(commit.now.0).unwrap_or(0);
    match crate::runtime_persistence::enqueue_pending_turn_inputs_tx(tx, &batch, now).await {
        Ok(_) => Ok(()),
        Err(error @ lash_core_execution::StoreError::Contended) => Err(super::store_failure(error)),
        Err(error) => Err(refused(error.to_string())),
    }
}

/// Bind the admitted mail to its run, each row still open and unbound.
async fn admit(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    inputs: &[InputId],
    batches: &[BatchId],
) -> Result<(), DurableError> {
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
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
        if bound.is_none() {
            return Err(moved(input.as_str()));
        }
        sqlx::query(SQL.session_mail.record_run_input.sql())
            .bind(session.as_str())
            .bind(input.as_str())
            .bind(run.as_str())
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    }
    for batch in batches {
        let bound: Option<String> = sqlx::query_scalar(SQL.session_mail.bind_batch.sql())
            .bind(session.as_str())
            .bind(batch.as_str())
            .bind(run.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
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
