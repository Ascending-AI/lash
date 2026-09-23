use super::*;

#[async_trait::async_trait]
impl QueuedWorkStore for PostgresSessionStore {
    async fn begin_or_resume_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        request: lash_core::store::BeginQueuedRun,
    ) -> Result<lash_core::store::QueuedRunAdmission, StoreError> {
        self.begin_run(fence, request).await
    }
    async fn select_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        scope: &lash_core::ExecutionScope,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &lash_core::PersistedSessionConfig,
        policy: QueuedWorkClaimPolicy,
    ) -> Result<lash_core::store::SelectedQueuedRun, StoreError> {
        self.select_run(fence, scope, owner, max_inputs, configuration, policy)
            .await
    }
    async fn pending_queued_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<lash_core::store::QueuedRunAdmission>, StoreError> {
        self.pending_run(session_id).await
    }
    async fn queued_run(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> Result<Option<lash_core::store::QueuedRunAdmission>, StoreError> {
        self.run_by_scope(scope).await
    }
    async fn settle_queued_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        settlement: lash_core::store::QueuedRunCommit,
    ) -> Result<lash_core::store::QueuedRunAdmission, StoreError> {
        self.settle_run(fence, settlement).await
    }

    async fn enqueue_queued_work(
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

    async fn enqueue_queued_work_with_outcome(
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

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        // The fence is validated live, so its fencing token is the
        // currently-live session-lease generation; claims pin it and are
        // claimable only across a different generation (ADR 0029).
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let (selected_rows, mut selected_batches, candidates) =
            scan_queued_work_candidates_postgres(
                &mut tx,
                now,
                session_id,
                generation,
                QueuedWorkClaimBoundary::Idle,
                MAX_SESSION_COMMAND_BATCHES_PER_CLAIM,
            )
            .await?;
        let selected_len = select_leading_session_command(&candidates);
        if selected_len == 0 {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }

        selected_batches.truncate(selected_len);
        match claim_queued_work_rows_postgres(
            &mut tx,
            now,
            session_id,
            owner,
            generation,
            &selected_rows[..selected_len],
            selected_batches,
            &candidates[..selected_len],
        )
        .await?
        {
            ClaimTransactionOutcome::Commit(claim) => {
                tx.commit().await.map_err(store_sqlx_error)?;
                Ok(claim)
            }
            ClaimTransactionOutcome::Rollback(_) => {
                tx.rollback().await.map_err(store_sqlx_error)?;
                Ok(None)
            }
        }
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        policy: QueuedWorkClaimPolicy,
    ) -> Result<QueuedWorkClaimOutcome, StoreError> {
        if policy.max_rows == 0 {
            return Ok(QueuedWorkClaimOutcome::Refused(
                QueuedWorkClaimRefusal::ZeroLimit,
            ));
        }
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let generation = session_execution_lease.fencing_token;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let (selected_rows, mut selected_batches, candidates) =
            scan_queued_work_candidates_postgres(
                &mut tx,
                now,
                session_id,
                generation,
                boundary,
                policy.max_rows,
            )
            .await?;
        let prefix = select_turn_work_claim_prefix(&candidates, boundary, &policy, now)?;
        let selected_len = match prefix {
            TurnWorkClaimPrefix::Selected { len } => len,
            TurnWorkClaimPrefix::Refused { reason: refusal } => {
                // The candidate query applies the boundary rule in SQL, so an empty
                // scan reaches the claim state machine as a bare `Empty`. Re-ask it
                // with the unfiltered ready head (and, failing that, look for
                // deferred work) so this backend names the same fact every other one
                // names.
                let refusal = if refusal == QueuedWorkClaimRefusal::Empty {
                    postgres_refusal_for_empty_scan(
                        &mut tx, session_id, generation, boundary, &policy,
                    )
                    .await?
                    .into_refusal()
                } else {
                    refusal
                };
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(QueuedWorkClaimOutcome::Refused(refusal));
            }
        };

        selected_batches.truncate(selected_len);
        match claim_queued_work_rows_postgres(
            &mut tx,
            now,
            session_id,
            owner,
            generation,
            &selected_rows[..selected_len],
            selected_batches,
            &candidates[..selected_len],
        )
        .await?
        {
            ClaimTransactionOutcome::Commit(claim) => {
                tx.commit().await.map_err(store_sqlx_error)?;
                Ok(match claim {
                    Some(claim) => QueuedWorkClaimOutcome::Claimed(claim),
                    None => QueuedWorkClaimOutcome::Refused(QueuedWorkClaimRefusal::Empty),
                })
            }
            ClaimTransactionOutcome::Rollback(_) => {
                tx.rollback().await.map_err(store_sqlx_error)?;
                Ok(QueuedWorkClaimOutcome::Refused(
                    QueuedWorkClaimRefusal::ClaimRaceLost,
                ))
            }
        }
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
        policy: QueuedWorkClaimPolicy,
    ) -> Result<(Option<lash_core::TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        #[cfg(test)]
        self.checkpoint_probe_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !checkpoint_work_pending_postgres(
            &self.pool,
            crate::turn_ingress::injected_lease_epoch_ms(
                #[cfg(any(test, feature = "testing"))]
                self.lease_clock_for_testing.as_ref(),
            ),
            session_id,
            session_execution_lease.fencing_token,
            turn_id,
            checkpoint,
            max_inputs,
            policy.max_rows,
        )
        .await?
        {
            return Ok((None, None));
        }

        #[cfg(test)]
        self.checkpoint_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let input = claim_pending_turn_inputs_postgres_tx(
            &mut tx,
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.clone(),
                checkpoint,
            },
        )
        .await?;
        let input = match input {
            ClaimTransactionOutcome::Commit(input) => input,
            ClaimTransactionOutcome::Rollback(input) => {
                tx.rollback().await.map_err(store_sqlx_error)?;
                return Ok((input, None));
            }
        };
        let queued = claim_ready_queued_work_postgres_tx(
            &mut tx,
            session_id,
            session_execution_lease,
            owner,
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            policy,
        )
        .await?;
        match queued {
            ClaimTransactionOutcome::Commit(queued) => {
                super::queued_run_assignment::assign_checkpoint_members_tx(
                    &mut tx,
                    session_id,
                    turn_id,
                    input.as_ref(),
                    queued.as_ref(),
                )
                .await?;
                tx.commit().await.map_err(store_sqlx_error)?;
                Ok((input, queued))
            }
            ClaimTransactionOutcome::Rollback(queued) => {
                tx.rollback().await.map_err(store_sqlx_error)?;
                Ok((None, queued))
            }
        }
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[lash_core::BatchId],
        policy: QueuedWorkClaimPolicy,
    ) -> Result<lash_core::SelectedQueuedWorkClaimOutcome, StoreError> {
        if batch_ids.is_empty() {
            return Ok(lash_core::SelectedQueuedWorkClaimOutcome::new(
                None,
                Vec::new(),
            ));
        }
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let result = claim_selected_queued_work_postgres_tx(
            &mut tx,
            session_id,
            session_execution_lease,
            owner,
            boundary,
            batch_ids,
            policy,
        )
        .await?;
        if result.claim.is_some() {
            tx.commit().await.map_err(store_sqlx_error)?;
        } else {
            tx.rollback().await.map_err(store_sqlx_error)?;
        }
        Ok(result)
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .abandon_claim
                .sql(),
        )
        .bind(claim.session_id.as_str())
        .bind(&claim.claim_id)
        .bind(&claim.lease_token)
        .bind(lash_core::store_backend_support::queued_work_abandon_restore_claim_id(claim))
        .bind(lash_core::store_backend_support::queued_work_abandon_restore_claim_token(claim))
        .execute(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        // One statement, not a loop: the claims a batch abandon gives up are
        // bound as five parallel arrays, so the statement's own text is fixed
        // however many there are.
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let session_ids = claims
            .iter()
            .map(|claim| claim.session_id.as_str().to_string())
            .collect::<Vec<_>>();
        let claim_ids = claims
            .iter()
            .map(|claim| claim.claim_id.clone())
            .collect::<Vec<_>>();
        let claim_tokens = claims
            .iter()
            .map(|claim| claim.lease_token.clone())
            .collect::<Vec<_>>();
        let restore_claim_ids = claims
            .iter()
            .map(|claim| {
                lash_core::store_backend_support::queued_work_abandon_restore_claim_id(claim)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        let restore_claim_tokens = claims
            .iter()
            .map(|claim| {
                lash_core::store_backend_support::queued_work_abandon_restore_claim_token(claim)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches_postgres
                .abandon_claims
                .sql(),
        )
        .bind(&session_ids)
        .bind(&claim_ids)
        .bind(&claim_tokens)
        .bind(&restore_claim_ids)
        .bind(&restore_claim_tokens)
        .execute(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let sql = crate::turn_ingress::turn_ingress_sql();
        let row = sqlx::query(sql.queued_batches_postgres.select_cancelable.sql())
            .bind(session_id.as_str())
            .bind(batch_id)
            .bind(now as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let Some(row) = row else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        };
        let run_owns_batch: bool = sqlx::query_scalar(sql.queued_runs.pending_member.sql())
            .bind(session_id.as_str())
            .bind("batch")
            .bind(batch_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        if run_owns_batch {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(None);
        }
        let batch = queued_work_batch_from_row(&mut tx, queued_batch_row(row)?).await?;
        // A host cancel is a wake's terminal transition too: the fence lands
        // with the removal, or a redelivery of the withdrawn wake would be
        // admitted again (FIG-3545).
        if let Some(wake) = lash_core::store::claim_plan::TerminalProcessWake::of_batch(&batch) {
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

    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        let marker = lash_core::store_backend_support::session_command_batch_completion_key(
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

    async fn list_queued_work(
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

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::store::PendingSessionWorkOrdering, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
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
        .bind(now as i64)
        .bind(QueuedWorkKind::Control.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        let ordering_key = |kind: &'static str, at: Option<i64>, seq: Option<i64>| {
            at.zip(seq)
                .map(|(at, seq)| {
                    Ok(lash_core::store::PendingWorkOrderingKey {
                        enqueued_at_ms: u64_from_sql(kind, "enqueued_at_ms", at)?,
                        enqueue_seq: u64_from_sql(kind, "enqueue_seq", seq)?,
                    })
                })
                .transpose()
        };
        Ok(lash_core::store::PendingSessionWorkOrdering {
            session_command: ordering_key("QueuedWorkBatch", command_at, command_seq)?,
            turn_input: ordering_key("PendingTurnInput", input_at, input_seq)?,
        })
    }

    async fn list_pending_queued_work(
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
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .list_unclaimed
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(now as i64)
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
