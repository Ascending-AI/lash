//! Turn phase state, the session commit and turn cancel requests on SQLite: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

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
use lash_store_sql::durable::turns::TurnStatements;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer};
use crate::conn::cached_execute;

/// `turn_phases`: V0's (FIG-5170) side table of `session_runs`. Each phase
/// row carries exactly what its phase restores from (FIG-5221): no
/// checkpoint while admitted and one after, and the model pin exactly in the
/// model phase, whose attempt is the phase argument and whose call is
/// `model_calls`, the count of model calls the turn admitted.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS turn_phases (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    phase TEXT NOT NULL CONSTRAINT ck_turn_phases_phase
        CHECK (phase IN ('admitted', 'model', 'tools')),
    phase_arg INTEGER,
    iteration INTEGER NOT NULL,
    checkpoint_ref TEXT,
    model_request_ref TEXT,
    model_deadline_ms INTEGER,
    model_stream_from TEXT,
    turn_deadline_ms INTEGER,
    written_epoch INTEGER NOT NULL,
    model_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_turn_phases_arg CHECK ((phase IN ('model', 'tools')) = (phase_arg IS NOT NULL)),
    CONSTRAINT ck_turn_phases_checkpoint CHECK ((phase = 'admitted') = (checkpoint_ref IS NULL)),
    CONSTRAINT ck_turn_phases_model CHECK (
        (phase = 'model') = (model_request_ref IS NOT NULL)
        AND (phase = 'model') = (model_deadline_ms IS NOT NULL)
        AND (phase = 'model') = (model_stream_from IS NOT NULL))
);

-- The plugin namespaces an unfinished turn's run changed (FIG-5301): the
-- entry (values address and metadata, JSON) and its matching values body,
-- NULL exactly for base values, dropped with the phase row when the turn ends.
CREATE TABLE IF NOT EXISTS turn_namespaces (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    plugin TEXT NOT NULL,
    entry TEXT NOT NULL,
    body BLOB,
    PRIMARY KEY (session_id, run, plugin)
);
";

static SQL: LazyLock<TurnStatements> =
    LazyLock::new(|| TurnStatements::render(crate::schema_layout::MAIN));

fn refuse<T>(refusal: DomainRefusal) -> Answer<T> {
    Ok(Err(DurableError::Domain(refusal)))
}

/// A store codec's refusal, as the durable port reports it.
fn encoding(error: &lash_core_execution::StoreError) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
}

