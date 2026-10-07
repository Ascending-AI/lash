//! Pending turn-input row projection and host withdrawal.
//!
//! The durable representation of queued turn inputs, mirroring the SQLite
//! backend's `pending_turn_inputs` module. Originated in `session_factory.rs`;
//! every item keeps its previous path through the crate-root glob.

use crate::*;

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    session_id: SessionId,
    source_key: Option<String>,
    state: lash_core_execution::TurnInputState,
    input_json: String,
    enqueued_at_ms: u64,
    /// The run whose admission holds the row; `None` while it is open.
    pub(crate) admitted_run: Option<String>,
    run_spec_hash: Option<String>,
    trace_cause: lash_core_execution::TraceCause,
}

pub(crate) fn pending_turn_input_row(row: PgRow) -> Result<PendingTurnInputRow, StoreError> {
    let ingress_json: String = row.get("ingress_json");
    let ingress: lash_core_execution::TurnInputIngress =
        store_decode_json(&ingress_json, "turn-input ingress")?;
    let state = lash_core_execution::TurnInputState::from_persisted(
        row.get::<String, _>("state").as_str(),
        ingress,
        row.get::<Option<i64>, _>("terminal_at_ms")
            .map(|at| u64_from_sql("PendingTurnInput", "terminal_at_ms", at))
            .transpose()?,
    )?;
    Ok(PendingTurnInputRow {
        enqueue_seq: u64_from_sql("PendingTurnInput", "enqueue_seq", row.get("enqueue_seq"))?,
        input_id: row.get("input_id"),
        session_id: SessionId::parse(row.get::<String, _>("session_id"))?,
        source_key: row.get("source_key"),
        state,
        input_json: row.get("input_json"),
        enqueued_at_ms: u64_from_sql(
            "PendingTurnInput",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
        admitted_run: row.get("admitted_run"),
        run_spec_hash: row.get("run_spec_hash"),
        trace_cause: lash_core_execution::store_backend_support::decode_trace_cause(
            "PendingTurnInput",
            row.get::<Option<String>, _>("trace_cause_json").as_deref(),
        )?,
    })
}

pub(crate) fn pending_turn_input_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
    Ok(lash_core_execution::PendingTurnInput {
        input_id: row.input_id.try_into()?,
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        state: row.state,
        enqueued_at_ms: row.enqueued_at_ms,
        input: store_decode_json(&row.input_json, "turn input")?,
        run_spec: row
            .run_spec_hash
            .map(lash_core_execution::RunSpecHash::from_stored),
        trace_cause: row.trace_cause,
    })
}

pub(crate) fn pending_turn_input_read_from_row(
    row: PgRow,
) -> Result<lash_core_execution::PendingTurnInputRead, StoreError> {
    let row = pending_turn_input_row(row)?;
    let admitted_run = row.admitted_run.clone();
    let input = pending_turn_input_from_row(row)?;
    Ok(match admitted_run {
        Some(run) => lash_core_execution::PendingTurnInputRead::admitted(
            input,
            lash_core_execution::TurnId::parse(run)?,
        ),
        None => lash_core_execution::PendingTurnInputRead::open(input),
    })
}

pub(crate) async fn load_pending_turn_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    input_id: &str,
) -> Result<Option<lash_core_execution::PendingTurnInput>, StoreError> {
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .select_by_id
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(pending_turn_input_row)
        .transpose()?
        .map(pending_turn_input_from_row)
        .transpose()
}

