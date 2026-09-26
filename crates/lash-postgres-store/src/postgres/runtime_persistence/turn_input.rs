use super::*;

#[async_trait::async_trait]
impl TurnInputStore for PostgresSessionStore {
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

    async fn turn_is_committed(
        &self,
        address: &lash_core_execution::facade_support::TurnAddress,
    ) -> Result<bool, StoreError> {
        let operation_key =
            lash_core_execution::OperationId::turn(&address.session_id, &address.turn_id, "final")
                .storage_key()?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query_scalar(
            crate::session_sql::session_sql()
                .turn_commits
                .exists_for_turn
                .sql(),
        )
        .bind(address.session_id.as_str())
        .bind(operation_key)
        .fetch_one(&mut *connection)
        .await
        .map_err(store_sqlx_error)
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

    async fn enqueue_pending_turn_input(
        &self,
        draft: lash_core_execution::PendingTurnInputDraft,
    ) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, &draft.session_id).await?;
        ensure_session_not_closing_tx(&mut tx, &draft.session_id).await?;
        let now = self.clock.timestamp_ms();
        let enqueue_seq: i64 = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs_postgres
                .select_next_enqueue_seq
                .sql(),
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let enqueue_seq_u64 = u64_from_sql("PendingTurnInput", "enqueue_seq", enqueue_seq)?;
        let input_id = draft.input_id.clone().unwrap_or_else(|| {
            lash_core_execution::store_backend_support::derive_pending_turn_input_id(
                &draft.session_id,
                draft.source_key.as_deref(),
                now,
                enqueue_seq_u64,
            )
        });
        let state = lash_core_execution::TurnInputState::open(draft.ingress.clone());
        let submission_digest = draft.submission_digest().map_err(|err| {
            StoreError::Backend(format!(
                "failed to digest pending turn input submission: {err}"
            ))
        })?;
        let ingress_json = encode_json(&draft.ingress)?;
        let input_json = encode_json(&draft.input)?;
        let input = if let Some(source_key) = draft.source_key.as_deref() {
            let row = sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs_postgres
                    .insert_or_adopt_existing
                    .sql(),
            )
            .bind(enqueue_seq)
            .bind(&input_id)
            .bind(draft.session_id.as_str())
            .bind(source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .bind(&submission_digest)
            .fetch_one(&mut *tx)
            .await
            .map_err(|err| pending_turn_input_insert_error(err, &draft.session_id, &input_id))?;
            let existing_digest: String =
                row.try_get("submission_digest").map_err(store_sqlx_error)?;
            let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
            if existing_digest != submission_digest {
                return Err(StoreError::PendingTurnInputSourceKeyConflict {
                    session_id: draft.session_id.clone(),
                    source_key: source_key.to_string(),
                    existing_input_id: input.input_id.clone(),
                });
            }
            input
        } else if draft.input_id.is_some() {
            let row = sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs_postgres
                    .insert_or_adopt_by_input_id
                    .sql(),
            )
            .bind(enqueue_seq)
            .bind(&input_id)
            .bind(draft.session_id.as_str())
            .bind(&draft.source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .bind(&submission_digest)
            .fetch_one(&mut *tx)
            .await
            // The `ON CONFLICT (input_id)` arbiter absorbs the id's unique
            // violation, so only an unrelated insert failure reaches here.
            .map_err(store_sqlx_error)?;
            let existing_digest: String =
                row.try_get("submission_digest").map_err(store_sqlx_error)?;
            draft.adopt_provisioned_row(
                pending_turn_input_from_row(pending_turn_input_row(row)?)?,
                &existing_digest,
            )?
        } else {
            sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs_postgres
                    .insert_new
                    .sql(),
            )
            .bind(enqueue_seq)
            .bind(&input_id)
            .bind(draft.session_id.as_str())
            .bind(&draft.source_key)
            .bind(&ingress_json)
            .bind(state.as_str())
            .bind(&input_json)
            .bind(now as i64)
            .bind(&submission_digest)
            .execute(&mut *tx)
            .await
            .map_err(|err| pending_turn_input_insert_error(err, &draft.session_id, &input_id))?;
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
    ) -> Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .list_undelivered
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(now as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let inputs = rows
            .into_iter()
            .map(pending_turn_input_read_from_row)
            .collect::<Result<Vec<_>, StoreError>>()?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(inputs)
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TurnInputApplication>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let rows = sqlx::query(
            crate::session_sql::session_sql()
                .turn_commits
                .select_all_for_session
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        let mut commits = Vec::with_capacity(rows.len());
        for row in rows {
            let turn_id = row.get::<String, _>(0);
            let result_json: String = row.get(1);
            let result = lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                session_id,
                &turn_id,
                &result_json,
                self.fleet_format,
            )?;
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
        targets: &[lash_core_execution::PendingTurnInputCancelTarget],
    ) -> Result<Vec<lash_core_execution::PendingTurnInputCancelReceipt>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let targets = targets.to_vec();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        // Lease row first, then the input rows in queue order: the order every
        // claim and commit takes them in (FIG-3589).
        load_session_execution_lease_tx(&mut tx, session_id).await?;
        let mut covered = std::collections::BTreeSet::new();
        for target in &targets {
            if let Some(row) =
                load_pending_turn_input_row_by_target_tx(&mut tx, session_id, target, false).await?
            {
                covered.insert(lash_core_execution::InputId::from(row.input_id));
            }
        }
        lock_cancel_rows_in_queue_order(&mut tx, session_id, CancelLockScope::Targets(&covered))
            .await?;
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome =
                match load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &target, true)
                    .await?
                {
                    Some(row) => {
                        cancel_pending_turn_input_row_tx(&mut tx, row, now, &covered).await?
                    }
                    None => lash_core_execution::PendingTurnInputCancelOutcome::NotFound,
                };
            results.push(lash_core_execution::PendingTurnInputCancelReceipt { target, outcome });
        }
        let released = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .delete_released_turn_park_returning
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if let Some(released) = released {
            let released_turn_id: String = released.get(0);
            let released_park_id: i64 = released.get(1);
            crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
                &mut tx,
                session_id,
                &released_turn_id,
                released_park_id,
                &lash_core_execution::store::ParkEventKind::Cancelled {
                    cause: lash_core_execution::store::ParkCancelCause::InputWithdrawn,
                },
                now,
            )
            .await?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(results)
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &SessionId,
        anchor: &lash_core_execution::PendingTurnInputCancelTarget,
    ) -> Result<lash_core_execution::PendingTurnInputSuffixCancelOutcome, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let anchor = anchor.clone();
        let now = postgres_transaction_epoch_ms(&mut tx).await?;
        // Lease row first, then the input rows in queue order: the order every
        // claim and commit takes them in (FIG-3589).
        load_session_execution_lease_tx(&mut tx, session_id).await?;
        let Some(anchor_row) =
            load_pending_turn_input_row_by_target_tx(&mut tx, session_id, &anchor, false).await?
        else {
            tx.commit().await.map_err(store_sqlx_error)?;
            return Ok(
                lash_core_execution::PendingTurnInputSuffixCancelOutcome::AnchorNotFound { anchor },
            );
        };
        lock_cancel_rows_in_queue_order(
            &mut tx,
            session_id,
            CancelLockScope::Suffix(anchor_row.enqueue_seq),
        )
        .await?;
        let rows = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs_postgres
                .select_suffix
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(anchor_row.enqueue_seq as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .map(pending_turn_input_row)
        .collect::<Result<Vec<_>, StoreError>>()?;
        let covered = rows
            .iter()
            .map(|row| lash_core_execution::InputId::from(row.input_id.clone()))
            .collect::<std::collections::BTreeSet<_>>();
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            outcomes.push(cancel_pending_turn_input_row_tx(&mut tx, row, now, &covered).await?);
        }
        let released = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .delete_released_turn_park_returning
                .sql(),
        )
        .bind(session_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        if let Some(released) = released {
            let released_turn_id: String = released.get(0);
            let released_park_id: i64 = released.get(1);
            crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
                &mut tx,
                session_id,
                &released_turn_id,
                released_park_id,
                &lash_core_execution::store::ParkEventKind::Cancelled {
                    cause: lash_core_execution::store::ParkCancelCause::InputWithdrawn,
                },
                now,
            )
            .await?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(lash_core_execution::PendingTurnInputSuffixCancelOutcome::Outcomes { anchor, outcomes })
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
        claim_pending_turn_inputs_postgres(
            &self.pool,
            #[cfg(any(test, feature = "testing"))]
            self.lease_clock_for_testing.as_ref(),
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
        claim_pending_turn_inputs_postgres(
            &self.pool,
            #[cfg(any(test, feature = "testing"))]
            self.lease_clock_for_testing.as_ref(),
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
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs_postgres
                .abandon_claim
                .sql(),
        )
        .bind(claim.session_id.as_str())
        .bind(&claim.claim_id)
        .bind(&claim.lease_token)
        .bind(lash_core_execution::runtime::TurnInputStateKind::PendingActive.as_str())
        .bind(lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn.as_str())
        .execute(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn bind_turn_input_claim(
        &self,
        claim: &lash_core_execution::TurnInputClaim,
        turn_id: &lash_core_execution::TurnId,
        receipt_input_id: &lash_core_execution::InputId,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .bind_claim
                .sql(),
        )
        .bind(claim.session_id.as_str())
        .bind(&claim.claim_id)
        .bind(&claim.lease_token)
        .bind(turn_id.as_str())
        .bind(receipt_input_id.as_str())
        .execute(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn bind_turn_input_claim_of_receipt(
        &self,
        session_id: &SessionId,
        receipt_input_id: &lash_core_execution::InputId,
        generation: u64,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        let sql = crate::turn_ingress::turn_ingress_sql();
        let facts: Option<(Option<String>, Option<String>, i64, String)> =
            sqlx::query_as(sql.pending_inputs_postgres.settlement_facts.sql())
                .bind(session_id.as_str())
                .bind(receipt_input_id.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        if let Some((Some(claim_id), Some(claim_token), claim_generation, _)) = facts
            && claim_generation == sql_session_lease_generation(generation)?
        {
            sqlx::query(sql.pending_inputs.bind_claim.sql())
                .bind(session_id.as_str())
                .bind(claim_id)
                .bind(claim_token)
                .bind(turn_id.as_str())
                .bind(receipt_input_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn reclaim_turn_bound_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<Option<lash_core_execution::TurnInputClaim>, StoreError> {
        reclaim_turn_bound_inputs_postgres(
            &self.pool,
            #[cfg(any(test, feature = "testing"))]
            self.lease_clock_for_testing.as_ref(),
            session_id,
            session_execution_lease,
            owner,
            turn_id,
        )
        .await
    }

    async fn abandon_turn_input_claims(
        &self,
        claims: &[lash_core_execution::TurnInputClaim],
    ) -> Result<(), StoreError> {
        if claims.is_empty() {
            return Ok(());
        }
        // FIG-1573: restore each row to the open spelling its own `ingress_json`
        // carries, exactly as the singular sibling does — a next-turn row goes
        // back to `deferred_next_turn`, never `pending_active`. The whole batch
        // runs in ONE statement inside ONE transaction: a batch abandon is one
        // caller giving up one set of rows, and a failure between two
        // statements would leave half the batch claimed by a claim id the
        // caller has already dropped.
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        {
            // The claims a batch abandon gives up are bound as three parallel
            // arrays, so the statement's own text is fixed however many there
            // are; it used to be built one tuple at a time.
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
            sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs_postgres
                    .abandon_claims
                    .sql(),
            )
            .bind(&session_ids)
            .bind(&claim_ids)
            .bind(&claim_tokens)
            .bind(lash_core_execution::runtime::TurnInputStateKind::PendingActive.as_str())
            .bind(lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }

    async fn orphaned_active_turn_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        scope: lash_core_execution::OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<lash_core_execution::TurnId>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        // Re-validated inside this transaction, not upstream: the lane can be
        // displaced between an upstream check and this write, and a
        // stale-generation repair would clear the new holder's claim columns.
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let turn_ids = orphaned_active_turn_ids_tx(
            &mut tx,
            session_id,
            session_execution_lease.fencing_token,
            scope,
        )
        .await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(turn_ids)
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        turn_id: &lash_core_execution::TurnId,
        observed: &lash_core_execution::TurnCancelIntentSnapshot,
        settlement: Option<&lash_core_execution::TurnCancelClosureSettlement>,
    ) -> Result<lash_core_execution::TurnCancelRepairResult, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        ensure_session_execution_lease_tx(&mut tx, session_id, session_execution_lease).await?;
        let closure =
            settlement.map(lash_core_execution::TurnCancelClosureSettlement::authorization);
        let stored: Option<String> = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .closures_postgres
                .select_by_turn
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let closure_required = stored.is_some()
            || !matches!(
                observed,
                lash_core_execution::TurnCancelIntentSnapshot::Absent
            );
        if closure_required != settlement.is_some()
            || closure.is_some_and(|authorization| {
                authorization.session_id() != session_id || authorization.turn_id() != turn_id
            })
        {
            return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            });
        }
        if let Some(closure) = closure {
            let expected = serde_json::to_string(closure).map_err(|error| {
                StoreError::RecordEncodingFailed {
                    record_kind: "TurnCancelClosureAuthorization".to_string(),
                    message: error.to_string(),
                }
            })?;
            if stored.as_deref() != Some(expected.as_str()) {
                return Err(StoreError::TurnCancelClosureAuthorizationMismatch {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                });
            }
        }
        let repaired = repair_orphaned_active_turn_inputs_tx(
            &mut tx,
            session_id,
            session_execution_lease.fencing_token,
            turn_id,
            observed,
            settlement,
        )
        .await?;
        if settlement.is_some()
            && matches!(
                repaired,
                lash_core_execution::TurnCancelRepairResult::Applied(_)
            )
        {
            sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .closures
                    .delete_by_turn
                    .sql(),
            )
            .bind(session_id.as_str())
            .bind(turn_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(repaired)
    }
}
