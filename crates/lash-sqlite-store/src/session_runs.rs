//! The SQLite half of the logical-run family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_runs` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the run admission, session deletion
//! and the factory's catalog reads share, plus
//! [`RunStore`] for the bound store.

use std::sync::LazyLock;

use lash_core_store::store::CONTROL_INTENT_FORMAT;
use lash_core_store::store::ControlIntent;
use lash_core_store::store::ControlIntentId;
use lash_core_store::store::ControlIntentKind;
use lash_core_store::store::ControlIntentState;
use lash_core_store::store::InputStatus;
use lash_core_store::store::RunAdmissionRecord;
use lash_core_store::store::RunEndOutcome;
use lash_core_store::store::RunStore;
use lash_core_store::store::RunTerminal;
use lash_core_store::store::RunTerminalCause;
use lash_core_store::store::RunTerminalWriteDecision;
use lash_core_store::store::RunTurns;
use lash_core_store::store::UnfinishedRun;
use lash_core_store::store::decide_run_terminal_write;
use lash_core_store::store::run_binding_conflict;
use lash_core_store::store::stored_intent_kind;
use lash_core_store::store::stored_intent_state;
use lash_core_store::store_backend_support::turn_cancel::EndedRunInputRelease;
use lash_protocol_types::{InputId, SessionId, TurnId};
use lash_store_sql::session_runs::{
    control_intents::ControlIntentStatements, run_inputs::SessionRunInputStatements,
    runs::SessionRunStatements,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::conn::TxOutcome;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

/// Every logical-run statement the session catalog issues.
pub(crate) struct SessionRunsSql {
    pub(crate) runs: SessionRunStatements,
    pub(crate) inputs: SessionRunInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_RUNS_SQL: LazyLock<SessionRunsSql> = LazyLock::new(|| {
    // The run-input reads name the turn-input lifecycle.
    let dialect =
        crate::schema_layout::MAIN.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE);
    SessionRunsSql {
        runs: SessionRunStatements::render(dialect),
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
type TerminalRow = (Option<String>, Option<i64>, Option<i64>, Option<String>);

/// The terminal evidence of `run` in `session_id`, read on `conn`.
pub(crate) fn run_terminal_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Option<RunTerminal>, StoreError> {
    let row: Option<TerminalRow> = conn
        .prepare_cached(session_runs_sql().runs.select_terminal.sql())
        .and_then(|mut statement| {
            statement.query_row(params![session_id.as_str(), run.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
        })
        .optional()
        .map_err(sqlite_error)?;
    let Some((cause_json, head_revision, at_ms, kind)) = row else {
        let deleted = conn
            .prepare_cached(
                crate::session_sql::session_sql()
                    .deleted_sqlite
                    .exists
                    .sql(),
            )
            .and_then(|mut statement| statement.query_row(params![session_id.as_str()], |_| Ok(())))
            .optional()
            .map_err(sqlite_error)?
            .is_some();
        if !deleted {
            return Ok(None);
        }
        return Ok(close_session_intent_conn(conn, session_id)?
            .and_then(|intent| intent.session_deleted_terminal(run)));
    };
    let (cause_json, at_ms, kind) = match (cause_json, at_ms, kind) {
        (None, None, None) if head_revision.is_none() => return Ok(None),
        (Some(cause), Some(at), Some(kind)) => (cause, at, kind),
        _ => {
            return Err(stored_data_corrupt(
                "RunTerminal",
                "partial terminal evidence",
            ));
        }
    };
    let terminal = RunTerminal::from_stored(
        session_id.clone(),
        run.clone(),
        &cause_json,
        head_revision
            .map(|revision| stored_u64("RunTerminal", revision))
            .transpose()?,
        stored_u64("RunTerminal", at_ms)?,
    )?;
    if terminal.kind().as_str() != kind {
        return Err(stored_data_corrupt(
            "RunTerminal",
            "terminal kind disagrees with cause",
        ));
    }
    Ok(Some(terminal))
}

/// Name the session's published head revision as the terminal's window,
/// in the terminal writer's transaction. Nothing is read beyond the head's
/// revision row: the window is decoded only when a report reads it. A
/// session with no head names no window.
pub(crate) fn terminal_window_revision(
    tx: &Connection,
    session: &SessionId,
) -> Result<Option<i64>, StoreError> {
    tx.prepare_cached(session_runs_sql().runs.select_window_head.sql())
        .map_err(sqlite_error)?
        .query_row(params![session.as_str()], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(sqlite_error)
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
    tx: &crate::conn::FencedTx<'_>,
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
    let window_revision = if matches!(terminal.cause, RunTerminalCause::SessionDeleted { .. }) {
        None
    } else {
        terminal_window_revision(tx, &terminal.session_id)?
    };
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
            window_revision,
        ],
    )
    .map_err(sqlite_error)?;
    if written != 1 {
        return Err(StoreError::Backend(format!(
            "run `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.run, terminal.session_id
        )));
    }
    if let Some(revision) = terminal.head_revision {
        crate::revisions::record_terminal_conn(
            tx,
            &terminal.session_id,
            revision,
            lash_durable::DurableInstant(crate::clamp_epoch_ms(terminal.at_ms)),
        )?;
    }
    release_run_rows_conn(tx, &terminal.session_id, &terminal.run, terminal.at_ms)
}

/// Release every row of either admission table `run` still holds, in the
/// caller's transaction: accepted input is open again in the state its
/// submitted delivery names, and the session is woken to admit it again.
///
/// [`EndedRunInputRelease`] owns the addressed-input selection and disposition;
/// this adapter applies its withdrawals at `at_ms` in the same transaction.
fn release_run_rows_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    at_ms: u64,
) -> Result<(), StoreError> {
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
    // What the run let go of is the session's to admit again. A run ends in
    // its own session's commit, whose owner reads its mail next, or in its
    // close, which wakes the session itself: no hint follows.
    crate::durable::wake_session_tx(tx, session_id, false, at_ms)?;
    let ended = run_turns_conn(tx, session_id, run)?;
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
    let addressed = EndedRunInputRelease::select(
        &ended,
        open_rows.into_iter().map(|row| {
            let ingress = crate::pending_turn_inputs::decode_turn_input_ingress(row.ingress_json)?;
            Ok((row.input_id, ingress.active_turn_id().cloned()))
        }),
    )?;
    if addressed.is_empty() {
        return Ok(());
    }
    let request =
        crate::persistence::turn_cancel::load_turn_cancel_request_conn(tx, session_id, run)?;
    for input_id in addressed.dropped_inputs(request.as_ref().map(|request| request.undelivered)) {
        crate::conn::cached_execute(
            tx,
            sql.pending_inputs.cancel.sql(),
            params![
                session_id.as_str(),
                input_id.as_str(),
                lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str(),
                crate::clamp_epoch_ms(at_ms),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

/// The run's execution met a typed refusal no retry can change
/// (FIG-4018): the same transaction as a lost run's, ending it with the
/// refusal.
pub(crate) fn end_refused_run_conn(
    tx: &crate::conn::FencedTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    refusal: &lash_core_store::runtime_error::RuntimeError,
    at_ms: u64,
) -> Result<RunEndOutcome, StoreError> {
    let target = lash_execution_contracts::run::RunRef {
        session: session_id.clone(),
        run: run.clone(),
    };
    match unanswered_run_conn(tx, &target)? {
        UnansweredRun::Ended(terminal) => Ok(RunEndOutcome::AlreadyEnded(*terminal)),
        UnansweredRun::Unknown => Ok(RunEndOutcome::Unknown),
        UnansweredRun::Open => {
            let cause = RunTerminalCause::Refused {
                refusal: refusal.into(),
            };
            write_unanswered_run_end_conn(tx, &target, at_ms, cause).map(RunEndOutcome::Ended)
        }
    }
}

/// The command run that applied the session's command lane until it was
/// empty ends (FIG-4202): its row opens and its terminal is written in one
/// transaction, arming its scope close.
pub(crate) fn end_command_run_conn(
    tx: &crate::conn::FencedTx<'_>,
    session: &SessionId,
    run: &TurnId,
    at_ms: u64,
) -> Result<RunEndOutcome, StoreError> {
    if let Some(terminal) = run_terminal_conn(tx, session, run)? {
        return Ok(RunEndOutcome::AlreadyEnded(terminal));
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
    target: &lash_execution_contracts::run::RunRef,
) -> Result<UnansweredRun, StoreError> {
    let session = &target.session;
    let run = &target.run;
    if let Some(terminal) = run_terminal_conn(tx, session, run)? {
        return Ok(UnansweredRun::Ended(Box::new(terminal)));
    }
    let exists: bool = tx
        .prepare_cached(session_runs_sql().runs.select_terminal.sql())
        .and_then(|mut statement| {
            statement.query_row(params![session.as_str(), run.as_str()], |_| Ok(true))
        })
        .optional()
        .map_err(sqlite_error)?
        .unwrap_or(false);
    Ok(if exists {
        UnansweredRun::Open
    } else {
        UnansweredRun::Unknown
    })
}

/// End an open run no commit answered with `cause`.
fn write_unanswered_run_end_conn(
    tx: &crate::conn::FencedTx<'_>,
    target: &lash_execution_contracts::run::RunRef,
    at_ms: u64,
    cause: RunTerminalCause,
) -> Result<RunTerminal, StoreError> {
    let session = &target.session;
    let run = &target.run;
    let terminal = RunTerminal {
        session_id: session.clone(),
        run: run.clone(),
        cause,
        head_revision: None,
        at_ms,
    };
    // The run's own input is dropped and its batches cancelled first; the
    // terminal write then releases whatever else the run still held.
    let sql = session_runs_sql();
    let mut inputs = {
        let mut stmt = tx
            .prepare_cached(sql.inputs.bound_inputs.sql())
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
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .cancel_input
                .sql(),
            params![
                session.as_str(),
                input,
                crate::clamp_epoch_ms(at_ms),
                lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str(),
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

/// The session's unfinished run, with the head its admission recorded,
/// read on `conn`.
pub(crate) fn unfinished_run_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<UnfinishedRun>, StoreError> {
    let row: Option<(String, String)> = conn
        .prepare_cached(session_runs_sql().runs.select_unfinished.sql())
        .and_then(|mut statement| {
            statement.query_row([session_id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        })
        .optional()
        .map_err(sqlite_error)?;
    row.map(|(run, json)| {
        Ok(UnfinishedRun {
            run: TurnId::parse(run)?,
            head: RunAdmissionRecord::from_stored(&json)?.head(),
        })
    })
    .transpose()
}

/// What owns session `session_id`'s head, read in a head commit's
/// transaction (FIG-4202): its unfinished run and its earliest open session
/// command.
pub(crate) fn head_ownership_facts_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<lash_core_store::store::HeadOwnershipFacts, StoreError> {
    let unfinished_run = unfinished_run_conn(conn, session_id)?.map(|unfinished| unfinished.run);
    let open_command: Option<i64> = conn
        .prepare_cached(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .pending_session_work_ordering
                .sql(),
        )
        .and_then(|mut statement| {
            statement.query_row(params![session_id.as_str()], |row| row.get(1))
        })
        .map_err(sqlite_error)?;
    Ok(lash_core_store::store::HeadOwnershipFacts {
        unfinished_run,
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
) -> Result<Option<RunAdmissionRecord>, StoreError> {
    let json: Option<Option<String>> = conn
        .prepare_cached(session_runs_sql().runs.select_admission.sql())
        .and_then(|mut statement| {
            statement.query_row(params![session_id.as_str(), run.as_str()], |row| row.get(0))
        })
        .optional()
        .map_err(sqlite_error)?;
    json.flatten()
        .map(|json| RunAdmissionRecord::from_stored(&json))
        .transpose()
}

/// The turns `run` executes ([`RunTurns`]), read on `conn`: the run itself
/// and the turns its recorded admission's rows were accepted under.
pub(crate) fn run_turns_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<RunTurns, StoreError> {
    let Some(admission) = run_admission_conn(conn, session_id, run)? else {
        return Ok(RunTurns::new(run, Vec::new()));
    };
    let sql = crate::turn_ingress::turn_ingress_sql();
    let source_key = |statement: &str, id: &str| -> Result<Option<String>, StoreError> {
        let key: Option<Option<String>> = conn
            .prepare_cached(statement)
            .and_then(|mut statement| {
                statement.query_row(params![session_id.as_str(), id], |row| row.get(0))
            })
            .optional()
            .map_err(sqlite_error)?;
        Ok(key.flatten())
    };
    let mut members = Vec::new();
    for input in admission.input_ids() {
        members.extend(source_key(
            sql.pending_inputs.select_source_key_by_id.sql(),
            input.as_str(),
        )?);
    }
    for batch in admission.batch_ids() {
        members.extend(source_key(
            sql.queued_batches.select_source_key_by_id.sql(),
            batch.as_str(),
        )?);
    }
    Ok(RunTurns::new(run, members))
}

/// The queued-work batches `run`'s recorded admission took, read on `conn`:
/// none for a run with no admission or an input-headed one.
/// Cancel batch `batch_id` of session `session_id`, held by a run that
/// ends unanswered, into its `cancelled` tombstone at `at_ms` (ADR 0101 §8).
pub(crate) fn cancel_run_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    batch_id: &str,
    at_ms: u64,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .cancel_batch
            .sql(),
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
    run_turns_conn(conn, session_id, &unfinished.run).map(Some)
}

pub(crate) fn admitted_batches_conn(
    conn: &Connection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Vec<String>, StoreError> {
    Ok(run_admission_conn(conn, session_id, run)?
        .map(|admission| admission.batch_ids())
        .unwrap_or_default()
        .into_iter()
        .map(|batch| batch.to_string())
        .collect())
}

/// Write the terminal evidence `commit` carries, if any, in its transaction
/// at head revision `head_revision`: the commit of a run's final physical
/// turn ends the run (FIG-3600 S7).
pub(crate) fn write_commit_run_terminal_conn(
    tx: &crate::conn::FencedTx<'_>,
    commit: &lash_core_store::store::RuntimeCommit,
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
    conn.prepare_cached(session_runs_sql().inputs.select_run.sql())
        .and_then(|mut statement| {
            statement.query_row(params![session_id.as_str(), input.as_str()], |row| {
                row.get::<_, Option<String>>(0)
            })
        })
        .optional()
        .map_err(sqlite_error)?
        .flatten()
        .map(TurnId::parse)
        .transpose()
        .map_err(StoreError::from)
}

/// What a follower of `input` of `session_id` reads, in one statement on
/// `conn`: its binding, whether its row is open, and the session's fault.
fn input_status_conn(
    conn: &Connection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<InputStatus, StoreError> {
    let (run, open, fault_json, fault_at_ms, forsaken) = conn
        .prepare_cached(session_runs_sql().inputs.select_status.sql())
        .and_then(|mut stmt| {
            stmt.query_row(params![session_id.as_str(), input.as_str()], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            })
        })
        .map_err(sqlite_error)?;
    Ok(InputStatus {
        run: run
            .map(TurnId::parse)
            .transpose()
            .map_err(StoreError::from)?,
        open,
        fault: lash_core_store::store::SessionFault::from_stored_columns(
            session_id,
            fault_json,
            fault_at_ms,
        )?,
        forsaken,
    })
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
            sql.inputs.bind.sql(),
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
    })
}

pub(crate) struct StoredIntentRow {
    id: i64,
    session_id: String,
    format: i64,
    kind_json: String,
    state_json: String,
    created_at_ms: i64,
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
        )
    }
}

/// `session_id`'s `close_session` intent, read on `conn`: its deletion
/// tombstone.
pub(crate) fn close_session_intent_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<ControlIntent>, StoreError> {
    conn.prepare_cached(session_runs_sql().intents.select_close_session.sql())
        .and_then(|mut statement| statement.query_row(params![session_id.as_str()], intent_row))
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

/// Record a new intent of `session_id` in the caller's transaction: `kind`,
/// `Pending`. Answers it with its allocated id.
pub(crate) fn insert_intent_conn(
    tx: &Connection,
    session_id: &SessionId,
    kind: ControlIntentKind,
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
            ],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    let id = ControlIntentId::from_sequence(stored_u64("ControlIntent", id)?);
    Ok(ControlIntent {
        id,
        session_id: session_id.clone(),
        format: CONTROL_INTENT_FORMAT,
        kind,
        state,
        created_at_ms: at_ms,
    })
}

/// The store half of session `session_id`'s close, in the caller's
/// transaction ([`ControlIntentStore::begin_session_close`]).
///
/// The close names the runs it releases: every run without terminal
/// evidence, each ended `Cancelled` by `SessionDeleted`. It wakes the
/// session actor, whose close steps act on the intent, and answers the
/// actor it woke for the wake sink once the transaction commits.
///
/// [`ControlIntentStore::begin_session_close`]: lash_core_store::store::ControlIntentStore::begin_session_close
pub(crate) fn begin_session_close_conn(
    tx: &crate::conn::FencedTx<'_>,
    session_id: &SessionId,
    at_ms: u64,
) -> Result<(Option<ControlIntent>, Option<lash_durable::Woken>), StoreError> {
    if let Some(intent) = close_session_intent_conn(tx, session_id)? {
        return Ok((Some(intent), None));
    }
    let exists = tx
        .prepare_cached(
            crate::session_sql::session_sql()
                .meta_sqlite
                .exists_materialized
                .sql(),
        )
        .and_then(|mut statement| statement.query_row(params![session_id.as_str()], |_| Ok(())))
        .optional()
        .map_err(sqlite_error)?
        .is_some();
    if !exists {
        return Ok((None, None));
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
    let runs: Vec<TurnId> = runs.into_iter().collect();
    let intent = insert_intent_conn(
        tx,
        session_id,
        ControlIntentKind::CloseSession { runs: runs.clone() },
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
    let closed = crate::conn::cached_execute(
        tx,
        crate::session_sql::session_sql().meta.begin_close.sql(),
        params![
            session_id.as_str(),
            sql_i64("control intent id", intent.id.sequence())?,
        ],
    )
    .map_err(sqlite_error)?;
    if closed != 1 {
        return Err(StoreError::Contended);
    }
    let woken = crate::durable::wake_session_tx(tx, session_id, false, at_ms)?;
    Ok((Some(intent), woken))
}

/// Forget the session's run records in its
/// deletion transaction. A `close_session` intent stays: it is the tombstone
/// the factory answers the deleted session's runs from.
pub(crate) fn delete_session_runs_conn(
    tx: &Connection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        session_runs_sql().runs.delete_by_session.sql(),
        params![session_id.as_str()],
    )
    .map_err(sqlite_error)?;
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

    async fn admit_at_checkpoint(
        &self,
        request: &lash_core_store::store::CheckpointAdmissionRequest,
    ) -> Result<lash_core_store::store::CheckpointAdmission, StoreError> {
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
        session_id: &SessionId,
        run: &TurnId,
        refusal: &lash_core_store::runtime_error::RuntimeError,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        lash_core_store::store::validate_session_id(session_id)?;
        let session_id = session_id.clone();
        let run = run.clone();
        let refusal = refusal.clone();
        self.conn
            .write_flow(move |tx| {
                commit(end_refused_run_conn(tx, &session_id, &run, &refusal, at_ms))
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn end_command_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        lash_core_store::store::validate_session_id(session_id)?;
        let session_id = session_id.clone();
        let run = run.clone();
        self.conn
            .write_flow(move |tx| commit(end_command_run_conn(tx, &session_id, &run, at_ms)))
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

    async fn input_status(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<InputStatus, StoreError> {
        let session_id = session_id.clone();
        let input = input.clone();
        self.conn
            .call(move |conn| Ok(input_status_conn(conn, &session_id, &input)))
            .await
            .map_err(sqlite_error)?
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
