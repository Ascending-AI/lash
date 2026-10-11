//! The session's mailbox on PostgreSQL: the `session_mail` domain's read and
//! apply (L3s, FIG-5196).
//!
//! The dispatch in `durable/mod.rs` calls [`apply`] inside the fenced owner
//! commit, after the fence; a refusal rolls the whole commit back.

use lash_durable::domain::{
    DomainRefusal, MailBatch, MailBatchKind, MailInput, OperationCancelAnswer,
    OperationCancelRequest, SessionMailWrite, SessionMailbox, queued_input_run,
};
use lash_durable::{ActorKey, DurableInstant, Woken};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_sansio::{BatchId, InputId, SessionId, TurnId};
use sqlx::PgConnection;

use super::steps::{self, Entry, Shape, Step, Ty, Value};
use super::{Committing, SQL, get, sqlx_failure};

lash_store_sql::statements! {
    /// The session-mail statements only PostgreSQL issues: `UNNEST` binds a
    /// run's admitted mail as one list.
    struct PostgresSessionMailStatements @ "durable_session_mail_postgres" {
        /// Bind each input `?2[i]` of session `?1` to run `?3`, its admitting
        /// owner, or keep it bound there, as the neutral `bind_input` does
        /// one input; returns each input it bound.
        bind_inputs = "UPDATE pending_turn_inputs AS input
             SET admitted_run = ?3, admitted_by = ?3
             FROM UNNEST(?2::text[]) AS named(input_id)
             WHERE input.session_id = ?1 AND input.input_id = named.input_id
               AND (input.admitted_run IS NULL OR input.admitted_run = ?3)
               AND input.state IN ('deferred_next_turn', 'pending_active')
             RETURNING input.input_id";

        /// Record that run `?3` of session `?1` executes each input `?2[i]`.
        record_run_inputs = "INSERT INTO session_run_inputs (session_id, input_id, run)
             SELECT ?1, named.input_id, ?3 FROM UNNEST(?2::text[]) AS named(input_id)
             ON CONFLICT (session_id, input_id) DO NOTHING";

        /// Bind each batch `?2[i]` of session `?1` to run `?3`, as the
        /// neutral `bind_batch` does one batch; returns each batch it bound.
        bind_batches = "UPDATE queued_work_batches AS batch
             SET admitted_run = ?3, admitted_by = ?3
             FROM UNNEST(?2::text[]) AS named(batch_id)
             WHERE batch.session_id = ?1 AND batch.batch_id = named.batch_id
               AND (batch.admitted_run IS NULL OR batch.admitted_run = ?3)
               AND batch.terminal_cause IS NULL
             RETURNING batch.batch_id";
    }
}

