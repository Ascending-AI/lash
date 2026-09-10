use super::*;

#[async_trait::async_trait]
impl TurnInputStore for Store {
    async fn turn_is_committed(
        &self,
        address: &lash_core::facade_support::TurnAddress,
    ) -> Result<bool, StoreError> {
        let session_id = address.session_id.clone();
        let operation_key =
            lash_core::OperationId::turn(&address.session_id, &address.turn_id, "final")
                .storage_key()?;
        self.conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_turn_commits WHERE session_id = ?1 AND turn_id = ?2)",
                    params![session_id.as_str(), operation_key],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }

    async fn record_turn_cancel_request(
        &self,
        request: lash_core::facade_support::TurnCancelRequest,
    ) -> Result<lash_core::TurnCancelRequestRecord, StoreError> {
        let session_id = request.address.session_id.clone();
        let turn_id = request.address.turn_id.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let operation_key = lash_core::OperationId::turn(
                        &session_id,
                        &turn_id,
                        "final",
                    )
                    .storage_key()?;
                    let committed = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM runtime_turn_commits WHERE session_id = ?1 AND turn_id = ?2)",
                            params![session_id.as_str(), operation_key],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(sqlite_error)?;
                    if committed {
                        return Ok(lash_core::TurnCancelRequestRecord {
                            request,
                            outcome: None,
                        });
                    }
                    // First writer wins, except that a stronger mode escalates
                    // the durable request; the repair outcome accumulated so
                    // far stays attached.
                    if let Some(mut existing) =
                        load_turn_cancel_request_conn(tx, &session_id, &turn_id)?
                    {
                        if request.mode.is_stronger_than(existing.request.mode) {
                            existing.request = request;
                            tx.execute(
                                "UPDATE turn_cancel_requests SET record_json = ?3
                                 WHERE session_id = ?1 AND turn_id = ?2",
                                params![
                                    session_id.as_str(),
                                    turn_id.as_str(),
                                    encode_json(&existing)?
                                ],
                            )
                            .map_err(sqlite_error)?;
                        }
                        return Ok(existing);
                    }
                    let record = lash_core::TurnCancelRequestRecord {
                        request,
                        outcome: None,
                    };
                    tx.execute(
                        "INSERT OR IGNORE INTO turn_cancel_requests
                         (session_id, turn_id, record_json) VALUES (?1, ?2, ?3)",
                        params![session_id.as_str(), turn_id.as_str(), encode_json(&record)?],
                    )
                    .map_err(sqlite_error)?;
                    load_turn_cancel_request_conn(tx, &session_id, &turn_id)?.ok_or_else(|| {
                        StoreError::Backend("turn cancel request insert disappeared".to_string())
                    })
                })();
                Ok(match outcome {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn turn_cancel_request(
        &self,
        address: &lash_core::facade_support::TurnAddress,
    ) -> Result<Option<lash_core::TurnCancelRequestRecord>, StoreError> {
        let session_id = address.session_id.clone();
        let turn_id = address.turn_id.clone();
        self.conn
            .call(move |conn| Ok(load_turn_cancel_request_conn(conn, &session_id, &turn_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn turn_cancel_request_intent(
        &self,
        address: &lash_core::facade_support::TurnAddress,
    ) -> Result<Option<lash_core::facade_support::TurnCancelRequest>, StoreError> {
        Ok(self
            .turn_cancel_request(address)
            .await?
            .map(|record| record.request))
    }

    async fn reconcile_turn_cancel_winner(
        &self,
        address: &lash_core::facade_support::TurnAddress,
        evidence: &lash_core::facade_support::TurnCancellationEvidence,
    ) -> Result<(), StoreError> {
        let session_id = address.session_id.clone();
        let turn_id = address.turn_id.clone();
        let evidence = evidence.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    reconcile_turn_cancel_winner_conn(tx, &session_id, &turn_id, &evidence)
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core::PendingTurnInputDraft,
    ) -> Result<lash_core::PendingTurnInput, StoreError> {
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::PendingTurnInput, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &draft.session_id)?;
                    if let Some(source_key) = draft.source_key.as_deref() {
                        let existing_id: Option<String> = tx
                            .query_row(
                                "SELECT input_id
                                 FROM pending_turn_inputs
                                 WHERE session_id = ?1 AND source_key = ?2",
                                params![draft.session_id.as_str(), source_key],
                                |row| row.get(0),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some(input_id) = existing_id {
                            let existing = load_pending_turn_input_by_id_conn(
                                tx,
                                &draft.session_id,
                                &input_id,
                            )?
                            .ok_or_else(|| {
                                StoreError::Backend(
                                    "pending turn input source row disappeared".to_string(),
                                )
                            })?;
                            if !draft.submitted_content_matches(&existing).map_err(|err| {
                                StoreError::Backend(format!(
                                    "failed to compare pending turn input submission: {err}"
                                ))
                            })? {
                                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                                    session_id: draft.session_id.clone(),
                                    source_key: source_key.to_string(),
                                    existing_input_id: existing.input_id.clone(),
                                });
                            }
                            return Ok(existing);
                        }
                    }
                    let input_id = draft.input_id.clone().unwrap_or_else(|| {
                        lash_core::store_backend_support::derive_pending_turn_input_id(
                            &draft.session_id,
                            draft.source_key.as_deref(),
                            now,
                            nonce,
                        )
                    });
                    let state = draft.ingress.initial_state();
                    tx.execute(
                        "INSERT INTO pending_turn_inputs (
                            input_id, session_id, source_key, ingress_json, state,
                            input_json, enqueued_at_ms
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            input_id.as_str(),
                            draft.session_id.as_str(),
                            draft.source_key.as_deref(),
                            encode_json(&draft.ingress)?,
                            state.as_str(),
                            encode_json(&draft.input)?,
                            now as i64,
                        ],
                    )
                    .map_err(sqlite_error)?;
                    load_pending_turn_input_by_id_conn(tx, &draft.session_id, &input_id)?
                        .ok_or_else(|| {
                            StoreError::Backend("pending turn input insert disappeared".to_string())
                        })
                })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::PendingTurnInput>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let now = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<lash_core::PendingTurnInput>, StoreError> = (|| {
                    let rows = {
                        let mut stmt = conn
                            .prepare(&format!(
                                "SELECT {PENDING_TURN_INPUT_COLUMNS}
                                 FROM pending_turn_inputs
                                 WHERE session_id = ?1
                                   AND state IN (?2, ?3)
                                   AND (claim_token IS NULL OR NOT EXISTS (
                                        SELECT 1 FROM session_execution_leases sel
                                        WHERE sel.session_id = ?1
                                          AND sel.lease_token IS NOT NULL
                                          AND sel.lease_expires_at_ms > ?4
                                          AND sel.lease_fencing_token
                                              = pending_turn_inputs.claim_session_lease_generation
                                   ))
                                 ORDER BY enqueue_seq ASC"
                            ))
                            .map_err(sqlite_error)?;
                        let rows = stmt
                            .query_map(
                                params![
                                    session_id.as_str(),
                                    lash_core::TurnInputState::PendingActive.as_str(),
                                    lash_core::TurnInputState::DeferredNextTurn.as_str(),
                                    now as i64
                                ],
                                pending_turn_input_row_from_sql,
                            )
                            .map_err(sqlite_error)?;
                        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                    };
                    rows.into_iter().map(pending_turn_input_from_row).collect()
                })(
                );
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core::TurnInputApplication>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT turn_id, result_json
                             FROM runtime_turn_commits
                             WHERE session_id = ?1",
                        )
                        .map_err(sqlite_error)?;
                    let rows = stmt
                        .query_map(params![session_id.as_str()], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(sqlite_error)?;
                    let mut commits = Vec::new();
                    for row in rows {
                        let (turn_id, result_json) = row.map_err(sqlite_error)?;
                        let result: RuntimeCommitReceipt = serde_json::from_str(&result_json)
                            .map_err(|err| {
                                StoreError::Backend(format!(
                                    "failed to decode runtime turn commit result: {err}"
                                ))
                            })?;
                        commits.push((
                            result.head_revision,
                            turn_id,
                            result.turn_input_applications,
                        ));
                    }
                    commits.sort_by(|left, right| {
                        (left.0, left.1.as_str()).cmp(&(right.0, right.1.as_str()))
                    });
                    Ok(commits
                        .into_iter()
                        .flat_map(|(_, _, applications)| applications)
                        .collect())
                })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core::PendingTurnInputCancelReceipt>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let targets = targets.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Vec<lash_core::PendingTurnInputCancelReceipt>, StoreError> =
                    (|| {
                        let mut results = Vec::with_capacity(targets.len());
                        for target in targets {
                            let outcome = match load_pending_turn_input_row_by_target_conn(
                                tx,
                                &session_id,
                                &target,
                            )? {
                                Some(row) => cancel_pending_turn_input_row_conn(tx, row, now)?,
                                None => lash_core::PendingTurnInputCancelOutcome::NotFound,
                            };
                            results
                                .push(lash_core::PendingTurnInputCancelReceipt { target, outcome });
                        }
                        Ok(results)
                    })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core::PendingTurnInputCancelTarget,
    ) -> Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let anchor = anchor.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::PendingTurnInputSuffixCancelOutcome, StoreError> =
                    (|| {
                        let Some(anchor_row) =
                            load_pending_turn_input_row_by_target_conn(tx, &session_id, &anchor)?
                        else {
                            return Ok(
                                lash_core::PendingTurnInputSuffixCancelOutcome::AnchorNotFound {
                                    anchor,
                                },
                            );
                        };
                        let rows = {
                            let mut stmt = tx
                                .prepare(&format!(
                                    "SELECT {PENDING_TURN_INPUT_COLUMNS}
                                     FROM pending_turn_inputs
                                     WHERE session_id = ?1 AND enqueue_seq >= ?2
                                     ORDER BY enqueue_seq ASC"
                                ))
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![session_id.as_str(), anchor_row.enqueue_seq as i64],
                                    pending_turn_input_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                        };
                        let mut outcomes = Vec::with_capacity(rows.len());
                        for row in rows {
                            outcomes.push(cancel_pending_turn_input_row_conn(tx, row, now)?);
                        }
                        Ok(lash_core::PendingTurnInputSuffixCancelOutcome::Outcomes {
                            anchor,
                            outcomes,
                        })
                    })();
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
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
        claim_pending_turn_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
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
        claim_pending_turn_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
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
        let session_id = claim.session_id.clone();
        let claim_id = claim.claim_id.clone();
        let lease_token = claim.lease_token.clone();
        let restored_state = match claim.mode {
            lash_core::TurnInputClaimMode::ActiveTurn { .. } => {
                lash_core::TurnInputState::PendingActive
            }
            lash_core::TurnInputClaimMode::NextTurn => lash_core::TurnInputState::DeferredNextTurn,
        };
        self.conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE pending_turn_inputs
                     SET state = CASE
                             WHEN state = ?4 THEN ?5
                             ELSE state
                         END,
                         claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = ?1 AND claim_id = ?2 AND claim_token = ?3",
                    params![
                        session_id.as_str(),
                        claim_id.as_str(),
                        lease_token,
                        lash_core::TurnInputState::Accepted.as_str(),
                        restored_state.as_str(),
                    ],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn orphaned_active_turn_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: lash_core::OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<lash_core::TurnId>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let session_execution_lease = session_execution_lease.clone();
        let scope = OwnedOrphanedScope::from(scope);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Vec<lash_core::TurnId>, StoreError> = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    orphaned_active_turn_ids_conn(
                        tx,
                        &session_id,
                        session_execution_lease.fencing_token,
                        scope.borrow(),
                    )
                })();
                Ok(match outcome {
                    Ok(repaired) => TxOutcome::Commit(Ok(repaired)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        turn_id: &lash_core::TurnId,
        decision: lash_core::TurnCancelRepairDecision,
    ) -> Result<lash_core::TurnCancelInputOutcome, StoreError> {
        let session_id = session_id.clone();
        let session_execution_lease = session_execution_lease.clone();
        let turn_id = turn_id.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_execution_lease_conn(
                        tx,
                        &session_id,
                        &session_execution_lease,
                        now,
                    )?;
                    repair_orphaned_active_turn_inputs_conn(
                        tx,
                        &session_id,
                        session_execution_lease.fencing_token,
                        &turn_id,
                        &decision,
                    )
                })();
                Ok(match outcome {
                    Ok(repaired) => TxOutcome::Commit(Ok(repaired)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[lash_core::TurnInputClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        // FIG-1573: the restored state is the claim's own mode, exactly as the
        // singular sibling resolves it. A next-turn claim restored to
        // `pending_active` would be addressable only by a turn id that never
        // claimed it. Both partitions are written in ONE transaction: a batch
        // abandon is one caller giving up one set of rows, and a crash between
        // two statements would leave half the batch claimed by a claim id the
        // caller has already dropped.
        let mut statements = Vec::new();
        for (mode, claims) in [
            (
                lash_core::TurnInputState::PendingActive,
                claims
                    .iter()
                    .filter(|claim| {
                        matches!(claim.mode, lash_core::TurnInputClaimMode::ActiveTurn { .. })
                    })
                    .collect::<Vec<_>>(),
            ),
            (
                lash_core::TurnInputState::DeferredNextTurn,
                claims
                    .iter()
                    .filter(|claim| matches!(claim.mode, lash_core::TurnInputClaimMode::NextTurn))
                    .collect::<Vec<_>>(),
            ),
        ] {
            if claims.is_empty() {
                continue;
            }
            statements.push(abandon_turn_input_claims_statement(&claims, mode));
        }
        self.conn
            .write(move |tx| {
                for (sql, values) in &statements {
                    tx.execute(sql, rusqlite::params_from_iter(values.iter()))?;
                }
                Ok(())
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }
}

/// One `UPDATE` restoring a batch of abandoned claims to `restored_state`.
///
/// Split out so the plural abandon can execute every mode partition inside a
/// single transaction (FIG-1573).
fn abandon_turn_input_claims_statement(
    claims: &[&lash_core::TurnInputClaim],
    restored_state: lash_core::TurnInputState,
) -> (String, Vec<rusqlite::types::Value>) {
    let accepted_state =
        lash_core::store_backend_support::state_sql_literal(lash_core::TurnInputState::Accepted);
    let mut sql = format!(
        "UPDATE pending_turn_inputs
             SET state = CASE
                     WHEN state = {accepted_state} THEN ?
                     ELSE state
                 END,
                 claim_id = NULL,
                 claim_owner_id = NULL,
                 claim_owner_incarnation_id = NULL,
                 claim_token = NULL,
                 claim_session_lease_generation = 0
             WHERE (session_id, claim_id, claim_token) IN ("
    );
    let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(claims.len() * 3 + 1);
    values.push(restored_state.as_str().to_string().into());
    for (index, claim) in claims.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?, ?)");
        values.push(claim.session_id.as_str().to_string().into());
        values.push(claim.claim_id.clone().into());
        values.push(claim.lease_token.clone().into());
    }
    sql.push(')');
    (sql, values)
}
