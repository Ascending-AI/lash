//! Root admission: one transaction under the session history lock.
use super::*;
use lash_core_execution::store::*;
use lash_core_execution::store_backend_support::sealed_shift_fence;
type PgTx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

async fn prepare_tx(
    tx: &mut PgTx<'_>,
    session: &SessionId,
    admission: &AdmissionId,
    executor: &RunExecutor,
    fleet: lash_core_execution::FleetFormat,
) -> Result<ShiftAdmissionPreparation, StoreError> {
    let epoch = super::shift_epoch::shift_epoch_tx(tx, session).await?;
    let park = super::turn_park::turn_park_for_update(tx, session).await?;
    let mut inputs = Vec::new();
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
    for sql in [
        statements.list_undelivered.sql(),
        statements.list_accepted.sql(),
    ] {
        for row in sqlx::query(sql)
            .bind(session.as_str())
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
        {
            inputs.push(pending_turn_input_read_from_row(row)?);
        }
    }
    let rows = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .list_by_session
            .sql(),
    )
    .bind(session.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let queued = rows
        .into_iter()
        .map(|row| queued_work_batch_from_row(queued_batch_row(row)?))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let head = inputs
        .iter()
        .filter(|read| read.input.state.is_next_turn_input(None))
        .min_by_key(|read| read.input.enqueue_seq);
    let bound = match head {
        Some(head) => {
            crate::session_runs::run_binding_conn(tx, session, &head.input.input_id).await?
        }
        None => None,
    };
    let ordering = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .family
            .pending_session_work_ordering
            .sql(),
    )
    .bind(session.as_str())
    .bind(crate::QueuedWorkKind::Control.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let command_seq: Option<i64> = ordering.try_get(1).map_err(store_sqlx_error)?;
    let command_seq = command_seq
        .map(|seq| {
            u64::try_from(seq).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "PendingSessionWorkOrdering",
                message: "negative command sequence".into(),
            })
        })
        .transpose()?;
    let selection = if epoch.closing.is_some() || epoch.control_pending || epoch.fault.is_some() {
        None
    } else {
        select_shift_work(
            session,
            admission,
            executor,
            ShiftAdmissionQueue {
                follow_on: super::turn_cancel::pending_follow_on_tx(tx, session, false)
                    .await?
                    .as_ref(),
                unfinished: crate::session_runs::unfinished_run_conn(tx, session)
                    .await?
                    .as_ref(),
                command_seq,
                queued: &queued,
                inputs: &inputs,
                bound_head: bound,
            },
        )?
        .map(|(run, work)| ShiftAdmissionSelection {
            run,
            work,
            observed_epoch: epoch.epoch,
        })
    };
    Ok(ShiftAdmissionPreparation {
        prospective_fence: sealed_shift_fence(session.clone(), epoch.epoch + 1, admission.clone()),
        epoch,
        park,
        head: crate::support::load_session_head_meta_tx(tx, session, false, fleet).await?,
        selection,
    })
}

pub(crate) async fn read_tx(
    tx: &mut PgTx<'_>,
    session: &SessionId,
    admission: &AdmissionId,
) -> Result<Option<ShiftAdmissionReceipt>, StoreError> {
    let json: Option<String> = sqlx::query_scalar(
        crate::session_runs::session_runs_sql()
            .runs
            .read_shift_admission
            .sql(),
    )
    .bind(session.as_str())
    .bind(admission.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    json.map(|json| {
        serde_json::from_str(&json).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "ShiftAdmissionReceipt",
            message: error.to_string(),
        })
    })
    .transpose()
}

pub(crate) async fn prepare(
    store: &PostgresStore,
    session: &SessionId,
    admission: &AdmissionId,
    executor: &RunExecutor,
) -> Result<ShiftAdmissionPreparation, StoreError> {
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    let result = prepare_tx(&mut tx, session, admission, executor, store.fence.fleet()).await?;
    tx.rollback().await.map_err(store_sqlx_error)?;
    Ok(result)
}

