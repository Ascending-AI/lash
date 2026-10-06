//! Turn phase state, the session commit and turn cancel requests on SQLite: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use std::sync::LazyLock;

use lash_core_execution::store_backend_support::turn_cancel::{
    turn_cancel_mode_from_wire, turn_cancel_mode_wire, turn_cancel_undelivered_from_wire,
    turn_cancel_undelivered_wire,
};
use lash_durable::domain::{
    DomainRefusal, ModelPin, SessionCommitWrite, TurnCancelAnswer, TurnCancelRequest, TurnPhase,
    TurnRow, TurnWrite,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, Woken};
use lash_sansio::{SessionId, TurnId};
use lash_store_sql::durable::turns::TurnStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt};
use crate::conn::cached_execute;

/// `turn_phases`: V0's (FIG-5170) side table of `session_runs`.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS turn_phases (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    phase TEXT NOT NULL CONSTRAINT ck_turn_phases_phase
        CHECK (phase IN ('admitted', 'prepared', 'model', 'tools', 'waiting', 'committing')),
    phase_arg INTEGER,
    iteration INTEGER NOT NULL,
    checkpoint_ref TEXT,
    model_attempt INTEGER,
    model_request_ref TEXT,
    model_deadline_ms INTEGER,
    turn_deadline_ms INTEGER,
    written_epoch INTEGER NOT NULL,
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_turn_phases_arg CHECK ((phase IN ('model', 'tools')) = (phase_arg IS NOT NULL)),
    CONSTRAINT ck_turn_phases_model CHECK (
        (model_attempt IS NULL) = (model_request_ref IS NULL)
        AND (model_attempt IS NULL) = (model_deadline_ms IS NULL))
);
";

static SQL: LazyLock<TurnStatements> =
    LazyLock::new(|| TurnStatements::render(crate::schema_layout::MAIN));

fn refuse<T>(refusal: DomainRefusal) -> Answer<T> {
    Ok(Err(DurableError::Domain(refusal)))
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn model_columns(model: Option<&ModelPin>) -> (Option<i64>, Option<String>, Option<i64>) {
    model.map_or((None, None, None), |pin| {
        (
            Some(i64::from(pin.attempt)),
            Some(pin.request_ref.clone()),
            Some(pin.deadline.0),
        )
    })
}

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &TurnWrite) -> Answer<()> {
    match write {
        TurnWrite::Admit {
            session,
            run,
            admission_json,
            turn_deadline,
        } => {
            let open = tx
                .prepare_cached(SQL.open_run.sql())?
                .query_row([session.as_str()], |_| Ok(()))
                .optional()?;
            if open.is_some() {
                return refuse(DomainRefusal::OpenTurnExists {
                    session: session.clone(),
                });
            }
            cached_execute(
                tx,
                SQL.insert_run.sql(),
                rusqlite::params![session.as_str(), run.as_str(), admission_json],
            )?;
            let (phase, argument) = TurnPhase::Admitted.stored();
            cached_execute(
                tx,
                SQL.insert_phase.sql(),
                rusqlite::params![
                    session.as_str(),
                    run.as_str(),
                    phase,
                    argument.map(signed),
                    0_i64,
                    Option::<String>::None,
                    Option::<i64>::None,
                    Option::<String>::None,
                    Option::<i64>::None,
                    turn_deadline.map(|deadline| deadline.0),
                    commit.epoch.0,
                ],
            )?;
            Ok(Ok(()))
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
            let (attempt, request, deadline) = model_columns(model.as_ref());
            let advanced = tx
                .prepare_cached(SQL.advance_phase.sql())?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        phase,
                        argument.map(signed),
                        i64::from(*iteration),
                        checkpoint_ref,
                        attempt,
                        request,
                        deadline,
                        commit.epoch.0,
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            match advanced {
                Some(()) => Ok(Ok(())),
                None => refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                }),
            }
        }
        TurnWrite::Terminal {
            session,
            run,
            terminal,
            cause_json,
            head_revision,
        } => {
            let ended = tx
                .prepare_cached(SQL.end_run.sql())?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        terminal.as_str(),
                        cause_json.as_deref().unwrap_or("null"),
                        head_revision.map(signed),
                        commit.now.0,
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if ended.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            cached_execute(
                tx,
                SQL.delete_phase.sql(),
                rusqlite::params![session.as_str(), run.as_str()],
            )?;
            Ok(Ok(()))
        }
    }
}

pub(super) fn apply_session_commit(
    tx: &Connection,
    _commit: &Committing<'_>,
    write: &SessionCommitWrite,
) -> Answer<()> {
    let session = write.session.as_str();
    let found = tx
        .prepare_cached(SQL.head.sql())?
        .query_row([session], |row| row.get::<_, i64>(0))
        .optional()?;
    let current = found.map_or(0, |head| u64::try_from(head).unwrap_or(0));
    if current != write.expected_head {
        return refuse(DomainRefusal::HeadMoved {
            session: write.session.clone(),
            expected: write.expected_head,
            found: found.map(|head| u64::try_from(head).unwrap_or(0)),
        });
    }
    let next = signed(write.expected_head.saturating_add(1));
    cached_execute(
        tx,
        SQL.insert_revision.sql(),
        rusqlite::params![session, next, write.commit_json],
    )?;
    cached_execute(tx, SQL.move_head.sql(), rusqlite::params![session, next])?;
    Ok(Ok(()))
}

