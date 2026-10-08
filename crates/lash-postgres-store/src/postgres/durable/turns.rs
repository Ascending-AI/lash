//! Turn phase state, the session commit and turn cancel requests on PostgreSQL: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_core_execution::store::{HeadWriter, RunAdmissionRecord, RunTerminalCause};
use lash_core_execution::store_backend_support::turn_cancel::{
    turn_cancel_mode_from_wire, turn_cancel_mode_wire, turn_cancel_undelivered_from_wire,
    turn_cancel_undelivered_wire,
};
use lash_durable::domain::{
    DomainRefusal, RunValuesWrite, SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest,
    TurnEnd, TurnNamespace, TurnRow, TurnWrite, UnfinishedPhase,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, Woken};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::durable::turns::TurnStatements;
use sqlx::{PgConnection, Row};

use super::{Committing, corrupt, integer, sqlx_failure};

static SQL: LazyLock<TurnStatements> =
    LazyLock::new(|| TurnStatements::render(Dialect::postgres()));

fn refuse<T>(refusal: DomainRefusal) -> Result<T, DurableError> {
    Err(DurableError::Domain(refusal))
}

/// A store codec's refusal, as the durable port reports it.
fn encoding(error: &lash_core_execution::StoreError) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
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
            admission,
            turn_deadline,
        } => {
            let open: Option<String> = sqlx::query_scalar(SQL.open_run.sql())
                .bind(session.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
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
                .bind(admission.to_stored().map_err(|error| encoding(&error))?)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            let (phase, argument) = UnfinishedPhase::Admitted.stored();
            sqlx::query(SQL.insert_phase.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(phase)
                .bind(argument.map(integer::<i64>).transpose()?)
                .bind(0_i64)
                .bind(Option::<String>::None)
                .bind(Option::<String>::None)
                .bind(Option::<i64>::None)
                .bind(turn_deadline.map(|deadline| deadline.0))
                .bind(commit.epoch.0)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            Ok(())
        }
        TurnWrite::Advance {
            session,
            run,
            phase,
            iteration,
        } => {
            let (stored, argument) = phase.stored();
            let pin = phase.model();
            let advanced: Option<String> = sqlx::query_scalar(SQL.advance_phase.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(stored)
                .bind(argument.map(integer::<i64>).transpose()?)
                .bind(i64::from(*iteration))
                .bind(phase.checkpoint())
                .bind(pin.map(|pin| pin.request_ref.as_str()))
                .bind(pin.map(|pin| pin.deadline.0))
                .bind(commit.epoch.0)
                .bind(pin.map(|pin| i64::from(pin.call)))
                .bind(pin.map(|pin| pin.stream_from.as_str()))
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
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
        TurnWrite::TraceExported { session, run } => {
            let recorded: Option<String> = sqlx::query_scalar(SQL.trace_exported.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(commit.epoch.0)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if recorded.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            Ok(())
        }
        TurnWrite::Namespaces {
            session,
            run,
            namespaces,
        } => {
            let open: Option<String> = sqlx::query_scalar(SQL.open_named_run.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            if open.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            for namespace in namespaces {
                let entry = serde_json::to_string(&namespace.entry)
                    .map_err(|error| corrupt("turn namespace entry", &error.to_string()))?;
                sqlx::query(SQL.upsert_namespace.sql())
                    .bind(session.as_str())
                    .bind(run.as_str())
                    .bind(namespace.plugin.as_str())
                    .bind(entry)
                    .bind(namespace.values.body())
                    .bind(matches!(namespace.values, RunValuesWrite::Held))
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(sqlx_failure)?;
            }
            Ok(())
        }
        TurnWrite::Terminal {
            session,
            run,
            cause,
            head_revision,
        } => {
            let window = super::super::session_runs::terminal_window_json(
                &mut *tx,
                session,
                commit.checkpoint_ref_chunk,
            )
            .await
            .map_err(|error| encoding(&error))?;
            let ended: Option<String> = sqlx::query_scalar(SQL.end_run.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .bind(cause.kind().as_str())
                .bind(cause.to_stored().map_err(|error| encoding(&error))?)
                .bind(head_revision.map(integer::<i64>).transpose()?)
                .bind(commit.now.0)
                .bind(window)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
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
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            sqlx::query(SQL.delete_namespaces.sql())
                .bind(session.as_str())
                .bind(run.as_str())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            super::session_mail::settle_held(&mut *tx, session, run, commit.now.0).await
        }
    }
}

pub(super) async fn apply_session_commit(
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    write: &SessionCommitWrite,
) -> Result<(), DurableError> {
    let refused = |error: &lash_core_execution::StoreError| {
        DurableError::session_commit(write.session.clone(), error)
    };
    let runtime_commit = lash_core_execution::store::decode_session_commit(&write.commit_json)
        .map_err(|error| refused(&error))?;
    if runtime_commit.session_id != write.session
        || runtime_commit.expected_head_revision != write.expected_head
    {
        return Err(DurableError::Domain(DomainRefusal::SessionCommitRefused {
            session: write.session.clone(),
            code: lash_core_execution::RuntimeErrorCode::StoreRefused,
            cause: None,
            reason: format!(
                "the commit names session {} at head {}, not {} at {}",
                runtime_commit.session_id,
                runtime_commit.expected_head_revision,
                write.session,
                write.expected_head
            ),
        }));
    }
    let planner =
        lash_core_execution::store::RuntimeCommitPlanner::prepare(runtime_commit, tx.fleet())
            .map_err(|error| refused(&error))?;
    let now = integer::<u64>(commit.now.0)?;
    let writer = head_writer(commit.actor, &write.session);
    crate::runtime_persistence::apply_runtime_commit_tx(
        tx,
        &planner,
        writer,
        now,
        commit.checkpoint_ref_chunk,
    )
    .await
    .map(drop)
    .map_err(|error| refused(&error))
}

/// Who writes a session head commit fenced by `actor`: the session's own
/// actor, whose epoch fences every head write it makes (FIG-5355), or any
/// other owner, which the head's ownership gate checks as a store writer.
fn head_writer(actor: &ActorKey, session: &SessionId) -> HeadWriter {
    if ActorKey::session(session.as_str()).is_ok_and(|own| &own == actor) {
        HeadWriter::SessionActor
    } else {
        HeadWriter::Store
    }
}

pub(super) async fn request_cancel(
    tx: &mut PgConnection,
    request: &TurnCancelRequest,
    now: DurableInstant,
) -> Result<(TurnCancelAnswer, Option<Woken>), DurableError> {
    let session = request.session.as_str();
    let run = request.run.as_str();
    let actor = ActorKey::session(session).map_err(|_| corrupt("session id", session))?;
    // A turn no run opened yet is its queued input: the withdraw takes the
    // open row first, and an admission that bound it first leaves the open
    // run this request cancels, which the read below then sees (FIG-5262).
    if let Some(input) =
        super::session_mail::withdraw_queued(tx, &request.session, &request.run, now.0).await?
    {
        let (woken, _) = super::wake_within(tx, &actor, false, now).await?;
        return Ok((TurnCancelAnswer::Withdrawn { input }, Some(woken)));
    }
    let open: Option<String> = sqlx::query_scalar(SQL.open_named_run.sql())
        .bind(session)
        .bind(run)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
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
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            TurnCancelAnswer::Requested
        }
        Some(mut accepted) if request.escalates(&accepted) => {
            sqlx::query(SQL.escalate_cancel.sql())
                .bind(session)
                .bind(run)
                .bind(&request.request_id)
                .bind(&request.origin)
                .bind(&request.reason)
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            accepted.escalate_to(request);
            TurnCancelAnswer::Escalated { accepted }
        }
        Some(accepted) if accepted.undelivered != request.undelivered => {
            return Ok((TurnCancelAnswer::PolicyConflict { accepted }, None));
        }
        Some(accepted) => return Ok((TurnCancelAnswer::AlreadyRequested { accepted }, None)),
    };
    let (woken, _) = super::wake_within(tx, &actor, true, now).await?;
    Ok((answer, Some(woken)))
}

/// A stored cancel request: request id, origin, reason, disposition, mode.
pub(super) type StoredCancel = (String, Option<String>, Option<String>, String, String);

/// The cancel request run `run` of `session` accepted.
async fn cancel_of(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Option<TurnCancelRequest>, DurableError> {
    let stored: Option<StoredCancel> = sqlx::query_as(SQL.cancel_of.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    stored
        .map(|stored| decode_cancel(session, run, stored))
        .transpose()
}

/// Run `run` of session `session`'s accepted cancel request, as
/// `turn_cancel_requests` stores it.
pub(super) fn decode_cancel(
    session: &SessionId,
    run: &TurnId,
    (request_id, origin, reason, disposition, mode): StoredCancel,
) -> Result<TurnCancelRequest, DurableError> {
    let (Ok(undelivered), Ok(mode)) = (
        turn_cancel_undelivered_from_wire(&disposition),
        turn_cancel_mode_from_wire(&mode),
    ) else {
        return Err(corrupt(
            "turn cancel request",
            &format!("{disposition}/{mode}"),
        ));
    };
    Ok(TurnCancelRequest {
        session: session.clone(),
        run: run.clone(),
        request_id,
        origin,
        reason,
        undelivered,
        mode,
    })
}

pub(super) async fn turn_end(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Option<TurnEnd>, DurableError> {
    let Some(row) = sqlx::query(SQL.ended.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let cause: String = row.try_get(0).map_err(sqlx_failure)?;
    let head_revision: Option<i64> = row.try_get(1).map_err(sqlx_failure)?;
    let cause = RunTerminalCause::from_stored(&cause).map_err(|error| encoding(&error))?;
    let head_revision = head_revision.map(integer::<u64>).transpose()?;
    Ok(Some(TurnEnd {
        cause,
        head_revision,
    }))
}

pub(super) async fn turn_namespaces(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Vec<TurnNamespace>, DurableError> {
    let rows = sqlx::query(SQL.namespaces.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    rows.into_iter()
        .map(|row| {
            let plugin: String = row.try_get(0).map_err(sqlx_failure)?;
            let entry: String = row.try_get(1).map_err(sqlx_failure)?;
            let body: Option<Vec<u8>> = row.try_get(2).map_err(sqlx_failure)?;
            let entry = serde_json::from_str(&entry)
                .map_err(|_| corrupt("turn namespace entry", &plugin))?;
            Ok(TurnNamespace {
                plugin,
                entry,
                body: body.map(Into::into),
            })
        })
        .collect()
}

pub(super) async fn turn(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Option<TurnRow>, DurableError> {
    let Some(row) = sqlx::query(SQL.unfinished.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let decode = |error: sqlx::Error| sqlx_failure(error);
    let run: String = row.try_get(0).map_err(decode)?;
    let run = TurnId::try_from(run.clone()).map_err(|_| corrupt("turn id", &run))?;
    let stored_phase: String = row.try_get(2).map_err(decode)?;
    let argument: Option<i64> = row.try_get(3).map_err(decode)?;
    let argument = argument.map(integer::<u64>).transpose()?;
    let checkpoint: Option<String> = row.try_get(5).map_err(decode)?;
    let request: Option<String> = row.try_get(6).map_err(decode)?;
    let deadline: Option<i64> = row.try_get(7).map_err(decode)?;
    let stream_from: Option<String> = row.try_get(11).map_err(decode)?;
    let pin = request
        .zip(deadline.map(DurableInstant))
        .zip(stream_from)
        .map(|((request, deadline), stream_from)| (request, deadline, stream_from));
    let model_calls: i64 = row.try_get(10).map_err(decode)?;
    let model_calls = integer::<u32>(model_calls)?;
    let phase = UnfinishedPhase::parse(&stored_phase, argument, checkpoint, pin, model_calls)
        .ok_or_else(|| corrupt("turn phase", &stored_phase))?;
    let iteration: i64 = row.try_get(4).map_err(decode)?;
    let iteration = integer::<u32>(iteration)?;
    let turn_deadline: Option<i64> = row.try_get(8).map_err(decode)?;
    let written_epoch = Epoch(row.try_get(9).map_err(decode)?);
    let admission: String = row.try_get(1).map_err(decode)?;
    let admission =
        RunAdmissionRecord::from_stored(&admission).map_err(|error| encoding(&error))?;
    let trace_exported: bool = row.try_get(12).map_err(decode)?;
    let cancel = cancel_of(tx, session, &run).await?;
    Ok(Some(TurnRow {
        session: session.clone(),
        run,
        admission,
        phase,
        iteration,
        model_calls,
        turn_deadline: turn_deadline.map(DurableInstant),
        written_epoch,
        cancel,
        trace_exported,
    }))
}

/// The storage laws of a turn's stored forms (FIG-5221) on PostgreSQL: the
/// DDL itself refuses a phase row without what its phase restores from, and
/// a run terminal whose kind its cause does not derive.
#[cfg(test)]
mod ddl_tests {
    use sqlx::Connection as _;

    use crate::testing::IsolatedDatabase;

    async fn connection() -> Option<(IsolatedDatabase, sqlx::PgConnection)> {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping the DDL laws: database URL is not set");
            return None;
        };
        let database = IsolatedDatabase::create(&database_url).await;
        crate::testing::connect(database.url())
            .await
            .expect("provision the isolated store");
        let connection = sqlx::PgConnection::connect(database.url())
            .await
            .expect("connect to the isolated store");
        Some((database, connection))
    }

    /// Whether `result` is a CHECK constraint's refusal.
    fn refused<T: std::fmt::Debug>(result: Result<T, sqlx::Error>) -> bool {
        match result {
            Err(error) => error
                .as_database_error()
                .and_then(|error| error.code())
                .is_some_and(|code| code == "23514"),
            Ok(_) => false,
        }
    }

    async fn phase(
        connection: &mut sqlx::PgConnection,
        run: &str,
        phase: &str,
        argument: Option<i64>,
        checkpoint: Option<&str>,
        pin: Option<(&str, i64)>,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query(
            "INSERT INTO lash_turn_phases (session_id, run, phase, phase_arg, iteration,
                 checkpoint_ref, model_request_ref, model_deadline_ms, model_stream_from,
                 written_epoch)
             VALUES ('s', $1, $2, $3, 0, $4, $5, $6, $7, 1)",
        )
        .bind(run)
        .bind(phase)
        .bind(argument)
        .bind(checkpoint)
        .bind(pin.map(|(request, _)| request))
        .bind(pin.map(|(_, deadline)| deadline))
        .bind(pin.map(|_| "stream-cursor"))
        .execute(crate::observed_sql::executor(connection))
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_ddl_refuses_a_model_phase_without_its_pin() {
        let Some((_database, mut conn)) = connection().await else {
            return;
        };
        assert!(refused(
            phase(&mut conn, "unpinned", "model", Some(1), Some("{}"), None).await
        ));
        assert!(refused(
            phase(
                &mut conn,
                "tools-pinned",
                "tools",
                Some(1),
                Some("{}"),
                Some(("r", 5))
            )
            .await
        ));
        phase(
            &mut conn,
            "pinned",
            "model",
            Some(1),
            Some("{}"),
            Some(("r", 5)),
        )
        .await
        .expect("a pinned model phase is stored");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_ddl_refuses_a_non_admitted_phase_without_a_checkpoint() {
        let Some((_database, mut conn)) = connection().await else {
            return;
        };
        assert!(refused(
            phase(&mut conn, "model", "model", Some(1), None, Some(("r", 5))).await
        ));
        assert!(refused(
            phase(&mut conn, "tools", "tools", Some(1), None, None).await
        ));
        assert!(refused(
            phase(
                &mut conn,
                "admitted-checkpoint",
                "admitted",
                None,
                Some("{}"),
                None
            )
            .await
        ));
        phase(&mut conn, "admitted", "admitted", None, None, None)
            .await
            .expect("an admitted phase");
        phase(&mut conn, "tools-ok", "tools", Some(1), Some("{}"), None)
            .await
            .expect("a tools phase");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_ddl_refuses_a_terminal_kind_its_cause_does_not_derive() {
        let Some((_database, mut conn)) = connection().await else {
            return;
        };
        let mut end = async |run: &str, kind: &str, cause: &str| {
            sqlx::query(
                "INSERT INTO lash_session_runs (session_id, run, admission_json, terminal_kind,
                     terminal_cause_json, terminal_head_revision, terminal_at_ms)
                 VALUES ('s', $1, NULL, $2, $3, NULL, 1)",
            )
            .bind(run)
            .bind(kind)
            .bind(cause)
            .execute(crate::observed_sql::executor(&mut conn))
            .await
        };
        let cancelled = r#"{"cause":"operator_cancelled","intent":1}"#;
        let refused_cause = r#"{"cause":"refused","refusal":{"code":"x","message":"m"}}"#;
        assert!(refused(end("a", "answered", cancelled).await));
        assert!(refused(end("c", "answered", refused_cause).await));
        assert!(refused(
            end("d", "cancelled", r#"{"cause":"unknown"}"#).await
        ));
        end("e", "cancelled", cancelled)
            .await
            .expect("a cancelled cause ends cancelled");
        end("f", "failed", refused_cause)
            .await
            .expect("a refused cause fails");
        end("g", "answered", r#"{"cause":"commands_applied"}"#)
            .await
            .expect("applied commands answer");
    }
}
