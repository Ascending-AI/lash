//! Checkpoint admission (FIG-3927): the writes that bind open rows of both
//! admission tables to a running turn's run, and the bindless read of the
//! command lane. A run's own admission is the session actor's mail drain.
//!
//! Every admission runs in one transaction: it reads back what an earlier
//! execution of the same step already bound, and otherwise composes from
//! open rows under `FOR UPDATE` and binds them, each write predicated on the
//! row still being open.

use super::*;
use lash_core_execution::store::{CheckpointAdmission, CheckpointAdmissionRequest};

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// Admit the checkpoint work of `request`'s run, keyed by its step
/// ([`RunStore::admit_at_checkpoint`]).
///
/// A read-only probe answers the common empty checkpoint without a write
/// transaction. It also reports rows the step already bound, so a
/// re-executed step always reaches the read-back.
///
/// [`RunStore::admit_at_checkpoint`]: lash_core_execution::store::RunStore::admit_at_checkpoint
pub(crate) async fn admit_at_checkpoint_postgres(
    store: &crate::PostgresStore,
    request: &CheckpointAdmissionRequest,
) -> Result<CheckpointAdmission, StoreError> {
    #[cfg(test)]
    store
        .checkpoint_probe_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !checkpoint_work_pending_postgres(&store.pool, request, &store.observer).await? {
        return Ok(CheckpointAdmission::default());
    }
    #[cfg(test)]
    store
        .checkpoint_write_transaction_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let session_id = &request.session_id;
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    #[cfg(any(test, feature = "testing"))]
    store
        .set_transaction_lease_clock_for_testing(&mut tx)
        .await?;
    let mode = lash_core_execution::TurnInputAdmissionMode::ActiveTurn {
        turn_id: request.turn_id.clone(),
        checkpoint: request.checkpoint,
    };
    let recorded =
        read_step_admission_tx(&mut tx, session_id, &request.run, &request.step, mode).await?;
    if !recorded.is_empty() {
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(recorded);
    }
    let inputs = if request.max_inputs == 0 {
        None
    } else {
        compose_active_turn_inputs_tx(
            &mut tx,
            session_id,
            &request.turn_id,
            request.checkpoint,
            request.max_inputs,
        )
        .await?
    };
    if let Some(inputs) = inputs.as_ref() {
        bind_turn_inputs_tx(&mut tx, &request.run, &request.step, inputs).await?;
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(CheckpointAdmission { inputs })
}

/// The leading open session-command run the session applies next (ADR 0101
/// §4, FIG-3927 §2.7). The command lane takes no admission: the commit that
/// applies the run settles it, predicated on each row still being open.
pub(crate) async fn open_session_command_run_postgres(
    store: &crate::PostgresStore,
    session_id: &SessionId,
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = crate::observed_sql::control("BEGIN", connection.begin())
        .await
        .map_err(store_sqlx_error)?;
    let (mut batches, candidates) = scan_queued_work_candidates_tx(
        &mut tx,
        session_id,
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches_postgres
            .admission_candidates_idle
            .sql(),
        SESSION_COMMAND_BATCHES_PER_RUN,
    )
    .await?;
    batches.truncate(select_leading_session_command(&candidates));
    crate::observed_sql::control("ROLLBACK", tx.rollback())
        .await
        .map_err(store_sqlx_error)?;
    Ok(batches)
}

/// Whether `request`'s checkpoint has anything to admit or read back,
/// answered by one read-only probe.
async fn checkpoint_work_pending_postgres(
    pool: &PgPool,
    request: &CheckpointAdmissionRequest,
    observer: &crate::StoreObserver,
) -> Result<bool, StoreError> {
    let mut connection = acquire_runtime_connection(pool, observer).await?;
    // One statement per checkpoint, chosen exhaustively: the admitted
    // minimum-boundary set is what the checkpoint decides, and an optional
    // predicate over a bound boundary cannot seek an index.
    let family = &crate::turn_ingress::turn_ingress_sql().family_postgres;
    let sql = match request.checkpoint {
        lash_core_execution::CheckpointKind::AfterWork => {
            family.checkpoint_work_pending_after_work.sql()
        }
        lash_core_execution::CheckpointKind::BeforeCompletion => {
            family.checkpoint_work_pending_before_completion.sql()
        }
    };
    sqlx::query_scalar(sql)
        .bind(request.session_id.as_str())
        .bind(request.turn_id.as_str())
        .bind(i64::try_from(request.max_inputs).unwrap_or(i64::MAX))
        .bind(request.run.as_str())
        .bind(request.step.as_str())
        .fetch_one(crate::observed_sql::executor(&mut *connection))
        .await
        .map_err(store_sqlx_error)
}

/// What `run` already bound under `step`, both families in `enqueue_seq`
/// order: a re-executed admission step answers exactly this.
async fn read_step_admission_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    step: &str,
    mode: lash_core_execution::TurnInputAdmissionMode,
) -> Result<CheckpointAdmission, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let input_rows = sqlx::query(sql.pending_inputs.select_admitted_by_step.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .bind(step)
        .fetch_all(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    let inputs = input_rows
        .into_iter()
        .map(|row| pending_turn_input_from_row(pending_turn_input_row(row)?))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CheckpointAdmission {
        inputs: (!inputs.is_empty()).then(|| lash_core_execution::AdmittedTurnInputs {
            session_id: session_id.clone(),
            mode,
            inputs,
            applications: Vec::new(),
        }),
    })
}

