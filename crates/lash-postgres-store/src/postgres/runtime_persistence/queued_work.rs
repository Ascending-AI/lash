//! The queued-work half of [`IngressStore`] for [`PostgresSessionStore`], as
//! inherent methods the trait implementation forwards to: enqueue, host
//! withdrawal, the completion marker, and the open-work reads.

use super::*;

impl PostgresSessionStore {
    pub(super) async fn enqueue_queued_work_pg(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(StoreError::Backend)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, &batch.session_id).await?;
        let queued = enqueue_queued_work_tx(&mut tx, &batch, self.clock.timestamp_ms()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(queued)
    }

    pub(super) async fn enqueue_queued_work_with_outcome_pg(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        batch
            .validate_process_wake_source()
            .map_err(StoreError::Backend)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, &batch.session_id).await?;
        let queued =
            enqueue_queued_work_with_outcome_tx(&mut tx, &batch, self.clock.timestamp_ms()).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(queued)
    }

    pub(super) async fn cancel_queued_work_batch_pg(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let sql = crate::turn_ingress::turn_ingress_sql();
        let row = sqlx::query(sql.queued_batches_postgres.select_cancelable.sql())
            .bind(session_id.as_str())
            .bind(batch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let row = queued_batch_row(row)?;
        let batch = queued_work_batch_from_row(&mut tx, row).await?;
        // A host cancel is a wake's terminal transition too: the fence lands
        // with the removal, or a redelivery of the withdrawn wake would be
        // admitted again (FIG-3545).
        if let Some(wake) = lash_core_execution::store::TerminalProcessWake::of_batch(&batch) {
            raise_wake_redelivery_fence_tx(&mut tx, session_id, &wake).await?;
        }
        sqlx::query(sql.queued_batches_postgres.delete_cancelled.sql())
            .bind(batch_id)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(batch))
    }

    pub(super) async fn queued_work_batch_completed_pg(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        let marker =
            lash_core_execution::store_backend_support::session_command_batch_completion_key(
                session_id, batch_id,
            )?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query_scalar(
            crate::session_sql::session_sql()
                .turn_commits
                .exists_for_turn
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(marker)
        .fetch_one(&mut *connection)
        .await
        .map_err(store_sqlx_error)
    }

    pub(super) async fn list_queued_work_pg(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // One snapshot for the batch rows and their item rows. Under the
        // default READ COMMITTED every statement re-snapshots, so a batch
        // consumed between the two reads is seen as a header with no payloads.
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .list_by_session
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut batches = Vec::new();
        for row in rows {
            batches.push(queued_work_batch_from_row(&mut tx, queued_batch_row(row)?).await?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(batches)
    }

    pub(super) async fn pending_session_work_ordering_pg(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::PendingSessionWorkOrdering, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let (command_at, command_seq, input_at, input_seq): (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .pending_session_work_ordering
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(QueuedWorkKind::Control.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        let ordering_key = |kind: &'static str, at: Option<i64>, seq: Option<i64>| {
            at.zip(seq)
                .map(|(at, seq)| {
                    Ok(lash_core_execution::store::PendingWorkOrderingKey {
                        enqueued_at_ms: u64_from_sql(kind, "enqueued_at_ms", at)?,
                        enqueue_seq: u64_from_sql(kind, "enqueue_seq", seq)?,
                    })
                })
                .transpose()
        };
        Ok(lash_core_execution::store::PendingSessionWorkOrdering {
            session_command: ordering_key("QueuedWorkBatch", command_at, command_seq)?,
            turn_input: ordering_key("PendingTurnInput", input_at, input_seq)?,
        })
    }

    pub(super) async fn list_open_queued_work_pg(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // One snapshot for the batch rows and their item rows; see
        // `list_queued_work`.
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .list_open
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut batches = Vec::new();
        for row in rows {
            batches.push(queued_work_batch_from_row(&mut tx, queued_batch_row(row)?).await?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(batches)
    }
}
