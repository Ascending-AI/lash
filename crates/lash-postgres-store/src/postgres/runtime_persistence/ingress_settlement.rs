//! A commit's ingress settlement (FIG-3927), inside its write transaction.
//!
//! Every row a commit names is settled under the run that admitted it: the
//! shared verdict decides over the row as read under `FOR UPDATE`, and the
//! write keeps the run predicate as its backstop. A row the run does not
//! hold refuses the whole commit. The session-command run a commit applied
//! settles bindlessly, predicated on each row still being open.

use super::*;

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// Settle `commit`'s ingress and applied commands at `now`. Returns the rows
/// it released or dropped.
pub(super) async fn settle_commit_ingress_tx(
    tx: &mut PgTx<'_>,
    commit: &RuntimeCommit,
    now: u64,
) -> Result<lash_core_execution::TurnCancelInputOutcome, StoreError> {
    let session_id = &commit.session_id;
    if let Some(commands) = commit.applied_commands.as_ref() {
        for batch_id in &commands.batch_ids {
            crate::queued_work::settle_open_command_tx(tx, commit, batch_id, now).await?;
        }
    }
    let mut affected_inputs = Vec::new();
    if let Some(ingress) = commit.ingress.as_ref() {
        let run = &ingress.run;
        for completion in &ingress.completed_inputs {
            for input_id in &completion.input_ids {
                admitted_input_tx(tx, session_id, run, input_id).await?;
                settle_admitted_input_tx(
                    tx,
                    session_id,
                    run,
                    input_id,
                    lash_core_execution::runtime::TurnInputStateKind::Completed,
                    now,
                )
                .await?;
                // The completing commit binds the input to the run that
                // applied it: a checkpoint-admitted input carries no binding
                // until now, and a run-admitted input's is this same row.
                crate::session_runs::bind_applied_input_tx(tx, session_id, input_id, run).await?;
            }
        }
        for completion in &ingress.completed_batches {
            for batch_id in &completion.batch_ids {
                crate::queued_work::complete_admitted_batch_tx(
                    tx,
                    session_id,
                    run,
                    batch_id,
                    lash_core_execution::store::IngressTerminal {
                        cause: lash_core_execution::store::IngressTerminalCause::Delivered,
                        at_ms: now,
                    },
                )
                .await?;
            }
        }
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
                match row {
                    lash_core_execution::store::IngressRowId::Input(input_id) => {
                        let held = admitted_input_tx(tx, session_id, run, input_id).await?;
                        match disposition {
                            lash_core_execution::TurnCancelUndeliveredInputPolicy::Defer => {
                                release_admitted_input_tx(tx, session_id, run, input_id).await?;
                            }
                            lash_core_execution::TurnCancelUndeliveredInputPolicy::Drop => {
                                settle_admitted_input_tx(
                                    tx,
                                    session_id,
                                    run,
                                    input_id,
                                    lash_core_execution::runtime::TurnInputStateKind::Cancelled,
                                    now,
                                )
                                .await?;
                            }
                        }
                        affected_inputs.push((
                            held.enqueue_seq,
                            lash_core_execution::TurnCancelAffectedInput {
                                input_id: input_id.clone(),
                                payload: held.input,
                                disposition,
                            },
                        ));
                    }
                    lash_core_execution::store::IngressRowId::Batch(batch_id) => {
                        match disposition {
                            // A deferred command keeps its ingress position.
                            lash_core_execution::TurnCancelUndeliveredInputPolicy::Defer => {
                                let batch =
                                    admitted_batch_tx(tx, session_id, run, batch_id).await?;
                                release_admitted_batch_tx(tx, session_id, run, batch_id).await?;
                            }
                            lash_core_execution::TurnCancelUndeliveredInputPolicy::Drop => {
                                crate::queued_work::complete_admitted_batch_tx(
                                    tx,
                                    session_id,
                                    run,
                                    batch_id,
                                    lash_core_execution::store::IngressTerminal {
                                        cause:
                                            lash_core_execution::store::IngressTerminalCause::Cancelled,
                                        at_ms: now,
                                    },
                                )
                                .await?;
                            }
                        }
                    }
                }
            }
        }
    }
    // The outcome reports every row the commit released or dropped, in the
    // order they were submitted.
    affected_inputs.sort_by_key(|(enqueue_seq, _)| *enqueue_seq);
    Ok(lash_core_execution::TurnCancelInputOutcome {
        affected_inputs: affected_inputs
            .into_iter()
            .map(|(_, affected)| affected)
            .collect(),
    })
}

/// Input `input_id`, which run `run` must hold, locked for the commit.
async fn admitted_input_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .select_by_id_for_update
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut **tx))
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

/// The hydrated batch `batch_id`, which run `run` must hold, locked for
/// the commit.
async fn admitted_batch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<QueuedWorkBatch, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let observed: Option<Option<String>> =
        sqlx::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &lash_core_execution::store::IngressRowId::Batch(batch_id.clone()),
        observed.as_ref().map(Option::as_deref),
    )?;
    load_queued_batch(tx, batch_id.as_str())
        .await?
        .ok_or_else(|| StoreError::Backend(format!("admitted batch `{batch_id}` vanished")))
}

/// Settle input `input_id`, held by `run`, into the terminal `state` at
/// `now`.
async fn settle_admitted_input_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_core_execution::InputId,
    state: lash_core_execution::runtime::TurnInputStateKind,
    now: u64,
) -> Result<(), StoreError> {
    let settled = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .settle_admitted
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .bind(state.as_str())
    .bind(run.as_str())
    .bind(crate::support::clamp_epoch_ms(now))
    .execute(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
    require_settlement_applied(session_id, run, input_row(input_id), settled)
}

/// Hand input `input_id`, held by `run`, back open at its own position.
async fn release_admitted_input_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<(), StoreError> {
    let released = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .release_admitted
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .bind(run.as_str())
    .execute(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
    require_settlement_applied(session_id, run, input_row(input_id), released)
}

/// Hand batch `batch_id`, held by `run`, back open at its own position.
async fn release_admitted_batch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let released = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .release_admitted
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(batch_id.as_str())
    .bind(run.as_str())
    .execute(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
    require_settlement_applied(
        session_id,
        run,
        lash_core_execution::store::IngressRowId::Batch(batch_id.clone()),
        released,
    )
}

fn input_row(input_id: &lash_core_execution::InputId) -> lash_core_execution::store::IngressRowId {
    lash_core_execution::store::IngressRowId::Input(input_id.clone())
}

/// Backstop: the verdict was taken over this row under its lock earlier in
/// the same transaction, so the run predicate cannot legitimately miss. A
/// miss is recorded as evidence and then fails closed.
fn require_settlement_applied(
    session_id: &SessionId,
    run: &TurnId,
    row: lash_core_execution::store::IngressRowId,
    rows_affected: u64,
) -> Result<(), StoreError> {
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        &row.to_string(),
        rows_affected,
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            run: run.clone(),
            row: Box::new(row.clone()),
            admitted_run: None,
        },
    )
}
