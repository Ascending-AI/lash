//! The PostgreSQL half of the logical-root family (FIG-3600 S7).
//!
//! `lash-store-sql`'s `session_roots` module owns every statement: the family
//! forks nothing. This module renders them once and holds the in-transaction
//! reads and writes the commit path, the queued-run settlement, the
//! admission step, session deletion and the factory's catalog reads share, plus
//! [`RootStore`] for the session store.

use std::sync::LazyLock;

use lash_core_execution::store::{
    CONTROL_INTENT_FORMAT, ClaimToken, ControlIntent, ControlIntentId, ControlIntentKind,
    ControlIntentState, EnginePark, IntentObligation, IntentSettle, ObligationKey, ObligationState,
    ParkCancelCause, ParkEventKind, RootAdmission, RootEnd, RootStore, RootTerminal,
    RootTerminalCause, RootTerminalWriteDecision, RootTurns, UnfinishedRoot, close_admission,
    decide_root_terminal_write, refused_run_owns_root, root_binding_conflict, stored_intent_kind,
    stored_intent_state,
};
use lash_sansio::{InputId, SessionId, TurnId};
use lash_store_sql::Dialect;
use lash_store_sql::session_roots::{
    control_intents::ControlIntentStatements,
    root_inputs::SessionRootInputStatements,
    roots::{RootVerbStatements, SessionRootStatements},
};
use sqlx::{PgConnection, Row};

use crate::support::{store_sqlx_error, u64_from_sql};
use crate::{PostgresStore, StoreError, acquire_runtime_connection};

/// Every logical-root statement this store issues.
pub(crate) struct SessionRootsSql {
    pub(crate) roots: SessionRootStatements,
    pub(crate) verbs: RootVerbStatements,
    pub(crate) inputs: SessionRootInputStatements,
    pub(crate) intents: ControlIntentStatements,
}

static SESSION_ROOTS_SQL: LazyLock<SessionRootsSql> = LazyLock::new(|| {
    // The admission read names the head input's lifecycle (FIG-3840).
    let dialect = Dialect::postgres().with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE);
    SessionRootsSql {
        roots: SessionRootStatements::render(dialect),
        verbs: RootVerbStatements::render(
            dialect.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
        ),
        inputs: SessionRootInputStatements::render(dialect),
        intents: ControlIntentStatements::render(dialect),
    }
});

/// This store's logical-root statements, rendered once at first use.
pub(crate) fn session_roots_sql() -> &'static SessionRootsSql {
    &SESSION_ROOTS_SQL
}

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

