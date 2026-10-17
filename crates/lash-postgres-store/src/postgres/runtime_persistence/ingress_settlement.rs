//! A commit's ingress settlement (FIG-3927), inside its write transaction.
//!
//! Every row a commit names is settled under the run that admitted it: the
//! settling write is predicated on the row's run, and a row the run does
//! not hold refuses the whole commit. The writes are steps of the commit's
//! program (`durable::head_commit`); what is read here is the rows a cancel
//! released or dropped, whose payloads the commit's outcome reports, and
//! the standing of a row whose settlement refused.

use super::*;

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// A row a cancel released or dropped: its disposition, and for an input
/// its ingress position with what the outcome reports of it.
pub(crate) type ReleasedRow = (
    lash_core_execution::store::IngressRowId,
    lash_core_execution::TurnCancelUndeliveredInputPolicy,
    Option<(u64, lash_core_execution::TurnCancelAffectedInput)>,
);

/// The rows `commit`'s cancel released or dropped, each verdict taken
/// under the row's lock: a row its run does not hold refuses the commit.
pub(crate) async fn released_rows_tx(
    tx: &mut PgTx<'_>,
    commit: &RuntimeCommit,
) -> Result<Vec<ReleasedRow>, StoreError> {
    let session_id = &commit.session_id;
    let Some(ingress) = commit.ingress.as_ref() else {
        return Ok(Vec::new());
    };
    let run = &ingress.run;
    let mut released = Vec::new();
    for (rows, disposition) in [
        (
            &ingress.released,
            lash_core_execution::TurnCancelUndeliveredInputPolicy::Defer,
        ),
        (
            &ingress.dropped,
            lash_core_execution::TurnCancelUndeliveredInputPolicy::Drop,
        ),
    ] {
        for row in rows {
            let affected = match row {
                lash_core_execution::store::IngressRowId::Input(input_id) => {
                    let held = admitted_input_tx(tx, session_id, run, input_id).await?;
                    Some((
                        held.enqueue_seq,
                        lash_core_execution::TurnCancelAffectedInput {
                            input_id: input_id.clone(),
                            payload: held.input.into_live("PendingTurnInput")?,
                            disposition,
                        },
                    ))
                }
                lash_core_execution::store::IngressRowId::Batch(batch_id) => {
                    admitted_batch_tx(tx, session_id, run, batch_id).await?;
                    None
                }
            };
            released.push((row.clone(), disposition, affected));
        }
    }
    Ok(released)
}

/// Input `input_id`, which run `run` must hold, locked for the commit.
async fn admitted_input_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
    let row = crate::observed_sql::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .select_by_id_for_update
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(store_sqlx_error)?
    .map(pending_turn_input_row)
    .transpose()?;
    lash_core_execution::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &input_row(input_id),
        row.as_ref().map(|row| row.admitted_run.as_deref()),
    )?;
    let row =
        row.ok_or_else(|| StoreError::Backend(format!("admitted input `{input_id}` vanished")))?;
    pending_turn_input_from_row(row)
}

/// Batch `batch_id`, which run `run` must hold, locked for the commit.
async fn admitted_batch_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    run: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let observed: Option<Option<String>> =
        crate::observed_sql::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(store_sqlx_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &lash_core_execution::store::IngressRowId::Batch(batch_id.clone()),
        observed.as_ref().map(Option::as_deref),
    )
}

fn input_row(input_id: &lash_core_execution::InputId) -> lash_core_execution::store::IngressRowId {
    lash_core_execution::store::IngressRowId::Input(input_id.clone())
}

/// Why `row`'s settlement under `run` refused: the shared verdict over the
/// row as it stands, and when that verdict admits it, the settling write's
/// own miss, recorded as evidence and failed closed.
pub(crate) async fn ingress_refusal_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    run: &TurnId,
    row: &lash_core_execution::store::IngressRowId,
) -> StoreError {
    let verdict = match row {
        lash_core_execution::store::IngressRowId::Input(input_id) => {
            admitted_input_tx(tx, session_id, run, input_id)
                .await
                .map(drop)
        }
        lash_core_execution::store::IngressRowId::Batch(batch_id) => {
            admitted_batch_tx(tx, session_id, run, batch_id).await
        }
    };
    if let Err(error) = verdict {
        return error;
    }
    match lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        &row.to_string(),
        0,
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            run: run.clone(),
            row: Box::new(row.clone()),
            admitted_run: None,
        },
    ) {
        Err(error) => error,
        Ok(()) => StoreError::Backend(format!("ingress row `{row}` refused its settlement")),
    }
}

/// Why open command `batch_id`'s settlement refused: withdrawn, or bound
/// to a run.
pub(crate) async fn command_refusal_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    batch_id: &lash_core_execution::BatchId,
) -> StoreError {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let observed: Result<Option<Option<String>>, _> =
        crate::observed_sql::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await;
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => return store_sqlx_error(error),
    };
    if let Err(error) = lash_core_execution::store_backend_support::require_open_command(
        session_id,
        batch_id,
        observed.as_ref().map(Option::as_deref),
    ) {
        return error;
    }
    match lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        batch_id.as_str(),
        0,
        || StoreError::SessionCommandWithdrawn {
            session_id: session_id.clone(),
            batch_id: batch_id.clone(),
        },
    ) {
        Err(error) => error,
        Ok(()) => StoreError::Backend(format!("command `{batch_id}` refused its settlement")),
    }
}