pub(super) fn request_cancel(
    tx: &Connection,
    request: &TurnCancelRequest,
    now: DurableInstant,
) -> Answer<(TurnCancelAnswer, Option<Woken>)> {
    let session = request.session.as_str();
    let run = request.run.as_str();
    let open = tx
        .prepare_cached(SQL.open_named_run.sql())?
        .query_row([session, run], |_| Ok(()))
        .optional()?;
    if open.is_none() {
        return Ok(Ok((TurnCancelAnswer::AlreadyEnded, None)));
    }
    let accepted = match cancel_of(tx, &request.session, &request.run)? {
        Ok(accepted) => accepted,
        Err(error) => return Ok(Err(error)),
    };
    let answer = match accepted {
        None => {
            cached_execute(
                tx,
                SQL.insert_cancel.sql(),
                rusqlite::params![
                    session,
                    run,
                    request.request_id,
                    request.origin,
                    request.reason,
                    turn_cancel_undelivered_wire(request.undelivered),
                    turn_cancel_mode_wire(request.mode),
                ],
            )?;
            TurnCancelAnswer::Requested
        }
        Some(mut accepted) if request.escalates(&accepted) => {
            cached_execute(
                tx,
                SQL.escalate_cancel.sql(),
                rusqlite::params![session, run, turn_cancel_mode_wire(request.mode)],
            )?;
            accepted.mode = request.mode;
            TurnCancelAnswer::Escalated { accepted }
        }
        Some(accepted) if accepted.undelivered != request.undelivered => {
            return Ok(Ok((TurnCancelAnswer::PolicyConflict { accepted }, None)));
        }
        Some(accepted) => {
            return Ok(Ok((TurnCancelAnswer::AlreadyRequested { accepted }, None)));
        }
    };
    let actor = match ActorKey::session(session) {
        Ok(actor) => actor,
        Err(_) => return Ok(Err(corrupt("session id", session))),
    };
    Ok(super::wake_within(tx, &actor, true, now)?.map(|(woken, _)| (answer, Some(woken))))
}

/// The cancel request run `run` of `session` accepted.
fn cancel_of(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
) -> Answer<Option<TurnCancelRequest>> {
    let stored = tx
        .prepare_cached(SQL.cancel_of.sql())?
        .query_row([session.as_str(), run.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .optional()?;
    let Some((request_id, origin, reason, disposition, mode)) = stored else {
        return Ok(Ok(None));
    };
    let (Ok(undelivered), Ok(mode)) = (
        turn_cancel_undelivered_from_wire(&disposition),
        turn_cancel_mode_from_wire(&mode),
    ) else {
        return Ok(Err(corrupt(
            "turn cancel request",
            &format!("{disposition}/{mode}"),
        )));
    };
    Ok(Ok(Some(TurnCancelRequest {
        session: session.clone(),
        run: run.clone(),
        request_id,
        origin,
        reason,
        undelivered,
        mode,
    })))
}

pub(super) fn turn(tx: &Connection, session: &SessionId) -> Answer<Option<TurnRow>> {
    struct Stored {
        run: String,
        admission_json: String,
        phase: String,
        argument: Option<i64>,
        iteration: i64,
        checkpoint_ref: Option<String>,
        attempt: Option<i64>,
        request: Option<String>,
        deadline: Option<i64>,
        turn_deadline: Option<i64>,
        epoch: i64,
    }
    let stored = tx
        .prepare_cached(SQL.unfinished.sql())?
        .query_row([session.as_str()], |row| {
            Ok(Stored {
                run: row.get(0)?,
                admission_json: row.get(1)?,
                phase: row.get(2)?,
                argument: row.get(3)?,
                iteration: row.get(4)?,
                checkpoint_ref: row.get(5)?,
                attempt: row.get(6)?,
                request: row.get(7)?,
                deadline: row.get(8)?,
                turn_deadline: row.get(9)?,
                epoch: row.get(10)?,
            })
        })
        .optional()?;
    let Some(stored) = stored else {
        return Ok(Ok(None));
    };
    let Ok(run) = TurnId::try_from(stored.run.clone()) else {
        return Ok(Err(corrupt("turn id", &stored.run)));
    };
    let argument = stored.argument.and_then(|value| u64::try_from(value).ok());
    let Some(phase) = TurnPhase::parse(&stored.phase, argument) else {
        return Ok(Err(corrupt("turn phase", &stored.phase)));
    };
    let model = match (stored.attempt, stored.request, stored.deadline) {
        (Some(attempt), Some(request_ref), Some(deadline)) => Some(ModelPin {
            attempt: u32::try_from(attempt).unwrap_or(u32::MAX),
            request_ref,
            deadline: DurableInstant(deadline),
        }),
        _ => None,
    };
    let cancel = match cancel_of(tx, session, &run)? {
        Ok(cancel) => cancel,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(Some(TurnRow {
        session: session.clone(),
        run,
        admission_json: stored.admission_json,
        phase,
        iteration: u32::try_from(stored.iteration).unwrap_or(u32::MAX),
        checkpoint_ref: stored.checkpoint_ref,
        model,
        turn_deadline: stored.turn_deadline.map(DurableInstant),
        written_epoch: Epoch(stored.epoch),
        cancel,
    })))
}