/// The terminal evidence of `root` in `session_id`, read on `conn`.
pub(crate) async fn root_terminal_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Option<RootTerminal>, StoreError> {
    let row = sqlx::query(session_roots_sql().roots.select_terminal.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .fetch_optional(&mut *conn)
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
        .fetch_one(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
        if !deleted {
            return Ok(None);
        }
        return Ok(close_session_intent_conn(conn, session_id)
            .await?
            .and_then(|intent| intent.session_deleted_terminal(root)));
    };
    let cause_json: Option<String> = row.try_get(0).map_err(store_sqlx_error)?;
    let head_revision: Option<i64> = row.try_get(1).map_err(store_sqlx_error)?;
    let at_ms: Option<i64> = row.try_get(2).map_err(store_sqlx_error)?;
    let (Some(cause_json), Some(at_ms)) = (cause_json, at_ms) else {
        return Ok(None);
    };
    RootTerminal::from_stored(
        session_id.clone(),
        root.clone(),
        &cause_json,
        head_revision
            .map(|revision| u64_from_sql("RootTerminal", "terminal_head_revision", revision))
            .transpose()?,
        u64_from_sql("RootTerminal", "terminal_at_ms", at_ms)?,
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
///
/// Here too is where the root's park ends (FIG-4780): a root with terminal
/// evidence holds no park, whatever kind of root it is, so the write that
/// ends the root clears its park and appends the feed event its cause names.
pub(crate) async fn write_root_terminal_conn(
    conn: &mut PgConnection,
    terminal: &RootTerminal,
) -> Result<(), StoreError> {
    let stored = root_terminal_conn(conn, &terminal.session_id, &terminal.root).await?;
    if decide_root_terminal_write(stored.as_ref(), terminal)?
        == RootTerminalWriteDecision::AlreadyWritten
    {
        return release_root_rows_conn(conn, &terminal.session_id, &terminal.root, terminal.at_ms)
            .await;
    }
    let sql = session_roots_sql();
    sqlx::query(sql.roots.insert_open.sql())
        .bind(terminal.session_id.as_str())
        .bind(terminal.root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    let columns = terminal.to_stored()?;
    let written = sqlx::query(sql.roots.write_terminal.sql())
        .bind(terminal.session_id.as_str())
        .bind(terminal.root.as_str())
        .bind(columns.kind)
        .bind(columns.cause_json)
        .bind(
            columns
                .head_revision
                .map(|revision| sql_i64("terminal head revision", revision))
                .transpose()?,
        )
        .bind(sql_i64("terminal instant", columns.at_ms)?)
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if written != 1 {
        return Err(StoreError::Backend(format!(
            "root `{}` of session `{}` gained terminal evidence inside its own write",
            terminal.root, terminal.session_id
        )));
    }
    // A terminal root owes its scope close (ADR 0109 §3): the terminal
    // transaction arms the row's obligation, due at the terminal instant.
    crate::obligation_ledger::arm_obligation_tx(
        conn,
        &lash_core_execution::store::ObligationKey::ScopeClose {
            session_id: terminal.session_id.clone(),
            root: terminal.root.clone(),
        },
        columns.at_ms,
    )
    .await?;
    crate::runtime_persistence::turn_park::end_root_park_conn(
        conn,
        &terminal.session_id,
        &terminal.root,
        &terminal.cause,
        terminal.at_ms,
    )
    .await?;
    release_root_rows_conn(conn, &terminal.session_id, &terminal.root, terminal.at_ms).await
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
async fn release_root_rows_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
    at_ms: u64,
) -> Result<(), StoreError> {
    let verbs = &session_roots_sql().verbs;
    let own: Vec<String> = sqlx::query_scalar(verbs.bound_inputs.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .fetch_all(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    for input in own {
        sqlx::query(verbs.unbind.sql())
            .bind(session_id.as_str())
            .bind(&input)
            .execute(&mut *conn)
            .await
            .map_err(store_sqlx_error)?;
    }
    let sql = crate::turn_ingress::turn_ingress_sql();
    sqlx::query(sql.pending_inputs.release_root.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(sql.queued_batches.release_root.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    let ended = RootTurns::new(
        root,
        root_admission_conn(conn, session_id, root).await?.as_ref(),
    );
    let open_rows = sqlx::query(sql.pending_inputs_postgres.select_pending_active.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut *conn)
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
        .bind(root.as_str())
        .fetch_optional(&mut *conn)
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
                .execute(&mut *conn)
                .await
                .map_err(store_sqlx_error)?;
        }
        if request.is_some() {
            crate::runtime_persistence::turn_cancel::append_turn_cancel_outcome_conn(
                conn,
                session_id,
                root,
                lash_core_execution::TurnCancelAffectedInput {
                    input_id: input.input_id,
                    payload: input.input,
                    disposition,
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// Store half of recovery after the engine proves a root's execution is lost
/// (`loss`). The terminal, ingress settlement and scope-close arm commit
/// together under the session history lock. A root that already has
/// terminal evidence, or no row, is left as it is, and so is a root the
/// engine holds no run of that never recorded its admission: it started
/// nothing, and its ingress obligation still owns its input.
pub(crate) async fn end_lost_root_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &lash_core_execution::engine::RootRef,
    loss: lash_core_execution::engine::RootRunLoss,
    at_ms: u64,
) -> Result<Option<RootTerminal>, StoreError> {
    match unanswered_root_tx(tx, target).await? {
        UnansweredRoot::Open => {
            if loss == lash_core_execution::engine::RootRunLoss::NoRun
                && root_admission_conn(tx, &target.session, &target.root)
                    .await?
                    .is_none()
            {
                return Ok(None);
            }
            write_unanswered_root_end_tx(tx, target, at_ms, |cancelled_by| {
                RootTerminalCause::SubstrateLost { cancelled_by }
            })
            .await
            .map(Some)
        }
        UnansweredRoot::Ended(_) | UnansweredRoot::Unknown => Ok(None),
    }
}

/// The root's run under `fence` met a typed refusal no retry can change
/// (FIG-4018): the same transaction as a lost root's, ending it with the
/// refusal, once the run is shown to still own the root (FIG-4200).
pub(crate) async fn end_refused_root_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &lash_core_execution::store::DriveFence,
    root: &TurnId,
    refusal: &lash_core_execution::RuntimeError,
    at_ms: u64,
) -> Result<RootEnd, StoreError> {
    let target = lash_core_execution::engine::RootRef {
        session: fence.session().clone(),
        root: root.clone(),
    };
    // The drive epoch's row lock is taken before the session history lock,
    // in the order a fenced commit takes them, so the two never deadlock. A
    // session whose row is gone holds no open root.
    let current =
        match crate::runtime_persistence::drive_epoch::drive_epoch_locked_tx(tx, &target.session)
            .await
        {
            Ok(current) => Some(current),
            Err(StoreError::DriveEpochUnavailable { .. }) => None,
            Err(error) => return Err(error),
        };
    match unanswered_root_tx(tx, &target).await? {
        UnansweredRoot::Ended(terminal) => Ok(RootEnd::AlreadyEnded(*terminal)),
        UnansweredRoot::Unknown => Ok(RootEnd::Unknown),
        UnansweredRoot::Open => {
            let current = current.ok_or_else(|| StoreError::DriveEpochUnavailable {
                session_id: target.session.clone(),
            })?;
            if !refused_run_owns_root(&target.session, fence, &current)? {
                return Ok(RootEnd::Superseded);
            }
            write_unanswered_root_end_tx(tx, &target, at_ms, |_| RootTerminalCause::Refused {
                code: refusal.code.clone(),
                message: refusal.message.clone(),
                refusal_cause: refusal.cause.clone(),
            })
            .await
            .map(RootEnd::Ended)
        }
    }
}

/// The command root whose run under `fence` applied the session's command
/// lane until it was empty ends (FIG-4202): its row opens and its terminal
/// is written in one transaction, arming its scope close, once the run is
/// shown to still own the session's drive epoch.
pub(crate) async fn end_command_root_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &lash_core_execution::store::DriveFence,
    root: &TurnId,
    at_ms: u64,
) -> Result<RootEnd, StoreError> {
    let session = fence.session();
    // The drive epoch's row lock first, in the order a fenced commit takes
    // it (see `end_refused_root_tx`).
    let current =
        crate::runtime_persistence::drive_epoch::drive_epoch_locked_tx(tx, session).await?;
    if let Some(terminal) = root_terminal_conn(&mut *tx, session, root).await? {
        return Ok(RootEnd::AlreadyEnded(terminal));
    }
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
    write_root_terminal_conn(&mut *tx, &terminal).await?;
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

/// Where `target` stands, read under the session history lock.
async fn unanswered_root_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &lash_core_execution::engine::RootRef,
) -> Result<UnansweredRoot, StoreError> {
    let session = &target.session;
    let root = &target.root;
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session).await?;
    if let Some(terminal) = root_terminal_conn(&mut *tx, session, root).await? {
        return Ok(UnansweredRoot::Ended(Box::new(terminal)));
    }
    Ok(
        if sqlx::query(session_roots_sql().roots.select_terminal.sql())
            .bind(session.as_str())
            .bind(root.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .is_some()
        {
            UnansweredRoot::Open
        } else {
            UnansweredRoot::Unknown
        },
    )
}

/// End an open root no commit answered, with the cause `cause` makes of the
/// root's recorded cancellation request, if any.
async fn write_unanswered_root_end_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    target: &lash_core_execution::engine::RootRef,
    at_ms: u64,
    cause: impl FnOnce(Option<String>) -> RootTerminalCause,
) -> Result<RootTerminal, StoreError> {
    let session = &target.session;
    let root = &target.root;
    // `select_request` yields request_id, origin, reason, disposition, mode.
    let request: Option<
        lash_core_execution::store_backend_support::turn_cancel::TurnCancelRequestRow,
    > = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .cancel_requests
            .select_request
            .sql(),
    )
    .bind(session.as_str())
    .bind(root.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let cause = cause(request.map(|row| row.0));
    let terminal = RootTerminal {
        session_id: session.clone(),
        root: root.clone(),
        cause,
        head_revision: None,
        at_ms,
    };
    sqlx::query(
        crate::session_sql::session_sql()
            .head
            .clear_pending_follow_on
            .sql(),
    )
    .bind(session.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;

    // The root's own input is dropped and its batches cancelled first; the
    // terminal write then releases whatever else the root still held.
    let sql = &session_roots_sql().verbs;
    let mut inputs: Vec<String> = sqlx::query_scalar(sql.bound_inputs.sql())
        .bind(session.as_str())
        .bind(root.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let batches = admitted_batches_conn(tx, session, root).await?;
    inputs.sort();
    inputs.dedup();
    for input in inputs {
        sqlx::query(sql.cancel_input.sql())
            .bind(session.as_str())
            .bind(input)
            .bind(crate::support::clamp_epoch_ms(at_ms))
            .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    for batch in batches {
        cancel_root_batch_tx(tx, session, &batch, at_ms).await?;
    }
    // The terminal write applies the root's cancel request's `undelivered`
    // disposition to open input addressed to a turn the root ends
    // (FIG-3927 §2.4, FIG-3946).
    write_root_terminal_conn(&mut *tx, &terminal).await?;
    Ok(terminal)
}

/// Cancel batch `batch_id` of session `session_id`, held by a root a verb
/// ends, into its `cancelled` tombstone at `at_ms` (ADR 0101 §8). A wake's
/// cancellation is its terminal transition, so its receiver floor rises in
/// the same transaction (ADR 0101 §9).
pub(crate) async fn cancel_root_batch_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    batch_id: &str,
    at_ms: u64,
) -> Result<(), StoreError> {
    if let Some(batch) = crate::queued_work::load_queued_batch(tx, batch_id).await?
        && batch.terminal.is_none()
        && let Some(wake) = lash_core_execution::store::TerminalProcessWake::of_batch(&batch)
    {
        crate::runtime_persistence::raise_wake_redelivery_fence_tx(tx, session_id, &wake).await?;
    }
    sqlx::query(session_roots_sql().verbs.cancel_batch.sql())
        .bind(session_id.as_str())
        .bind(batch_id)
        .bind(crate::support::clamp_epoch_ms(at_ms))
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// Decode a root's recorded admission (`session_roots.admission_json`).
pub(crate) fn decode_root_admission(json: &str) -> Result<RootAdmission, StoreError> {
    serde_json::from_str(json).map_err(|error| StoreError::StoredDataCorrupt {
        record_kind: "RootAdmission",
        message: error.to_string(),
    })
}

/// The session's unfinished root, with the head its admission recorded,
/// read on `conn`.
pub(crate) async fn unfinished_root_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<UnfinishedRoot>, StoreError> {
    let row: Option<(String, String)> =
        sqlx::query_as(session_roots_sql().roots.select_unfinished.sql())
            .bind(session_id.as_str())
            .fetch_optional(&mut *conn)
            .await
            .map_err(store_sqlx_error)?;
    row.map(|(root, json)| {
        let admission = decode_root_admission(&json)?;
        Ok(UnfinishedRoot {
            root: TurnId::from(root),
            head: admission.head,
            executor: admission.executor,
        })
    })
    .transpose()
}

/// What owns session `session_id`'s head, read in a head commit's
/// transaction (FIG-4202): its unfinished root, the follow-on its head owes
/// (`owed_follow_on`, read with the head) and its earliest open session
/// command.
pub(crate) async fn head_ownership_facts_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    owed_follow_on: Option<TurnId>,
) -> Result<lash_core_execution::store::HeadOwnershipFacts, StoreError> {
    let unfinished_root = unfinished_root_conn(conn, session_id)
        .await?
        .map(|unfinished| unfinished.root);
    let row: (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .family
            .pending_session_work_ordering
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(lash_core_execution::QueuedWorkKind::Control.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(store_sqlx_error)?;
    Ok(lash_core_execution::store::HeadOwnershipFacts {
        unfinished_root,
        owed_follow_on,
        open_command: row
            .1
            .map(|seq| u64_from_sql("QueuedWorkBatch", "enqueue_seq", seq))
            .transpose()?,
    })
}

/// The turns session `session_id`'s unfinished root runs, if a root is
/// unfinished, read on `conn`.
pub(crate) async fn unfinished_root_turns_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<RootTurns>, StoreError> {
    let Some(unfinished) = unfinished_root_conn(conn, session_id).await? else {
        return Ok(None);
    };
    let admission = root_admission_conn(conn, session_id, &unfinished.root).await?;
    Ok(Some(RootTurns::new(&unfinished.root, admission.as_ref())))
}

/// `root`'s recorded admission, read on `conn`: `None` for a root with no
/// admission.
async fn root_admission_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Option<RootAdmission>, StoreError> {
    let json: Option<Option<String>> =
        sqlx::query_scalar(session_roots_sql().roots.select_admission.sql())
            .bind(session_id.as_str())
            .bind(root.as_str())
            .fetch_optional(&mut *conn)
            .await
            .map_err(store_sqlx_error)?;
    json.flatten()
        .map(|json| decode_root_admission(&json))
        .transpose()
}

/// The queued-work batches `root`'s recorded admission took, read on `conn`:
/// none for a root with no admission or an input-headed one.
pub(crate) async fn admitted_batches_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
) -> Result<Vec<String>, StoreError> {
    let Some(admission) = root_admission_conn(conn, session_id, root).await? else {
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

/// The root input `input` of `session_id` is bound to, read on `conn`.
pub(crate) async fn root_binding_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    input: &InputId,
) -> Result<Option<TurnId>, StoreError> {
    sqlx::query_scalar::<_, String>(session_roots_sql().inputs.select_root.sql())
        .bind(session_id.as_str())
        .bind(input.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)
        .map(|root| root.map(TurnId::from))
}

/// Bind `input` to `root` in the caller's transaction, set-if-absent: the
/// commit that applies a checkpoint-admitted input records the root that
/// applied it. A root admission already wrote the same binding for its own
/// inputs, so the insert is a no-op for them.
pub(crate) async fn bind_applied_input_tx(
    conn: &mut PgConnection,
    session_id: &SessionId,
    input: &InputId,
    root: &TurnId,
) -> Result<(), StoreError> {
    sqlx::query(session_roots_sql().inputs.insert.sql())
        .bind(session_id.as_str())
        .bind(input.as_str())
        .bind(root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// Bind each of `inputs` to `root`, set-if-absent, and open `root`'s row, in
/// the caller's transaction. A binding to another root refuses the whole
/// write.
pub(crate) async fn bind_root_inputs_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &TurnId,
    inputs: &[InputId],
) -> Result<(), StoreError> {
    for input in inputs {
        if let Some(bound) = root_binding_conn(conn, session_id, input).await?
            && bound != *root
        {
            return Err(root_binding_conflict(session_id, input, &bound, root));
        }
    }
    let sql = session_roots_sql();
    sqlx::query(sql.roots.insert_open.sql())
        .bind(session_id.as_str())
        .bind(root.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    for input in inputs {
        sqlx::query(sql.inputs.insert.sql())
            .bind(session_id.as_str())
            .bind(input.as_str())
            .bind(root.as_str())
            .execute(&mut *conn)
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
    let engine_ref: Option<String> = row.try_get(6).map_err(store_sqlx_error)?;
    let obligation_id: Option<String> = row.try_get(7).map_err(store_sqlx_error)?;
    let obligation_state: Option<String> = row.try_get(8).map_err(store_sqlx_error)?;
    let corrupt = |field: &str| StoreError::StoredDataCorrupt {
        record_kind: "ControlIntent",
        message: format!("{field} out of range"),
    };
    ControlIntent::from_stored(
        u64_from_sql("ControlIntent", "intent_id", id)?,
        SessionId::from(session_id),
        u32::try_from(format).map_err(|_| corrupt("format"))?,
        &kind_json,
        &state_json,
        u64_from_sql("ControlIntent", "created_at_ms", created_at_ms)?,
        engine_ref,
        obligation_id,
        obligation_state,
    )
}

/// `session_id`'s `close_session` intent, read on `conn`: its deletion
/// tombstone.
pub(crate) async fn close_session_intent_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<ControlIntent>, StoreError> {
    sqlx::query(session_roots_sql().intents.select_close_session.sql())
        .bind(session_id.as_str())
        .fetch_optional(&mut *conn)
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
    sqlx::query(session_roots_sql().intents.select_by_id.sql())
        .bind(sql_i64("control intent id", id.sequence())?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .as_ref()
        .map(decode_intent)
        .transpose()
}

/// Session `session_id`'s open verbs (pending, or failed and retryable), in
/// id order, read on `conn`.
pub(crate) async fn open_verbs_by_session_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Vec<ControlIntent>, StoreError> {
    let rows = sqlx::query(
        session_roots_sql()
            .intents
            .select_open_verbs_by_session
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_all(&mut *conn)
    .await
    .map_err(store_sqlx_error)?;
    rows.iter().map(decode_intent).collect()
}

/// Record a new intent of `session_id` in the caller's transaction: `kind`,
/// `Pending`, with its `ControlIntent` obligation armed due at `at_ms` on the
/// same row (ADR 0109). Answers it with its allocated id.
pub(crate) async fn insert_intent_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    kind: ControlIntentKind,
    engine: Option<&EnginePark>,
    at_ms: u64,
) -> Result<ControlIntent, StoreError> {
    let state = ControlIntentState::Pending;
    let (state_code, state_json) = stored_intent_state(&state)?;
    let id: i64 = sqlx::query_scalar(session_roots_sql().intents.insert.sql())
        .bind(session_id.as_str())
        .bind(i64::from(CONTROL_INTENT_FORMAT))
        .bind(kind.code())
        .bind(stored_intent_kind(&kind)?)
        .bind(state_code)
        .bind(state_json)
        .bind(sql_i64("control intent instant", at_ms)?)
        .bind(engine.map(EnginePark::as_str))
        .fetch_one(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    let id = ControlIntentId::from_sequence(u64_from_sql("ControlIntent", "intent_id", id)?);
    let obligation = crate::obligation_ledger::arm_obligation_tx(
        conn,
        &ObligationKey::ControlIntent { intent_id: id },
        at_ms,
    )
    .await?
    .ok_or_else(|| StoreError::StoredDataCorrupt {
        record_kind: "ControlIntent",
        message: "a freshly recorded intent already carries an obligation".into(),
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
pub(crate) async fn write_intent_state_conn(
    conn: &mut PgConnection,
    prior: &ControlIntent,
    next: &ControlIntent,
) -> Result<bool, StoreError> {
    let (state_code, state_json) = stored_intent_state(&next.state)?;
    let (_, prior_json) = stored_intent_state(&prior.state)?;
    let changed = sqlx::query(session_roots_sql().intents.update_state.sql())
        .bind(sql_i64("control intent id", next.id.sequence())?)
        .bind(state_code)
        .bind(state_json)
        .bind(prior_json)
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if changed == 1 {
        // A session close's acknowledgement owes its physical delete
        // (ADR 0109 §4), armed in this transaction.
        crate::session_delete_ledger::arm_on_close_acknowledged_conn(conn, prior, next).await?;
    }
    Ok(changed == 1)
}

/// One attempt to settle intent `id`'s engine half under obligation claim
/// `claim` in the caller's transaction (ADR 0109 claim fencing):
/// [`IntentSettle::ClaimLost`] and nothing written when the obligation is no
/// longer claimed under `claim`; otherwise `decide` answers the state to
/// write over the stored one, or `None` to leave it. `None` when another
/// writer moved the row between the read and the compare-and-set: the
/// caller retries on a fresh transaction.
pub(crate) async fn settle_intent_claimed_conn(
    conn: &mut PgConnection,
    id: ControlIntentId,
    claim: &ClaimToken,
    decide: impl FnOnce(&ControlIntentState) -> Option<ControlIntentState>,
) -> Result<Option<IntentSettle>, StoreError> {
    let sql = session_roots_sql();
    let intent_id = sql_i64("control intent id", id.sequence())?;
    let held: i64 = sqlx::query_scalar(sql.intents.select_claim_held.sql())
        .bind(intent_id)
        .bind(claim.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(store_sqlx_error)?;
    let stored = load_intent_conn(conn, id)
        .await?
        .ok_or(StoreError::ControlIntentUnknown { intent: id })?;
    if held == 0 {
        return Ok(Some(IntentSettle::ClaimLost));
    }
    let Some(state) = decide(&stored.state) else {
        return Ok(Some(IntentSettle::Held(Box::new(stored))));
    };
    let (state_code, state_json) = stored_intent_state(&state)?;
    let (_, prior_json) = stored_intent_state(&stored.state)?;
    let changed = sqlx::query(sql.intents.update_state_claimed.sql())
        .bind(intent_id)
        .bind(state_code)
        .bind(state_json)
        .bind(prior_json)
        .bind(claim.as_str())
        .execute(&mut *conn)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if changed != 1 {
        return Ok(None);
    }
    let mut settled = stored.clone();
    settled.state = state;
    // A session close's acknowledgement owes its physical delete (ADR 0109
    // §4), armed in this transaction.
    crate::session_delete_ledger::arm_on_close_acknowledged_conn(conn, &stored, &settled).await?;
    Ok(Some(IntentSettle::Held(Box::new(settled))))
}

/// The store half of session `session_id`'s close, in the caller's
/// transaction ([`ControlIntentStore::begin_session_close`]).
///
/// It takes the session's history-mutation lock first, the lock acceptance
/// takes, so no input is accepted into a session once its close committed.
/// The close names the roots it releases: every root without terminal
/// evidence (its logical-root rows, its parked root, its pending queued
/// run), each ended `Cancelled` by `SessionDeleted`, plus the roots of the
/// open verbs it supersedes, whose engine half then never runs.
///
/// [`ControlIntentStore::begin_session_close`]: lash_core_execution::store::ControlIntentStore::begin_session_close
pub(crate) async fn begin_session_close_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    at_ms: u64,
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
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if meta.is_none() {
        return Ok(None);
    }
    let sql = session_roots_sql();
    let mut roots = std::collections::BTreeSet::new();
    let open: Vec<String> = sqlx::query_scalar(sql.roots.select_open_roots.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    roots.extend(open.into_iter().map(TurnId::from));
    // The parked root is released with the park, whose feed event outlives
    // the session (FIG-3659).
    let released = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_parks
            .delete_by_session_returning
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if let Some(released) = released {
        let parked_root: String = released.try_get(0).map_err(store_sqlx_error)?;
        let park_id: i64 = released.try_get(1).map_err(store_sqlx_error)?;
        crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
            tx,
            session_id,
            &parked_root,
            park_id,
            &ParkEventKind::Cancelled {
                cause: ParkCancelCause::SessionDeleted,
            },
            at_ms,
        )
        .await?;
        roots.insert(TurnId::from(parked_root));
    }
    let verbs = open_verbs_by_session_conn(tx, session_id).await?;
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
    )
    .await?;
    for root in &roots {
        if root_terminal_conn(tx, session_id, root).await?.is_none() {
            write_root_terminal_conn(
                tx,
                &RootTerminal {
                    session_id: session_id.clone(),
                    root: root.clone(),
                    cause: RootTerminalCause::SessionDeleted { intent: intent.id },
                    head_revision: None,
                    at_ms,
                },
            )
            .await?;
        }
    }
    for verb in verbs {
        let mut superseded = verb.clone();
        superseded.state = ControlIntentState::Superseded { by: intent.id };
        if !write_intent_state_conn(tx, &verb, &superseded).await? {
            return Err(StoreError::Contended);
        }
    }
    let closed = sqlx::query(crate::session_sql::session_sql().meta.begin_close.sql())
        .bind(session_id.as_str())
        .bind(sql_i64("control intent id", intent.id.sequence())?)
        .bind(close_admission(intent.id).as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if closed != 1 {
        return Err(StoreError::Contended);
    }
    Ok(Some(intent))
}

/// Forget what session `session_id`'s roots hold, in its deletion's
/// transaction: its roots, its bindings and its verbs. A `close_session`
/// intent stays: it is the tombstone the factory answers the deleted
/// session's roots from.
pub(crate) async fn delete_session_roots_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let sql = session_roots_sql();
    for statement in [
        sql.roots.delete_by_session.sql(),
        sql.inputs.delete_by_session.sql(),
        sql.intents.delete_verbs_by_session.sql(),
    ] {
        sqlx::query(statement)
            .bind(session_id.as_str())
            .execute(&mut *conn)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl RootStore for PostgresStore {
    async fn unfinished_root(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<UnfinishedRoot>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        unfinished_root_conn(&mut connection, session_id).await
    }

    async fn admit_root(
        &self,
        request: &lash_core_execution::store::AdmitRootRequest,
    ) -> Result<Option<RootAdmission>, StoreError> {
        crate::runtime_persistence::admit_root_postgres(self, request).await
    }

    async fn admit_at_checkpoint(
        &self,
        request: &lash_core_execution::store::CheckpointAdmissionRequest,
    ) -> Result<lash_core_execution::store::CheckpointAdmission, StoreError> {
        crate::runtime_persistence::admit_at_checkpoint_postgres(self, request).await
    }

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Option<RootTerminal>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        root_terminal_conn(&mut connection, session_id, root).await
    }

    async fn end_refused_root(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        root: &TurnId,
        refusal: &lash_core_execution::RuntimeError,
        at_ms: u64,
    ) -> Result<RootEnd, StoreError> {
        lash_core_execution::store::validate_session_id(fence.session())?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = crate::begin_guarded(&mut *connection, &self.fence).await?;
        let end = end_refused_root_tx(&mut tx, fence, root, refusal, at_ms).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(end)
    }

    async fn end_command_root(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        root: &TurnId,
        at_ms: u64,
    ) -> Result<RootEnd, StoreError> {
        lash_core_execution::store::validate_session_id(fence.session())?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = crate::begin_guarded(&mut *connection, &self.fence).await?;
        let end = end_command_root_tx(&mut tx, fence, root, at_ms).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(end)
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
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        root_binding_conn(&mut connection, session_id, input).await
    }

    async fn bound_turn_scopes(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Vec<TurnId>, StoreError> {
        let scopes: Vec<String> =
            sqlx::query_scalar(session_roots_sql().inputs.bound_turn_scopes.sql())
                .bind(session_id.as_str())
                .bind(root.as_str())
                .fetch_all(&self.pool)
                .await
                .map_err(store_sqlx_error)?;
        Ok(scopes.into_iter().map(TurnId::from).collect())
    }

    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = crate::begin_guarded(&mut *connection, &self.fence).await?;
        crate::runtime_persistence::ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        bind_root_inputs_conn(&mut tx, session_id, root, inputs).await?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}

lash_store_sql::statements! {
    /// `control_intents` obligation statements only PostgreSQL issues (ADR 0109 §1.1).
    pub(crate) struct ControlIntentObligationPostgresStatements @ "control_intent" {
        /// At most `?2` obligations due at `?1`, oldest due first, each row
        /// locked for the caller's claim and skipped by every concurrent
        /// claimant: two deployments' relays take disjoint pages.
        obligation_select_due_locking = "SELECT obligation_id FROM control_intents
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";
    }
}

lash_store_sql::statements! {
    /// `session_roots` obligation statements only PostgreSQL issues (ADR 0109 §1.1).
    pub(crate) struct SessionRootObligationPostgresStatements @ "session_root" {
        /// At most `?2` obligations due at `?1`, oldest due first, each row
        /// locked for the caller's claim and skipped by every concurrent
        /// claimant: two deployments' relays take disjoint pages.
        obligation_select_due_locking = "SELECT obligation_id FROM session_roots
             WHERE obligation_state IN ('due', 'claimed') AND obligation_due_at_ms <= ?1
             ORDER BY obligation_due_at_ms, obligation_id
             LIMIT ?2
             FOR UPDATE SKIP LOCKED";
    }
}