pub(crate) async fn read(
    store: &PostgresStore,
    session: &SessionId,
    admission: &AdmissionId,
) -> Result<Option<ShiftAdmissionReceipt>, StoreError> {
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    let result = read_tx(&mut tx, session, admission).await?;
    tx.rollback().await.map_err(store_sqlx_error)?;
    Ok(result)
}

pub(crate) async fn commit(
    store: &PostgresStore,
    request: &ShiftAdmissionWrite,
    anchor: &lash_core_execution::TraceAnchor,
) -> Result<ShiftAdmissionReceipt, StoreError> {
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    #[cfg(any(test, feature = "testing"))]
    store
        .set_transaction_lease_clock_for_testing(&mut tx)
        .await?;
    let session = &request.session_id;
    lock_session_history_mutation_tx(&mut tx, session).await?;
    super::shift_epoch::shift_epoch_locked_tx(&mut tx, session).await?;
    if let Some(mut receipt) = read_tx(&mut tx, session, &request.admission).await? {
        if receipt.run_start != request.run_start {
            receipt.seal = ShiftEpochSeal::ExecutionLost;
        }
        tx.rollback().await.map_err(store_sqlx_error)?;
        return Ok(receipt);
    }
    request.validate()?;
    let current = prepare_tx(
        &mut tx,
        session,
        &request.admission,
        &request.executor,
        store.fence.fleet(),
    )
    .await?;
    let selection = request
        .preparation
        .selection
        .clone()
        .ok_or_else(|| StoreError::Backend("root admission lacks a selection".into()))?;
    if current.selection != request.preparation.selection
        || current.epoch != request.preparation.epoch
        || current.park != request.preparation.park
    {
        return Err(StoreError::PreparedRunAdmissionStale {
            session_id: session.clone(),
            run: selection.run.clone(),
        });
    }
    let seal = super::shift_epoch::seal_shift_epoch_tx(
        &mut tx,
        session,
        &request.admission,
        selection.observed_epoch,
        &request.run_start,
        Some(&RunHold {
            run: selection.run.clone(),
            executor: request.executor.clone(),
        }),
    )
    .await?;
    let run_admission = if let (ShiftEpochSeal::Sealed(fence), Some(prepared)) =
        (&seal, &request.run)
    {
        let mut prepared = prepared.clone();
        prepared.request.fence = fence.clone();
        prepared.request.unsealed_epoch = None;
        let live =
            crate::support::load_session_head_meta_tx(&mut tx, session, true, store.fence.fleet())
                .await?;
        if crate::session_runs::run_admission_conn(&mut tx, session, &selection.run)
            .await?
            .is_none()
            && let Some(head) = &live
        {
            prepared.request.base.revision = head.head_revision;
            prepared.request.base.leaf = head.leaf_node_id.clone();
            prepared.request.base.checkpoint = head.checkpoint_ref.clone();
        }
        let admission = super::admission::admit_run_tx(
            store,
            &mut tx,
            &prepared.request,
            Some(&prepared),
            anchor,
        )
        .await?
        .ok_or_else(|| StoreError::PreparedRunAdmissionStale {
            session_id: session.clone(),
            run: selection.run.clone(),
        })?;
        let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
            session,
            &selection.run,
        )?;
        let committed = sqlx::query_scalar(session_sql().turn_commits.exists_for_turn.sql())
            .bind(session.as_str())
            .bind(key)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        Some(RunAdmissionAnswer::Admitted {
            head_verdict: inspect_shift_admitted_head(&admission.base, live.as_ref(), committed),
            admission: Box::new(admission),
        })
    } else {
        None
    };
    let receipt = ShiftAdmissionReceipt {
        cancel_intent: super::turn_cancel::load_turn_cancel_intent_snapshot_tx(
            &mut tx,
            session,
            &selection.run,
        )
        .await?,
        selection,
        run_start: request.run_start.clone(),
        seal,
        run_admission,
    };
    let json =
        serde_json::to_string(&receipt).map_err(|error| StoreError::Backend(error.to_string()))?;
    sqlx::query(
        crate::session_runs::session_runs_sql()
            .runs
            .write_shift_admission
            .sql(),
    )
    .bind(session.as_str())
    .bind(request.admission.as_str())
    .bind(json)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(receipt)
}
