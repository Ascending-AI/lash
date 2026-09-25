//! [`TurnCancelStore`] for [`PostgresSessionStore`]: the cancellation
//! binding, closure obligations and the durable cancel request.

use super::*;

#[async_trait::async_trait]
impl TurnCancelStore for PostgresSessionStore {
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &ExecutionScope,
    ) -> Result<(), StoreError> {
        admitted_scope
            .validate()
            .map_err(|error| StoreError::StoredDataCorrupt {
                record_kind: "TurnCancellationBinding",
                message: error.to_string(),
            })?;
        let admitted_physical_scope = admitted_scope
            .session_id()
            .is_none()
            .then(|| admitted_scope.clone());
        let admitted_scope_json = admitted_physical_scope
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| StoreError::RecordEncodingFailed {
                record_kind: "TurnCancellationBinding".to_string(),
                message: error.to_string(),
            })?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let existing: Option<(String, Option<String>)> = sqlx::query_as(
            crate::turn_ingress::turn_ingress_sql()
                .bindings_postgres
                .select_by_session_for_update
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        match existing {
            Some((expected, encoded_scope))
                if expected != binding_id
                    || encoded_scope
                        .as_deref()
                        .map(serde_json::from_str::<ExecutionScope>)
                        .transpose()
                        .map_err(|error| StoreError::StoredDataCorrupt {
                            record_kind: "TurnCancellationBinding",
                            message: error.to_string(),
                        })?
                        != admitted_physical_scope =>
            {
                return Err(StoreError::TurnCancelBindingMismatch {
                    session_id: session_id.clone(),
                    expected,
                    presented: binding_id.to_string(),
                });
            }
            Some(_) => {}
            None => {
                sqlx::query(
                    crate::turn_ingress::turn_ingress_sql()
                        .bindings_postgres
                        .insert_new
                        .sql(),
                )
                .bind(session_id.as_str())
                .bind(binding_id)
                .bind(&admitted_scope_json)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                let selected: (String, Option<String>) = sqlx::query_as(
                    crate::turn_ingress::turn_ingress_sql()
                        .bindings
                        .select_by_session
                        .sql(),
                )
                .bind(session_id.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                if selected.0 != binding_id || selected.1 != admitted_scope_json {
                    return Err(StoreError::TurnCancelBindingMismatch {
                        session_id: session_id.clone(),
                        expected: format!("{} at {:?}", selected.0, selected.1),
                        presented: binding_id.to_string(),
                    });
                }
            }
        }
        tx.commit().await.map_err(store_sqlx_error)
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
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, authorization.session_id()).await?;
        if authorization.session_id() != session_execution_lease.session_id
            || authorization.authorizing_fencing_token() != session_execution_lease.fencing_token
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
            crate::turn_cancel_closure::lock_scope(&mut tx, &scope_id)
                .await
                .map_err(store_sqlx_error)?;
            let retired: bool = sqlx::query_scalar(
                crate::turn_ingress::turn_ingress_sql()
                    .retired_scopes
                    .exists_for_scope
                    .sql(),
            )
            .bind(&scope_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
            if retired {
                return Err(StoreError::TurnCancelClosureScopeRetired { scope_id });
            }
        }
        let selected: Option<(String, Option<String>)> = sqlx::query_as(
            crate::turn_ingress::turn_ingress_sql()
                .bindings
                .select_by_session
                .sql(),
        )
        .bind(authorization.session_id().as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let admitted_physical_scope = authorization
            .admitted_scope()
            .session_id()
            .is_none()
            .then(|| authorization.admitted_scope().clone());
        let selected_matches = selected.as_ref().is_some_and(|(binding, encoded_scope)| {
            binding == authorization.binding_id()
                && encoded_scope
                    .as_deref()
                    .map(serde_json::from_str::<ExecutionScope>)
                    .transpose()
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
        let encoded = serde_json::to_string(authorization).map_err(|error| {
            StoreError::RecordEncodingFailed {
                record_kind: "TurnCancelClosureAuthorization".to_string(),
                message: error.to_string(),
            }
        })?;
        let existing: Option<String> = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .closures_postgres
                .select_by_turn
                .sql(),
        )
        .bind(authorization.session_id().as_str())
        .bind(authorization.turn_id().as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let outcome = match existing {
            Some(existing) if existing == encoded => {
                lash_core_execution::TurnCancelClosureAuthorizationOutcome::AdoptedExact
            }
            Some(_) => {
                return Err(StoreError::TurnCancelClosureConflict {
                    session_id: authorization.session_id().clone(),
                    turn_id: authorization.turn_id().clone(),
                });
            }
            None => {
                if load_turn_cancel_intent_snapshot_tx(
                    &mut tx,
                    authorization.session_id(),
                    authorization.turn_id(),
                )
                .await?
                    != *authorization.observed_intent()
                {
                    return Err(StoreError::TurnCancelIntentChanged {
                        session_id: authorization.session_id().clone(),
                        turn_id: authorization.turn_id().clone(),
                    });
                }
                sqlx::query(
                    crate::turn_ingress::turn_ingress_sql()
                        .closures
                        .insert_new
                        .sql(),
                )
                .bind(authorization.session_id().as_str())
                .bind(authorization.turn_id().as_str())
                .bind(encoded)
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
                lash_core_execution::TurnCancelClosureAuthorizationOutcome::Authorized
            }
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(outcome)
    }

    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &ExecutionScope,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        self.validate_turn_cancellation_binding(
            session_id,
            session_execution_lease,
            binding_id,
            admitted_scope,
        )
        .await?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let rows: Vec<String> = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .closures
                .list_by_session
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        rows.into_iter()
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
        let rows: Vec<String> = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .closures
                .list_by_session
                .sql(),
        )
        .bind(self.session_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(store_sqlx_error)?;
        rows.into_iter()
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
        let session_id = &request.address.session_id;
        let turn_id = &request.address.turn_id;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        let operation_key =
            lash_core_execution::OperationId::turn(session_id, turn_id, "final").storage_key()?;
        let committed: bool = sqlx::query_scalar(
            crate::session_sql::session_sql()
                .turn_commits
                .exists_for_turn
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(operation_key)
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if committed {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(lash_core_execution::TurnCancelRequestRecord {
                request,
                outcome: None,
            });
        }
        // The first policy acceptor is immutable. A stronger same-policy
        // request advances only the closure-CAS revision; effective timing is
        // recorded by the gate. A request that disagrees about the
        // undelivered-input disposition is not an escalation: the gate refuses
        // it, so it leaves the row and its revision untouched.
        match load_turn_cancel_request_tx(&mut tx, session_id, turn_id).await? {
            Some(existing) if request.escalates(&existing.request) => {
                let revision = match load_turn_cancel_intent_snapshot_tx(
                    &mut tx, session_id, turn_id,
                )
                .await?
                {
                    lash_core_execution::TurnCancelIntentSnapshot::Present { revision, .. } => {
                        StoreError::checked_monotonic_increment(
                            "turn_cancel_intent_revision",
                            revision,
                        )?
                    }
                    lash_core_execution::TurnCancelIntentSnapshot::Absent => {
                        return Err(StoreError::Backend(
                            "turn cancel request disappeared during escalation".to_string(),
                        ));
                    }
                };
                let revision = i64::try_from(revision).map_err(|_| {
                    StoreError::Backend(
                        "turn cancel intent revision exceeds PostgreSQL BIGINT".to_string(),
                    )
                })?;
                sqlx::query(
                    crate::turn_ingress::turn_ingress_sql()
                        .cancel_requests
                        .advance_intent_revision
                        .sql(),
                )
                .bind(session_id.as_str())
                .bind(turn_id.as_str())
                .bind(revision)
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
                    crate::turn_ingress::turn_ingress_sql()
                        .cancel_requests_postgres
                        .insert_first
                        .sql(),
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
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<Option<lash_core_execution::TurnCancelRequestRecord>, StoreError> {
        load_turn_cancel_request_pg(&self.pool, &address.session_id, &address.turn_id).await
    }

    async fn turn_cancel_request_intent(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<lash_core_execution::TurnCancelIntentSnapshot, StoreError> {
        load_turn_cancel_intent_snapshot_pg(&self.pool, &address.session_id, &address.turn_id).await
    }

    async fn reconcile_turn_cancel_winner(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
        observed: &lash_core_execution::TurnCancelIntentSnapshot,
        evidence: &lash_core_execution::facade_support::TurnCancellationEvidence,
    ) -> Result<bool, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, &address.session_id).await?;
        let applied = reconcile_turn_cancel_winner_tx(
            &mut tx,
            &address.session_id,
            &address.turn_id,
            observed,
            evidence,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(applied)
    }
}
