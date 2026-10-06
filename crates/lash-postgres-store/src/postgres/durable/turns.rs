//! Turn phase state, the session commit and turn-cancel mail on PostgreSQL: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_durable::domain::{
    DomainRefusal, ModelPin, SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnPhase,
    TurnRow, TurnWrite,
};
use lash_durable::{DurableError, DurableInstant, Epoch, Woken};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::durable::turns::TurnStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, corrupt, sqlx_failure};

static SQL: LazyLock<TurnStatements> =
    LazyLock::new(|| TurnStatements::render(Dialect::postgres()));

fn refuse<T>(refusal: DomainRefusal) -> Result<T, DurableError> {
    Err(DurableError::Domain(refusal))
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &TurnWrite,
) -> Result<(), DurableError> {
    match write {
        TurnWrite::Admit {
            session,
            run,
            admission_json,
            turn_deadline,
        } => {
            let open: Option<String> = sqlx::query_scalar(SQL.open_run.sql())
                .bind(session.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            if open.is_some() {
                return refuse(DomainRefusal::OpenTurnExists {
                    session: session.clone(),
                });
            }
            sqlx::query(SQL.insert_run.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(admission_json)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            let (phase, argument) = TurnPhase::Admitted.stored();
            sqlx::query(SQL.insert_phase.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(phase)
                .bind(argument.map(signed))
                .bind(0_i64)
                .bind(Option::<String>::None)
                .bind(Option::<i64>::None)
                .bind(Option::<String>::None)
                .bind(Option::<i64>::None)
                .bind(turn_deadline.map(|deadline| deadline.0))
                .bind(commit.epoch.0)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        TurnWrite::Advance {
            session,
            run,
            phase,
            iteration,
            checkpoint_ref,
            model,
        } => {
            let (phase, argument) = phase.stored();
            let advanced: Option<String> = sqlx::query_scalar(SQL.advance_phase.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(phase)
                .bind(argument.map(signed))
                .bind(i64::from(*iteration))
                .bind(checkpoint_ref.as_deref())
                .bind(model.as_ref().map(|pin| i64::from(pin.attempt)))
                .bind(model.as_ref().map(|pin| pin.request_ref.as_str()))
                .bind(model.as_ref().map(|pin| pin.deadline.0))
                .bind(commit.epoch.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            if advanced.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            Ok(())
        }
        TurnWrite::Terminal {
            session,
            run,
            terminal,
            cause_json,
            head_revision,
        } => {
            let ended: Option<String> = sqlx::query_scalar(SQL.end_run.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(terminal.as_str())
                .bind(cause_json.as_deref().unwrap_or("null"))
                .bind(head_revision.map(signed))
                .bind(commit.now.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            if ended.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            sqlx::query(SQL.delete_phase.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
    }
}

pub(super) async fn apply_session_commit(
    tx: &mut PgConnection,
    _commit: &Committing<'_>,
    write: &SessionCommitWrite,
) -> Result<(), DurableError> {
    let session = write.session.as_str();
    let found: Option<i64> = sqlx::query_scalar(SQL.head.sql())
        .bind(session)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let found = found.map(|head| u64::try_from(head).unwrap_or(0));
    if found.unwrap_or(0) != write.expected_head {
        return refuse(DomainRefusal::HeadMoved {
            session: write.session.clone(),
            expected: write.expected_head,
            found,
        });
    }
    let next = signed(write.expected_head.saturating_add(1));
    sqlx::query(SQL.insert_revision.sql())
        .bind(session)
        .bind(next)
        .bind(&write.commit_json)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    sqlx::query(SQL.move_head.sql())
        .bind(session)
        .bind(next)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    Ok(())
}

pub(super) async fn request_cancel(
    _tx: &mut PgConnection,
    _request: &TurnCancelRequest,
    _now: DurableInstant,
) -> Result<(TurnCancelAnswer, Option<Woken>), DurableError> {
    todo!("L3 (FIG-5172): record a turn cancel request and wake the session on PostgreSQL")
}

pub(super) async fn turn(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Option<TurnRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.unfinished.sql())
        .bind(session.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let decode = |error: sqlx::Error| sqlx_failure(error);
    let run: String = row.try_get(0).map_err(decode)?;
    let run = TurnId::try_from(run.clone()).map_err(|_| corrupt("turn id", &run))?;
    let phase: String = row.try_get(2).map_err(decode)?;
    let argument: Option<i64> = row.try_get(3).map_err(decode)?;
    let argument = argument.and_then(|value| u64::try_from(value).ok());
    let phase = TurnPhase::parse(&phase, argument).ok_or_else(|| corrupt("turn phase", &phase))?;
    let attempt: Option<i64> = row.try_get(6).map_err(decode)?;
    let request: Option<String> = row.try_get(7).map_err(decode)?;
    let deadline: Option<i64> = row.try_get(8).map_err(decode)?;
    let model = match (attempt, request, deadline) {
        (Some(attempt), Some(request_ref), Some(deadline)) => Some(ModelPin {
            attempt: u32::try_from(attempt).unwrap_or(u32::MAX),
            request_ref,
            deadline: DurableInstant(deadline),
        }),
        _ => None,
    };
    let iteration: i64 = row.try_get(4).map_err(decode)?;
    let turn_deadline: Option<i64> = row.try_get(9).map_err(decode)?;
    Ok(Some(TurnRow {
        session: session.clone(),
        run,
        admission_json: row.try_get(1).map_err(decode)?,
        phase,
        iteration: u32::try_from(iteration).unwrap_or(u32::MAX),
        checkpoint_ref: row.try_get(5).map_err(decode)?,
        model,
        turn_deadline: turn_deadline.map(DurableInstant),
        written_epoch: Epoch(row.try_get(10).map_err(decode)?),
    }))
}
