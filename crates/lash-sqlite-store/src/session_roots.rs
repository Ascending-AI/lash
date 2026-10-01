//! The SQLite half of the logical-root family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_roots` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the root admission, session deletion
//! and the factory's catalog reads share, plus
//! [`RootStore`] for the bound store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    CONTROL_INTENT_FORMAT, ClaimToken, ControlIntent, ControlIntentId, ControlIntentKind,
    ControlIntentState, EnginePark, IntentObligation, IntentSettle, ObligationKey, ObligationState,
    ParkCancelCause, ParkEventKind, RootAdmission, RootEnd, RootStore, RootTerminal,
    RootTerminalCause, RootTerminalWriteDecision, RootTurns, UnfinishedRoot, close_admission,
    decide_root_terminal_write, refused_run_owns_root, root_binding_conflict,
    scope_close_obligation_id, stored_intent_kind, stored_intent_state,
};
use lash_sansio::{InputId, SessionId, TurnId};
use lash_store_sql::session_roots::{
    control_intents::ControlIntentStatements,
    root_inputs::SessionRootInputStatements,
    roots::{RootVerbStatements, SessionRootStatements},
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::conn::TxOutcome;
use crate::schema_layout::Schema;
use crate::{StoreError, sqlite_error, stored_data_corrupt};

/// Every logical-root statement the session catalog issues.
pub(crate) struct SessionRootsSql {
    pub(crate) roots: SessionRootStatements,
    pub(crate) verbs: RootVerbStatements,
    pub(crate) inputs: SessionRootInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_ROOTS_SQL: LazyLock<SessionRootsSql> = LazyLock::new(|| {
    // The root verbs name the turn-input lifecycle.
    let dialect = Schema::Main
        .dialect()
        .with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE);
    SessionRootsSql {
        roots: SessionRootStatements::render(dialect),
        verbs: RootVerbStatements::render(
            dialect.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
        ),
        inputs: SessionRootInputStatements::render(dialect),
        intents: ControlIntentStatements::render(dialect),
    }
});

/// The session catalog's logical-root statements, rendered once at first use.
pub(crate) fn session_roots_sql() -> &'static SessionRootsSql {
    &SESSION_ROOTS_SQL
}

fn stored_u64(record: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| stored_data_corrupt(record, "a negative counter"))
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The stored terminal-evidence row: serialized cause, head revision and
/// the terminal instant, all unset until the root goes terminal.
type TerminalRow = (Option<String>, Option<i64>, Option<i64>);

/// The terminal evidence of `root` in `session_id`, read on `conn`.
pub(crate) fn root_terminal_conn(
    conn: &Connection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Option<RootTerminal>, StoreError> {
    let row: Option<TerminalRow> = conn
        .query_row(
            session_roots_sql().roots.select_terminal.sql(),
            params![session_id.as_str(), root.as_str()],
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
            .and_then(|intent| intent.session_deleted_terminal(root)));
    };
    let (Some(cause_json), Some(at_ms)) = (cause_json, at_ms) else {
        return Ok(None);
    };
    RootTerminal::from_stored(
        session_id.clone(),
        root.clone(),
        &cause_json,
        head_revision
            .map(|revision| stored_u64("RootTerminal", revision))
            .transpose()?,
        stored_u64("RootTerminal", at_ms)?,
    )
    .map(Some)
}

