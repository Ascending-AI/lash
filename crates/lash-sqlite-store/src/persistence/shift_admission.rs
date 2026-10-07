//! Root admission: one write transaction and a retained nonce receipt.
use super::*;
use lash_core_execution::store::*;
use lash_core_execution::store_backend_support::sealed_shift_fence;

fn prepare_conn(
    tx: &Connection,
    session: &SessionId,
    admission: &AdmissionId,
    executor: &RunExecutor,
    fleet: lash_core_execution::FleetFormat,
) -> Result<ShiftAdmissionPreparation, StoreError> {
    let epoch = super::shift_epoch::shift_epoch_conn(tx, session)?;
    let park = super::turn_park::turn_park_conn(tx, session)?;
    let mut inputs = Vec::new();
    let sql = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
    for query in [sql.list_undelivered.sql(), sql.list_accepted.sql()] {
        let mut statement = tx.prepare(query).map_err(sqlite_error)?;
        let rows = statement
            .query_map(params![session.as_str()], pending_turn_input_row_from_sql)
            .map_err(sqlite_error)?;
        for row in rows {
            inputs.push(pending_turn_input_read_from_row(
                row.map_err(sqlite_error)?,
            )?);
        }
    }
    let mut statement = tx
        .prepare(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .list_by_session
                .sql(),
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session.as_str()], queued_batch_row_from_sql)
        .map_err(sqlite_error)?;
    let queued = rows
        .map(|row| queued_work_batch_from_row(row.map_err(sqlite_error)?))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let head = inputs
        .iter()
        .filter(|read| read.input.state.is_next_turn_input(None))
        .min_by_key(|read| read.input.enqueue_seq);
    let bound = head
        .map(|head| crate::session_runs::run_binding_conn(tx, session, &head.input.input_id))
        .transpose()?
        .flatten();
    let command_seq: Option<i64> = tx
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .pending_session_work_ordering
                .sql(),
            params![session.as_str(), crate::QueuedWorkKind::Control.as_str()],
            |row| row.get(1),
        )
        .map_err(sqlite_error)?;
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
                follow_on: super::turn_cancel::pending_follow_on_conn(tx, session)?.as_ref(),
                unfinished: crate::session_runs::unfinished_run_conn(tx, session)?.as_ref(),
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
        head: crate::codec::try_load_session_head_meta_from_conn(tx, session, fleet)?,
        selection,
    })
}

pub(crate) fn read_conn(
    tx: &Connection,
    session: &SessionId,
    admission: &AdmissionId,
) -> Result<Option<ShiftAdmissionReceipt>, StoreError> {
    let json: Option<String> = tx
        .query_row(
            crate::session_runs::session_runs_sql()
                .runs
                .read_shift_admission
                .sql(),
            params![session.as_str(), admission.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    json.map(|json| {
        serde_json::from_str(&json).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "ShiftAdmissionReceipt",
            message: error.to_string(),
        })
    })
    .transpose()
}

pub(crate) async fn prepare(
    store: &SqliteStore,
    session: &SessionId,
    admission: &AdmissionId,
    executor: &RunExecutor,
) -> Result<ShiftAdmissionPreparation, StoreError> {
    let fleet = store.conn.fleet();
    let (session, admission, executor) = (session.clone(), admission.clone(), executor.clone());
    store
        .conn
        .read(move |tx| Ok(prepare_conn(tx, &session, &admission, &executor, fleet)))
        .await
        .map_err(sqlite_error)?
}

pub(crate) async fn read(
    store: &SqliteStore,
    session: &SessionId,
    admission: &AdmissionId,
) -> Result<Option<ShiftAdmissionReceipt>, StoreError> {
    let (session, admission) = (session.clone(), admission.clone());
    store
        .conn
        .read(move |tx| Ok(read_conn(tx, &session, &admission)))
        .await
        .map_err(sqlite_error)?
}

pub(crate) async fn commit(
    store: &SqliteStore,
    request: &ShiftAdmissionWrite,
    anchor: &lash_core_execution::TraceAnchor,
) -> Result<ShiftAdmissionReceipt, StoreError> {
    let request = request.clone();
    let anchor = anchor.clone();
    let now = store.clock.timestamp_ms();
    store.conn.write_flow(move |tx| {
        let result = (|| {
            let session = &request.session_id;
            if let Some(mut receipt) = read_conn(tx, session, &request.admission)? {
                if receipt.run_start != request.run_start { receipt.seal = ShiftEpochSeal::ExecutionLost; }
                return Ok(receipt);
            }
            request.validate()?;
            let current = prepare_conn(tx, session, &request.admission, &request.executor, tx.fleet())?;
            let selection = request.preparation.selection.clone().ok_or_else(|| StoreError::Backend("root admission lacks a selection".into()))?;
            if current.selection != request.preparation.selection || current.epoch != request.preparation.epoch || current.park != request.preparation.park {
                return Err(StoreError::PreparedRunAdmissionStale { session_id: session.clone(), run: selection.run.clone() });
            }
            let seal = super::shift_epoch::seal_shift_epoch_conn(tx, session, &request.admission, selection.observed_epoch, &request.run_start, Some(&RunHold { run: selection.run.clone(), executor: request.executor.clone() }))?;
            let run_admission = if let (ShiftEpochSeal::Sealed(fence), Some(prepared)) = (&seal, &request.run) {
                let mut prepared = prepared.clone();
                prepared.request.fence = fence.clone();
                prepared.request.unsealed_epoch = None;
                let live = crate::codec::try_load_session_head_meta_from_conn(tx, session, tx.fleet())?;
                // A new composition is proposed on the durable head. Its exact
                // preimage must still match before binding anything.
                if crate::session_runs::run_admission_conn(tx, session, &selection.run)?.is_none()
                    && let Some(head) = &live {
                        prepared.request.base.revision = head.head_revision;
                        prepared.request.base.leaf = head.leaf_node_id.clone();
                        prepared.request.base.checkpoint = head.checkpoint_ref.clone();
                }
                let admission = match super::admission::admit_run_conn(tx, tx.fleet(), &prepared.request, Some(&prepared), &anchor, now)? {
                    TxOutcome::Commit(Some(admission)) => admission,
                    _ => return Err(StoreError::PreparedRunAdmissionStale { session_id: session.clone(), run: selection.run.clone() }),
                };
                let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(session, &selection.run)?;
                let committed = tx.query_row(session_sql().turn_commits.exists_for_turn.sql(), params![session.as_str(), key], |row| row.get(0)).map_err(sqlite_error)?;
                Some(RunAdmissionAnswer::Admitted { head_verdict: inspect_shift_admitted_head(&admission.base, live.as_ref(), committed), admission: Box::new(admission) })
            } else { None };
            let receipt = ShiftAdmissionReceipt { selection, run_start: request.run_start.clone(), seal, run_admission };
            tx.execute(crate::session_runs::session_runs_sql().runs.write_shift_admission.sql(), params![session.as_str(), request.admission.as_str(), encode_json(&receipt)?]).map_err(sqlite_error)?;
            Ok(receipt)
        })();
        Ok(match result { Ok(receipt) => TxOutcome::Commit(Ok(receipt)), Err(error) => TxOutcome::Rollback(Err(error)) })
    }).await.map_err(sqlite_error)?
}
