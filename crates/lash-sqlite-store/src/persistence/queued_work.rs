//! The queued-work half of [`IngressStore`] for [`Store`], as inherent
//! methods the trait implementation forwards to: enqueue, host withdrawal,
//! the completion marker, and the open-work reads.

use super::*;

impl SqliteStore {
    pub(super) async fn enqueue_queued_work_sqlite(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(StoreError::Backend)?;
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = ensure_session_not_deleted_conn(tx, &batch.session_id)
                    .and_then(|()| enqueue_queued_work_conn(tx, &batch, now, nonce));
                // Roll back the partially-inserted batch/items on a
                // `StoreError` while still returning the typed error.
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(super) async fn enqueue_queued_work_with_outcome_sqlite(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(StoreError::Backend)?;
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = ensure_session_not_deleted_conn(tx, &batch.session_id)
                    .and_then(|()| enqueue_queued_work_conn_with_outcome(tx, &batch, now, nonce));
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(super) async fn cancel_queued_work_batch_sqlite(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let batch_id = batch_id.to_string();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Option<QueuedWorkBatch>, StoreError> = (|| {
                    let sql = crate::turn_ingress::turn_ingress_sql();
                    let row = tx
                        .query_row(
                            sql.queued_batches_sqlite.select_cancelable.sql(),
                            params![session_id.as_str(), batch_id.as_str()],
                            queued_batch_row_from_sql,
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some(row) = row else {
                        return Ok(None);
                    };
                    let batch = queued_work_batch_from_conn(tx, row)?;
                    // A host cancel is a wake's terminal transition too: the
                    // fence lands with the removal, or a redelivery of the
                    // withdrawn wake would be admitted again (FIG-3545).
                    if let Some(wake) =
                        lash_core_execution::store::TerminalProcessWake::of_batch(&batch)
                    {
                        crate::queued_work::raise_wake_redelivery_fence_conn(
                            tx,
                            &session_id,
                            &wake,
                        )?;
                    }
                    crate::conn::cached_execute(
                        tx,
                        sql.queued_batches_sqlite.delete_cancelled.sql(),
                        params![session_id.as_str(), batch_id.as_str()],
                    )
                    .map_err(sqlite_error)?;
                    Ok(Some(batch))
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(super) async fn queued_work_batch_completed_sqlite(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let marker =
            lash_core_execution::store_backend_support::session_command_batch_completion_key(
                &session_id,
                batch_id,
            )?;
        self.conn
            .call(move |conn| {
                conn.query_row(
                    crate::session_sql::session_sql()
                        .turn_commits
                        .exists_for_turn
                        .sql(),
                    params![session_id.as_str(), marker],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }

    pub(super) async fn list_queued_work_sqlite(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        #[cfg(feature = "testing")]
        let hydration_pause = self.conn.fault_injector();
        // One snapshot for the batch rows and their item rows: see
        // `SqliteConnection::read`.
        self.conn
            .read(move |tx| {
                let outcome: Result<Vec<QueuedWorkBatch>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = tx
                            .prepare(
                                crate::turn_ingress::turn_ingress_sql()
                                    .queued_batches
                                    .list_by_session
                                    .sql(),
                            )
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(params![session_id.as_str()], queued_batch_row_from_sql)
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    // Test seam: the window this snapshot closes is between the
                    // batch rows above and their item rows below.
                    #[cfg(feature = "testing")]
                    if let Some(injector) = hydration_pause.as_ref() {
                        injector.reach_queued_work_hydration();
                    }
                    rows.into_iter()
                        .map(|row| queued_work_batch_from_conn(tx, row))
                        .collect()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(super) async fn pending_session_work_ordering_sqlite(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::PendingSessionWorkOrdering, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        self.conn
            .call(move |conn| {
                let outcome: Result<
                    lash_core_execution::store::PendingSessionWorkOrdering,
                    StoreError,
                > = (|| {
                    let (command_at, command_seq, input_at, input_seq): (
                        Option<i64>,
                        Option<i64>,
                        Option<i64>,
                        Option<i64>,
                    ) = conn
                        .query_row(
                            crate::turn_ingress::turn_ingress_sql()
                                .family
                                .pending_session_work_ordering
                                .sql(),
                            params![session_id.as_str(), QueuedWorkKind::Control.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )
                        .map_err(sqlite_error)?;
                    let ordering_key = |kind: &'static str, at: Option<i64>, seq: Option<i64>| {
                        at.zip(seq)
                            .map(|(at, seq)| {
                                Ok(lash_core_execution::store::PendingWorkOrderingKey {
                                    enqueued_at_ms: u64_from_sql(kind, "enqueued_at_ms", at)
                                        .map_err(sqlite_error)?,
                                    enqueue_seq: u64_from_sql(kind, "enqueue_seq", seq)
                                        .map_err(sqlite_error)?,
                                })
                            })
                            .transpose()
                    };
                    Ok(lash_core_execution::store::PendingSessionWorkOrdering {
                        session_command: ordering_key("QueuedWorkBatch", command_at, command_seq)?,
                        turn_input: ordering_key("PendingTurnInput", input_at, input_seq)?,
                    })
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    pub(super) async fn list_open_queued_work_sqlite(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        #[cfg(feature = "testing")]
        let hydration_pause = self.conn.fault_injector();
        // One snapshot for the batch rows and their item rows: see
        // `SqliteConnection::read`.
        self.conn
            .read(move |tx| {
                let outcome: Result<Vec<QueuedWorkBatch>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = tx
                            .prepare(
                                crate::turn_ingress::turn_ingress_sql()
                                    .queued_batches
                                    .list_open
                                    .sql(),
                            )
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(params![session_id.as_str()], queued_batch_row_from_sql)
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    // Test seam: the window this snapshot closes is between the
                    // batch rows above and their item rows below.
                    #[cfg(feature = "testing")]
                    if let Some(injector) = hydration_pause.as_ref() {
                        injector.reach_queued_work_hydration();
                    }
                    rows.into_iter()
                        .map(|row| queued_work_batch_from_conn(tx, row))
                        .collect()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }
}
