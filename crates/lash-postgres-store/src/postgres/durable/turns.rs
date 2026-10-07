//! Turn phase state, the session commit and turn cancel requests on PostgreSQL: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_core_execution::store_backend_support::turn_cancel::{
    turn_cancel_mode_from_wire, turn_cancel_mode_wire, turn_cancel_undelivered_from_wire,
    turn_cancel_undelivered_wire,
};
use lash_durable::domain::{
    DomainRefusal, ModelPin, SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnEnd,
    TurnPhase, TurnRow, TurnTerminal, TurnWrite,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, Woken};
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
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    write: &SessionCommitWrite,
) -> Result<(), DurableError> {
    use lash_core_execution::StoreError;
    let refused = |reason: String| {
        DurableError::Domain(DomainRefusal::SessionCommitRefused {
            session: write.session.clone(),
            reason,
        })
    };
    let runtime_commit = lash_core_execution::store::decode_session_commit(&write.commit_json)
        .map_err(|error| refused(error.to_string()))?;
    if runtime_commit.session_id != write.session
        || runtime_commit.expected_head_revision != write.expected_head
    {
        return Err(refused(format!(
            "the commit names session {} at head {}, not {} at {}",
            runtime_commit.session_id,
            runtime_commit.expected_head_revision,
            write.session,
            write.expected_head
        )));
    }
    let planner =
        lash_core_execution::store::RuntimeCommitPlanner::prepare(runtime_commit, tx.fleet())
            .map_err(|error| refused(error.to_string()))?;
    let now = u64::try_from(commit.now.0).unwrap_or(0);
    match crate::runtime_persistence::apply_runtime_commit_tx(tx, &planner, now).await {
        Ok(_) => Ok(()),
        Err(StoreError::HeadRevisionConflict { expected, actual }) => {
            Err(DurableError::Domain(DomainRefusal::HeadMoved {
                session: write.session.clone(),
                expected,
                found: Some(actual),
            }))
        }
        Err(error @ StoreError::Contended) => Err(super::store_failure(error)),
        Err(error) => Err(refused(error.to_string())),
    }
}

pub(super) async fn request_cancel(
    tx: &mut PgConnection,
    request: &TurnCancelRequest,
    now: DurableInstant,
) -> Result<(TurnCancelAnswer, Option<Woken>), DurableError> {
    let session = request.session.as_str();
    let run = request.run.as_str();
    let open: Option<String> = sqlx::query_scalar(SQL.open_named_run.sql())
        .bind(session)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    if open.is_none() {
        return Ok((TurnCancelAnswer::AlreadyEnded, None));
    }
    let answer = match cancel_of(tx, &request.session, &request.run).await? {
        None => {
            sqlx::query(SQL.insert_cancel.sql())
                .bind(session)
                .bind(run)
                .bind(&request.request_id)
                .bind(&request.origin)
                .bind(&request.reason)
                .bind(turn_cancel_undelivered_wire(request.undelivered))
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            TurnCancelAnswer::Requested
        }
        Some(mut accepted) if request.escalates(&accepted) => {
            sqlx::query(SQL.escalate_cancel.sql())
                .bind(session)
                .bind(run)
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(&mut *tx)
                .await
                .map_err(sqlx_failure)?;
            accepted.mode = request.mode;
            TurnCancelAnswer::Escalated { accepted }
        }
        Some(accepted) if accepted.undelivered != request.undelivered => {
            return Ok((TurnCancelAnswer::PolicyConflict { accepted }, None));
        }
        Some(accepted) => return Ok((TurnCancelAnswer::AlreadyRequested { accepted }, None)),
    };
    let actor = ActorKey::session(session).map_err(|_| corrupt("session id", session))?;
    let (woken, _) = super::wake_within(tx, &actor, true, now).await?;
    Ok((answer, Some(woken)))
}

/// A stored cancel request: request id, origin, reason, disposition, mode.
type StoredCancel = (String, Option<String>, Option<String>, String, String);

/// The cancel request run `run` of `session` accepted.
async fn cancel_of(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Option<TurnCancelRequest>, DurableError> {
    let stored: Option<StoredCancel> = sqlx::query_as(SQL.cancel_of.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let Some((request_id, origin, reason, disposition, mode)) = stored else {
        return Ok(None);
    };
    let (Ok(undelivered), Ok(mode)) = (
        turn_cancel_undelivered_from_wire(&disposition),
        turn_cancel_mode_from_wire(&mode),
    ) else {
        return Err(corrupt(
            "turn cancel request",
            &format!("{disposition}/{mode}"),
        ));
    };
    Ok(Some(TurnCancelRequest {
        session: session.clone(),
        run: run.clone(),
        request_id,
        origin,
        reason,
        undelivered,
        mode,
    }))
}

pub(super) async fn turn_end(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Option<TurnEnd>, DurableError> {
    let Some(row) = sqlx::query(SQL.ended.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let terminal: String = row.try_get(0).map_err(sqlx_failure)?;
    let cause_json: Option<String> = row.try_get(1).map_err(sqlx_failure)?;
    let head_revision: Option<i64> = row.try_get(2).map_err(sqlx_failure)?;
    let terminal =
        TurnTerminal::parse(&terminal).ok_or_else(|| corrupt("turn terminal", &terminal))?;
    Ok(Some(TurnEnd {
        terminal,
        cause_json: cause_json.filter(|cause| cause != "null"),
        head_revision: head_revision.and_then(|revision| u64::try_from(revision).ok()),
    }))
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
    let written_epoch = Epoch(row.try_get(10).map_err(decode)?);
    let admission_json = row.try_get(1).map_err(decode)?;
    let checkpoint_ref = row.try_get(5).map_err(decode)?;
    let cancel = cancel_of(tx, session, &run).await?;
    Ok(Some(TurnRow {
        session: session.clone(),
        run,
        admission_json,
        phase,
        iteration: u32::try_from(iteration).unwrap_or(u32::MAX),
        checkpoint_ref,
        model,
        turn_deadline: turn_deadline.map(DurableInstant),
        written_epoch,
        cancel,
    }))
}