/// Write `terminal` in the caller's transaction, deciding it against the
/// stored evidence first: the same terminal is a no-op, another one is
/// [`StoreError::RootAlreadyTerminal`].
///
/// Every way a root ends goes through here, so here is where it lets go of
/// the rows it still holds (FIG-3927): after whatever settlement its caller
/// wrote, every row still bound to the root is released open at its own
/// position. No row stays bound to a root that has terminal evidence.
pub(crate) fn write_root_terminal_conn(
    tx: &Connection,
    terminal: &RootTerminal,
) -> Result<(), StoreError> {
    let stored = root_terminal_conn(tx, &terminal.session_id, &terminal.root)?;
    if decide_root_terminal_write(stored.as_ref(), terminal)?
        == RootTerminalWriteDecision::AlreadyWritten
    {
        return release_root_rows_conn(tx, &terminal.session_id, &terminal.root, terminal.at_ms);
    }
    let sql = session_roots_sql();
    crate::conn::cached_execute(
        tx,
        sql.roots.insert_open.sql(),
        params![terminal.session_id.as_str(), terminal.root.as_str()],
    )
    .map_err(sqlite_error)?;
    let columns = terminal.to_stored()?;
    let written = crate::conn::cached_execute(
        tx,
        sql.roots.write_terminal.sql(),
        params![
            terminal.session_id.as_str(),
            terminal.root.as_str(),
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
            "root `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.root, terminal.session_id
        )));
    }
    // A terminal root owes its scope close (ADR 0109 §3): the terminal
    // transaction arms the row's obligation, due at the terminal instant.
    crate::obligation_ledger::arm_obligation_id_tx(
        tx,
        &ObligationKey::ScopeClose {
            session_id: terminal.session_id.clone(),
            root: terminal.root.clone(),
        },
        &scope_close_obligation_id(&terminal.session_id, &terminal.root),
        columns.at_ms,
    )?;
    release_root_rows_conn(tx, &terminal.session_id, &terminal.root, terminal.at_ms)
}

/// Release every row of either admission table `root` still holds, in the
/// caller's transaction: accepted input is open again in the state its
/// submitted delivery names, and each row owes its session a drive again.
///
/// Open input addressed to a turn the root ends ([`RootTurns`]: its own
/// physical turns and the turns its admission's members were accepted
/// under) names a turn that will never run again, so the root's disposition
/// applies to it here (FIG-3946): the undelivered disposition of the root's
/// cancellation request if it has one, else `Defer`. `Defer` writes
/// nothing: the row is next-turn input at its own position by rule, its
/// submitted delivery unchanged (ADR 0101 §5.1). `Drop` withdraws it into
/// its tombstone, settling its ingress obligation at the terminal instant
/// `at_ms` (FIG-4098). Either is recorded once on the request's outcome. No
/// open row is bound to a root with terminal evidence.
///
/// An input the root's admission took as its own (`session_root_inputs`)
/// that is still open is unbound from the root too, so a later root can
/// admit it: a terminal root answers nothing more.
fn release_root_rows_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    at_ms: u64,
) -> Result<(), StoreError> {
    let verbs = &session_roots_sql().verbs;
    let own = {
        let mut stmt = tx
            .prepare_cached(verbs.bound_inputs.sql())
            .map_err(sqlite_error)?;
        stmt.query_map(params![session_id.as_str(), root.as_str()], |row| {
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
        sql.pending_inputs.release_root.sql(),
        params![session_id.as_str(), root.as_str()],
    )
    .map_err(sqlite_error)?;
    crate::conn::cached_execute(
        tx,
        sql.queued_batches.release_root.sql(),
        params![session_id.as_str(), root.as_str()],
    )
    .map_err(sqlite_error)?;
    let ended = RootTurns::new(root, root_admission_conn(tx, session_id, root)?.as_ref());
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
            addressed.push((row.input_id, row.input_json));
        }
    }
    if addressed.is_empty() {
        return Ok(());
    }
    let request =
        crate::persistence::turn_cancel::load_turn_cancel_request_conn(tx, session_id, root)?;
    let disposition = request.as_ref().map_or(
        lash_core_execution::TurnCancelUndeliveredInputPolicy::Defer,
        |record| record.request.undelivered,
    );
    for (input_id, input_json) in addressed {
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
        if request.is_some() {
            crate::persistence::turn_cancel::append_turn_cancel_outcome_conn(
                tx,
                session_id,
                root,
                lash_core_execution::TurnCancelAffectedInput {
                    input_id: input_id.into(),
                    payload: crate::persistence::turn_cancel::decode_stored_json(
                        &input_json,
                        "turn input",
                    )?,
                    disposition,
                },
            )?;
        }
    }
    Ok(())
}

/// The engine proved the root's execution is lost (`loss`). This
/// transaction makes its inputs and root terminal together, so the existing
/// scope-close obligation takes over before recovery acknowledges the loss.
/// A root that already has terminal evidence, or no row, is left as it is,
/// and so is a root the engine holds no run of that never recorded its
/// admission: it started nothing, and its ingress obligation still owns its
/// input.
pub(crate) fn end_lost_root_conn(
    tx: &Connection,
    target: &lash_core_execution::engine::RootRef,
    loss: lash_core_execution::engine::RootRunLoss,
    at_ms: u64,
) -> Result<Option<RootTerminal>, StoreError> {
    match unanswered_root_conn(tx, target)? {
        UnansweredRoot::Open => {
            if loss == lash_core_execution::engine::RootRunLoss::NoRun
                && root_admission_conn(tx, &target.session, &target.root)?.is_none()
            {
                return Ok(None);
            }
            write_unanswered_root_end_conn(tx, target, at_ms, |cancelled_by| {
                RootTerminalCause::SubstrateLost { cancelled_by }
            })
            .map(Some)
        }
        UnansweredRoot::Ended(_) | UnansweredRoot::Unknown => Ok(None),
    }
}

