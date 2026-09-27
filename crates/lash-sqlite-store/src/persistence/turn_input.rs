use super::*;

fn decode_binding_scope(
    encoded: Option<&str>,
) -> Result<Option<lash_core_execution::ExecutionScope>, StoreError> {
    encoded
        .map(|encoded| {
            serde_json::from_str(encoded).map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "TurnCancellationBinding",
                message: error.to_string(),
            })
        })
        .transpose()
}

#[async_trait::async_trait]
impl TurnInputStore for Store {
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &lash_core_execution::ExecutionScope,
    ) -> Result<(), StoreError> {
        admitted_scope
            .validate()
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "TurnCancellationBinding",
                message: error.to_string(),
            })?;
        let session_id = session_id.clone();
        let fence = session_execution_lease.clone();
        let binding_id = binding_id.to_string();
        let admitted_physical_scope = admitted_scope
            .session_id()
            .is_none()
            .then(|| admitted_scope.clone());
        let admitted_scope_json = admitted_physical_scope
            .as_ref()
            .map(encode_json)
            .transpose()?;
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    ensure_session_execution_lease_conn(tx, &session_id, &fence, now)?;
                    let sql = crate::turn_ingress::turn_ingress_sql();
                    let existing = tx
                        .query_row(
                            sql.bindings.select_by_session.sql(),
                            params![session_id.as_str()],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    match existing {
                        Some((expected, encoded_scope))
                            if expected != binding_id
                                || decode_binding_scope(encoded_scope.as_deref())?
                                    != admitted_physical_scope =>
                        {
                            Err(StoreError::TurnCancelBindingMismatch {
                                session_id,
                                expected,
                                presented: binding_id,
                            })
                        }
                        Some(_) => Ok(()),
                        None => {
                            tx.execute(
                                sql.bindings_sqlite.insert_new.sql(),
                                params![session_id.as_str(), binding_id, admitted_scope_json],
                            )
                            .map_err(sqlite_error)?;
                            Ok(())
                        }
                    }
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn authorize_turn_cancel_closure(
        &self,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        authorization: &lash_core_execution::TurnCancelClosureAuthorization,
    ) -> Result<lash_core_execution::TurnCancelClosureAuthorizationOutcome, StoreError> {
        authorization
            .validate()
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "TurnCancelClosureAuthorization",
                message: error.to_string(),
            })?;
        if authorization.admitted_scope().session_id().is_none()
            && let Some(owner) = &self.turn_cancel_closure_owner
        {
            owner
                .register(authorization.admitted_scope(), authorization.binding_id())
                .await
                .map_err(|error| {
                    // The owner refuses a participant under a retired scope:
                    // the same refusal this catalog's own retired-scope row
                    // answers, so it keeps the same type.
                    match authorization.admitted_scope().journal_identity() {
                        Ok(identity)
                            if error.code
                                == lash_core_execution::RuntimeErrorCode::EffectScopeRetired =>
                        {
                            StoreError::TurnCancelClosureScopeRetired {
                                scope_id: identity.key().to_string(),
                            }
                        }
                        _ => StoreError::Backend(error.to_string()),
                    }
                })?;
        }
        let fence = session_execution_lease.clone();
        let authorization = authorization.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core_execution::TurnCancelClosureAuthorizationOutcome, StoreError> =
                    (|| {
                        let sql = crate::turn_ingress::turn_ingress_sql();
                        ensure_session_not_deleted_conn(tx, authorization.session_id())?;
                        if authorization.session_id() != fence.session_id
                            || authorization.authorizing_fencing_token() != fence.fencing_token
                        {
                            return Err(StoreError::SessionExecutionLeaseExpired {
                                session_id: authorization.session_id().clone(),
                            });
                        }
                        if authorization.admitted_scope().session_id().is_none() {
                            let scope_id = authorization
                                .admitted_scope()
                                .journal_identity()
                                .map_err(|error| StoreError::Backend(error.to_string()))?
                                .key()
                                .to_string();
                            let retired = tx
                                .query_row(
                                    sql.retired_scopes.exists_for_scope.sql(),
                                    params![scope_id],
                                    |row| row.get::<_, bool>(0),
                                )
                                .map_err(sqlite_error)?;
                            if retired {
                                return Err(StoreError::TurnCancelClosureScopeRetired { scope_id });
                            }
                        }
                        let selected = tx
                            .query_row(
                                sql.bindings.select_by_session.sql(),
                                params![authorization.session_id().as_str()],
                                |row| {
                                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                                },
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        let admitted_physical_scope = authorization
                            .admitted_scope()
                            .session_id()
                            .is_none()
                            .then(|| authorization.admitted_scope().clone());
                        let selected_matches =
                            selected.as_ref().is_some_and(|(binding, encoded_scope)| {
                                binding == authorization.binding_id()
                                    && decode_binding_scope(encoded_scope.as_deref())
                                        .is_ok_and(|scope| scope == admitted_physical_scope)
                            });
                        if !selected_matches {
                            return Err(StoreError::TurnCancelBindingMismatch {
                                session_id: authorization.session_id().clone(),
                                expected: selected
                                    .map(|(binding, scope)| format!("{binding} at {scope:?}"))
                                    .unwrap_or_default(),
                                presented: authorization.binding_id().to_string(),
                            });
                        }
                        let encoded = encode_json(&authorization)?;
                        let existing = tx
                            .query_row(
                                sql.closures_sqlite.select_by_turn.sql(),
                                params![
                                    authorization.session_id().as_str(),
                                    authorization.turn_id().as_str()
                                ],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        match existing {
                            Some(existing) if existing == encoded => {
                                Ok(lash_core_execution::TurnCancelClosureAuthorizationOutcome::AdoptedExact)
                            }
                            Some(_) => Err(StoreError::TurnCancelClosureConflict {
                                session_id: authorization.session_id().clone(),
                                turn_id: authorization.turn_id().clone(),
                            }),
                            None => {
                                if load_turn_cancel_intent_snapshot_conn(
                                    tx,
                                    authorization.session_id(),
                                    authorization.turn_id(),
                                )? != *authorization.observed_intent()
                                {
                                    return Err(StoreError::TurnCancelIntentChanged {
                                        session_id: authorization.session_id().clone(),
                                        turn_id: authorization.turn_id().clone(),
                                    });
                                }
                                tx.execute(
                                    sql.closures.insert_new.sql(),
                                    params![
                                        authorization.session_id().as_str(),
                                        authorization.turn_id().as_str(),
                                        encoded
                                    ],
                                )
                                .map_err(sqlite_error)?;
                                Ok(lash_core_execution::TurnCancelClosureAuthorizationOutcome::Authorized)
                            }
                        }
                    })();
                Ok(match outcome {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &lash_core_execution::ExecutionScope,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        self.validate_turn_cancellation_binding(
            session_id,
            session_execution_lease,
            binding_id,
            admitted_scope,
        )
        .await?;
        let session_id = session_id.clone();
        let encoded = self
            .conn
            .call(move |conn| {
                let mut statement = conn.prepare(
                    crate::turn_ingress::turn_ingress_sql()
                        .closures
                        .list_by_session
                        .sql(),
                )?;
                let rows = statement
                    .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        encoded
            .into_iter()
            .map(|encoded| {
                serde_json::from_str(&encoded).map_err(|error| StoreError::StoredDataCorrupt {
                    record_kind: "TurnCancelClosureAuthorization",
                    message: error.to_string(),
                })
            })
            .collect()
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        let session_id =
            self.session_id
                .get()
                .cloned()
                .ok_or(StoreError::UnsupportedStoreOperation {
                    operation: "pending_turn_cancel_closure_pins requires a session-bound store",
                })?;
        let encoded = self
            .conn
            .call(move |conn| {
                let mut statement = conn.prepare(
                    crate::turn_ingress::turn_ingress_sql()
                        .closures
                        .list_by_session
                        .sql(),
                )?;
                let rows = statement
                    .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(sqlite_error)?;
        encoded
            .into_iter()
            .map(|encoded| {
                serde_json::from_str(&encoded).map_err(|error| StoreError::StoredDataCorrupt {
                    record_kind: "TurnCancelClosureAuthorization",
                    message: error.to_string(),
                })
            })
            .collect()
    }

    async fn turn_is_committed(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<bool, StoreError> {
        let session_id = address.session_id.clone();
        let operation_key =
            lash_core_execution::OperationId::turn(&address.session_id, &address.turn_id, "final")
                .storage_key()?;
        self.conn
            .call(move |conn| {
                conn.query_row(
                    crate::session_sql::session_sql()
                        .turn_commits
                        .exists_for_turn
                        .sql(),
                    params![session_id.as_str(), operation_key],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await
            .map_err(sqlite_error)
    }

    async fn record_turn_cancel_request(
        &self,
        request: lash_core_execution::facade_support::TurnCancelRequest,
    ) -> Result<lash_core_execution::TurnCancelRequestRecord, StoreError> {
        let session_id = request.address.session_id.clone();
        let turn_id = request.address.turn_id.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let operation_key =
                        lash_core_execution::OperationId::turn(&session_id, &turn_id, "final")
                            .storage_key()?;
                    let committed = tx
                        .query_row(
                            crate::session_sql::session_sql()
                                .turn_commits
                                .exists_for_turn
                                .sql(),
                            params![session_id.as_str(), operation_key],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(sqlite_error)?;
                    if committed {
                        return Ok(lash_core_execution::TurnCancelRequestRecord {
                            request,
                            outcome: None,
                        });
                    }
                    // The first policy acceptor is immutable. A stronger
                    // same-policy request advances only the closure-CAS
                    // revision; effective timing is recorded by the gate. A
                    // request that disagrees about the undelivered-input
                    // disposition is not an escalation: the gate refuses it, so
                    // it leaves the row and its revision untouched.
                    if let Some(existing) =
                        load_turn_cancel_request_conn(tx, &session_id, &turn_id)?
                    {
                        if request.escalates(&existing.request) {
                            let revision = match load_turn_cancel_intent_snapshot_conn(
                                tx,
                                &session_id,
                                &turn_id,
                            )? {
                                lash_core_execution::TurnCancelIntentSnapshot::Present {
                                    revision,
                                    ..
                                } => StoreError::checked_monotonic_increment(
                                    "turn_cancel_intent_revision",
                                    revision,
                                )?,
                                lash_core_execution::TurnCancelIntentSnapshot::Absent => {
                                    return Err(StoreError::Backend(
                                        "turn cancel request disappeared during escalation"
                                            .to_string(),
                                    ));
                                }
                            };
                            let revision = i64::try_from(revision).map_err(|_| {
                                StoreError::Backend(
                                    "turn cancel intent revision exceeds SQLite range".to_string(),
                                )
                            })?;
                            tx.execute(
                                crate::turn_ingress::turn_ingress_sql()
                                    .cancel_requests
                                    .advance_intent_revision
                                    .sql(),
                                params![session_id.as_str(), turn_id.as_str(), revision,],
                            )
                            .map_err(sqlite_error)?;
                        }
                        return Ok(existing);
                    }
                    let record = lash_core_execution::TurnCancelRequestRecord {
                        request,
                        outcome: None,
                    };
                    tx.execute(
                        crate::turn_ingress::turn_ingress_sql()
                            .cancel_requests_sqlite
                            .insert_first
                            .sql(),
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
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
        let session_id = address.session_id.clone();
        let turn_id = address.turn_id.clone();
        self.conn
            .call(move |conn| Ok(load_turn_cancel_request_conn(conn, &session_id, &turn_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn turn_cancel_request_intent(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
        let session_id = address.session_id.clone();
        let turn_id = address.turn_id.clone();
        self.conn
            .call(move |conn| {
                Ok(load_turn_cancel_intent_snapshot_conn(
                    conn,
                    &session_id,
                    &turn_id,
                ))
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn reconcile_turn_cancel_winner(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
        observed: &lash_core_execution::TurnCancelIntentSnapshot,
        evidence: &lash_core_execution::facade_support::TurnCancellationEvidence,
    ) -> Result<bool, StoreError> {
        let session_id = address.session_id.clone();
        let turn_id = address.turn_id.clone();
        let evidence = evidence.clone();
        let observed = observed.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    reconcile_turn_cancel_winner_conn(
                        tx,
                        &session_id,
                        &turn_id,
                        &observed,
                        &evidence,
                    )
                })();
                Ok(match outcome {
                    Ok(applied) => TxOutcome::Commit(Ok(applied)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn enqueue_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
    ) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
        let drafts = batch.drafts().len() as u64;
        let first_nonce = self.commit_count.fetch_add(drafts, AtomicOrdering::Relaxed);
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = enqueue_pending_turn_inputs_conn(tx, &batch, now, first_nonce);
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_run_spec(
        &self,
        session_id: &SessionId,
        hash: &lash_core_execution::RunSpecHash,
    ) -> Result<Option<lash_core_execution::RunSpec>, StoreError> {
        let session_id = session_id.clone();
        let hash = hash.clone();
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let stored: Option<String> = conn
                        .query_row(
                            crate::turn_ingress::turn_ingress_sql()
                                .run_specs
                                .select_spec
                                .sql(),
                            params![session_id.as_str(), hash.as_str()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    stored
                        .map(|json| {
                            lash_core_execution::RunSpec::from_canonical_json(&json).map_err(
                                |error| StoreError::StoredDataCorrupt {
                                    record_kind: "RunSpec",
                                    message: error.to_string(),
                                },
                            )
                        })
                        .transpose()
                })();
                Ok(outcome)
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

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnInputApplication>, StoreError> {
        let session_id = SessionId::from(session_id.to_string());
        let fleet = self.fleet_format();
        self.conn
            .call(move |conn| {
                let outcome = (|| {
                    let mut stmt = conn
                        .prepare(
                            crate::session_sql::session_sql()
                                .turn_commits
                                .select_all_for_session
                                .sql(),
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
                        let result =
                            lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                                &session_id,
                                &turn_id,
                                &result_json,
                                fleet,
                            )?;
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
                            &lash_core_execution::store::ParkEventKind::Cancelled {
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
                                &lash_core_execution::store::ParkEventKind::Cancelled {
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

/// Admit every draft of `batch` under the database write lock the caller's
/// transaction holds (FIG-3842): a draft a stored row already answers returns
/// that row, and every other draft is inserted in request order at the next
/// positions of the session's ingress sequence. Any refusal rolls the whole
/// batch back.
fn enqueue_pending_turn_inputs_conn(
    tx: &Connection,
    batch: &lash_core_execution::PendingTurnInputBatch,
    now: u64,
    first_nonce: u64,
) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let session_id = batch.session_id();
    ensure_session_not_deleted_conn(tx, session_id)?;
    ensure_session_not_closing_conn(tx, session_id)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut interned = std::collections::BTreeSet::new();
    let mut admitted = Vec::with_capacity(batch.drafts().len());
    for (nonce, draft) in (first_nonce..).zip(batch.drafts()) {
        let submission_digest = support::turn_input_submission_digest(draft)?;
        let by_source_key = match draft.source_key.as_deref() {
            Some(source_key) => tx
                .query_row(
                    sql.pending_inputs.select_id_by_source_key.sql(),
                    params![session_id.as_str(), source_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?,
            None => None,
        };
        let by_input_id = match (&by_source_key, draft.input_id.as_deref()) {
            (None, Some(input_id)) => tx
                .query_row(
                    sql.pending_inputs.select_session_by_input_id.sql(),
                    params![input_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sqlite_error)?,
            _ => None,
        };
        let input_id = match support::decide_turn_input_draft_admission(
            draft,
            &submission_digest,
            by_source_key,
            by_input_id,
        )? {
            support::TurnInputDraftAdmission::Existing { input_id } => input_id,
            support::TurnInputDraftAdmission::New => {
                let input_id = draft.input_id.clone().unwrap_or_else(|| {
                    support::derive_pending_turn_input_id(
                        session_id,
                        draft.source_key.as_deref(),
                        now,
                        nonce,
                    )
                });
                let state = lash_core_execution::TurnInputState::open(draft.ingress.clone());
                let run_spec = admit_run_spec_conn(tx, draft, &mut interned)?;
                tx.execute(
                    sql.pending_inputs.insert_new.sql(),
                    params![
                        crate::session_ingress::allocate_sequence(tx, session_id)?,
                        input_id.as_str(),
                        session_id.as_str(),
                        draft.source_key.as_deref(),
                        encode_json(&draft.ingress)?,
                        state.as_str(),
                        encode_json(&draft.input)?,
                        submission_digest.as_str(),
                        now as i64,
                        run_spec.column(),
                    ],
                )
                .map_err(|err| {
                    crate::sqlite_pending_turn_input_insert_error(err, session_id, &input_id)
                })?;
                // The admitted input owes its session a drive (ADR 0109 §3):
                // the row is armed in the transaction that admits it, so no
                // crash between the commit and the drive ask loses the ask.
                crate::ingress_obligation::arm_turn_input_tx(tx, session_id, &input_id, now)?;
                input_id
            }
        };
        admitted.push(
            load_pending_turn_input_by_id_conn(tx, session_id, &input_id)?.ok_or_else(|| {
                StoreError::Backend("admitted pending turn input disappeared".to_string())
            })?,
        );
    }
    Ok(admitted)
}

/// Admit `draft`'s run spec inside its enqueue transaction (FIG-3838): refuse
/// a steering spec that differs from its running turn's, then intern a
/// non-default spec once per hash and refuse different bytes under an
/// interned hash. Both refusals roll the whole admission back. A hash this
/// batch's transaction already interned (`interned`) is not interned again.
fn admit_run_spec_conn(
    tx: &Connection,
    draft: &lash_core_execution::PendingTurnInputDraft,
    interned: &mut std::collections::BTreeSet<String>,
) -> Result<lash_core_execution::store_backend_support::RunSpecAdmission, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let spec = support::RunSpecAdmission::of(draft)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    if let Some(turn_id) = support::steering_run_spec_target(draft) {
        let addressed: Option<(String, Option<String>)> = tx
            .query_row(
                sql.pending_inputs.select_run_spec_by_source_key.sql(),
                params![draft.session_id.as_str(), turn_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        match addressed {
            Some((state, hash)) => support::check_steering_run_spec(
                &draft.session_id,
                turn_id,
                &spec,
                Some((state.as_str(), hash.as_deref())),
            )?,
            // No input started `turn_id` under a source key: the addressed
            // turn may still be a running root of another kind (FIG-3877).
            None => check_unsourced_steering_run_spec_conn(tx, draft, turn_id, &spec)?,
        }
    }
    if let Some((hash, canonical)) = spec.interned()
        && !interned.contains(hash)
    {
        tx.execute(
            sql.run_specs.intern.sql(),
            params![draft.session_id.as_str(), hash, canonical],
        )
        .map_err(sqlite_error)?;
        let stored: String = tx
            .query_row(
                sql.run_specs.select_spec.sql(),
                params![draft.session_id.as_str(), hash],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        spec.check_interned(&draft.session_id, &stored)?;
        interned.insert(hash.to_string());
    }
    Ok(spec)
}

/// The steering verdict over the root kinds no `source_key`-filed input
/// starts (FIG-3877), read inside the admission transaction:
///
/// * `turn_id` is the follow-on the head owes: it inherits the shape its
///   fact recorded at the switch. A fact written before the field existed
///   falls back to the spec of the input that started its parent root; a
///   queued parent leaves the queued-run evidence to decide.
/// * `turn_id` is the pending queued run's current position: the spec its
///   member inputs are filed under, or the default spec for a position that
///   owns no input.
/// * Otherwise nothing running names `turn_id`: the steering input is a
///   next-turn root under its own spec.
fn check_unsourced_steering_run_spec_conn(
    tx: &Connection,
    draft: &lash_core_execution::PendingTurnInputDraft,
    turn_id: &lash_core_execution::TurnId,
    spec: &lash_core_execution::store_backend_support::RunSpecAdmission,
) -> Result<(), StoreError> {
    use lash_core_execution::store_backend_support as support;
    let sql = crate::turn_ingress::turn_ingress_sql();
    // `Some(hash)` is the shape the running root resolved under (`None` =
    // the default spec); `None` means the evidence did not decide.
    let mut running: Option<Option<String>> = None;
    if let Some(owed) =
        pending_follow_on_conn(tx, &draft.session_id)?.filter(|owed| owed.is_turn(turn_id))
    {
        running = match &owed.resolved_run {
            Some(resolved) => Some(resolved.spec.as_ref().map(|hash| hash.as_str().to_string())),
            // A fact written before the shape was recorded: the parent
            // root's own starting input names the shape instead.
            None => tx
                .query_row(
                    sql.pending_inputs.select_run_spec_by_source_key.sql(),
                    params![draft.session_id.as_str(), owed.root_turn_id().as_str()],
                    |row| row.get::<_, Option<String>>(1),
                )
                .optional()
                .map_err(sqlite_error)?,
        };
    }
    if running.is_none()
        && let Some(run) = load_run_conn(tx, &draft.session_id, None)?
        && run.position.turn_id == *turn_id
    {
        // The spec the position resolved under: the first member input's,
        // or the default spec while selection has not committed and for a
        // position that owns no input.
        let mut hash = None;
        for member in run.members.iter().flatten() {
            if let lash_core_execution::store::QueuedRunMember::Input(input_id) = member {
                hash = tx
                    .query_row(
                        sql.pending_inputs.select_run_spec_by_input_id.sql(),
                        params![draft.session_id.as_str(), input_id.as_str()],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(sqlite_error)?
                    .flatten();
                break;
            }
        }
        running = Some(hash);
    }
    if let Some(hash) = running {
        support::check_running_root_run_spec(&draft.session_id, turn_id, spec, hash.as_deref())?;
    }
    Ok(())
}
