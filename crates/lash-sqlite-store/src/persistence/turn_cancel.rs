//! [`TurnCancelStore`] for [`Store`]: the cancellation binding, closure
//! obligations and the durable cancel request.

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
impl TurnCancelStore for Store {
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
}