/// The root's run under `fence` met a typed refusal no retry can change
/// (FIG-4018): the same transaction as a lost root's, ending it with the
/// refusal, once the run is shown to still own the root (FIG-4200).
pub(crate) fn end_refused_root_conn(
    tx: &Connection,
    fence: &lash_core_execution::store::DriveFence,
    root: &TurnId,
    refusal: &lash_core_execution::RuntimeError,
    at_ms: u64,
) -> Result<RootEnd, StoreError> {
    let target = lash_core_execution::engine::RootRef {
        session: fence.session().clone(),
        root: root.clone(),
    };
    match unanswered_root_conn(tx, &target)? {
        UnansweredRoot::Ended(terminal) => Ok(RootEnd::AlreadyEnded(*terminal)),
        UnansweredRoot::Unknown => Ok(RootEnd::Unknown),
        UnansweredRoot::Open => {
            let current = crate::persistence::drive_epoch_conn(tx, &target.session)?;
            if !refused_run_owns_root(&target.session, fence, &current)? {
                return Ok(RootEnd::Superseded);
            }
            write_unanswered_root_end_conn(tx, &target, at_ms, |_| RootTerminalCause::Refused {
                code: refusal.code.clone(),
                message: refusal.message.clone(),
                refusal_cause: refusal.cause.clone(),
            })
            .map(RootEnd::Ended)
        }
    }
}

/// The command root whose run under `fence` applied the session's command
/// lane until it was empty ends (FIG-4202): its row opens and its terminal
/// is written in one transaction, arming its scope close, once the run is
/// shown to still own the session's drive epoch.
pub(crate) fn end_command_root_conn(
    tx: &Connection,
    fence: &lash_core_execution::store::DriveFence,
    root: &TurnId,
    at_ms: u64,
) -> Result<RootEnd, StoreError> {
    let session = fence.session();
    if let Some(terminal) = root_terminal_conn(tx, session, root)? {
        return Ok(RootEnd::AlreadyEnded(terminal));
    }
    let current = crate::persistence::drive_epoch_conn(tx, session)?;
    if !refused_run_owns_root(session, fence, &current)? {
        return Ok(RootEnd::Superseded);
    }
    let terminal = RootTerminal {
        session_id: session.clone(),
        root: root.clone(),
        cause: RootTerminalCause::CommandsApplied,
        head_revision: None,
        at_ms,
    };
    write_root_terminal_conn(tx, &terminal)?;
    Ok(RootEnd::Ended(terminal))
}

/// Where a root no commit answered stands.
enum UnansweredRoot {
    /// It has terminal evidence.
    Ended(Box<RootTerminal>),
    /// The store holds no row for it.
    Unknown,
    /// It is admitted and has not ended.
    Open,
}

fn unanswered_root_conn(
    tx: &Connection,
    target: &lash_core_execution::engine::RootRef,
) -> Result<UnansweredRoot, StoreError> {
    let session = &target.session;
    let root = &target.root;
    if let Some(terminal) = root_terminal_conn(tx, session, root)? {
        return Ok(UnansweredRoot::Ended(Box::new(terminal)));
    }
    let exists: bool = tx
        .query_row(
            session_roots_sql().roots.select_terminal.sql(),
            params![session.as_str(), root.as_str()],
            |_| Ok(true),
        )
        .optional()
        .map_err(sqlite_error)?
        .unwrap_or(false);
    Ok(if exists {
        UnansweredRoot::Open
    } else {
        UnansweredRoot::Unknown
    })
}

