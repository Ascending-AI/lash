use super::*;

#[async_trait::async_trait]
impl TurnInputStore for Store {
    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core_execution::PendingTurnInputDraft,
    ) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
        let nonce = self.commit_count.fetch_add(1, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::PendingTurnInput, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &draft.session_id)?;
                    let submission_digest = draft.submission_digest().map_err(|err| {
                        StoreError::Backend(format!(
                            "failed to digest pending turn input submission: {err}"
                        ))
                    })?;
                    if let Some(source_key) = draft.source_key.as_deref() {
                        let existing: Option<(String, String)> = tx
                            .query_row(
                                crate::turn_ingress::turn_ingress_sql()
                                    .pending_inputs_sqlite
                                    .select_id_by_source_key
                                    .sql(),
                                params![draft.session_id.as_str(), source_key],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((input_id, existing_digest)) = existing {
                            if existing_digest != submission_digest {
                                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                                    session_id: draft.session_id.clone(),
                                    source_key: source_key.to_string(),
                                    existing_input_id: input_id.into(),
                                });
                            }
                            return load_pending_turn_input_by_id_conn(
                                tx,
                                &draft.session_id,
                                &input_id,
                            )?
                            .ok_or_else(|| {
                                StoreError::Backend(
                                    "pending turn input source row disappeared".to_string(),
                                )
                            });
                        }
                    }
                    if let Some(input_id) = draft.input_id.as_deref() {
                        let holder: Option<(String, String)> = tx
                            .query_row(
                                crate::turn_ingress::turn_ingress_sql()
                                    .pending_inputs_sqlite
                                    .select_session_by_input_id
                                    .sql(),
                                params![input_id],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((holder, existing_digest)) = holder {
                            let existing = load_pending_turn_input_by_id_conn(
                                tx,
                                &SessionId::from(holder),
                                input_id,
                            )?
                            .ok_or_else(|| {
                                StoreError::Backend(
                                    "pending turn input id row disappeared".to_string(),
                                )
                            })?;
                            return draft.adopt_provisioned_row(existing, &existing_digest);
                        }
                    }
                    let input_id = draft.input_id.clone().unwrap_or_else(|| {
                        lash_core_execution::store_backend_support::derive_pending_turn_input_id(
                            &draft.session_id,
                            draft.source_key.as_deref(),
                            now,
                            nonce,
                        )
                    });
                    let state = lash_core_execution::TurnInputState::open(draft.ingress.clone());
                    tx.execute(
                        crate::turn_ingress::turn_ingress_sql()
                            .pending_inputs_sqlite
                            .insert_new
                            .sql(),
                        params![
                            input_id.as_str(),
                            draft.session_id.as_str(),
                            draft.source_key.as_deref(),
                            encode_json(&draft.ingress)?,
                            state.as_str(),
                            encode_json(&draft.input)?,
                            submission_digest.as_str(),
                            now as i64,
                        ],
                    )
                    .map_err(|err| {
                        crate::sqlite_pending_turn_input_insert_error(
                            err,
                            &draft.session_id,
                            &input_id,
                        )
                    })?;
                    load_pending_turn_input_by_id_conn(tx, &draft.session_id, &input_id)?
                        .ok_or_else(|| {
                            StoreError::Backend("pending turn input insert disappeared".to_string())
                        })
                })(
                );
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
    ) -> Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let now = self.clock.timestamp_ms();
        self.conn
            .call(move |conn| {
                let outcome: Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> =
                    (|| {
                        let rows = {
                            let mut stmt = conn
                                .prepare(
                                    crate::turn_ingress::turn_ingress_sql()
                                        .pending_inputs
                                        .list_undelivered
                                        .sql(),
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![session_id.as_str(), now as i64],
                                    pending_turn_input_read_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                        };
                        rows.into_iter()
                            .map(pending_turn_input_read_from_row)
                            .collect()
                    })();
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &SessionId,
        targets: &[lash_core_execution::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core_execution::PendingTurnInputCancelReceipt>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let targets = targets.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<
                    Vec<lash_core_execution::PendingTurnInputCancelReceipt>,
                    StoreError,
                > = (|| {
                    // Every input this cancel names, so a bound row whose
                    // receipt is cancelled alongside it may go (FIG-3589).
                    let mut covered = std::collections::BTreeSet::new();
                    for target in &targets {
                        if let Some(row) =
                            load_pending_turn_input_row_by_target_conn(tx, &session_id, target)?
                        {
                            covered.insert(lash_core_execution::InputId::from(row.input_id));
                        }
                    }
                    let mut results = Vec::with_capacity(targets.len());
                    for target in targets {
                        let outcome = match load_pending_turn_input_row_by_target_conn(
                            tx,
                            &session_id,
                            &target,
                        )? {
                            Some(row) => {
                                cancel_pending_turn_input_row_conn(tx, row, now, &covered)?
                            }
                            None => lash_core_execution::PendingTurnInputCancelOutcome::NotFound,
                        };
                        results.push(lash_core_execution::PendingTurnInputCancelReceipt {
                            target,
                            outcome,
                        });
                    }
                    let released: Option<(String, i64)> = tx
                        .query_row(
                            crate::turn_ingress::turn_ingress_sql()
                                .family
                                .delete_released_turn_park_returning
                                .sql(),
                            params![session_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    if let Some((released_turn_id, released_park_id)) = released {
                        crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                            tx,
                            &session_id,
                            &released_turn_id,
                            released_park_id,
                            &lash_core_execution::store::TurnParkEventKind::Cancelled {
                                cause: lash_core_execution::store::ParkCancelCause::InputWithdrawn,
                            },
                            crate::clamp_epoch_ms(now),
                        )?;
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
        anchor: &lash_core_execution::PendingTurnInputCancelTarget,
    ) -> Result<lash_core_execution::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let anchor = anchor.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::PendingTurnInputSuffixCancelOutcome, StoreError> =
                    (|| {
                        let Some(anchor_row) =
                            load_pending_turn_input_row_by_target_conn(tx, &session_id, &anchor)?
                        else {
                            return Ok(
                                lash_core_execution::PendingTurnInputSuffixCancelOutcome::AnchorNotFound {
                                    anchor,
                                },
                            );
                        };
                        let rows = {
                            let mut stmt = tx
                                .prepare(
                                    crate::turn_ingress::turn_ingress_sql()
                                        .pending_inputs_sqlite
                                        .select_suffix
                                        .sql(),
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![session_id.as_str(), anchor_row.enqueue_seq as i64],
                                    pending_turn_input_row_from_sql,
                                )
                                .map_err(sqlite_error)?;
                            rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
                        };
                        let covered = rows
                            .iter()
                            .map(|row| lash_core_execution::InputId::from(row.input_id.clone()))
                            .collect::<std::collections::BTreeSet<_>>();
                        let mut outcomes = Vec::with_capacity(rows.len());
                        for row in rows {
                            outcomes.push(cancel_pending_turn_input_row_conn(
                                tx, row, now, &covered,
                            )?);
                        }
                        let released: Option<(String, i64)> = tx
                            .query_row(
                                crate::turn_ingress::turn_ingress_sql()
                                    .family
                                    .delete_released_turn_park_returning
                                    .sql(),
                                params![session_id.as_str()],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((released_turn_id, released_park_id)) = released {
                            crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                                tx,
                                &session_id,
                                &released_turn_id,
                                released_park_id,
                                &lash_core_execution::store::TurnParkEventKind::Cancelled {
                                    cause:
                                        lash_core_execution::store::ParkCancelCause::InputWithdrawn,
                                },
                                crate::clamp_epoch_ms(now),
                            )?;
                        }
                        Ok(lash_core_execution::PendingTurnInputSuffixCancelOutcome::Outcomes {
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
        turn_id: &lash_core_execution::TurnId,
        checkpoint: lash_core_execution::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<lash_core_execution::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core_execution::TurnInputClaimMode::ActiveTurn {
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
    ) -> Result<Option<lash_core_execution::TurnInputClaim>, StoreError> {
        claim_pending_turn_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
            session_id,
            session_execution_lease,
            owner,
            max_inputs,
            lash_core_execution::TurnInputClaimMode::NextTurn,
        )
        .await
    }

    async fn abandon_turn_input_claim(
        &self,
        claim: &lash_core_execution::TurnInputClaim,
    ) -> Result<(), StoreError> {
        let session_id = claim.session_id.clone();
        let claim_id = claim.claim_id.clone();
        let lease_token = claim.lease_token.clone();
        self.conn
            .write(move |tx| {
                tx.execute(
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs_sqlite
                        .abandon_claim
                        .sql(),
                    params![
                        session_id.as_str(),
                        claim_id.as_str(),
                        lease_token,
                        lash_core_execution::runtime::TurnInputStateKind::PendingActive.as_str(),
                        lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn.as_str(),
                    ],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn bind_turn_input_claim(
        &self,
        claim: &lash_core_execution::TurnInputClaim,
        turn_id: &lash_core_execution::TurnId,
        receipt_input_id: &lash_core_execution::InputId,
    ) -> Result<(), StoreError> {
        let session_id = claim.session_id.clone();
        let claim_id = claim.claim_id.clone();
        let lease_token = claim.lease_token.clone();
        let turn_id = turn_id.clone();
        let receipt_input_id = receipt_input_id.clone();
        self.conn
            .write(move |tx| {
                tx.execute(
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs
                        .bind_claim
                        .sql(),
                    params![
                        session_id.as_str(),
                        claim_id.as_str(),
                        lease_token,
                        turn_id.as_str(),
                        receipt_input_id.as_str(),
                    ],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn bind_turn_input_claim_of_receipt(
        &self,
        session_id: &SessionId,
        receipt_input_id: &lash_core_execution::InputId,
        generation: u64,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        let receipt_input_id = receipt_input_id.clone();
        let turn_id = turn_id.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    let sql = crate::turn_ingress::turn_ingress_sql();
                    let facts: Option<(Option<String>, Option<String>, i64)> = tx
                        .query_row(
                            sql.pending_inputs_sqlite.settlement_facts.sql(),
                            params![session_id.as_str(), receipt_input_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some((Some(claim_id), Some(claim_token), claim_generation)) = facts else {
                        return Ok(());
                    };
                    if claim_generation != sql_session_lease_generation(generation)? {
                        return Ok(());
                    }
                    tx.execute(
                        sql.pending_inputs.bind_claim.sql(),
                        params![
                            session_id.as_str(),
                            claim_id,
                            claim_token,
                            turn_id.as_str(),
                            receipt_input_id.as_str(),
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn reclaim_turn_bound_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<Option<lash_core_execution::TurnInputClaim>, StoreError> {
        reclaim_turn_bound_inputs_sqlite(
            &self.conn,
            self.clock.timestamp_ms(),
            session_id,
            session_execution_lease,
            owner,
            turn_id,
        )
        .await
    }

    async fn orphaned_active_turn_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: lash_core_execution::OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<lash_core_execution::TurnId>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let session_execution_lease = session_execution_lease.clone();
        let scope = OwnedOrphanedScope::from(scope);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<Vec<lash_core_execution::TurnId>, StoreError> = (|| {
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
                })(
                );
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
        turn_id: &lash_core_execution::TurnId,
        observed: &lash_core_execution::TurnCancelIntentSnapshot,
        settlement: Option<&lash_core_execution::TurnCancelClosureSettlement>,
    ) -> Result<lash_core_execution::TurnCancelRepairResult, StoreError> {
        let session_id = session_id.clone();
        let session_execution_lease = session_execution_lease.clone();
        let turn_id = turn_id.clone();
        let observed = observed.clone();
        let settlement = settlement.cloned();
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
                    let closure = settlement
                        .as_ref()
                        .map(lash_core_execution::TurnCancelClosureSettlement::authorization);
                    let stored = tx
                        .query_row(
                            crate::turn_ingress::turn_ingress_sql()
                                .closures_sqlite
                                .select_by_turn
                                .sql(),
                            params![session_id.as_str(), turn_id.as_str()],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let closure_required = stored.is_some()
                        || !matches!(
                            observed,
                            lash_core_execution::TurnCancelIntentSnapshot::Absent
                        );
                    if closure_required != settlement.is_some()
                        || closure.is_some_and(|authorization| {
                            authorization.session_id() != session_id
                                || authorization.turn_id() != turn_id
                        })
                    {
                        return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                            session_id: session_id.clone(),
                            turn_id: turn_id.clone(),
                        });
                    }
                    if let Some(closure) = closure
                        && stored.as_deref() != Some(encode_json(closure)?.as_str())
                    {
                        return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                            session_id: session_id.clone(),
                            turn_id: turn_id.clone(),
                        });
                    }
                    let repaired = repair_orphaned_active_turn_inputs_conn(
                        tx,
                        &session_id,
                        session_execution_lease.fencing_token,
                        &turn_id,
                        &observed,
                        settlement.as_ref(),
                    )?;
                    if settlement.is_some()
                        && matches!(
                            repaired,
                            lash_core_execution::TurnCancelRepairResult::Applied(_)
                        )
                    {
                        tx.execute(
                            crate::turn_ingress::turn_ingress_sql()
                                .closures
                                .delete_by_turn
                                .sql(),
                            params![session_id.as_str(), turn_id.as_str()],
                        )
                        .map_err(sqlite_error)?;
                    }
                    Ok(repaired)
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
        claims: &[lash_core_execution::TurnInputClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        // FIG-1573: the restored open spelling derives from each row's own
        // ingress, exactly as the singular sibling resolves it — a next-turn
        // row restores to `deferred_next_turn`, never `pending_active`. The
        // whole batch is written in ONE statement inside ONE transaction: a
        // batch abandon is one caller giving up one set of rows, and a crash
        // between two statements would leave half the batch claimed by a
        // claim id the caller has already dropped.
        // The triples the batch gives up are bound as one JSON array, so the
        // statement's own text is fixed however many claims there are.
        let triples = claims
            .iter()
            .map(|claim| {
                [
                    claim.session_id.as_str(),
                    claim.claim_id.as_str(),
                    claim.lease_token.as_str(),
                ]
            })
            .collect::<Vec<_>>();
        let triples = encode_json(&triples)?;
        self.conn
            .write(move |tx| {
                tx.execute(
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs_sqlite
                        .abandon_claims
                        .sql(),
                    params![
                        triples,
                        lash_core_execution::runtime::TurnInputStateKind::PendingActive.as_str(),
                        lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn.as_str(),
                    ],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }
}