/// The open input addressed to active turn `turn_id` that `checkpoint`
/// admits, up to `max_inputs`.
async fn compose_active_turn_inputs_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    turn_id: &TurnId,
    checkpoint: lash_core_execution::CheckpointKind,
    max_inputs: usize,
) -> Result<Option<lash_core_execution::AdmittedTurnInputs>, StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_postgres;
    let statement = match checkpoint {
        lash_core_execution::CheckpointKind::AfterWork => {
            statements.admission_candidates_active_turn_after_work.sql()
        }
        lash_core_execution::CheckpointKind::BeforeCompletion => statements
            .admission_candidates_active_turn_before_completion
            .sql(),
    };
    let rows = sqlx::query(statement)
        .bind(session_id.as_str())
        .bind(i64::try_from(max_inputs).unwrap_or(i64::MAX))
        .bind(turn_id.as_str())
        .fetch_all(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    let inputs = rows
        .into_iter()
        .map(|row| pending_turn_input_from_row(pending_turn_input_row(row)?))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(lash_core_execution::store::plan_checkpoint_input_admission(
        session_id, turn_id, checkpoint, inputs,
    ))
}

/// Bind every input of `admitted` to `run` under `step`.
async fn bind_turn_inputs_tx(
    tx: &mut PgTx<'_>,
    run: &TurnId,
    step: &str,
    admitted: &lash_core_execution::AdmittedTurnInputs,
) -> Result<(), StoreError> {
    let state = lash_core_execution::store::turn_input_state_after_admission(&admitted.mode)
        .map(|state| state.as_str());
    let statement = crate::turn_ingress::turn_ingress_sql()
        .pending_inputs
        .admit
        .sql();
    for input in &admitted.inputs {
        let bound = sqlx::query(statement)
            .bind(admitted.session_id.as_str())
            .bind(input.input_id.as_str())
            .bind(state)
            .bind(run.as_str())
            .bind(step)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        // Backstop: the composition locked this row open in the same
        // transaction, so the open predicate cannot legitimately miss.
        lash_core_execution::store_backend_support::require_fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
            crate::POSTGRES_BACKEND,
            input.input_id.as_str(),
            bound,
            || StoreError::Contended,
        )?;
    }
    Ok(())
}

/// One locked candidate scan of the open queued work by `statement`, one of
/// the named admission-candidate scans: the batches it hydrates to and the
/// candidates the shared prefix rule decides over.
async fn scan_queued_work_candidates_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    statement: &str,
    max_rows: usize,
) -> Result<(Vec<QueuedWorkBatch>, Vec<TurnLaneCandidate>), StoreError> {
    let rows = sqlx::query(statement)
        .bind(session_id.as_str())
        .bind(admission_scan_limit(max_rows))
        .fetch_all(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
    let mut batches = Vec::with_capacity(rows.len());
    for row in rows {
        batches.push(queued_work_batch_from_row(queued_batch_row(row)?)?);
    }
    let candidates = batches.iter().map(turn_lane_candidate).collect();
    Ok((batches, candidates))
}