/// End an open root no commit answered, with the cause `cause` makes of the
/// root's recorded cancellation request, if any.
fn write_unanswered_root_end_conn(
    tx: &Connection,
    target: &lash_core_execution::engine::RootRef,
    at_ms: u64,
    cause: impl FnOnce(Option<String>) -> RootTerminalCause,
) -> Result<RootTerminal, StoreError> {
    let session = &target.session;
    let root = &target.root;
    let record = crate::persistence::turn_cancel::load_turn_cancel_request_conn(tx, session, root)?;
    let cause = cause(
        record
            .as_ref()
            .map(|record| record.request.request_id.clone()),
    );
    let terminal = RootTerminal {
        session_id: session.clone(),
        root: root.clone(),
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

    // The root's own input is dropped and its batches cancelled first; the
    // terminal write then releases whatever else the root still held.
    let sql = &session_roots_sql().verbs;
    let mut inputs = {
        let mut stmt = tx
            .prepare_cached(sql.bound_inputs.sql())
            .map_err(sqlite_error)?;
        stmt.query_map(params![session.as_str(), root.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?
    };
    let batches = admitted_batches_conn(tx, session, root)?;
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
        cancel_root_batch_conn(tx, session, &batch, at_ms)?;
    }
    // The terminal write applies the root's cancel request's `undelivered`
    // disposition to open input addressed to a turn the root ends
    // (FIG-3927 §2.4, FIG-3946).
    write_root_terminal_conn(tx, &terminal)?;
    Ok(terminal)
}

/// Decode a root's recorded admission (`session_roots.admission_json`).
pub(crate) fn decode_root_admission(json: &str) -> Result<RootAdmission, StoreError> {
    serde_json::from_str(json).map_err(|error| stored_data_corrupt("RootAdmission", error))
}

/// The session's unfinished root, with the head its admission recorded,
/// read on `conn`.
pub(crate) fn unfinished_root_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<UnfinishedRoot>, StoreError> {
    let row: Option<(String, String)> = conn
        .query_row(
            session_roots_sql().roots.select_unfinished.sql(),
            [session_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    row.map(|(root, json)| {
        Ok(UnfinishedRoot {
            root: TurnId::from(root),
            head: decode_root_admission(&json)?.head,
        })
    })
    .transpose()
}

/// What owns session `session_id`'s head, read in a head commit's
/// transaction (FIG-4202): its unfinished root, the follow-on its head owes
/// (`owed_follow_on`, read with the head) and its earliest open session
/// command.
pub(crate) fn head_ownership_facts_conn(
    conn: &Connection,
    session_id: &SessionId,
    owed_follow_on: Option<TurnId>,
) -> Result<lash_core_execution::store::HeadOwnershipFacts, StoreError> {
    let unfinished_root = unfinished_root_conn(conn, session_id)?.map(|unfinished| unfinished.root);
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
        unfinished_root,
        owed_follow_on,
        open_command: open_command
            .map(|seq| stored_u64("QueuedWorkBatch", seq))
            .transpose()?,
    })
}

/// `root`'s recorded admission, read on `conn`: `None` for a root with no
/// admission.
fn root_admission_conn(
    conn: &Connection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Option<RootAdmission>, StoreError> {
    let json: Option<Option<String>> = conn
        .query_row(
            session_roots_sql().roots.select_admission.sql(),
            params![session_id.as_str(), root.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.flatten()
        .map(|json| decode_root_admission(&json))
        .transpose()
}

/// The queued-work batches `root`'s recorded admission took, read on `conn`:
/// none for a root with no admission or an input-headed one.
/// Cancel batch `batch_id` of session `session_id`, held by a root a verb
/// ends, into its `cancelled` tombstone at `at_ms` (ADR 0101 §8). A wake's
/// cancellation is its terminal transition, so its receiver floor rises in
/// the same write (ADR 0101 §9).
pub(crate) fn cancel_root_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    batch_id: &str,
    at_ms: u64,
) -> Result<(), StoreError> {
    if let Some(batch) = crate::queued_work::load_queued_batch_by_id_conn(tx, batch_id)?
        && batch.terminal.is_none()
        && let Some(wake) = lash_core_execution::store::TerminalProcessWake::of_batch(&batch)
    {
        crate::queued_work::raise_wake_redelivery_fence_conn(tx, session_id, &wake)?;
    }
    crate::conn::cached_execute(
        tx,
        session_roots_sql().verbs.cancel_batch.sql(),
        params![session_id.as_str(), batch_id, crate::clamp_epoch_ms(at_ms)],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// The turns session `session_id`'s unfinished root runs, if a root is
/// unfinished, read on `conn`.
pub(crate) fn unfinished_root_turns_conn(
    conn: &Connection,
    session_id: &SessionId,
) -> Result<Option<RootTurns>, StoreError> {
    let Some(unfinished) = unfinished_root_conn(conn, session_id)? else {
        return Ok(None);
    };
    let admission = root_admission_conn(conn, session_id, &unfinished.root)?;
    Ok(Some(RootTurns::new(&unfinished.root, admission.as_ref())))
}

pub(crate) fn admitted_batches_conn(
    conn: &Connection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Vec<String>, StoreError> {
    let Some(admission) = root_admission_conn(conn, session_id, root)? else {
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
/// at head revision `head_revision`: the commit of a root's final physical
/// turn ends the root (FIG-3600 S7).
pub(crate) fn write_commit_root_terminal_conn(
    tx: &Connection,
    commit: &lash_core_execution::store::RuntimeCommit,
    head_revision: u64,
    at_ms: u64,
) -> Result<(), StoreError> {
    match commit.root_terminal.as_deref().cloned() {
        Some(write) => write_root_terminal_conn(
            tx,
            &write.into_terminal(commit.session_id.clone(), head_revision, at_ms),
        ),
        None => Ok(()),
    }
}

/// The root input `input` of `session_id` is bound to, read on `conn`.
pub(crate) fn root_binding_conn(
    conn: &Connection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<Option<TurnId>, StoreError> {
    conn.query_row(
        session_roots_sql().inputs.select_root.sql(),
        params![session_id.as_str(), input.as_str()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(sqlite_error)
    .map(|root| root.map(TurnId::from))
}

/// Bind `input` to `root` in the caller's transaction, set-if-absent: the
/// commit that applies a checkpoint-admitted input records the root that
/// applied it. A root admission already wrote the same binding for its own
/// inputs, so the insert is a no-op for them.
pub(crate) fn bind_applied_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    input: &InputId,
    root: &TurnId,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        session_roots_sql().inputs.insert.sql(),
        params![session_id.as_str(), input.as_str(), root.as_str()],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Bind each of `inputs` to `root`, set-if-absent, and open `root`'s row, in
/// the caller's transaction. A binding to another root refuses the whole
/// write.
pub(crate) fn bind_root_inputs_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    inputs: &[InputId],
) -> Result<(), StoreError> {
    let sql = session_roots_sql();
    for input in inputs {
        if let Some(bound) = root_binding_conn(tx, session_id, input)?
            && bound != *root
        {
            return Err(root_binding_conflict(session_id, input, &bound, root));
        }
    }
    crate::conn::cached_execute(
        tx,
        sql.roots.insert_open.sql(),
        params![session_id.as_str(), root.as_str()],
    )
    .map_err(sqlite_error)?;
    for input in inputs {
        crate::conn::cached_execute(
            tx,
            sql.inputs.insert.sql(),
            params![session_id.as_str(), input.as_str(), root.as_str()],
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
            SessionId::from(self.session_id),
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
        session_roots_sql().intents.select_close_session.sql(),
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
        session_roots_sql().intents.select_by_id.sql(),
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
            session_roots_sql()
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
            session_roots_sql().intents.insert.sql(),
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
        session_roots_sql().intents.update_state.sql(),
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
    let sql = session_roots_sql();
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
/// The close names the roots it releases: every root without terminal
/// evidence (its logical-root rows, its parked root, its pending queued
/// run), each ended `Cancelled` by `SessionDeleted`, plus the roots of the
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
    let sql = session_roots_sql();
    let mut roots = std::collections::BTreeSet::new();
    {
        let mut statement = tx
            .prepare_cached(sql.roots.select_open_roots.sql())
            .map_err(sqlite_error)?;
        let open = statement
            .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        roots.extend(open.into_iter().map(TurnId::from));
    }
    // The parked root is released with the park, whose feed event outlives
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
    if let Some((parked_root, park_id)) = released {
        crate::persistence::turn_park_feed::log_turn_park_closed_conn(
            tx,
            session_id,
            &parked_root,
            park_id,
            &ParkEventKind::Cancelled {
                cause: ParkCancelCause::SessionDeleted,
            },
            crate::clamp_epoch_ms(at_ms),
        )?;
        roots.insert(TurnId::from(parked_root));
    }
    let verbs = open_verbs_by_session_conn(tx, session_id)?;
    for verb in &verbs {
        match &verb.kind {
            ControlIntentKind::Redrive { root, .. }
            | ControlIntentKind::Cancel { root, .. }
            | ControlIntentKind::Fork { root, .. } => {
                roots.insert(root.clone());
            }
            ControlIntentKind::CloseSession { .. } => {}
        }
    }
    let roots: Vec<TurnId> = roots.into_iter().collect();
    let intent = insert_intent_conn(
        tx,
        session_id,
        ControlIntentKind::CloseSession {
            roots: roots.clone(),
        },
        None,
        at_ms,
    )?;
    for root in &roots {
        if root_terminal_conn(tx, session_id, root)?.is_none() {
            write_root_terminal_conn(
                tx,
                &RootTerminal {
                    session_id: session_id.clone(),
                    root: root.clone(),
                    cause: RootTerminalCause::SessionDeleted { intent: intent.id },
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

/// Forget what session `session_id`'s roots hold, in its deletion's
/// transaction: its roots, its bindings and its verbs. A `close_session`
/// intent stays: it is the tombstone the factory answers the deleted
/// session's roots from.
pub(crate) fn delete_session_roots_conn(
    tx: &Connection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let sql = session_roots_sql();
    for statement in [
        sql.roots.delete_by_session.sql(),
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
impl RootStore for crate::SqliteStore {
    async fn unfinished_root(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRoot>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| Ok(unfinished_root_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_root(
        &self,
        request: &lash_core_execution::store::AdmitRootRequest,
    ) -> Result<Option<RootAdmission>, StoreError> {
        crate::persistence::admit_root_sqlite(self, request).await
    }

    async fn admit_at_checkpoint(
        &self,
        request: &lash_core_execution::store::CheckpointAdmissionRequest,
    ) -> Result<lash_core_execution::store::CheckpointAdmission, StoreError> {
        crate::persistence::admit_at_checkpoint_sqlite(self, request).await
    }

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Option<RootTerminal>, StoreError> {
        let session_id = session_id.clone();
        let root = root.clone();
        self.conn
            .call(move |conn| Ok(root_terminal_conn(conn, &session_id, &root)))
            .await
            .map_err(sqlite_error)?
    }

    async fn end_refused_root(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        root: &TurnId,
        refusal: &lash_core_execution::RuntimeError,
        at_ms: u64,
    ) -> Result<RootEnd, StoreError> {
        lash_core_execution::store::validate_session_id(fence.session())?;
        let fence = fence.clone();
        let root = root.clone();
        let refusal = refusal.clone();
        self.conn
            .write_flow(move |tx| commit(end_refused_root_conn(tx, &fence, &root, &refusal, at_ms)))
            .await
            .map_err(sqlite_error)?
    }

    async fn end_command_root(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        root: &TurnId,
        at_ms: u64,
    ) -> Result<RootEnd, StoreError> {
        lash_core_execution::store::validate_session_id(fence.session())?;
        let fence = fence.clone();
        let root = root.clone();
        self.conn
            .write_flow(move |tx| commit(end_command_root_conn(tx, &fence, &root, at_ms)))
            .await
            .map_err(sqlite_error)?
    }

    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        // An input's root is its binding: the admission that took it, the
        // commit whose checkpoint delivery applied it, or the fork that
        // rebound it.
        self.root_binding(session_id, input).await
    }

    async fn root_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError> {
        let session_id = session_id.clone();
        let input = input.clone();
        self.conn
            .call(move |conn| Ok(root_binding_conn(conn, &session_id, &input)))
            .await
            .map_err(sqlite_error)?
    }

    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError> {
        let session_id = session_id.clone();
        let root = root.clone();
        let scopes = self
            .conn
            .call(move |conn| {
                let mut stmt =
                    conn.prepare_cached(session_roots_sql().inputs.bound_turn_scopes.sql())?;
                stmt.query_map(params![session_id.as_str(), root.as_str()], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        Ok(scopes.into_iter().map(TurnId::from).collect())
    }

    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        let root = root.clone();
        let inputs = inputs.to_vec();
        self.conn
            .write_flow(move |tx| {
                commit((|| {
                    crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                    bind_root_inputs_conn(tx, &session_id, &root, &inputs)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}
