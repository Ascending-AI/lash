use super::*;

#[async_trait::async_trait]
impl QueuedWorkStore for Store {
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

    async fn enqueue_queued_work_with_outcome(
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

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let session_execution_lease = session_execution_lease.clone();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<TxOutcome<Option<QueuedWorkClaim>>, StoreError> = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    // The fence is validated live, so its fencing token is the
                    // currently-live session-lease generation; claims pin it and
                    // are claimable only across a different generation (ADR 0029).
                    let generation = session_execution_lease.fencing_token;
                    let (candidate_rows, candidate_batches, candidates) =
                        scan_queued_work_candidates_sqlite(
                            tx,
                            now,
                            &session_id,
                            generation,
                            QueuedWorkClaimBoundary::Idle,
                            MAX_SESSION_COMMAND_BATCHES_PER_CLAIM,
                        )?;
                    let selected_len = select_leading_session_command(&candidates);
                    if selected_len == 0 {
                        return Ok(TxOutcome::Commit(None));
                    }
                    let mut selected_batches = candidate_batches;
                    selected_batches.truncate(selected_len);
                    claim_queued_work_rows_sqlite(
                        tx,
                        now,
                        &session_id,
                        &owner,
                        generation,
                        &candidate_rows[..selected_len],
                        selected_batches,
                        &candidates[..selected_len],
                    )
                })(
                );
                match outcome {
                    Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
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
        let session_id = SessionId::from(session_id.to_string());
        let session_execution_lease = session_execution_lease.clone();
        let owner = owner.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<TxOutcome<QueuedWorkClaimOutcome>, StoreError> = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    let generation = session_execution_lease.fencing_token;
                    let (candidate_rows, candidate_batches, candidates) =
                        scan_queued_work_candidates_sqlite(
                            tx,
                            now,
                            &session_id,
                            generation,
                            boundary,
                            policy.max_rows,
                        )?;
                    let prefix =
                        select_turn_work_claim_prefix(&candidates, boundary, &policy, now)?;
                    let selected_len = match prefix {
                        TurnWorkClaimPrefix::Selected { len } => len,
                        TurnWorkClaimPrefix::Refused { reason: refusal } => {
                            // The candidate query applies the boundary rule in SQL,
                            // so an empty scan reaches the claim state machine as a
                            // bare `Empty`. Re-ask it with the unfiltered ready head
                            // (and, failing that, look for deferred work) so this
                            // backend names the same fact every other one names.
                            let refusal = if refusal == QueuedWorkClaimRefusal::Empty {
                                sqlite_refusal_for_empty_scan(
                                    tx,
                                    &session_id,
                                    now,
                                    generation,
                                    boundary,
                                    &policy,
                                )?
                                .into_refusal()
                            } else {
                                refusal
                            };
                            return Ok(TxOutcome::Commit(QueuedWorkClaimOutcome::Refused(refusal)));
                        }
                    };
                    let mut selected_batches = candidate_batches;
                    selected_batches.truncate(selected_len);
                    match claim_queued_work_rows_sqlite(
                        tx,
                        now,
                        &session_id,
                        &owner,
                        generation,
                        &candidate_rows[..selected_len],
                        selected_batches,
                        &candidates[..selected_len],
                    )? {
                        TxOutcome::Commit(Some(claim)) => {
                            Ok(TxOutcome::Commit(QueuedWorkClaimOutcome::Claimed(claim)))
                        }
                        TxOutcome::Commit(None) => Ok(TxOutcome::Commit(
                            QueuedWorkClaimOutcome::Refused(QueuedWorkClaimRefusal::Empty),
                        )),
                        TxOutcome::Rollback(_) => Ok(TxOutcome::Rollback(
                            QueuedWorkClaimOutcome::Refused(QueuedWorkClaimRefusal::ClaimRaceLost),
                        )),
                    }
                })(
                );
                // Lower a `StoreError` into the rollback arm so the closure body can keep
                // using `?` while still propagating the error to the caller.
                match outcome {
                    Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
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
        let now = self.clock.timestamp_ms();
        if !checkpoint_work_pending_sqlite(
            &self.conn,
            now,
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
        let session_id = SessionId::from(session_id.to_string());
        let session_execution_lease = session_execution_lease.clone();
        let owner = owner.clone();
        let turn_id = turn_id.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<
                    TxOutcome<(Option<lash_core::TurnInputClaim>, Option<QueuedWorkClaim>)>,
                    StoreError,
                > = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    let input = claim_pending_turn_inputs_sqlite_conn(
                        tx,
                        now,
                        &session_id,
                        &session_execution_lease,
                        &owner,
                        max_inputs,
                        lash_core::TurnInputClaimMode::ActiveTurn {
                            turn_id: turn_id.clone(),
                            checkpoint,
                        },
                    )?;
                    let input = match input {
                        TxOutcome::Commit(input) => input,
                        TxOutcome::Rollback(input) => {
                            return Ok(TxOutcome::Rollback((input, None)));
                        }
                    };
                    let queued = claim_ready_queued_work_sqlite_conn(
                        tx,
                        now,
                        &session_id,
                        &session_execution_lease,
                        &owner,
                        QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
                        policy,
                    )?;
                    match queued {
                        TxOutcome::Commit(queued) => {
                            super::queued_run_assignment::assign_checkpoint_members_conn(
                                tx,
                                &session_id,
                                &turn_id,
                                input.as_ref(),
                                queued.as_ref(),
                            )?;
                            Ok(TxOutcome::Commit((input, queued)))
                        }
                        TxOutcome::Rollback(queued) => Ok(TxOutcome::Rollback((None, queued))),
                    }
                })();
                match outcome {
                    Ok(TxOutcome::Commit(value)) => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(TxOutcome::Rollback(value)) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[lash_core::BatchId],
        policy: QueuedWorkClaimPolicy,
    ) -> Result<SelectedQueuedWorkClaimOutcome, StoreError> {
        if batch_ids.is_empty() {
            return Ok(SelectedQueuedWorkClaimOutcome::new(None, Vec::new()));
        }
        let session_id = SessionId::from(session_id.to_string());
        let fence = session_execution_lease.clone();
        let owner = owner.clone();
        let batch_ids = batch_ids.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = claim_selected_queued_work_sqlite_conn(
                    tx,
                    now,
                    &session_id,
                    &fence,
                    &owner,
                    boundary,
                    &batch_ids,
                    policy,
                );
                match outcome {
                    Ok(value) if value.claim.is_some() => Ok(TxOutcome::Commit(Ok(value))),
                    Ok(value) => Ok(TxOutcome::Rollback(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        let session_id = claim.session_id.clone();
        let claim_id = claim.claim_id.clone();
        let lease_token = claim.lease_token.clone();
        let restore_claim_id =
            lash_core::store_backend_support::queued_work_abandon_restore_claim_id(claim)
                .map(str::to_string);
        let restore_claim_token =
            lash_core::store_backend_support::queued_work_abandon_restore_claim_token(claim)
                .map(str::to_string);
        self.conn
            .write(move |tx| {
                tx.execute(
                    crate::turn_ingress::turn_ingress_sql()
                        .queued_batches
                        .abandon_claim
                        .sql(),
                    params![
                        session_id.as_str(),
                        claim_id.as_str(),
                        lease_token,
                        restore_claim_id,
                        restore_claim_token
                    ],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        let claims = claims.to_vec();
        self.conn
            .write(move |tx| {
                let mut changed = 0;
                for claim in claims {
                    changed += tx.execute(
                        crate::turn_ingress::turn_ingress_sql()
                            .queued_batches
                            .abandon_claim
                            .sql(),
                        params![
                            claim.session_id.as_str(),
                            claim.claim_id.as_str(),
                            claim.lease_token,
                            lash_core::store_backend_support::queued_work_abandon_restore_claim_id(
                                &claim,
                            ),
                            lash_core::store_backend_support::queued_work_abandon_restore_claim_token(
                                &claim,
                            ),
                        ],
                    )?;
                }
                Ok(changed)
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let batch_id = batch_id.to_string();
        let now = self.clock.timestamp_ms() as i64;
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Option<QueuedWorkBatch>, StoreError> = (|| {
                    let sql = crate::turn_ingress::turn_ingress_sql();
                    let row = tx
                        .query_row(
                            sql.queued_batches_sqlite.select_cancelable.sql(),
                            params![session_id.as_str(), batch_id.as_str(), now],
                            queued_batch_row_from_sql,
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some(row) = row else {
                        return Ok(None);
                    };
                    let batch = queued_work_batch_from_conn(tx, row)?;
                    tx.execute(
                        sql.queued_batches_sqlite.delete_cancelled.sql(),
                        params![session_id.as_str(), batch_id.as_str(), now],
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

    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let marker = lash_core::store_backend_support::session_command_batch_completion_key(
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

    async fn list_queued_work(
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

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core::store::PendingSessionWorkOrdering, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let now = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<lash_core::store::PendingSessionWorkOrdering, StoreError> =
                    (|| {
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
                                params![
                                    session_id.as_str(),
                                    now as i64,
                                    QueuedWorkKind::Control.as_str()
                                ],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                            )
                            .map_err(sqlite_error)?;
                        let ordering_key =
                            |kind: &'static str, at: Option<i64>, seq: Option<i64>| {
                                at.zip(seq)
                                    .map(|(at, seq)| {
                                        Ok(lash_core::store::PendingWorkOrderingKey {
                                            enqueued_at_ms: u64_from_sql(
                                                kind,
                                                "enqueued_at_ms",
                                                at,
                                            )
                                            .map_err(sqlite_error)?,
                                            enqueue_seq: u64_from_sql(kind, "enqueue_seq", seq)
                                                .map_err(sqlite_error)?,
                                        })
                                    })
                                    .transpose()
                            };
                        Ok(lash_core::store::PendingSessionWorkOrdering {
                            session_command: ordering_key(
                                "QueuedWorkBatch",
                                command_at,
                                command_seq,
                            )?,
                            turn_input: ordering_key("PendingTurnInput", input_at, input_seq)?,
                        })
                    })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let now = self.clock.timestamp_ms();
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
                                    .list_unclaimed
                                    .sql(),
                            )
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                params![session_id.as_str(), now as i64],
                                queued_batch_row_from_sql,
                            )
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
