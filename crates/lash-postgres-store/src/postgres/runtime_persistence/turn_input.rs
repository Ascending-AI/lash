use super::*;

#[async_trait::async_trait]
impl TurnInputStore for PostgresSessionStore {
    async fn record_turn_cancel_request(
        &self,
        request: lash_core::facade_support::TurnCancelRequest,
    ) -> Result<lash_core::TurnCancelRequestRecord, StoreError> {
        let session_id = &request.address.session_id;
        let turn_id = &request.address.turn_id;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        // First writer wins, except that a stronger mode escalates the durable
        // request in place; the repair outcome accumulated so far stays
        // attached because the affected-input arrays are untouched.
        match load_turn_cancel_request_tx(&mut tx, session_id, turn_id).await? {
            Some(existing) if request.mode.is_stronger_than(existing.request.mode) => {
                sqlx::query(
                    "UPDATE lash_turn_cancel_requests
                     SET request_id = $3, origin = $4, reason = $5, disposition = $6,
                         mode = $7
                     WHERE session_id = $1 AND turn_id = $2",
                )
                .bind(session_id.as_str())
                .bind(turn_id.as_str())
                .bind(&request.request_id)
                .bind(&request.origin)
                .bind(&request.reason)
                .bind(turn_cancel_disposition_wire(request.undelivered))
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            }
            Some(existing) => {
                tx.commit().await.map_err(store_sqlx_error)?;
                return Ok(existing);
            }
            None => {
                sqlx::query(
                    "INSERT INTO lash_turn_cancel_requests (
                         session_id, turn_id, request_id, origin, reason, disposition, mode
                     ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(session_id.as_str())
                .bind(turn_id.as_str())
                .bind(&request.request_id)
                .bind(&request.origin)
                .bind(&request.reason)
                .bind(turn_cancel_disposition_wire(request.undelivered))
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            }
        }
        let record = load_turn_cancel_request_tx(&mut tx, session_id, turn_id)
            .await?
            .ok_or_else(|| {
                StoreError::Backend("turn cancel request insert disappeared".to_string())
            })?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(record)
    }

    async fn turn_cancel_request(
        &self,
        address: &lash_core::facade_support::TurnAddress,
    ) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
        load_turn_cancel_request_pg(&self.pool, &address.session_id, &address.turn_id).await
    }

    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, &draft.session_id).await?;
        let now = self.clock.timestamp_ms();
        let enqueue_seq: i64 = sqlx::query_scalar(
            "SELECT nextval(pg_get_serial_sequence(
                'lash_pending_turn_inputs',
                'enqueue_seq'
             ))",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let enqueue_seq_u64 = u64_from_sql("PendingTurnInput", "enqueue_seq", enqueue_seq)?;
        let input_id = draft.input_id.clone().unwrap_or_else(|| {
            lash_core::store_backend_support::derive_pending_turn_input_id(
                &draft.session_id,
                draft.source_key.as_deref(),
                now,
                enqueue_seq_u64,
            )
        });
        let state = draft.ingress.initial_state();
        let ingress_json = encode_json(&draft.ingress)?;
        let input_json = encode_json(&draft.input)?;
        let input = if let Some(source_key) = draft.source_key.as_deref() {
            let row = sqlx::query(
                "INSERT INTO lash_pending_turn_inputs (
                    enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json,
                    enqueued_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (session_id, source_key) DO UPDATE
                 SET source_key = lash_pending_turn_inputs.source_key
                 RETURNING enqueue_seq, input_id, session_id, source_key, ingress_json,
                           state, input_json, enqueued_at_ms, claim_id, claim_fencing_token,
                           claim_owner_id, claim_owner_incarnation_id,
                           claim_token, claim_session_lease_generation",
            )
            .bind(enqueue_seq)
            .bind(&input_id)
            .bind(draft.session_id.as_str())
            .bind(source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
            if !draft.submitted_content_matches(&input).map_err(|err| {
                StoreError::Backend(format!(
                    "failed to compare pending turn input submission: {err}"
                ))
            })? {
                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                    session_id: draft.session_id.clone(),
                    source_key: source_key.to_string(),
                    existing_input_id: input.input_id.clone(),
                });
            }
            input
        } else {
            sqlx::query(
                "INSERT INTO lash_pending_turn_inputs (
                    enqueue_seq, input_id, session_id, source_key, ingress_json, state, input_json,
                    enqueued_at_ms
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(enqueue_seq)
            .bind(&input_id)
            .bind(draft.session_id.as_str())
            .bind(&draft.source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            load_pending_turn_input(&mut tx, &draft.session_id, &input_id)
                .await?
                .ok_or_else(|| {
                    StoreError::Backend("pending turn input insert disappeared".to_string())
                })?
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(input)
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::PendingTurnInput>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(&format!(
            "SELECT {PENDING_TURN_INPUT_COLUMNS}
             FROM lash_pending_turn_inputs
             WHERE session_id = $1
               AND state IN ($2, $3)
               AND (claim_token IS NULL OR NOT EXISTS (
                    SELECT 1 FROM lash_session_execution_leases sel
                    WHERE sel.session_id = $1
                      AND sel.lease_token IS NOT NULL
                      AND sel.lease_expires_at_ms > $4
                      AND sel.lease_fencing_token
                          = lash_pending_turn_inputs.claim_session_lease_generation
               ))
             ORDER BY enqueue_seq ASC"
        ))
        .bind(session_id.as_str())
        .bind(lash_core::TurnInputState::PendingActive.as_str())
        .bind(lash_core::TurnInputState::DeferredNextTurn.as_str())
        .bind(now as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let inputs = rows
            .into_iter()
            .map(pending_turn_input_row)
            .map(|row| row.and_then(pending_turn_input_from_row))
            .collect::<Result<Vec<_>, StoreError>>()?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(inputs)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::TurnInputApplication>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT turn_id, result_json
             FROM lash_runtime_turn_commits
             WHERE session_id = $1",
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        let mut commits = Vec::with_capacity(rows.len());
        for row in rows {
            let turn_id = row.get::<String, _>(0);
            let result_json: String = row.get(1);
            let result: RuntimeCommitReceipt =
                store_decode_json(&result_json, "runtime turn commit result")?;
            commits.push((
                result.head_revision,
                turn_id,
                result.turn_input_applications,
            ));
        }
        commits.sort_by(|left, right| (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str())));
        Ok(commits
            .into_iter()
            .flat_map(|(_, _, applications)| applications)
            .collect())
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelReceipt>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let targets = targets.to_vec();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome =
                match load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &target, true)
                    .await?
                {
                    Some(row) => cancel_pending_turn_input_row_tx(&mut tx, row, now).await?,
                    None => lash_core::PendingTurnInputCancelOutcome::NotFound,
                };
            results.push(lash_core::PendingTurnInputCancelReceipt { target, outcome });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(results)
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let anchor = anchor.clone();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let Some(anchor_row) =
            load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &anchor, true).await?
        else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { anchor });
        };
        let rows = sqlx::query(&format!(
            "SELECT {PENDING_TURN_INPUT_COLUMNS}
             FROM lash_pending_turn_inputs
             WHERE session_id = $1 AND enqueue_seq >= $2
             ORDER BY enqueue_seq ASC
             FOR UPDATE"
        ))
        .bind(session_id.as_str())
        .bind(anchor_row.enqueue_seq as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            outcomes.push(
                cancel_pending_turn_input_row_tx(&mut tx, pending_turn_input_row(row)?, now)
                    .await?,
            );
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes { anchor, outcomes })
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core::TurnId,
        checkpoint: lash_core::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_postgres(
            &self.pool,
            #[cfg(any(test, feature = "testing"))]
            self.lease_clock_for_testing.as_ref(),
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::ActiveTurn {
                turn_id: turn_id.clone(),
                checkpoint,
            },
        )
        .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<lash_core::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_postgres(
            &self.pool,
            #[cfg(any(test, feature = "testing"))]
            self.lease_clock_for_testing.as_ref(),
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core::TurnInputClaimMode::NextTurn,
        )
        .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core::TurnInputClaim,
    ) -> Result<(), StoreError> {
        let restored_state = match claim.mode {
            lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query(
            "UPDATE lash_pending_turn_inputs
             SET state = CASE
                     WHEN state = $4 THEN $5
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE session_id = $1 AND claim_id = $2 AND claim_token = $3",
        )
        .bind(claim.session_id.as_str())
        .bind(&claim.claim_id)
        .bind(&claim.lease_token)
        .bind(lash_core::TurnInputState::Accepted.as_str())
        .bind(restored_state.as_str())
        .execute(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[lash_core::TurnInputClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        // FIG-1573: restore each claim to its own mode's pre-claim state, exactly
        // as the singular sibling does. Hardcoding `pending_active` sent a
        // next-turn claim's rows to a state only an active-turn claim can reach,
        // stranding them behind a turn id that will never exist again. Both mode
        // partitions run in ONE transaction: a batch abandon is one caller
        // giving up one set of rows, and a failure between two statements would
        // leave half the batch claimed by a claim id the caller has dropped.
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        for (mode_state, restored_state) in [
            (
                lash_core::TurnInputState::PendingActive,
                lash_core::TurnInputState::PendingActive,
            ),
            (
                lash_core::TurnInputState::DeferredNextTurn,
                lash_core::TurnInputState::DeferredNextTurn,
            ),
        ] {
            let batch = claims
                .iter()
                .filter(|claim| {
                    let claim_state = match claim.mode {
                        lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                            lash_core::TurnInputState::PendingActive
                        }
                        lash_core::TurnInputClaimMode::NextTurn => {
                            lash_core::TurnInputState::DeferredNextTurn
                        }
                    };
                    claim_state == mode_state
                })
                .collect::<Vec<_>>();
            if batch.is_empty() {
                continue;
            }
            let accepted_state = lash_core::store_backend_support::state_sql_literal(
                lash_core::TurnInputState::Accepted,
            );
            let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(
                "UPDATE lash_pending_turn_inputs
                 SET state = CASE
                         WHEN state = ",
            );
            query.push(accepted_state).push(" THEN ");
            query.push_bind(restored_state.as_str());
            query.push(
                "     ELSE state
                     END,
                     claim_id = NULL,
                     claim_owner_id = NULL,
                     claim_owner_incarnation_id = NULL,
                     claim_token = NULL,
                     claim_session_lease_generation = 0
                 WHERE (session_id, claim_id, claim_token) IN ",
            );
            query.push_tuples(batch, |mut row, claim| {
                row.push_bind(claim.session_id.as_str())
                    .push_bind(&claim.claim_id)
                    .push_bind(&claim.lease_token);
            });
            query
                .build()
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> Result<lash_core::TurnCancelInputOutcome, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        // Re-validated inside this transaction, not upstream: the lane can be
        // displaced between an upstream check and this write, and a
        // stale-generation repair would clear the new holder's claim columns.
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let repaired = defer_orphaned_active_turn_inputs_tx(
            &mut tx,
            session_id,
            session_execution_lease.fencing_token,
            scope,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(repaired)
    }
}
