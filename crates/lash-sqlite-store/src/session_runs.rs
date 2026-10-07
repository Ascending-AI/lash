//! The SQLite half of the logical-run family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_runs` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the run admission, session deletion
//! and the factory's catalog reads share, plus
//! [`RunStore`] for the bound store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    CONTROL_INTENT_FORMAT, ClaimToken, ControlIntent, ControlIntentId, ControlIntentKind,
    ControlIntentState, EnginePark, IntentObligation, IntentSettle, ObligationKey, ObligationState,
    ParkCancelCause, ParkEventKind, RunAdmission, RunEndOutcome, RunExecutor, RunStore,
    RunTerminal, RunTerminalCause, RunTerminalWriteDecision, RunTurns, UnfinishedRun,
    close_admission, decide_run_terminal_write, refused_execution_owns_run, run_binding_conflict,
    stored_intent_kind, stored_intent_state,
};
use lash_sansio::{InputId, SessionId, TurnId};
use lash_store_sql::session_runs::{
    control_intents::ControlIntentStatements,
    run_inputs::SessionRunInputStatements,
    runs::{RunVerbStatements, SessionRunStatements},
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::conn::TxOutcome;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

/// Every logical-run statement the session catalog issues.
pub(crate) struct SessionRunsSql {
    pub(crate) runs: SessionRunStatements,
    pub(crate) verbs: RunVerbStatements,
    pub(crate) inputs: SessionRunInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_RUNS_SQL: LazyLock<SessionRunsSql> = LazyLock::new(|| {
    // The run verbs name the turn-input lifecycle.
    let dialect =
        crate::schema_layout::MAIN.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE);
    SessionRunsSql {
        runs: SessionRunStatements::render(dialect),
        verbs: RunVerbStatements::render(
            dialect.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
        ),
        inputs: SessionRunInputStatements::render(dialect),
        intents: ControlIntentStatements::render(dialect),
    }
});

/// The session catalog's logical-run statements, rendered once at first use.
pub(crate) fn session_runs_sql() -> &'static SessionRunsSql {
    &SESSION_RUNS_SQL
}

fn stored_u64(record: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| stored_data_corrupt(record, "a negative counter"))
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The stored terminal-evidence row: serialized cause, head revision and
/// the terminal instant, all unset until the run goes terminal.
type TerminalRow = (Option<String>, Option<i64>, Option<i64>);

