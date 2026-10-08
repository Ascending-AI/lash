//! The PostgreSQL half of the logical-run family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_runs` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the run admission, session deletion
//! and the factory's catalog reads share, plus
//! [`RunStore`] for the session store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    CONTROL_INTENT_FORMAT, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    RunAdmissionRecord, RunEndOutcome, RunStore, RunTerminal, RunTerminalCause,
    RunTerminalWriteDecision, RunTurns, UnfinishedRun, decide_run_terminal_write,
    run_binding_conflict, stored_intent_kind, stored_intent_state,
};
use lash_sansio::{InputId, SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::session_runs::{
    control_intents::ControlIntentStatements, run_inputs::SessionRunInputStatements,
    runs::SessionRunStatements,
};
use sqlx::{PgConnection, Row};

use crate::support::{store_sqlx_error, u64_from_sql};
use crate::{PostgresStore, StoreError, acquire_runtime_connection};

/// Every logical-run statement this store issues.
pub(crate) struct SessionRunsSql {
    pub(crate) runs: SessionRunStatements,
    pub(crate) inputs: SessionRunInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_RUNS_SQL: LazyLock<SessionRunsSql> = LazyLock::new(|| {
    // The admission read names the head input's lifecycle (FIG-3840).
    let dialect = Dialect::postgres().with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE);
    SessionRunsSql {
        runs: SessionRunStatements::render(dialect),
        inputs: SessionRunInputStatements::render(dialect),
        intents: ControlIntentStatements::render(dialect),
    }
});

/// This store's logical-run statements, rendered once at first use.
pub(crate) fn session_runs_sql() -> &'static SessionRunsSql {
    &SESSION_RUNS_SQL
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The terminal evidence of `run` in `session_id`, read on `conn`.
pub(crate) async fn run_terminal_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Option<RunTerminal>, StoreError> {
    let row = sqlx::query(session_runs_sql().runs.select_terminal.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        let deleted: bool = sqlx::query_scalar(
            crate::session_sql::session_sql()
                .deleted_postgres
                .exists
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_one(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
        if !deleted {
            return Ok(None);
        }
        return Ok(close_session_intent_conn(conn, session_id)
            .await?
            .and_then(|intent| intent.session_deleted_terminal(run)));
    };
    let cause_json: Option<String> = row.try_get(0).map_err(store_sqlx_error)?;
    let head_revision: Option<i64> = row.try_get(1).map_err(store_sqlx_error)?;
    let at_ms: Option<i64> = row.try_get(2).map_err(store_sqlx_error)?;
    let (Some(cause_json), Some(at_ms)) = (cause_json, at_ms) else {
        return Ok(None);
    };
    RunTerminal::from_stored(
        session_id.clone(),
        run.clone(),
        &cause_json,
        head_revision
            .map(|revision| u64_from_sql("RunTerminal", "terminal_head_revision", revision))
            .transpose()?,
        u64_from_sql("RunTerminal", "terminal_at_ms", at_ms)?,
    )
    .map(Some)
}

/// Capture the current bounded window in the terminal writer's transaction.
/// Its hydrated components keep report reads independent of later GC.
pub(crate) async fn terminal_window_json(
    conn: &mut PgConnection,
    session: &SessionId,
    chunk_size: usize,
) -> Result<Option<String>, StoreError> {
    super::runtime_persistence::history::window_conn(
        conn,
        session,
        lash_core_execution::store::WindowSelector::Current,
        lash_core_execution::FleetFormat::current(),
        chunk_size,
        #[cfg(any(test, feature = "testing"))]
        None,
    )
    .await?
    .map(|window| {
        serde_json::to_string(&window).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "RunTerminalWindow",
            message: error.to_string(),
        })
    })
    .transpose()
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
pub(crate) async fn write_run_terminal_conn(
    conn: &mut PgConnection,
    terminal: &RunTerminal,
    chunk_size: usize,
) -> Result<(), StoreError> {
    let stored = run_terminal_conn(conn, &terminal.session_id, &terminal.run).await?;
    if decide_run_terminal_write(stored.as_ref(), terminal)?
        == RunTerminalWriteDecision::AlreadyWritten
    {
        return release_run_rows_conn(conn, &terminal.session_id, &terminal.run, terminal.at_ms)
            .await;
    }
    let sql = session_runs_sql();
    sqlx::query(sql.runs.insert_open.sql())
        .bind(terminal.session_id.as_str())
        .bind(terminal.run.as_str())
        .execute(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    let window = if matches!(terminal.cause, RunTerminalCause::SessionDeleted { .. }) {
        None
    } else {
        terminal_window_json(conn, &terminal.session_id, chunk_size).await?
    };
    let columns = terminal.to_stored()?;
    let written = sqlx::query(sql.runs.write_terminal.sql())
        .bind(terminal.session_id.as_str())
        .bind(terminal.run.as_str())
        .bind(columns.kind)
        .bind(columns.cause_json)
        .bind(
            columns
                .head_revision
                .map(|revision| sql_i64("terminal head revision", revision))
                .transpose()?,
        )
        .bind(sql_i64("terminal instant", columns.at_ms)?)
        .bind(window)
        .execute(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if written != 1 {
        return Err(StoreError::Backend(format!(
            "run `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.run, terminal.session_id
        )));
    }
    release_run_rows_conn(conn, &terminal.session_id, &terminal.run, terminal.at_ms).await
}

/// Release every row of either admission table `run` still holds, in the
/// caller's transaction: accepted input is open again in the state its
/// submitted delivery names, and the session is woken to admit it again.
///
/// Open input addressed to a turn the run ends ([`RunTurns`]: its own
/// physical turns and the turns its admission's members were accepted
/// under) names a turn that will never run again, so the run's disposition
/// applies to it here (FIG-3946): the undelivered disposition of the run's
/// cancellation request if it has one, else `Defer`. `Defer` writes
/// nothing: the row is next-turn input at its own position by rule, its
/// submitted delivery unchanged (ADR 0101 §5.1). `Drop` withdraws it into
/// its tombstone at the terminal instant `at_ms` (FIG-4098). Either is
/// recorded once on the request's outcome. No open row is bound to a run
/// with terminal evidence.
///
/// An input the run's admission took as its own (`session_run_inputs`)
/// that is still open is unbound from the run too, so a later run can
/// admit it: a terminal run answers nothing more.
async fn release_run_rows_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    run: &TurnId,
    at_ms: u64,
) -> Result<(), StoreError> {
    let verbs = &session_runs_sql().inputs;
    let own: Vec<String> = sqlx::query_scalar(verbs.bound_inputs.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    for input in own {
        sqlx::query(verbs.unbind.sql())
            .bind(session_id.as_str())
            .bind(&input)
            .execute(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
    }
    let sql = crate::turn_ingress::turn_ingress_sql();
    sqlx::query(sql.pending_inputs.release_run.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .execute(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(sql.queued_batches.release_run.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .execute(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    // What the run let go of is the session's to admit again.
    crate::durable::wake_session_tx(conn, session_id, false, at_ms).await?;
    let ended = run_turns_conn(conn, session_id, run).await?;
    let open_rows = sqlx::query(sql.pending_inputs_postgres.select_pending_active.sql())
        .bind(session_id.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    let mut addressed = Vec::new();
    for row in open_rows {
        let input = crate::pending_turn_input_from_row(crate::pending_turn_input_row(row)?)?;
        if input
            .state
            .active_turn_id()
            .is_some_and(|turn| ended.contains(turn))
        {
            addressed.push(input);
        }
    }
    if addressed.is_empty() {
        return Ok(());
    }
    // `select_request` yields request_id, origin, reason, disposition, mode.
    let request: Option<
        lash_core_execution::store_backend_support::turn_cancel::TurnCancelRequestRow,
    > = sqlx::query_as(sql.cancel_requests.select_request.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    let disposition = request
        .as_ref()
        .map(|row| {
            lash_core_execution::store_backend_support::turn_cancel::turn_cancel_undelivered_from_wire(&row.3)
        })
        .transpose()?
        .unwrap_or(lash_core_execution::TurnCancelUndeliveredInputPolicy::Defer);
    for input in addressed {
        if disposition == lash_core_execution::TurnCancelUndeliveredInputPolicy::Drop {
            sqlx::query(sql.pending_inputs.cancel.sql())
                .bind(session_id.as_str())
                .bind(input.input_id.as_str())
                .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
                .bind(crate::support::clamp_epoch_ms(at_ms))
                .execute(crate::observed_sql::executor(&mut *conn))
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    Ok(())
}

/// The run's execution met a typed refusal no retry can change
/// (FIG-4018): the same transaction as a lost run's, ending it with the
/// refusal.
pub(crate) async fn end_refused_run_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    run: &TurnId,
    refusal: &lash_core_execution::RuntimeError,
    at_ms: u64,
    chunk_size: usize,
) -> Result<RunEndOutcome, StoreError> {
    let target = lash_core_execution::engine::RunRef {
        session: session_id.clone(),
        run: run.clone(),
    };
    match unanswered_run_tx(tx, &target).await? {
        UnansweredRun::Ended(terminal) => Ok(RunEndOutcome::AlreadyEnded(*terminal)),
        UnansweredRun::Unknown => Ok(RunEndOutcome::Unknown),
        UnansweredRun::Open => {
            let cause = RunTerminalCause::Refused {
                refusal: refusal.into(),
            };
            write_unanswered_run_end_tx(tx, &target, at_ms, cause, chunk_size)
                .await
                .map(RunEndOutcome::Ended)
        }
    }
}

/// The command run that applied the session's command lane until it was
/// empty ends (FIG-4202): its row opens and its terminal is written in one
/// transaction, arming its scope close.
pub(crate) async fn end_command_run_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session: &SessionId,
    run: &TurnId,
    at_ms: u64,
    chunk_size: usize,
) -> Result<RunEndOutcome, StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session).await?;
    if let Some(terminal) = run_terminal_conn(&mut *tx, session, run).await? {
        return Ok(RunEndOutcome::AlreadyEnded(terminal));
    }
    let terminal = RunTerminal {
        session_id: session.clone(),
        run: run.clone(),
        cause: RunTerminalCause::CommandsApplied,
        head_revision: None,
        at_ms,
    };
    write_run_terminal_conn(&mut *tx, &terminal, chunk_size).await?;
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

/// Where `target` stands, read under the session history lock.
async fn unanswered_run_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &lash_core_execution::engine::RunRef,
) -> Result<UnansweredRun, StoreError> {
    let session = &target.session;
    let run = &target.run;
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session).await?;
    if let Some(terminal) = run_terminal_conn(&mut *tx, session, run).await? {
        return Ok(UnansweredRun::Ended(Box::new(terminal)));
    }
    Ok(
        if sqlx::query(session_runs_sql().runs.select_terminal.sql())
            .bind(session.as_str())
            .bind(run.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?
            .is_some()
        {
            UnansweredRun::Open
        } else {
            UnansweredRun::Unknown
        },
    )
}

/// End an open run no commit answered with `cause`.
async fn write_unanswered_run_end_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &lash_core_execution::engine::RunRef,
    at_ms: u64,
    cause: RunTerminalCause,
    chunk_size: usize,
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
    let mut inputs: Vec<String> = sqlx::query_scalar(session_runs_sql().inputs.bound_inputs.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_all(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    let batches = admitted_batches_conn(tx, session, run).await?;
    inputs.sort();
    inputs.dedup();
    for input in inputs {
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .cancel_input
                .sql(),
        )
        .bind(session.as_str())
        .bind(input)
        .bind(crate::support::clamp_epoch_ms(at_ms))
        .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    }
    for batch in batches {
        cancel_run_batch_tx(tx, session, &batch, at_ms).await?;
    }
    // The terminal write applies the run's cancel request's `undelivered`
    // disposition to open input addressed to a turn the run ends
    // (FIG-3927 §2.4, FIG-3946).
    write_run_terminal_conn(&mut *tx, &terminal, chunk_size).await?;
    Ok(terminal)
}

/// Cancel batch `batch_id` of session `session_id`, held by a run a verb
/// ends, into its `cancelled` tombstone at `at_ms` (ADR 0101 §8).
pub(crate) async fn cancel_run_batch_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    batch_id: &str,
    at_ms: u64,
) -> Result<(), StoreError> {
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .cancel_batch
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(batch_id)
    .bind(crate::support::clamp_epoch_ms(at_ms))
    .execute(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// The session's unfinished run, with the head its admission recorded,
/// read on `conn`.
pub(crate) async fn unfinished_run_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<UnfinishedRun>, StoreError> {
    let row: Option<(String, String)> =
        sqlx::query_as(session_runs_sql().runs.select_unfinished.sql())
            .bind(session_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
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
pub(crate) async fn head_ownership_facts_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<lash_core_execution::store::HeadOwnershipFacts, StoreError> {
    let unfinished_run = unfinished_run_conn(conn, session_id)
        .await?
        .map(|unfinished| unfinished.run);
    let row: (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .family
            .pending_session_work_ordering
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_one(crate::observed_sql::executor(&mut *conn))
    .await
    .map_err(store_sqlx_error)?;
    Ok(lash_core_execution::store::HeadOwnershipFacts {
        unfinished_run,
        open_command: row
            .1
            .map(|seq| u64_from_sql("QueuedWorkBatch", "enqueue_seq", seq))
            .transpose()?,
    })
}

/// The turns session `session_id`'s unfinished run executes, if a run is
/// unfinished, read on `conn`.
pub(crate) async fn unfinished_run_turns_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<RunTurns>, StoreError> {
    let Some(unfinished) = unfinished_run_conn(conn, session_id).await? else {
        return Ok(None);
    };
    run_turns_conn(conn, session_id, &unfinished.run)
        .await
        .map(Some)
}

/// `run`'s recorded admission, read on `conn`: `None` for a run with no
/// admission.
pub(crate) async fn run_admission_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Option<RunAdmissionRecord>, StoreError> {
    let json: Option<Option<String>> =
        sqlx::query_scalar(session_runs_sql().runs.select_admission.sql())
            .bind(session_id.as_str())
            .bind(run.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
    json.flatten()
        .map(|json| RunAdmissionRecord::from_stored(&json))
        .transpose()
}

/// The turns `run` executes ([`RunTurns`]), read on `conn`: its own physical
/// turns and the turns its recorded admission's rows were accepted under.
pub(crate) async fn run_turns_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<RunTurns, StoreError> {
    let Some(admission) = run_admission_conn(conn, session_id, run).await? else {
        return Ok(RunTurns::new(run, Vec::new()));
    };
    let sql = crate::turn_ingress::turn_ingress_sql();
    let rows = admission
        .input_ids()
        .iter()
        .map(|input| {
            (
                sql.pending_inputs.select_source_key_by_id.sql(),
                input.to_string(),
            )
        })
        .chain(admission.batch_ids().into_iter().map(|batch| {
            (
                sql.queued_batches.select_source_key_by_id.sql(),
                batch.to_string(),
            )
        }));
    let mut members = Vec::new();
    for (statement, id) in rows {
        let key: Option<Option<String>> = sqlx::query_scalar(statement)
            .bind(session_id.as_str())
            .bind(id)
            .fetch_optional(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
        members.extend(key.flatten());
    }
    Ok(RunTurns::new(run, members))
}

/// The queued-work batches `run`'s recorded admission took, read on `conn`:
/// none for a run with no admission or an input-headed one.
pub(crate) async fn admitted_batches_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    run: &TurnId,
) -> Result<Vec<String>, StoreError> {
    Ok(run_admission_conn(conn, session_id, run)
        .await?
        .map(|admission| admission.batch_ids())
        .unwrap_or_default()
        .into_iter()
        .map(|batch| batch.to_string())
        .collect())
}

/// The run input `input` of `session_id` is bound to, read on `conn`.
pub(crate) async fn run_binding_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<Option<TurnId>, StoreError> {
    sqlx::query_scalar::<_, String>(session_runs_sql().inputs.select_run.sql())
        .bind(session_id.as_str())
        .bind(input.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?
        .map(TurnId::parse)
        .transpose()
        .map_err(StoreError::from)
}

/// Bind `input` to `run` in the caller's transaction, set-if-absent: the
/// commit that applies a checkpoint-admitted input records the run that
/// applied it. A run admission already wrote the same binding for its own
/// inputs, so the insert is a no-op for them.
pub(crate) async fn bind_applied_input_tx(
    conn: &mut PgConnection,
    session_id: &SessionId,
    input: &InputId,
    run: &TurnId,
) -> Result<(), StoreError> {
    sqlx::query(session_runs_sql().inputs.insert.sql())
        .bind(session_id.as_str())
        .bind(input.as_str())
        .bind(run.as_str())
        .execute(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// Bind each of `inputs` to `run`, set-if-absent, and open `run`'s row, in
/// the caller's transaction. A binding to another run refuses the whole
/// write.
pub(crate) async fn bind_run_inputs_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    run: &TurnId,
    inputs: &[InputId],
) -> Result<(), StoreError> {
    for input in inputs {
        if let Some(bound) = run_binding_conn(conn, session_id, input).await?
            && bound != *run
        {
            return Err(run_binding_conflict(session_id, input, &bound, run));
        }
    }
    let sql = session_runs_sql();
    sqlx::query(sql.runs.insert_open.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .execute(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    for input in inputs {
        sqlx::query(sql.inputs.insert.sql())
            .bind(session_id.as_str())
            .bind(input.as_str())
            .bind(run.as_str())
            .execute(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

pub(crate) fn decode_intent(row: &sqlx::postgres::PgRow) -> Result<ControlIntent, StoreError> {
    let id: i64 = row.try_get(0).map_err(store_sqlx_error)?;
    let session_id: String = row.try_get(1).map_err(store_sqlx_error)?;
    let format: i64 = row.try_get(2).map_err(store_sqlx_error)?;
    let kind_json: String = row.try_get(3).map_err(store_sqlx_error)?;
    let state_json: String = row.try_get(4).map_err(store_sqlx_error)?;
    let created_at_ms: i64 = row.try_get(5).map_err(store_sqlx_error)?;
    let corrupt = |field: &str| StoreError::StoredDataCorrupt {
        record_kind: "ControlIntent",
        message: format!("{field} out of range"),
    };
    ControlIntent::from_stored(
        u64_from_sql("ControlIntent", "intent_id", id)?,
        SessionId::parse(session_id)?,
        u32::try_from(format).map_err(|_| corrupt("format"))?,
        &kind_json,
        &state_json,
        u64_from_sql("ControlIntent", "created_at_ms", created_at_ms)?,
    )
}

/// `session_id`'s `close_session` intent, read on `conn`: its deletion
/// tombstone.
pub(crate) async fn close_session_intent_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<ControlIntent>, StoreError> {
    sqlx::query(session_runs_sql().intents.select_close_session.sql())
        .bind(session_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(decode_intent)
        .transpose()
}

/// Intent `id`, read on `conn`.
pub(crate) async fn load_intent_conn(
    conn: &mut PgConnection,
    id: ControlIntentId,
) -> Result<Option<ControlIntent>, StoreError> {
    sqlx::query(session_runs_sql().intents.select_by_id.sql())
        .bind(sql_i64("control intent id", id.sequence())?)
        .fetch_optional(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(decode_intent)
        .transpose()
}

/// Record a new intent of `session_id` in the caller's transaction: `kind`,
/// `Pending`. Answers it with its allocated id.
pub(crate) async fn insert_intent_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    kind: ControlIntentKind,
    at_ms: u64,
) -> Result<ControlIntent, StoreError> {
    let state = ControlIntentState::Pending;
    let (state_code, state_json) = stored_intent_state(&state)?;
    let id: i64 = sqlx::query_scalar(session_runs_sql().intents.insert.sql())
        .bind(session_id.as_str())
        .bind(i64::from(CONTROL_INTENT_FORMAT))
        .bind(kind.code())
        .bind(stored_intent_kind(&kind)?)
        .bind(state_code)
        .bind(state_json)
        .bind(sql_i64("control intent instant", at_ms)?)
        .fetch_one(crate::observed_sql::executor(&mut *conn))
        .await
        .map_err(store_sqlx_error)?;
    let id = ControlIntentId::from_sequence(u64_from_sql("ControlIntent", "intent_id", id)?);
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
/// It takes the session's history-mutation lock first, the lock acceptance
/// takes, so no input is accepted into a session once its close committed.
/// The close names the runs it releases: every run without terminal
/// evidence, each ended `Cancelled` by `SessionDeleted`. It wakes the
/// session actor, whose close steps act on the intent.
///
/// [`ControlIntentStore::begin_session_close`]: lash_core_execution::store::ControlIntentStore::begin_session_close
pub(crate) async fn begin_session_close_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    at_ms: u64,
    chunk_size: usize,
) -> Result<Option<ControlIntent>, StoreError> {
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session_id).await?;
    if let Some(intent) = close_session_intent_conn(tx, session_id).await? {
        return Ok(Some(intent));
    }
    let meta: Option<Option<i64>> = sqlx::query_scalar(
        crate::session_sql::session_sql()
            .meta
            .select_closing_intent
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?;
    if meta.is_none() {
        return Ok(None);
    }
    let sql = session_runs_sql();
    let mut runs = std::collections::BTreeSet::new();
    let open: Vec<String> = sqlx::query_scalar(sql.runs.select_open_runs.sql())
        .bind(session_id.as_str())
        .fetch_all(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    for run in open {
        runs.insert(TurnId::parse(run)?);
    }
    let runs: Vec<TurnId> = runs.into_iter().collect();
    let intent = insert_intent_conn(
        tx,
        session_id,
        ControlIntentKind::CloseSession { runs: runs.clone() },
        at_ms,
    )
    .await?;
    for run in &runs {
        if run_terminal_conn(tx, session_id, run).await?.is_none() {
            write_run_terminal_conn(
                tx,
                &RunTerminal {
                    session_id: session_id.clone(),
                    run: run.clone(),
                    cause: RunTerminalCause::SessionDeleted { intent: intent.id },
                    head_revision: None,
                    at_ms,
                },
                chunk_size,
            )
            .await?;
        }
    }
    let closed = sqlx::query(crate::session_sql::session_sql().meta.begin_close.sql())
        .bind(session_id.as_str())
        .bind(sql_i64("control intent id", intent.id.sequence())?)
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if closed != 1 {
        return Err(StoreError::Contended);
    }
    crate::durable::wake_session_tx(tx, session_id, false, at_ms).await?;
    Ok(Some(intent))
}

/// Forget what session `session_id`'s runs hold, in its deletion's
/// transaction: its runs and its bindings. A `close_session`
/// intent stays: it is the tombstone the factory answers the deleted
/// session's runs from.
pub(crate) async fn delete_session_runs_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let sql = session_runs_sql();
    for statement in [
        sql.runs.delete_by_session.sql(),
        sql.inputs.delete_by_session.sql(),
    ] {
        sqlx::query(statement)
            .bind(session_id.as_str())
            .execute(crate::observed_sql::executor(&mut *conn))
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl RunStore for PostgresStore {
    async fn unfinished_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRun>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        unfinished_run_conn(&mut connection, session_id).await
    }

    async fn admit_at_checkpoint(
        &self,
        request: &lash_core_execution::store::CheckpointAdmissionRequest,
    ) -> Result<lash_core_execution::store::CheckpointAdmission, StoreError> {
        crate::runtime_persistence::admit_at_checkpoint_postgres(self, request).await
    }

    async fn run_terminal(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Option<RunTerminal>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        run_terminal_conn(&mut connection, session_id, run).await
    }

    async fn end_refused_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        refusal: &lash_core_execution::RuntimeError,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = crate::begin_guarded(&mut *connection, &self.fence).await?;
        let end = end_refused_run_tx(
            &mut tx,
            session_id,
            run,
            refusal,
            at_ms,
            self.pools.maintenance.checkpoint_ref_chunk as usize,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(end)
    }

    async fn end_command_run(
        &self,
        session_id: &SessionId,
        run: &TurnId,
        at_ms: u64,
    ) -> Result<RunEndOutcome, StoreError> {
        lash_core_execution::store::validate_session_id(session_id)?;
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = crate::begin_guarded(&mut *connection, &self.fence).await?;
        let end = end_command_run_tx(
            &mut tx,
            session_id,
            run,
            at_ms,
            self.pools.maintenance.checkpoint_ref_chunk as usize,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(end)
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
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        run_binding_conn(&mut connection, session_id, input).await
    }

    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError> {
        let scopes: Vec<String> =
            sqlx::query_scalar(session_runs_sql().inputs.bound_turn_scopes.sql())
                .bind(session_id.as_str())
                .bind(run.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(store_sqlx_error)?;
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
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = crate::begin_guarded(&mut *connection, &self.fence).await?;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        bind_run_inputs_conn(&mut tx, session_id, run, inputs).await?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}
