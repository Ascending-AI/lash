//! The [`QueuedWorkStore`] operations for [`PostgresStore`], as
//! inherent methods the trait implementation forwards to: enqueue, host
//! withdrawal, the command receipt, and the open-work reads.

use super::*;

impl PostgresStore {
    pub(super) async fn enqueue_queued_work_pg(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
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
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
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
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = begin_guarded(&mut *connection, &self.fence).await?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let sql = crate::turn_ingress::turn_ingress_sql();
        let row = sqlx::query(sql.queued_batches_postgres.select_cancelable.sql())
            .bind(session_id.as_str())
            .bind(batch_id)
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let row = queued_batch_row(row)?;
        let batch = queued_work_batch_from_row(row)?;
        // The row lock `select_cancelable` took holds the openness decision.
        sqlx::query(sql.queued_batches.withdraw_open.sql())
            .bind(session_id.as_str())
            .bind(batch_id)
            .bind(crate::support::clamp_epoch_ms(self.clock.timestamp_ms()))
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(batch))
    }

    pub(super) async fn queued_work_batch_completion_pg(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<lash_core_execution::store::RuntimeCommitReceipt>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let row = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .select_command_completion
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(batch_id)
        .fetch_optional(crate::observed_sql::executor(&mut *connection))
        .await
        .map_err(store_sqlx_error)?;
        row.map(|row| {
            let operation_key: String = row.get(0);
            let result_json: String = row.get(1);
            lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                session_id,
                &operation_key,
                &result_json,
                self.fence.fleet(),
            )
        })
        .transpose()
    }

    pub(super) async fn list_queued_work_pg(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(crate::observed_sql::executor(&mut *tx))
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
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
        let mut batches = Vec::new();
        for row in rows {
            batches.push(queued_work_batch_from_row(queued_batch_row(row)?)?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(batches)
    }

    pub(super) async fn pending_session_work_ordering_pg(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::PendingSessionWorkOrdering, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
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
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        let ordering_key = |kind: &'static str, at: Option<i64>, seq: Option<i64>| {
            at.zip(seq)
                .map(|(at, seq)| -> Result<_, StoreError> {
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
        let mut connection = acquire_runtime_connection(&self.pool, &self.observer).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // One snapshot for the batch rows and their item rows; see
        // `list_queued_work`.
        sqlx::query(
            crate::connection_sql::connection_sql()
                .begin_repeatable_read_read_only
                .sql(),
        )
        .execute(crate::observed_sql::executor(&mut *tx))
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
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
        let mut batches = Vec::new();
        for row in rows {
            batches.push(queued_work_batch_from_row(queued_batch_row(row)?)?);
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(batches)
    }
}