pub(crate) async fn load_pending_turn_input_row_by_target_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    target: &lash_core_execution::PendingTurnInputCancelTarget,
    for_update: bool,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    // Two lock regimes, two named statements each: a cancel that is about to
    // write takes the row lock, a read-only observation must not.
    let sql = crate::turn_ingress::turn_ingress_sql();
    let statement = match (target, for_update) {
        (lash_core_execution::PendingTurnInputCancelTarget::InputId(_), false) => {
            sql.pending_inputs.select_by_id.sql()
        }
        (lash_core_execution::PendingTurnInputCancelTarget::InputId(_), true) => {
            sql.pending_inputs_postgres.select_by_id_for_update.sql()
        }
        (lash_core_execution::PendingTurnInputCancelTarget::SourceKey(_), false) => {
            sql.pending_inputs.select_by_source_key.sql()
        }
        (lash_core_execution::PendingTurnInputCancelTarget::SourceKey(_), true) => sql
            .pending_inputs_postgres
            .select_by_source_key_for_update
            .sql(),
    };
    let key = match target {
        lash_core_execution::PendingTurnInputCancelTarget::InputId(input_id) => input_id.as_str(),
        lash_core_execution::PendingTurnInputCancelTarget::SourceKey(source_key) => {
            source_key.as_str()
        }
    };
    let row = sqlx::query(statement)
        .bind(session_id.as_str())
        .bind(key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    row.map(pending_turn_input_row).transpose()
}

/// Which rows a cancel locks in queue order before it writes any.
pub(crate) enum CancelLockScope<'a> {
    /// The resolved explicit targets.
    Targets(&'a std::collections::BTreeSet<lash_core_execution::InputId>),
    /// The suffix from this `enqueue_seq`.
    Suffix(u64),
}

/// Lock every row a cancel may write in queue order, the order every other
/// multi-row writer locks the same rows in, so concurrent writers cannot
/// deadlock.
pub(crate) async fn lock_cancel_rows_in_queue_order(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    scope: CancelLockScope<'_>,
) -> Result<(), StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_postgres;
    let query = match scope {
        CancelLockScope::Targets(targets) => {
            let input_ids = targets
                .iter()
                .map(|input_id| input_id.as_str().to_string())
                .collect::<Vec<_>>();
            sqlx::query(statements.lock_cancel_targets_in_queue_order.sql())
                .bind(session_id.as_str())
                .bind(input_ids)
        }
        CancelLockScope::Suffix(anchor_seq) => {
            sqlx::query(statements.lock_cancel_suffix_in_queue_order.sql())
                .bind(session_id.as_str())
                .bind(i64::try_from(anchor_seq).unwrap_or(i64::MAX))
        }
    };
    query.fetch_all(&mut **tx).await.map_err(store_sqlx_error)?;
    Ok(())
}

/// Withdraw one locked row for the host at `now` (FIG-3927): an open row is
/// cancelled in one write (FIG-4098); a row a run admitted is that run's to
/// settle or release, so the cancel changes nothing and answers the run that
/// holds it.
pub(crate) async fn cancel_pending_turn_input_row_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: PendingTurnInputRow,
    now: u64,
) -> Result<lash_core_execution::PendingTurnInputCancelOutcome, StoreError> {
    let admitted_run = row.admitted_run.clone();
    let mut input = pending_turn_input_from_row(row)?;
    match input.state.kind() {
        lash_core_execution::runtime::TurnInputStateKind::Cancelled => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCancelled(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::Completed => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCompleted(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::PendingActive
        | lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn
        | lash_core_execution::runtime::TurnInputStateKind::Accepted => {
            if let Some(run) = admitted_run {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyAdmitted {
                        input,
                        run: lash_core_execution::TurnId::parse(run)?,
                    },
                );
            }
            let cancelled = sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs
                    .cancel
                    .sql(),
            )
            .bind(input.session_id.as_str())
            .bind(input.input_id.as_str())
            .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
            .bind(crate::support::clamp_epoch_ms(now))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
            if cancelled != 1 {
                return Err(StoreError::Backend(format!(
                    "open turn input `{}` was not withdrawn under its row lock",
                    input.input_id
                )));
            }
            input.state = lash_core_execution::TurnInputState::Cancelled {
                ingress: input.state.ingress(),
                at_ms: now.min(i64::MAX as u64),
            };
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}