pub(super) fn apply(tx: &Connection, commit: &Committing<'_>, write: &TurnWrite) -> Answer<()> {
    match write {
        TurnWrite::Admit {
            session,
            run,
            admission,
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
            let admission = match admission.to_stored() {
                Ok(admission) => admission,
                Err(error) => return Ok(Err(encoding(&error))),
            };
            cached_execute(
                tx,
                SQL.insert_run.sql(),
                rusqlite::params![session.as_str(), run.as_str(), admission],
            )?;
            let (phase, argument) = UnfinishedPhase::Admitted.stored();
            cached_execute(
                tx,
                SQL.insert_phase.sql(),
                rusqlite::params![
                    session.as_str(),
                    run.as_str(),
                    phase,
                    argument.map(integer::<i64>).transpose()?,
                    0_i64,
                    Option::<String>::None,
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
        } => {
            let (stored, argument) = phase.stored();
            let pin = phase.model();
            let advanced = tx
                .prepare_cached(SQL.advance_phase.sql())?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        stored,
                        argument.map(integer::<i64>).transpose()?,
                        i64::from(*iteration),
                        phase.checkpoint(),
                        pin.map(|pin| pin.request_ref.as_str()),
                        pin.map(|pin| pin.deadline.0),
                        commit.epoch.0,
                        pin.map(|pin| i64::from(pin.call)),
                        pin.map(|pin| pin.stream_from.as_str()),
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
        TurnWrite::Namespaces {
            session,
            run,
            namespaces,
        } => {
            let open = tx
                .prepare_cached(SQL.open_named_run.sql())?
                .query_row([session.as_str(), run.as_str()], |_| Ok(()))
                .optional()?;
            if open.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            for namespace in namespaces {
                let entry = match serde_json::to_string(&namespace.entry) {
                    Ok(entry) => entry,
                    Err(error) => {
                        return Ok(Err(corrupt("turn namespace entry", &error.to_string())));
                    }
                };
                cached_execute(
                    tx,
                    SQL.upsert_namespace.sql(),
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        namespace.plugin,
                        entry,
                        namespace.values.body(),
                        matches!(namespace.values, RunValuesWrite::Held),
                    ],
                )?;
            }
            Ok(Ok(()))
        }
        TurnWrite::Terminal {
            session,
            run,
            cause,
            head_revision,
        } => {
            let stored = match cause.to_stored() {
                Ok(stored) => stored,
                Err(error) => return Ok(Err(encoding(&error))),
            };
            let ended = tx
                .prepare_cached(SQL.end_run.sql())?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        cause.kind().as_str(),
                        stored,
                        head_revision.map(integer::<i64>).transpose()?,
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
            cached_execute(
                tx,
                SQL.delete_namespaces.sql(),
                rusqlite::params![session.as_str(), run.as_str()],
            )?;
            super::session_mail::settle_held(tx, session, run, commit.now.0)?;
            Ok(Ok(()))
        }
    }
}

pub(super) fn apply_session_commit(
    tx: &crate::conn::FencedTx<'_>,
    commit: &Committing<'_>,
    write: &SessionCommitWrite,
) -> Answer<()> {
    let refused = |error: &lash_core_execution::StoreError| {
        Ok(Err(DurableError::session_commit(
            write.session.clone(),
            error,
        )))
    };
    let runtime_commit = match lash_core_execution::store::decode_session_commit(&write.commit_json)
    {
        Ok(runtime_commit) => runtime_commit,
        Err(error) => return refused(&error),
    };
    if runtime_commit.session_id != write.session
        || runtime_commit.expected_head_revision != write.expected_head
    {
        return refuse(DomainRefusal::SessionCommitRefused {
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
        });
    }
    let planner =
        match lash_core_execution::store::RuntimeCommitPlanner::prepare(runtime_commit, tx.fleet())
        {
            Ok(planner) => planner,
            Err(error) => return refused(&error),
        };
    let now = integer::<u64>(commit.now.0)?;
    match crate::persistence::session_commit::apply_runtime_commit_conn(
        tx,
        &planner,
        head_writer(commit.actor, &write.session),
        commit.blob_profile,
        now,
    ) {
        Ok(_) => Ok(Ok(())),
        Err(error) => refused(&error),
    }
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

pub(super) fn request_cancel(
    tx: &Connection,
    request: &TurnCancelRequest,
    now: DurableInstant,
) -> Answer<(TurnCancelAnswer, Option<Woken>)> {
    let session = request.session.as_str();
    let run = request.run.as_str();
    let actor = match ActorKey::session(session) {
        Ok(actor) => actor,
        Err(_) => return Ok(Err(corrupt("session id", session))),
    };
    // A turn no run opened yet is its queued input: the withdraw takes the
    // open row first, and an admission that bound it first leaves the open
    // run this request cancels (FIG-5262).
    let withdrawn =
        match super::session_mail::withdraw_queued(tx, &request.session, &request.run, now.0)? {
            Ok(withdrawn) => withdrawn,
            Err(error) => return Ok(Err(error)),
        };
    if let Some(input) = withdrawn {
        return Ok(super::wake_within(tx, &actor, false, now)?
            .map(|(woken, _)| (TurnCancelAnswer::Withdrawn { input }, Some(woken))));
    }
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

pub(super) fn turn_end(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
) -> Answer<Option<TurnEnd>> {
    let stored = tx
        .prepare_cached(SQL.ended.sql())?
        .query_row([session.as_str(), run.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .optional()?;
    let Some((cause, head_revision)) = stored else {
        return Ok(Ok(None));
    };
    let cause = match RunTerminalCause::from_stored(&cause) {
        Ok(cause) => cause,
        Err(error) => return Ok(Err(encoding(&error))),
    };
    let head_revision = head_revision.map(integer::<u64>).transpose()?;
    Ok(Ok(Some(TurnEnd {
        cause,
        head_revision,
    })))
}

pub(super) fn turn_namespaces(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
) -> Answer<Vec<TurnNamespace>> {
    let mut statement = tx.prepare_cached(SQL.namespaces.sql())?;
    let rows = statement
        .query_map([session.as_str(), run.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut namespaces = Vec::with_capacity(rows.len());
    for (plugin, entry, body) in rows {
        let Ok(entry) = serde_json::from_str(&entry) else {
            return Ok(Err(corrupt("turn namespace entry", &plugin)));
        };
        namespaces.push(TurnNamespace {
            plugin,
            entry,
            body: body.map(Into::into),
        });
    }
    Ok(Ok(namespaces))
}

pub(super) fn turn(tx: &Connection, session: &SessionId) -> Answer<Option<TurnRow>> {
    struct Stored {
        run: String,
        admission: String,
        phase: String,
        argument: Option<i64>,
        iteration: i64,
        checkpoint: Option<String>,
        request: Option<String>,
        deadline: Option<i64>,
        turn_deadline: Option<i64>,
        epoch: i64,
        model_calls: i64,
        stream_from: Option<String>,
    }
    let stored = tx
        .prepare_cached(SQL.unfinished.sql())?
        .query_row([session.as_str()], |row| {
            Ok(Stored {
                run: row.get(0)?,
                admission: row.get(1)?,
                phase: row.get(2)?,
                argument: row.get(3)?,
                iteration: row.get(4)?,
                checkpoint: row.get(5)?,
                request: row.get(6)?,
                deadline: row.get(7)?,
                turn_deadline: row.get(8)?,
                epoch: row.get(9)?,
                model_calls: row.get(10)?,
                stream_from: row.get(11)?,
            })
        })
        .optional()?;
    let Some(stored) = stored else {
        return Ok(Ok(None));
    };
    let Ok(run) = TurnId::try_from(stored.run.clone()) else {
        return Ok(Err(corrupt("turn id", &stored.run)));
    };
    let admission = match RunAdmissionRecord::from_stored(&stored.admission) {
        Ok(admission) => admission,
        Err(error) => return Ok(Err(encoding(&error))),
    };
    let argument = stored.argument.map(integer::<u64>).transpose()?;
    let iteration = integer::<u32>(stored.iteration)?;
    let model_calls = integer::<u32>(stored.model_calls)?;
    let pin = stored
        .request
        .zip(stored.deadline.map(DurableInstant))
        .zip(stored.stream_from)
        .map(|((request, deadline), stream_from)| (request, deadline, stream_from));
    let Some(phase) =
        UnfinishedPhase::parse(&stored.phase, argument, stored.checkpoint, pin, model_calls)
    else {
        return Ok(Err(corrupt("turn phase", &stored.phase)));
    };
    let cancel = match cancel_of(tx, session, &run)? {
        Ok(cancel) => cancel,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(Some(TurnRow {
        session: session.clone(),
        run,
        admission,
        phase,
        iteration,
        model_calls,
        turn_deadline: stored.turn_deadline.map(DurableInstant),
        written_epoch: Epoch(stored.epoch),
        cancel,
    })))
}

#[cfg(test)]
mod ddl_tests {
    //! The storage laws of a turn's stored forms (FIG-5221): the DDL itself
    //! refuses a phase row without what its phase restores from, and a run
    //! terminal whose kind its cause does not derive.

    use rusqlite::{Connection, params};

    fn tables() -> Connection {
        let conn = Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(super::TABLES).expect("turn_phases");
        conn.execute_batch(crate::schema_fragments::SESSION_RUNS_TABLES)
            .expect("session_runs");
        conn
    }

    fn phase(
        conn: &Connection,
        run: &str,
        phase: &str,
        argument: Option<i64>,
        checkpoint: Option<&str>,
        pin: Option<(&str, i64)>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO turn_phases (session_id, run, phase, phase_arg, iteration,
                 checkpoint_ref, model_request_ref, model_deadline_ms, model_stream_from,
                 written_epoch)
             VALUES ('s', ?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, 1)",
            params![
                run,
                phase,
                argument,
                checkpoint,
                pin.map(|(request, _)| request),
                pin.map(|(_, deadline)| deadline),
                pin.map(|_| "stream-cursor")
            ],
        )
    }

    #[test]
    fn the_ddl_refuses_a_model_phase_without_its_pin() {
        let conn = tables();
        assert!(phase(&conn, "unpinned", "model", Some(1), Some("{}"), None).is_err());
        assert!(
            phase(
                &conn,
                "tools-pinned",
                "tools",
                Some(1),
                Some("{}"),
                Some(("r", 5))
            )
            .is_err()
        );
        phase(
            &conn,
            "pinned",
            "model",
            Some(1),
            Some("{}"),
            Some(("r", 5)),
        )
        .expect("a pinned model phase is stored");
    }

    #[test]
    fn the_ddl_refuses_a_non_admitted_phase_without_a_checkpoint() {
        let conn = tables();
        assert!(phase(&conn, "model", "model", Some(1), None, Some(("r", 5))).is_err());
        assert!(phase(&conn, "tools", "tools", Some(1), None, None).is_err());
        assert!(
            phase(
                &conn,
                "admitted-checkpoint",
                "admitted",
                None,
                Some("{}"),
                None
            )
            .is_err()
        );
        phase(&conn, "admitted", "admitted", None, None, None).expect("an admitted phase");
        phase(&conn, "tools-ok", "tools", Some(1), Some("{}"), None).expect("a tools phase");
    }

    #[test]
    fn the_ddl_refuses_a_terminal_kind_its_cause_does_not_derive() {
        let conn = tables();
        let end = |run: &str, kind: &str, cause: &str| {
            conn.execute(
                "INSERT INTO session_runs (session_id, run, admission_json, terminal_kind,
                     terminal_cause_json, terminal_head_revision, terminal_at_ms)
                 VALUES ('s', ?1, NULL, ?2, ?3, NULL, 1)",
                params![run, kind, cause],
            )
        };
        let cancelled = r#"{"cause":"operator_cancelled","intent":1}"#;
        let refused = r#"{"cause":"refused","refusal":{"code":"x","message":"m"}}"#;
        assert!(end("a", "answered", cancelled).is_err());
        assert!(end("c", "answered", refused).is_err());
        assert!(end("d", "cancelled", r#"{"cause":"unknown"}"#).is_err());
        end("e", "cancelled", cancelled).expect("a cancelled cause ends cancelled");
        end("f", "failed", refused).expect("a refused cause fails");
        end("g", "answered", r#"{"cause":"commands_applied"}"#).expect("applied commands answer");
    }
}