/// The terminal evidence of `run` in `session_id`, read on `conn`.
pub(crate) fn run_terminal_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Option<RunTerminal>, StoreError> {
    let row: Option<TerminalRow> = conn
        .query_row(
            session_runs_sql().runs.select_terminal.sql(),
            params![session_id.as_str(), run.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((cause_json, head_revision, at_ms)) = row else {
        let deleted = conn
            .query_row(
                crate::session_sql::session_sql()
                    .deleted_sqlite
                    .exists
                    .sql(),
                params![session_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sqlite_error)?
            .is_some();
        if !deleted {
            return Ok(None);
        }
        return Ok(close_session_intent_conn(conn, session_id)?
            .and_then(|intent| intent.session_deleted_terminal(run)));
    };
    let (Some(cause_json), Some(at_ms)) = (cause_json, at_ms) else {
        return Ok(None);
    };
    RunTerminal::from_stored(
        session_id.clone(),
        run.clone(),
        &cause_json,
        head_revision
            .map(|revision| stored_u64("RunTerminal", revision))
            .transpose()?,
        stored_u64("RunTerminal", at_ms)?,
    )
    .map(Some)
}

/// Write `terminal` in the caller's transaction, deciding it against the
/// stored evidence first: the same terminal is a no-op, another one is
/// [`StoreError::RunAlreadyTerminal`].
///
/// Every way a run ends goes through here, so here is where it lets go of
/// the rows it still holds (FIG-3927): after whatever settlement its caller
/// wrote, every row still bound to the run is released open at its own
/// position. No row stays bound to a run that has terminal evidence.
///
/// Here too is where the run's park ends (FIG-4780): a run with terminal
/// evidence holds no park, whatever kind of run it is, so the write that
/// ends the run clears its park and appends the feed event its cause names.
pub(crate) fn write_run_terminal_conn(
    tx: &Connection,
    terminal: &RunTerminal,
) -> Result<(), StoreError> {
    let stored = run_terminal_conn(tx, &terminal.session_id, &terminal.run)?;
    if decide_run_terminal_write(stored.as_ref(), terminal)?
        == RunTerminalWriteDecision::AlreadyWritten
    {
        return release_run_rows_conn(tx, &terminal.session_id, &terminal.run, terminal.at_ms);
    }
    let sql = session_runs_sql();
    crate::conn::cached_execute(
        tx,
        sql.runs.insert_open.sql(),
        params![terminal.session_id.as_str(), terminal.run.as_str()],
    )
    .map_err(sqlite_error)?;
    let columns = terminal.to_stored()?;
    let written = crate::conn::cached_execute(
        tx,
        sql.runs.write_terminal.sql(),
        params![
            terminal.session_id.as_str(),
            terminal.run.as_str(),
            columns.kind,
            columns.cause_json,
            columns
                .head_revision
                .map(|revision| sql_i64("terminal head revision", revision))
                .transpose()?,
            sql_i64("terminal instant", columns.at_ms)?,
        ],
    )
    .map_err(sqlite_error)?;
    if written != 1 {
        return Err(StoreError::Backend(format!(
            "run `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.run, terminal.session_id
        )));
    }
    // A terminal run owes its scope close (ADR 0109 §3): the terminal
    // transaction arms the row's obligation, due at the terminal instant.
    crate::obligation_ledger::arm_obligation_tx(
        tx,
        &ObligationKey::ScopeClose {
            session_id: terminal.session_id.clone(),
            run: terminal.run.clone(),
        },
        columns.at_ms,
    )?;
    crate::persistence::turn_park::end_run_park_conn(
        tx,
        &terminal.session_id,
        &terminal.run,
        &terminal.cause,
        terminal.at_ms,
    )?;
    release_run_rows_conn(tx, &terminal.session_id, &terminal.run, terminal.at_ms)
}

/// Release every row of either admission table `run` still holds, in the
/// caller's transaction: accepted input is open again in the state its
/// submitted delivery names, and each row owes its session a shift again.
///
/// Open input addressed to a turn the run ends ([`RunTurns`]: its own
/// physical turns and the turns its admission's members were accepted
/// under) names a turn that will never run again, so the run's disposition
/// applies to it here (FIG-3946): the undelivered disposition of the run's
/// cancellation request if it has one, else `Defer`. `Defer` writes
/// nothing: the row is next-turn input at its own position by rule, its
/// submitted delivery unchanged (ADR 0101 §5.1). `Drop` withdraws it into
/// its tombstone, settling its ingress obligation at the terminal instant
/// `at_ms` (FIG-4098). Either is recorded once on the request's outcome. No
/// open row is bound to a run with terminal evidence.
///
/// An input the run's admission took as its own (`session_run_inputs`)
/// that is still open is unbound from the run too, so a later run can
/// admit it: a terminal run answers nothing more.
fn release_run_rows_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    at_ms: u64,
) -> Result<(), StoreError> {
    let verbs = &session_runs_sql().verbs;
    let own = {
        let mut stmt = tx
            .prepare_cached(verbs.bound_inputs.sql())
            .map_err(sqlite_error)?;
        stmt.query_map(params![session_id.as_str(), run.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?
    };
    for input in own {
        crate::conn::cached_execute(tx, verbs.unbind.sql(), params![session_id.as_str(), input])
            .map_err(sqlite_error)?;
    }
    let sql = crate::turn_ingress::turn_ingress_sql();
    crate::conn::cached_execute(
        tx,
        sql.pending_inputs.release_run.sql(),
        params![session_id.as_str(), run.as_str()],
    )
    .map_err(sqlite_error)?;
    crate::conn::cached_execute(
        tx,
        sql.queued_batches.release_run.sql(),
        params![session_id.as_str(), run.as_str()],
    )
    .map_err(sqlite_error)?;
    let ended = RunTurns::new(run, run_admission_conn(tx, session_id, run)?.as_ref());
    let open_rows = {
        let mut stmt = tx
            .prepare_cached(sql.pending_inputs_sqlite.select_pending_active.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session_id.as_str()],
                crate::pending_turn_inputs::pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let mut addressed = Vec::new();
    for row in open_rows {
        let ingress = crate::pending_turn_inputs::decode_turn_input_ingress(row.ingress_json)?;
        if ingress
            .active_turn_id()
            .is_some_and(|turn| ended.contains(turn))
        {
            addressed.push(row.input_id);
        }
    }
    if addressed.is_empty() {
        return Ok(());
    }
    let request =
        crate::persistence::turn_cancel::load_turn_cancel_request_conn(tx, session_id, run)?;
    let disposition = request.as_ref().map_or(
        lash_core_execution::TurnCancelUndeliveredInputPolicy::Defer,
        |request| request.undelivered,
    );
    for input_id in addressed {
        if disposition == lash_core_execution::TurnCancelUndeliveredInputPolicy::Drop {
            crate::conn::cached_execute(
                tx,
                sql.pending_inputs.cancel.sql(),
                params![
                    session_id.as_str(),
                    input_id.as_str(),
                    lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
                    crate::clamp_epoch_ms(at_ms),
                ],
            )
            .map_err(sqlite_error)?;
        }
    }
    Ok(())
}

/// The engine proved the run's execution is lost (`loss`). This
/// transaction makes its inputs and run terminal together, so the existing
/// scope-close obligation takes over before recovery acknowledges the loss.
/// A run that already has terminal evidence, or no row, is left as it is.
/// A run the engine holds no execution of that never recorded its admission
/// started nothing, and its ingress obligation still owns its input: it is
/// not ended, and the executor a seal recorded for it is released
/// (FIG-4814), so the shift that obligation asks for runs it.
pub(crate) fn end_lost_run_conn(
    tx: &Connection,
    target: &lash_core_execution::engine::RunRef,
    loss: lash_core_execution::engine::RunLoss,
    at_ms: u64,
) -> Result<Option<RunTerminal>, StoreError> {
    match unanswered_run_conn(tx, target)? {
        UnansweredRun::Open => {
            if loss == lash_core_execution::engine::RunLoss::NoRun
                && run_admission_conn(tx, &target.session, &target.run)?.is_none()
            {
                // The executor a seal recorded for it is gone with its run,
                // so the run is free for the shift its ingress asks for.
                crate::conn::cached_execute(
                    tx,
                    session_runs_sql().runs.release_hold.sql(),
                    params![target.session.as_str(), target.run.as_str()],
                )
                .map_err(sqlite_error)?;
                return Ok(None);
            }
            write_unanswered_run_end_conn(tx, target, at_ms, |cancelled_by| {
                RunTerminalCause::SubstrateLost { cancelled_by }
            })
            .map(Some)
        }
        UnansweredRun::Ended(_) | UnansweredRun::Unknown => Ok(None),
    }
}

/// The run's execution under `fence` met a typed refusal no retry can change
/// (FIG-4018): the same transaction as a lost run's, ending it with the
/// refusal, once the execution is shown to still own the run (FIG-4200).
pub(crate) fn end_refused_run_conn(
    tx: &Connection,
    fence: &lash_core_execution::store::ShiftFence,
    run: &TurnId,
    refusal: &lash_core_execution::RuntimeError,
    at_ms: u64,
) -> Result<RunEndOutcome, StoreError> {
    let target = lash_core_execution::engine::RunRef {
        session: fence.session().clone(),
        run: run.clone(),
    };
    match unanswered_run_conn(tx, &target)? {
        UnansweredRun::Ended(terminal) => Ok(RunEndOutcome::AlreadyEnded(*terminal)),
        UnansweredRun::Unknown => Ok(RunEndOutcome::Unknown),
        UnansweredRun::Open => {
            let current = crate::persistence::shift_epoch_conn(tx, &target.session)?;
            if !refused_execution_owns_run(&target.session, fence, &current)? {
                return Ok(RunEndOutcome::Superseded);
            }
            write_unanswered_run_end_conn(tx, &target, at_ms, |_| RunTerminalCause::Refused {
                code: refusal.code.clone(),
                message: refusal.message.clone(),
                refusal_cause: refusal.cause.clone(),
            })
            .map(RunEndOutcome::Ended)
        }
    }
}

/// The command run whose execution under `fence` applied the session's command
/// lane until it was empty ends (FIG-4202): its row opens and its terminal
/// is written in one transaction, arming its scope close, once the run is
/// shown to still own the session's shift epoch.
pub(crate) fn end_command_run_conn(
    tx: &Connection,
    fence: &lash_core_execution::store::ShiftFence,
    run: &TurnId,
    at_ms: u64,
) -> Result<RunEndOutcome, StoreError> {
    let session = fence.session();
    if let Some(terminal) = run_terminal_conn(tx, session, run)? {
        return Ok(RunEndOutcome::AlreadyEnded(terminal));
    }
    let current = crate::persistence::shift_epoch_conn(tx, session)?;
    if !refused_execution_owns_run(session, fence, &current)? {
        return Ok(RunEndOutcome::Superseded);
    }
    let terminal = RunTerminal {
        session_id: session.clone(),
        run: run.clone(),
        cause: RunTerminalCause::CommandsApplied,
        head_revision: None,
        at_ms,
    };
    write_run_terminal_conn(tx, &terminal)?;
    Ok(RunEndOutcome::Ended(terminal))
}

/// Where a run no commit answered stands.
enum UnansweredRun {
    /// It has terminal evidence.
    Ended(Box<RunTerminal>),
    /// The store holds no row for it.
    Unknown,
    /// It is admitted and has not ended.
    Open,
}

fn unanswered_run_conn(
    tx: &Connection,
    target: &lash_core_execution::engine::RunRef,
) -> Result<UnansweredRun, StoreError> {
    let session = &target.session;
    let run = &target.run;
    if let Some(terminal) = run_terminal_conn(tx, session, run)? {
        return Ok(UnansweredRun::Ended(Box::new(terminal)));
    }
    let exists: bool = tx
        .query_row(
            session_runs_sql().runs.select_terminal.sql(),
            params![session.as_str(), run.as_str()],
            |_| Ok(true),
        )
        .optional()
        .map_err(sqlite_error)?
        .unwrap_or(false);
    Ok(if exists {
        UnansweredRun::Open
    } else {
        UnansweredRun::Unknown
    })
}

/// End an open run no commit answered, with the cause `cause` makes of the
/// run's recorded cancellation request, if any.
fn write_unanswered_run_end_conn(
    tx: &Connection,
    target: &lash_core_execution::engine::RunRef,
    at_ms: u64,
    cause: impl FnOnce(Option<String>) -> RunTerminalCause,
) -> Result<RunTerminal, StoreError> {
    let session = &target.session;
    let run = &target.run;
    let record = crate::persistence::turn_cancel::load_turn_cancel_request_conn(tx, session, run)?;
    let cause = cause(record.as_ref().map(|request| request.request_id.clone()));
    let terminal = RunTerminal {
        session_id: session.clone(),
        run: run.clone(),
        cause,
        head_revision: None,
        at_ms,
    };
    crate::conn::cached_execute(
        tx,
        crate::session_sql::session_sql()
            .head
            .clear_pending_follow_on
            .sql(),
        params![session.as_str()],
    )
    .map_err(sqlite_error)?;

    // The run's own input is dropped and its batches cancelled first; the
    // terminal write then releases whatever else the run still held.
    let sql = &session_runs_sql().verbs;
    let mut inputs = {
        let mut stmt = tx
            .prepare_cached(sql.bound_inputs.sql())
            .map_err(sqlite_error)?;
        stmt.query_map(params![session.as_str(), run.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?
    };
    let batches = admitted_batches_conn(tx, session, run)?;
    inputs.sort();
    inputs.dedup();
    for input in inputs {
        crate::conn::cached_execute(
            tx,
            sql.cancel_input.sql(),
            params![
                session.as_str(),
                input,
                crate::clamp_epoch_ms(at_ms),
                lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    for batch in batches {
        cancel_run_batch_conn(tx, session, &batch, at_ms)?;
    }
    // The terminal write applies the run's cancel request's `undelivered`
    // disposition to open input addressed to a turn the run ends
    // (FIG-3927 §2.4, FIG-3946).
    write_run_terminal_conn(tx, &terminal)?;
    Ok(terminal)
}

/// Decode a run's recorded admission (`session_runs.admission_json`).
pub(crate) fn decode_run_admission(json: &str) -> Result<RunAdmission, StoreError> {
    serde_json::from_str(json).map_err(|error| stored_data_corrupt("RunAdmission", error))
}

/// What run `run` holds, as a seal reads it in its transaction
/// (FIG-4814): the executor its admission recorded, else the one a seal
/// recorded for it. `None` when no executor is recorded for it.
pub(crate) fn held_run_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Option<lash_core_execution::store::HeldRun>, StoreError> {
    let row: Option<(Option<String>, Option<String>, bool)> = conn
        .query_row(
            session_runs_sql().runs.select_hold.sql(),
            params![session_id.as_str(), run.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((sealed, admission, ended)) = row else {
        return Ok(None);
    };
    lash_core_execution::store::RunExecutor::from_stored(admission.as_deref(), sealed.as_deref())
        .map(|executor| {
            executor.map(|executor| lash_core_execution::store::HeldRun { executor, ended })
        })
}

/// Record `hold`'s executor on its run, opening the run's row if it has
/// none, in the transaction of the seal that raised the epoch for it.
pub(crate) fn record_run_hold_conn(
    tx: &Connection,
    session_id: &SessionId,
    hold: &lash_core_execution::store::RunHold,
) -> Result<(), StoreError> {
    let sql = session_runs_sql();
    crate::conn::cached_execute(
        tx,
        sql.runs.insert_open.sql(),
        params![session_id.as_str(), hold.run.as_str()],
    )
    .map_err(sqlite_error)?;
    crate::conn::cached_execute(
        tx,
        sql.runs.write_hold.sql(),
        params![
            session_id.as_str(),
            hold.run.as_str(),
            hold.executor.to_stored()?
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// The session's unfinished run, with the head its admission recorded,
/// read on `conn`.
pub(crate) fn unfinished_run_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<UnfinishedRun>, StoreError> {
    let row: Option<(String, String)> = conn
        .query_row(
            session_runs_sql().runs.select_unfinished.sql(),
            [session_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(|(run, json)| {
        let admission = decode_run_admission(&json)?;
        Ok(UnfinishedRun {
            run: TurnId::parse(run)?,
            head: admission.head,
            executor: admission.executor,
        })
    })
    .transpose()
}

/// What owns session `session_id`'s head, read in a head commit's
/// transaction (FIG-4202): its unfinished run, the follow-on its head owes
/// (`owed_follow_on`, read with the head) and its earliest open session
/// command.
pub(crate) fn head_ownership_facts_conn(
    conn: &Connection,
    session_id: &SessionId,
    owed_follow_on: Option<TurnId>,
) -> Result<lash_core_execution::store::HeadOwnershipFacts, StoreError> {
    let unfinished_run = unfinished_run_conn(conn, session_id)?.map(|unfinished| unfinished.run);
    let open_command: Option<i64> = conn
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .pending_session_work_ordering
                .sql(),
            params![
                session_id.as_str(),
                lash_core_execution::QueuedWorkKind::Control.as_str()
            ],
            |row| row.get(1),
        )
        .map_err(sqlite_error)?;
    Ok(lash_core_execution::store::HeadOwnershipFacts {
        unfinished_run,
        owed_follow_on,
        open_command: open_command
            .map(|seq| stored_u64("QueuedWorkBatch", seq))
            .transpose()?,
    })
}

/// `run`'s recorded admission, read on `conn`: `None` for a run with no
/// admission.
pub(crate) fn run_admission_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Option<RunAdmission>, StoreError> {
    let json: Option<Option<String>> = conn
        .query_row(
            session_runs_sql().runs.select_admission.sql(),
            params![session_id.as_str(), run.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.flatten()
        .map(|json| decode_run_admission(&json))
        .transpose()
}

/// The queued-work batches `run`'s recorded admission took, read on `conn`:
/// none for a run with no admission or an input-headed one.
/// Cancel batch `batch_id` of session `session_id`, held by a run a verb
/// ends, into its `cancelled` tombstone at `at_ms` (ADR 0101 §8).
pub(crate) fn cancel_run_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    batch_id: &str,
    at_ms: u64,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        session_runs_sql().verbs.cancel_batch.sql(),
        params![session_id.as_str(), batch_id, crate::clamp_epoch_ms(at_ms)],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// The turns session `session_id`'s unfinished run executes, if a run is
/// unfinished, read on `conn`.
pub(crate) fn unfinished_run_turns_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<RunTurns>, StoreError> {
    let Some(unfinished) = unfinished_run_conn(conn, session_id)? else {
        return Ok(None);
    };
    let admission = run_admission_conn(conn, session_id, &unfinished.run)?;
    Ok(Some(RunTurns::new(&unfinished.run, admission.as_ref())))
}

pub(crate) fn admitted_batches_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Vec<String>, StoreError> {
    let Some(admission) = run_admission_conn(conn, session_id, run)? else {
        return Ok(Vec::new());
    };
    Ok(admission
        .queued
        .iter()
        .flat_map(|queued| {
            queued
                .batches
                .iter()
                .map(|batch| batch.batch_id.to_string())
        })
        .collect())
}

/// Write the terminal evidence `commit` carries, if any, in its transaction
/// at head revision `head_revision`: the commit of a run's final physical
/// turn ends the run (FIG-3600 S7).
pub(crate) fn write_commit_run_terminal_conn(
    tx: &Connection,
    commit: &lash_core_execution::store::RuntimeCommit,
    head_revision: u64,
    at_ms: u64,
) -> Result<(), StoreError> {
    match commit.run_terminal.as_deref().cloned() {
        Some(write) => write_run_terminal_conn(
            tx,
            &write.into_terminal(commit.session_id.clone(), head_revision, at_ms),
        ),
        None => Ok(()),
    }
}

/// The run input `input` of `session_id` is bound to, read on `conn`.
pub(crate) fn run_binding_conn(
    conn: &Connection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<Option<TurnId>, StoreError> {
    conn.query_row(
        session_runs_sql().inputs.select_run.sql(),
        params![session_id.as_str(), input.as_str()],
        |row| crate::codec::sql_identity(row.get::<_, String>(0)?),
    )
    .optional()
    .map_err(sqlite_error)
}

/// Bind `input` to `run` in the caller's transaction, set-if-absent: the
/// commit that applies a checkpoint-admitted input records the run that
/// applied it. A run admission already wrote the same binding for its own
/// inputs, so the insert is a no-op for them.
pub(crate) fn bind_applied_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    input: &InputId,
    run: &TurnId,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        session_runs_sql().inputs.insert.sql(),
        params![session_id.as_str(), input.as_str(), run.as_str()],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Bind each of `inputs` to `run`, set-if-absent, and open `run`'s row, in
/// the caller's transaction. A binding to another run refuses the whole
/// write.
pub(crate) fn bind_run_inputs_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    inputs: &[InputId],
) -> Result<(), StoreError> {
    let sql = session_runs_sql();
    for input in inputs {
        if let Some(bound) = run_binding_conn(tx, session_id, input)?
            && bound != *run
        {
            return Err(run_binding_conflict(session_id, input, &bound, run));
        }
    }
    crate::conn::cached_execute(
        tx,
        sql.runs.insert_open.sql(),
        params![session_id.as_str(), run.as_str()],
    )
    .map_err(sqlite_error)?;
    for input in inputs {
        crate::conn::cached_execute(
            tx,
            sql.inputs.insert.sql(),
            params![session_id.as_str(), input.as_str(), run.as_str()],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

pub(crate) fn intent_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredIntentRow> {
    Ok(StoredIntentRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        format: row.get(2)?,
        kind_json: row.get(3)?,
        state_json: row.get(4)?,
        created_at_ms: row.get(5)?,
        engine_ref: row.get(6)?,
        obligation_id: row.get(7)?,
        obligation_state: row.get(8)?,
    })
}

pub(crate) struct StoredIntentRow {
    id: i64,
    session_id: String,
    format: i64,
    kind_json: String,
    state_json: String,
    created_at_ms: i64,
    engine_ref: Option<String>,
    obligation_id: Option<String>,
    obligation_state: Option<String>,
}

impl StoredIntentRow {
    pub(crate) fn decode(self) -> Result<ControlIntent, StoreError> {
        ControlIntent::from_stored(
            stored_u64("ControlIntent", self.id)?,
            SessionId::parse(self.session_id)?,
            u32::try_from(self.format)
                .map_err(|_| stored_data_corrupt("ControlIntent", "format out of range"))?,
            &self.kind_json,
            &self.state_json,
            stored_u64("ControlIntent", self.created_at_ms)?,
            self.engine_ref,
            self.obligation_id,
            self.obligation_state,
        )
    }
}

/// `session_id`'s `close_session` intent, read on `conn`: its deletion
/// tombstone.
pub(crate) fn close_session_intent_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<ControlIntent>, StoreError> {
    conn.query_row(
        session_runs_sql().intents.select_close_session.sql(),
        params![session_id.as_str()],
        intent_row,
    )
    .optional()
    .map_err(sqlite_error)?
    .map(StoredIntentRow::decode)
    .transpose()
}

/// Intent `id`, read on `conn`.
pub(crate) fn load_intent_conn(
    conn: &Connection,
    id: ControlIntentId,
) -> Result<Option<ControlIntent>, StoreError> {
    conn.query_row(
        session_runs_sql().intents.select_by_id.sql(),
        params![sql_i64("control intent id", id.sequence())?],
        intent_row,
    )
    .optional()
    .map_err(sqlite_error)?
    .map(StoredIntentRow::decode)
    .transpose()
}

/// Session `session_id`'s open verbs (pending, or failed and retryable), in
/// id order, read on `conn`.
pub(crate) fn open_verbs_by_session_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Vec<ControlIntent>, StoreError> {
    let mut statement = conn
        .prepare_cached(
            session_runs_sql()
                .intents
                .select_open_verbs_by_session
                .sql(),
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session_id.as_str()], intent_row)
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    rows.into_iter().map(StoredIntentRow::decode).collect()
}

/// Record a new intent of `session_id` in the caller's transaction: `kind`,
/// `Pending`, with its `ControlIntent` obligation armed due at `at_ms` on the
/// same row (ADR 0109). Answers it with its allocated id.
pub(crate) fn insert_intent_conn(
    tx: &Connection,
    session_id: &SessionId,
    kind: ControlIntentKind,
    engine: Option<&EnginePark>,
    at_ms: u64,
) -> Result<ControlIntent, StoreError> {
    let state = ControlIntentState::Pending;
    let (state_code, state_json) = stored_intent_state(&state)?;
    let id: i64 = tx
        .query_row(
            session_runs_sql().intents.insert.sql(),
            params![
                session_id.as_str(),
                i64::from(CONTROL_INTENT_FORMAT),
                kind.code(),
                stored_intent_kind(&kind)?,
                state_code,
                state_json,
                sql_i64("control intent instant", at_ms)?,
                engine.map(EnginePark::as_str),
            ],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    let id = ControlIntentId::from_sequence(stored_u64("ControlIntent", id)?);
    let obligation = crate::obligation_ledger::arm_obligation_tx(
        tx,
        &ObligationKey::ControlIntent { intent_id: id },
        at_ms,
    )?
    .ok_or_else(|| {
        stored_data_corrupt(
            "ControlIntent",
            "a freshly recorded intent already carries an obligation",
        )
    })?;
    Ok(ControlIntent {
        id,
        session_id: session_id.clone(),
        format: CONTROL_INTENT_FORMAT,
        kind,
        state,
        created_at_ms: at_ms,
        engine: engine.cloned(),
        obligation: Some(IntentObligation {
            id: obligation,
            state: ObligationState::Due,
        }),
    })
}

/// Move intent `prior` to `next`'s state in the caller's transaction, if it
/// is still as `prior` read it. `false` when another writer moved it first.
pub(crate) fn write_intent_state_conn(
    tx: &Connection,
    prior: &ControlIntent,
    next: &ControlIntent,
) -> Result<bool, StoreError> {
    let (state_code, state_json) = stored_intent_state(&next.state)?;
    let (_, prior_json) = stored_intent_state(&prior.state)?;
    let changed = crate::conn::cached_execute(
        tx,
        session_runs_sql().intents.update_state.sql(),
        params![
            sql_i64("control intent id", next.id.sequence())?,
            state_code,
            state_json,
            prior_json,
        ],
    )
    .map_err(sqlite_error)?;
    if changed == 1 {
        // A session close's acknowledgement owes its physical delete
        // (ADR 0109 §4), armed in this transaction.
        crate::session_delete_ledger::arm_on_close_acknowledged_conn(tx, prior, next)?;
    }
    Ok(changed == 1)
}

/// Settle intent `prior`'s engine half to `next`'s state in the caller's
/// transaction under obligation claim `claim` (ADR 0109 claim fencing):
/// [`IntentSettle::ClaimLost`] and nothing written when the obligation is no
/// longer claimed under `claim`; otherwise `decide` answers the state to
/// write over the stored one, or `None` to leave it.
pub(crate) fn settle_intent_claimed_conn(
    tx: &Connection,
    id: ControlIntentId,
    claim: &ClaimToken,
    decide: impl FnOnce(&ControlIntentState) -> Option<ControlIntentState>,
) -> Result<IntentSettle, StoreError> {
    let sql = session_runs_sql();
    let intent_id = sql_i64("control intent id", id.sequence())?;
    let held: i64 = tx
        .query_row(
            sql.intents.select_claim_held.sql(),
            params![intent_id, claim.as_str()],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    let stored =
        load_intent_conn(tx, id)?.ok_or(StoreError::ControlIntentUnknown { intent: id })?;
    if held == 0 {
        return Ok(IntentSettle::ClaimLost);
    }
    let Some(state) = decide(&stored.state) else {
        return Ok(IntentSettle::Held(Box::new(stored)));
    };
    let (state_code, state_json) = stored_intent_state(&state)?;
    let (_, prior_json) = stored_intent_state(&stored.state)?;
    let changed = crate::conn::cached_execute(
        tx,
        sql.intents.update_state_claimed.sql(),
        params![
            intent_id,
            state_code,
            state_json,
            prior_json,
            claim.as_str()
        ],
    )
    .map_err(sqlite_error)?;
    if changed != 1 {
        // The write transaction is exclusive: nothing else can move the row
        // between the read and the write.
        return Err(StoreError::Contended);
    }
    let mut settled = stored.clone();
    settled.state = state;
    // A session close's acknowledgement owes its physical delete (ADR 0109
    // §4), armed in this transaction.
    crate::session_delete_ledger::arm_on_close_acknowledged_conn(tx, &stored, &settled)?;
    Ok(IntentSettle::Held(Box::new(settled)))
}

/// The store half of session `session_id`'s close, in the caller's
/// transaction ([`ControlIntentSqliteStore::begin_session_close`]).
///
/// The close names the runs it releases: every run without terminal
/// evidence (its logical-run rows, its parked run, its pending queued
/// run), each ended `Cancelled` by `SessionDeleted`, plus the runs of the
/// open verbs it supersedes, whose engine half then never runs.
///
/// [`ControlIntentSqliteStore::begin_session_close`]: lash_core_execution::store::ControlIntentSqliteStore::begin_session_close
pub(crate) fn begin_session_close_conn(
    tx: &Connection,
    session_id: &SessionId,
    at_ms: u64,
) -> Result<Option<ControlIntent>, StoreError> {
    if let Some(intent) = close_session_intent_conn(tx, session_id)? {
        return Ok(Some(intent));
    }
    let exists = tx
        .query_row(
            crate::session_sql::session_sql()
                .meta_sqlite
                .exists_materialized
                .sql(),
            params![session_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sqlite_error)?
        .is_some();
    if !exists {
        return Ok(None);
    }
    let sql = session_runs_sql();
    let mut runs = std::collections::BTreeSet::new();
    {
        let mut statement = tx
            .prepare_cached(sql.runs.select_open_runs.sql())
            .map_err(sqlite_error)?;
        let open = statement
            .query_map(params![session_id.as_str()], |row| {
                crate::codec::sql_identity::<TurnId>(row.get::<_, String>(0)?)
            })
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        runs.extend(open);
    }
    // The parked run is released with the park, whose feed event outlives
    // the session (FIG-3659).
    let released: Option<(String, i64)> = tx
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .turn_parks
                .delete_by_session_returning
                .sql(),
            params![session_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    if let Some((parked_run, park_id)) = released {
        crate::persistence::turn_park_feed::log_turn_park_closed_conn(
            tx,
            session_id,
            &parked_run,
            park_id,
            &ParkEventKind::Cancelled {
                cause: ParkCancelCause::SessionDeleted,
            },
            crate::clamp_epoch_ms(at_ms),
        )?;
        runs.insert(TurnId::parse(parked_run)?);
    }
    let verbs = open_verbs_by_session_conn(tx, session_id)?;
    for verb in &verbs {
        match &verb.kind {
            ControlIntentKind::Redrive { run, .. }
            | ControlIntentKind::Cancel { run, .. }
            | ControlIntentKind::Fork { run, .. } => {
                runs.insert(run.clone());
            }
            ControlIntentKind::CloseSession { .. } => {}
        }
    }
    let runs: Vec<TurnId> = runs.into_iter().collect();
    let intent = insert_intent_conn(
        tx,
        session_id,
        ControlIntentKind::CloseSession { runs: runs.clone() },
        None,
        at_ms,
    )?;
    for run in &runs {
        if run_terminal_conn(tx, session_id, run)?.is_none() {
            write_run_terminal_conn(
                tx,
                &RunTerminal {
                    session_id: session_id.clone(),
                    run: run.clone(),
                    cause: RunTerminalCause::SessionDeleted { intent: intent.id },
                    head_revision: None,
                    at_ms,
                },
            )?;
        }
    }
    for verb in verbs {
        let mut superseded = verb.clone();
        superseded.state = ControlIntentState::Superseded { by: intent.id };
        if !write_intent_state_conn(tx, &verb, &superseded)? {
            return Err(StoreError::Contended);
        }
    }
    let closed = crate::conn::cached_execute(
        tx,
        crate::session_sql::session_sql().meta.begin_close.sql(),
        params![
            session_id.as_str(),
            sql_i64("control intent id", intent.id.sequence())?,
            close_admission(intent.id).as_str(),
        ],
    )
    .map_err(sqlite_error)?;
    if closed != 1 {
        return Err(StoreError::Contended);
    }
    Ok(Some(intent))
}

/// Forget what session `session_id`'s runs hold, in its deletion's
/// transaction: its runs, its bindings and its verbs. A `close_session`
/// intent stays: it is the tombstone the factory answers the deleted
/// session's runs from.
pub(crate) fn delete_session_runs_conn(
    tx: &Connection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let sql = session_runs_sql();
    for statement in [
        sql.runs.delete_by_session.sql(),
        sql.runs.delete_shift_admissions.sql(),
        sql.inputs.delete_by_session.sql(),
        sql.intents.delete_verbs_by_session.sql(),
    ] {
        crate::conn::cached_execute(tx, statement, params![session_id.as_str()])
            .map_err(sqlite_error)?;
    }
    Ok(())
}

fn commit<T>(outcome: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

#[async_trait::async_trait]
impl RunStore for crate::SqliteStore {
    async fn prepare_shift_admission(
        &self,
        session_id: &SessionId,
        admission: &lash_core_execution::store::AdmissionId,
        executor: &RunExecutor,
    ) -> Result<lash_core_execution::store::ShiftAdmissionPreparation, StoreError> {
        crate::persistence::shift_admission::prepare(self, session_id, admission, executor).await
    }
    async fn read_shift_admission(
        &self,
        session_id: &SessionId,
        admission: &lash_core_execution::store::AdmissionId,
    ) -> Result<Option<lash_core_execution::store::ShiftAdmissionReceipt>, StoreError> {
        crate::persistence::shift_admission::read(self, session_id, admission).await
    }
    async fn commit_shift_admission(
        &self,
        request: &lash_core_execution::store::ShiftAdmissionWrite,
        anchor: &lash_core_execution::TraceAnchor,
    ) -> Result<lash_core_execution::store::ShiftAdmissionReceipt, StoreError> {
        crate::persistence::shift_admission::commit(self, request, anchor).await
    }

    async fn run_executor(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Option<RunExecutor>, StoreError> {
        let session_id = session_id.to_string();
        let run = run.to_string();
        let stored = self
            .conn
            .call(move |conn| {
                conn.query_row(
                    session_runs_sql().runs.select_hold.sql(),
                    params![session_id, run],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
            })
            .await
            .map_err(sqlite_error)?;
        stored
            .map(|(sealed, admission)| {
                RunExecutor::from_stored(admission.as_deref(), sealed.as_deref())
            })
            .transpose()
            .map(Option::flatten)
    }

    async fn unfinished_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRun>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(unfinished_run_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_run(
        &self,
        request: &lash_core_execution::store::AdmitRunRequest,
    ) -> Result<Option<RunAdmission>, StoreError> {
        lash_core_execution::store::admit_run_with_trace(self, request).await
    }

    async fn prepare_run_admission(
        &self,
        request: &lash_core_execution::store::AdmitRunRequest,
    ) -> Result<Option<lash_core_execution::store::PreparedRunAdmission>, StoreError> {
        Ok(crate::persistence::admit_run_sqlite(
            self,
            request,
            None,
            &lash_core_execution::TraceAnchor::Untraced,
        )
        .await?
        .map(
            |admission| lash_core_execution::store::PreparedRunAdmission {
                request: request.clone(),
                admission,
            },
        ))
    }

    async fn commit_run_admission(
        &self,
        prepared: &lash_core_execution::store::PreparedRunAdmission,
        anchor: &lash_core_execution::TraceAnchor,
    ) -> Result<Option<RunAdmission>, StoreError> {
        crate::persistence::admit_run_sqlite(self, &prepared.request, Some(prepared), anchor).await
    }

    async fn admit_at_checkpoint(
        &self,
        request: &lash_core_execution::store::CheckpointAdmissionRequest,
    ) -> Result<lash_core_execution::store::CheckpointAdmission, StoreError> {
        crate::persistence::admit_at_checkpoint_sqlite(self, request).await
    }

    async fn run_terminal(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Option<RunTerminal>, StoreError> {
        let session_id = session_id.clone();
        let run = run.clone();
        self.conn
            .call(move |conn| Ok(run_terminal_conn(conn, &session_id, &run)))
            .await
            .map_err(sqlite_error)?
    }

    async fn end_refused_run(
        &self,
        fence: &lash_core_execution::store::ShiftFence,
        run: &TurnId,
        refusal: &lash_core_execution::RuntimeError,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        lash_core_execution::store::validate_session_id(fence.session())?;
        let fence = fence.clone();
        let run = run.clone();
        let refusal = refusal.clone();
        self.conn
            .write_flow(move |tx| commit(end_refused_run_conn(tx, &fence, &run, &refusal, at_ms)))
            .await
            .map_err(sqlite_error)?
    }

    async fn end_command_run(
        &self,
        fence: &lash_core_execution::store::ShiftFence,
        run: &TurnId,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        lash_core_execution::store::validate_session_id(fence.session())?;
        let fence = fence.clone();
        let run = run.clone();
        self.conn
            .write_flow(move |tx| commit(end_command_run_conn(tx, &fence, &run, at_ms)))
            .await
            .map_err(sqlite_error)?
    }

    async fn run_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        // An input's run is its binding: the admission that took it, the
        // commit whose checkpoint delivery applied it, or the fork that
        // rebound it.
        self.run_binding(session_id, input).await
    }

    async fn run_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        let session_id = session_id.clone();
        let input = input.clone();
        self.conn
            .call(move |conn| Ok(run_binding_conn(conn, &session_id, &input)))
            .await
            .map_err(sqlite_error)?
    }

    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError> {
        let session_id = session_id.clone();
        let run = run.clone();
        let scopes = self
            .conn
            .call(move |conn| {
                let mut stmt =
                    conn.prepare_cached(session_runs_sql().inputs.bound_turn_scopes.sql())?;
                stmt.query_map(params![session_id.as_str(), run.as_str()], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        Ok(scopes
            .into_iter()
            .map(TurnId::parse)
            .collect::<Result<_, _>>()?)
    }

    async fn bind_run_inputs(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        let run = run.clone();
        let inputs = inputs.to_vec();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                    bind_run_inputs_conn(tx, &session_id, &run, &inputs)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}
