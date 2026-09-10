use super::*;

#[async_trait::async_trait]
impl QueuedWorkStore for Store {
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
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let session_id = session_id.to_string();
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
                    let (candidate_batches, candidates) = scan_queued_work_candidates_sqlite(
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
        session_id: &str,
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
        let session_id = session_id.to_string();
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
                    let (candidate_batches, candidates) = scan_queued_work_candidates_sqlite(
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
                // Lower a `StoreError` into the rollback arm so the closure body
                // can keep using `?` while still propagating the error to the
                // caller. Encode it as a `Result` carried out of the flow.
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
        session_id: &str,
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
        let session_id = session_id.to_string();
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
                            turn_id,
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
                        TxOutcome::Commit(queued) => Ok(TxOutcome::Commit((input, queued))),
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
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: QueuedWorkClaimPolicy,
    ) -> Result<SelectedQueuedWorkClaimOutcome, StoreError> {
        if batch_ids.is_empty() {
            return Ok(SelectedQueuedWorkClaimOutcome::new(None, Vec::new()));
        }
        let session_id = session_id.to_string();
        let fence = session_execution_lease.clone();
        let owner = owner.clone();
        let batch_ids = batch_ids.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<SelectedQueuedWorkClaimOutcome, StoreError> = (|| {
                    ensure_session_execution_lease_conn(tx, &session_id, &fence, now)?;
                    let generation = fence.fencing_token;
                    let requested_ids = batch_ids
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>();
                    let present_ids = {
                        let mut sql = "SELECT batch_id FROM queued_work_batches
                                       WHERE session_id = ? AND batch_id IN ("
                            .to_string();
                        sql.push_str(&vec!["?"; batch_ids.len()].join(", "));
                        sql.push(')');
                        let mut values: Vec<rusqlite::types::Value> =
                            vec![session_id.clone().into()];
                        values.extend(batch_ids.iter().cloned().map(Into::into));
                        let mut stmt = tx.prepare(&sql).map_err(sqlite_error)?;
                        stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(sqlite_error)?
                        .collect::<Result<std::collections::BTreeSet<_>, _>>()
                        .map_err(sqlite_error)?
                    };
                    let already_satisfied_batch_ids = batch_ids
                        .iter()
                        .filter(|batch_id| !present_ids.contains(batch_id.as_str()))
                        .cloned()
                        .collect::<Vec<_>>();
                    if present_ids.is_empty() {
                        return Ok(SelectedQueuedWorkClaimOutcome::new(
                            None,
                            already_satisfied_batch_ids,
                        ));
                    }
                    let requested_rows = {
                        let mut sql = format!(
                            "SELECT {QUEUED_WORK_COLUMNS}
                                     FROM queued_work_batches
                                     WHERE session_id = ? AND available_at_ms <= ?
                                       AND (claim_token IS NULL
                                            OR claim_session_lease_generation <> ?)
                                       AND batch_id IN (",
                            QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
                        );
                        sql.push_str(&vec!["?"; batch_ids.len()].join(", "));
                        sql.push_str(") ORDER BY enqueue_seq ASC");
                        let mut values: Vec<rusqlite::types::Value> = vec![
                            session_id.clone().into(),
                            (now as i64).into(),
                            sql_session_lease_generation(generation)?.into(),
                        ];
                        values.extend(batch_ids.iter().cloned().map(Into::into));
                        let mut stmt = tx.prepare(&sql).map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                rusqlite::params_from_iter(values.iter()),
                                queued_batch_row_from_sql,
                            )
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    if requested_rows.len() != present_ids.len() {
                        return Ok(SelectedQueuedWorkClaimOutcome::new(
                            None,
                            already_satisfied_batch_ids,
                        ));
                    }
                    let involved_claim_ids = requested_rows
                        .iter()
                        .filter_map(|row| row.claim_id.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>();
                    let mut validation_rows = requested_rows.clone();
                    if !involved_claim_ids.is_empty() {
                        let mut sql = format!(
                            "SELECT {QUEUED_WORK_COLUMNS}
                                     FROM queued_work_batches
                                     WHERE session_id = ? AND available_at_ms <= ?
                                       AND (claim_token IS NULL
                                            OR claim_session_lease_generation <> ?)
                                       AND claim_id IN (",
                            QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
                        );
                        sql.push_str(&vec!["?"; involved_claim_ids.len()].join(", "));
                        sql.push_str(") ORDER BY enqueue_seq ASC");
                        let mut values: Vec<rusqlite::types::Value> = vec![
                            session_id.clone().into(),
                            (now as i64).into(),
                            sql_session_lease_generation(generation)?.into(),
                        ];
                        values.extend(involved_claim_ids.iter().cloned().map(Into::into));
                        let mut stmt = tx.prepare(&sql).map_err(sqlite_error)?;
                        let claim_rows = stmt
                            .query_map(
                                rusqlite::params_from_iter(values.iter()),
                                queued_batch_row_from_sql,
                            )
                            .map_err(sqlite_error)?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(sqlite_error)?;
                        validation_rows.extend(claim_rows);
                        validation_rows.sort_by_key(|row| row.enqueue_seq);
                        validation_rows.dedup_by(|left, right| left.batch_id == right.batch_id);
                    }
                    let validation_batch_claims = validation_rows
                        .iter()
                        .map(|row| (row.batch_id.clone(), row.claim_id.clone()))
                        .collect::<Vec<_>>();
                    let interrupted_positions =
                        lash_core::store::queued_work::select_interrupted_exact_claim_indices(
                            &validation_batch_claims,
                            &batch_ids,
                        )
                        .map_err(|required_batch_ids| {
                            StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                                required_batch_ids,
                            }
                        })?;
                    let (mut rows, mut batches) = if let Some(interrupted_positions) =
                        interrupted_positions
                    {
                        let rows = interrupted_positions
                            .into_iter()
                            .map(|position| validation_rows[position].clone())
                            .collect::<Vec<_>>();
                        let batches = rows
                            .iter()
                            .map(|row| queued_work_batch_from_conn(tx, row.clone()))
                            .collect::<Result<Vec<_>, _>>()?;
                        (rows, batches)
                    } else {
                        let mut requested_batches = std::collections::BTreeMap::new();
                        for row in &requested_rows {
                            let batch = queued_work_batch_from_conn(tx, row.clone())?;
                            if batch.work_class() != lash_core::store::QueuedWorkClass::TurnWork {
                                return Ok(SelectedQueuedWorkClaimOutcome::new(
                                    None,
                                    already_satisfied_batch_ids,
                                ));
                            }
                            requested_batches.insert(row.batch_id.clone(), batch);
                        }
                        let span_rows = {
                            let mut stmt = tx
                                .prepare(&format!(
                                    "SELECT {QUEUED_WORK_COLUMNS}
                                         FROM queued_work_batches
                                         WHERE session_id = ?1 AND available_at_ms <= ?2
                                           AND (claim_token IS NULL
                                                OR claim_session_lease_generation <> ?3)
                                           AND enqueue_seq BETWEEN ?4 AND ?5
                                         ORDER BY enqueue_seq ASC",
                                    QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
                                ))
                                .map_err(sqlite_error)?;
                            stmt.query_map(
                                params![
                                    session_id,
                                    now as i64,
                                    sql_session_lease_generation(generation)?,
                                    requested_rows[0].enqueue_seq as i64,
                                    requested_rows
                                        .last()
                                        .expect("requested rows exist")
                                        .enqueue_seq as i64,
                                ],
                                queued_batch_row_from_sql,
                            )
                            .map_err(sqlite_error)?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(sqlite_error)?
                        };
                        let Some(first_position) = span_rows
                            .iter()
                            .position(|row| requested_ids.contains(&row.batch_id))
                        else {
                            return Ok(SelectedQueuedWorkClaimOutcome::new(
                                None,
                                already_satisfied_batch_ids,
                            ));
                        };
                        let rows = span_rows[first_position..]
                            .iter()
                            .take_while(|row| requested_ids.contains(&row.batch_id))
                            .cloned()
                            .collect::<Vec<_>>();
                        let batches = rows
                            .iter()
                            .map(|row| {
                                requested_batches
                                    .get(&row.batch_id)
                                    .expect("contiguous exact row was validated")
                                    .clone()
                            })
                            .collect::<Vec<_>>();
                        (rows, batches)
                    };
                    let candidates = rows
                        .iter()
                        .zip(batches.iter())
                        .map(|(row, batch)| claim_candidate_from_row(row, batch))
                        .collect::<Vec<_>>();
                    let selected_len = match select_exact_turn_work_claim_prefix(
                        &candidates,
                        boundary,
                        &policy,
                        now,
                    )? {
                        TurnWorkClaimPrefix::Selected { len } => len,
                        TurnWorkClaimPrefix::Refused { .. } => {
                            return Ok(SelectedQueuedWorkClaimOutcome::new(
                                None,
                                already_satisfied_batch_ids,
                            ));
                        }
                    };
                    rows.truncate(selected_len);
                    batches.truncate(selected_len);
                    let claim = match claim_queued_work_rows_sqlite(
                        tx,
                        now,
                        &session_id,
                        &owner,
                        generation,
                        batches,
                        &candidates,
                    )? {
                        TxOutcome::Commit(claim) => claim,
                        TxOutcome::Rollback(_) => None,
                    };
                    Ok(SelectedQueuedWorkClaimOutcome::new(
                        claim,
                        already_satisfied_batch_ids,
                    ))
                })(
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
                    "UPDATE queued_work_batches
                     SET claim_id = ?4,
                         claim_token = ?5,
                         claim_session_lease_generation = 0
                     WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3",
                    params![
                        session_id,
                        claim_id,
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
                        "UPDATE queued_work_batches
                         SET claim_id = ?4,
                             claim_token = ?5,
                             claim_session_lease_generation = 0
                         WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3",
                        params![
                            claim.session_id,
                            claim.claim_id,
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
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        let session_id = session_id.to_string();
        let batch_id = batch_id.to_string();
        let now = self.clock.timestamp_ms() as i64;
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Option<QueuedWorkBatch>, StoreError> = (|| {
                    let row = tx
                        .query_row(
                            &format!(
                                "SELECT {QUEUED_WORK_COLUMNS}
                             FROM queued_work_batches
                             WHERE session_id = ?1
                               AND batch_id = ?2
                               AND (claim_token IS NULL OR NOT EXISTS (
                                        SELECT 1 FROM session_execution_leases sel
                                        WHERE sel.session_id = ?1
                                          AND sel.lease_token IS NOT NULL
                                          AND sel.lease_expires_at_ms > ?3
                                          AND sel.lease_fencing_token
                                              = queued_work_batches.claim_session_lease_generation
                                   ))",
                                QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
                            ),
                            params![session_id, batch_id, now],
                            queued_batch_row_from_sql,
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some(row) = row else {
                        return Ok(None);
                    };
                    let batch = queued_work_batch_from_conn(tx, row)?;
                    tx.execute(
                        "DELETE FROM queued_work_batches
                         WHERE session_id = ?1
                           AND batch_id = ?2
                           AND (claim_token IS NULL OR NOT EXISTS (
                                SELECT 1 FROM session_execution_leases sel
                                WHERE sel.session_id = ?1
                                  AND sel.lease_token IS NOT NULL
                                  AND sel.lease_expires_at_ms > ?3
                                  AND sel.lease_fencing_token
                                      = queued_work_batches.claim_session_lease_generation
                           ))",
                        params![session_id, batch_id, now],
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
        session_id: &str,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        let session_id = session_id.to_string();
        let marker = lash_core::store_backend_support::session_command_batch_completion_key(
            &session_id,
            batch_id,
        )?;
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT EXISTS (
                        SELECT 1 FROM runtime_turn_commits
                        WHERE session_id = ?1 AND turn_id = ?2
                     )",
                    params![session_id, marker],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }

    async fn list_queued_work(&self, session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<QueuedWorkBatch>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = conn
                            .prepare(&format!(
                                "SELECT {QUEUED_WORK_COLUMNS}
                                 FROM queued_work_batches
                                 WHERE session_id = ?1
                                 ORDER BY enqueue_seq ASC",
                                QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
                            ))
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(params![session_id], queued_batch_row_from_sql)
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    rows.into_iter()
                        .map(|row| queued_work_batch_from_conn(conn, row))
                        .collect()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &str,
    ) -> Result<lash_core::store::PendingSessionWorkOrdering, StoreError> {
        let session_id = session_id.to_string();
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
                                "WITH earliest_command AS (
                                    SELECT enqueued_at_ms, enqueue_seq
                                    FROM queued_work_batches AS queued
                                    WHERE session_id = ?1
                                      AND work_kind = ?4
                                      AND (claim_token IS NULL OR NOT EXISTS (
                                           SELECT 1 FROM session_execution_leases AS lease
                                           WHERE lease.session_id = ?1
                                             AND lease.lease_token IS NOT NULL
                                             AND lease.lease_expires_at_ms > ?2
                                             AND lease.lease_fencing_token
                                                 = queued.claim_session_lease_generation
                                      ))
                                    ORDER BY enqueued_at_ms ASC, enqueue_seq ASC
                                    LIMIT 1
                                 ), earliest_input AS (
                                    SELECT enqueued_at_ms, enqueue_seq
                                    FROM pending_turn_inputs AS input
                                    WHERE session_id = ?1
                                      AND state = ?3
                                      AND (claim_token IS NULL OR NOT EXISTS (
                                           SELECT 1 FROM session_execution_leases AS lease
                                           WHERE lease.session_id = ?1
                                             AND lease.lease_token IS NOT NULL
                                             AND lease.lease_expires_at_ms > ?2
                                             AND lease.lease_fencing_token
                                                 = input.claim_session_lease_generation
                                      ))
                                    ORDER BY enqueued_at_ms ASC, enqueue_seq ASC
                                    LIMIT 1
                                 )
                                 SELECT command.enqueued_at_ms, command.enqueue_seq,
                                        input.enqueued_at_ms, input.enqueue_seq
                                 FROM (SELECT 1) AS singleton
                                 LEFT JOIN earliest_command AS command ON TRUE
                                 LEFT JOIN earliest_input AS input ON TRUE",
                                params![
                                    session_id,
                                    now as i64,
                                    lash_core::TurnInputState::DeferredNextTurn.as_str(),
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
        session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        let session_id = session_id.to_string();
        let now = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<QueuedWorkBatch>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = conn
                            .prepare(&format!(
                                "SELECT {QUEUED_WORK_COLUMNS}
                                 FROM queued_work_batches
                                 WHERE session_id = ?1
                                   AND (claim_token IS NULL OR NOT EXISTS (
                                        SELECT 1 FROM session_execution_leases sel
                                        WHERE sel.session_id = ?1
                                          AND sel.lease_token IS NOT NULL
                                          AND sel.lease_expires_at_ms > ?2
                                          AND sel.lease_fencing_token
                                              = queued_work_batches.claim_session_lease_generation
                                   ))
                                 ORDER BY enqueue_seq ASC",
                                QUEUED_WORK_COLUMNS = QUEUED_WORK_COLUMNS.join(", ")
                            ))
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(params![session_id, now as i64], queued_batch_row_from_sql)
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    rows.into_iter()
                        .map(|row| queued_work_batch_from_conn(conn, row))
                        .collect()
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }
}