static POSTGRES_SQL: std::sync::LazyLock<PostgresSessionMailStatements> =
    std::sync::LazyLock::new(|| {
        PostgresSessionMailStatements::render(lash_store_sql::Dialect::postgres())
    });

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
            let held: Option<i64> =
                crate::observed_sql::query_scalar(SQL.postgres.lock_session_writes.sql())
                    .bind(actor.as_str())
                    .bind(session.as_str())
                    .fetch_optional(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(store_sqlx_error)?;
            if held.is_some() {
                return Ok(());
            }
            crate::observed_sql::query(connection.savepoint_session_history.sql())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            crate::observed_sql::query(connection.lock_xact_session_history.sql())
                .bind(session.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?;
            let created = crate::observed_sql::query(SQL.actor.epoch_of.sql())
                .bind(actor.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(store_sqlx_error)?
                .is_some();
            if created {
                crate::observed_sql::query(connection.rollback_to_session_history.sql())
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(store_sqlx_error)?;
            }
            crate::observed_sql::query(connection.release_session_history.sql())
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
        crate::observed_sql::query(SQL.postgres.lock_actors.sql())
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

/// Record the inputs this stop affects at the terminal transaction's
/// boundary, after its head commit settled delivered inputs. A drop binds
/// them for `settle_held`; deferral keeps their original ingress position.
pub(super) async fn cancel_inputs(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    disposition: lash_core_execution::TurnCancelUndeliveredInputPolicy,
) -> Result<lash_core_execution::TurnCancelInputOutcome, DurableError> {
    let rows = crate::observed_sql::query(SQL.session_mail.cancel_inputs.sql())
        .bind(session.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let mut outcome = lash_core_execution::TurnCancelInputOutcome::default();
    for row in &rows {
        let Some(affected) =
            lash_core_execution::store_backend_support::turn_cancel::affected_input(
                run,
                disposition,
                get(row, 0)?,
                &get::<String>(row, 1)?,
                &get::<String>(row, 2)?,
            )
            .map_err(|error| undecodable("cancelled input", error))?
        else {
            continue;
        };
        if disposition == lash_core_execution::TurnCancelUndeliveredInputPolicy::Drop {
            let write = SessionMailWrite::Admit {
                session: session.clone(),
                run: run.clone(),
                inputs: vec![affected.input_id.clone()],
                batches: Vec::new(),
            };
            admit(&mut *tx, &write).await?;
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(outcome)
}

/// Session `session`'s standing: `(live, closing)`.
pub(super) async fn standing(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<(bool, bool), DurableError> {
    let row = crate::observed_sql::query(SQL.session_mail.standing.sql())
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
    let bound: Option<String> = crate::observed_sql::query_scalar(SQL.session_mail.bound_run.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    mailbox.bound_run = bound
        .map(TurnId::parse)
        .transpose()
        .map_err(|error| undecodable("bound run", error))?;
    let inputs = crate::observed_sql::query(SQL.session_mail.open_inputs.sql())
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
    let batches = crate::observed_sql::query(SQL.session_mail.open_batches.sql())
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
            get::<i32>(row, 4)? > 0,
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
    cancel_requested: bool,
) -> Result<MailBatch, DurableError> {
    Ok(MailBatch {
        batch: BatchId::parse(batch).map_err(|error| undecodable("batch id", error))?,
        enqueue_seq: u64::try_from(seq).map_err(|error| undecodable("batch order", error))?,
        kind: batch_kind(payload_json)?,
        after_current_turn: policy == "after_current_turn_commit",
        cancel_requested,
    })
}

/// What a batch whose payload is `payload_json` asks the session for.
fn batch_kind(payload_json: &str) -> Result<MailBatchKind, DurableError> {
    let payload: lash_core_execution::runtime::QueuedWorkPayload =
        serde_json::from_str(payload_json).map_err(|error| undecodable("batch payload", error))?;
    let lash_core_execution::runtime::QueuedWorkPayload::SessionCommand { command } = &payload;
    Ok(
        if matches!(
            **command,
            lash_core_execution::runtime::SessionCommand::RunPluginTask { .. }
        ) {
            MailBatchKind::Operation
        } else {
            MailBatchKind::Control
        },
    )
}

/// A host's cancel of plugin task `request.batch`'s operation run
/// (FIG-5343), in one mail transaction: a command no operation run entered
/// is withdrawn, with a wake; an entered one records the first request,
/// with a control wake, so its running handler is cancelled. Each write's
/// `UPDATE` waits on the row an entry holds and then re-reads it, so a
/// withdrawal never passes an entry.
pub(super) async fn request_operation_cancel(
    tx: &mut PgConnection,
    request: &OperationCancelRequest,
    now: DurableInstant,
) -> Result<(OperationCancelAnswer, Option<Woken>), DurableError> {
    let session = request.session.as_str();
    let batch = request.batch.as_str();
    let actor = ActorKey::session(session).map_err(|_| undecodable("session id", session))?;
    let open: Option<(String, Option<String>)> =
        crate::observed_sql::query_as(SQL.session_mail.open_operation.sql())
            .bind(session)
            .bind(batch)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    let Some((payload_json, _)) = open else {
        return Ok((OperationCancelAnswer::NotOpen, None));
    };
    if batch_kind(&payload_json)? != MailBatchKind::Operation {
        return Ok((OperationCancelAnswer::NotOpen, None));
    }
    let withdrawn: Option<String> =
        crate::observed_sql::query_scalar(SQL.session_mail.withdraw_unentered.sql())
            .bind(session)
            .bind(batch)
            .bind(now.0)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    if withdrawn.is_some() {
        let (woken, _) = super::wake_within(tx, &actor, false, now).await?;
        return Ok((OperationCancelAnswer::Withdrawn, Some(woken)));
    }
    let requested: Option<String> =
        crate::observed_sql::query_scalar(SQL.session_mail.request_operation_cancel.sql())
            .bind(session)
            .bind(batch)
            .bind(&request.request_json)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    if requested.is_some() {
        let (woken, _) = super::wake_within(tx, &actor, true, now).await?;
        return Ok((OperationCancelAnswer::Requested, Some(woken)));
    }
    // The row moved under the writes: it settled, or a racing request
    // was accepted first.
    let open: Option<(String, Option<String>)> =
        crate::observed_sql::query_as(SQL.session_mail.open_operation.sql())
            .bind(session)
            .bind(batch)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    Ok((
        match open.and_then(|(_, accepted)| accepted) {
            Some(request_json) => OperationCancelAnswer::AlreadyRequested { request_json },
            None => OperationCancelAnswer::NotOpen,
        },
        None,
    ))
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
    crate::observed_sql::query(SQL.session_mail.settle_held_inputs.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
        .bind(now_ms)
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    crate::observed_sql::query(SQL.session_mail.settle_held_batches.sql())
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
        crate::observed_sql::query_as(SQL.session_mail.open_inputs_named.sql())
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
        let withdrawn: Option<String> =
            crate::observed_sql::query_scalar(SQL.session_mail.withdraw_input.sql())
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
        SessionMailWrite::Admit { .. } => admit(tx, write).await,
        SessionMailWrite::Enqueue {
            session,
            draft_json,
        } => enqueue(tx, commit, session, draft_json).await,
        SessionMailWrite::EnterOperation { session, batch } => {
            enter_operation(tx, commit, session, batch).await
        }
    }
}

/// Enter plugin task `batch`'s operation run, its command still open.
async fn enter_operation(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    session: &SessionId,
    batch: &BatchId,
) -> Result<(), DurableError> {
    let entered: Option<String> =
        crate::observed_sql::query_scalar(SQL.session_mail.enter_operation.sql())
            .bind(session.as_str())
            .bind(batch.as_str())
            .bind(commit.now.0)
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    match entered {
        Some(_) => Ok(()),
        None => Err(DurableError::Domain(
            DomainRefusal::SessionCommandWithdrawn {
                session: session.clone(),
                batch: batch.clone(),
            },
        )),
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
    // The owner mails its own session: its next pass reads the input, so the
    // wake needs no hint.
    match crate::runtime_persistence::enqueue_pending_turn_inputs_tx(tx, &batch, now).await {
        Ok(_) => Ok(()),
        Err(error @ lash_core_execution::StoreError::Contended) => Err(super::store_failure(error)),
        Err(error) => Err(refused(error.to_string())),
    }
}

/// The session-mail statements an owner commit runs as steps.
pub(super) static BIND_INPUTS: Entry = Entry::new(
    || &POSTGRES_SQL.bind_inputs,
    &[Ty::Text, Ty::Texts, Ty::Text],
    Shape::Returning,
);
pub(super) static RECORD_RUN_INPUTS: Entry = Entry::new(
    || &POSTGRES_SQL.record_run_inputs,
    &[Ty::Text, Ty::Texts, Ty::Text],
    Shape::Exec,
);
pub(super) static BIND_BATCHES: Entry = Entry::new(
    || &POSTGRES_SQL.bind_batches,
    &[Ty::Text, Ty::Texts, Ty::Text],
    Shape::Returning,
);

/// `write`'s step program: an admission binds its inputs, records the run
/// executes them, and binds its batches, one statement per kind whatever
/// their number, each bind answering every item it names. An enqueue or an
/// operation's entry has none: what it admits is read.
pub(super) fn steps(write: &SessionMailWrite) -> Option<Vec<Step>> {
    let SessionMailWrite::Admit {
        session,
        run,
        inputs,
        batches,
    } = write
    else {
        return None;
    };
    let named = |items: Vec<&str>| {
        vec![
            Value::text(session.as_str()),
            Value::Texts(
                items
                    .into_iter()
                    .map(|item| Some(item.to_owned()))
                    .collect(),
            ),
            Value::text(run.as_str()),
        ]
    };
    let mut program = Vec::new();
    if !inputs.is_empty() {
        let items: Vec<&str> = inputs.iter().map(InputId::as_str).collect();
        program.push(Step::new(
            &BIND_INPUTS,
            named(items.clone()),
            Some(items.len() as u64),
        ));
        program.push(Step::any(&RECORD_RUN_INPUTS, named(items)));
    }
    if !batches.is_empty() {
        let items: Vec<&str> = batches.iter().map(BatchId::as_str).collect();
        program.push(Step::new(
            &BIND_BATCHES,
            named(items.clone()),
            Some(items.len() as u64),
        ));
    }
    Some(program)
}

/// Bind the admitted mail to its run, each row still open and unbound. The
/// first item, in the order named, that did not bind refuses the commit.
async fn admit(tx: &mut PgConnection, write: &SessionMailWrite) -> Result<(), DurableError> {
    let SessionMailWrite::Admit {
        session,
        inputs,
        batches,
        ..
    } = write
    else {
        return Ok(());
    };
    let program = steps(write).unwrap_or_default();
    let Some(refused) = steps::run(tx, &program).await? else {
        return Ok(());
    };
    let bound: std::collections::HashSet<String> = refused
        .rows
        .iter()
        .map(|row| get::<String>(row, 0))
        .collect::<Result<_, _>>()?;
    let named: Vec<&str> = if program[refused.index].entry.name() == BIND_INPUTS.name() {
        inputs.iter().map(InputId::as_str).collect()
    } else {
        batches.iter().map(BatchId::as_str).collect()
    };
    let item = named
        .into_iter()
        .find(|item| !bound.contains(*item))
        .unwrap_or_default();
    Err(DurableError::Domain(DomainRefusal::SessionMailMoved {
        session: session.clone(),
        item: item.to_owned(),
    }))
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
    .execute(crate::observed_sql::executor(pool))
    .await
    .map(|_| ())
}
