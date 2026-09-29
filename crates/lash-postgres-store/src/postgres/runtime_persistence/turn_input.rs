use super::*;

#[async_trait::async_trait]
impl IngressStore for PostgresSessionStore {
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        fence: &lash_core_execution::store::DriveFence,
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
        super::drive_epoch::require_fence_tx(&mut tx, session_id, fence).await?;
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
        fence: &lash_core_execution::store::DriveFence,
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
        if authorization.session_id() != fence.session()
            || authorization.authorizing_fencing_token() != fence.epoch()
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
        fence: &lash_core_execution::store::DriveFence,
        binding_id: &str,
        admitted_scope: &ExecutionScope,
    ) -> Result<Vec<lash_core_execution::TurnCancelClosureAuthorization>, StoreError> {
        self.validate_turn_cancellation_binding(session_id, fence, binding_id, admitted_scope)
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

    async fn enqueue_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
    ) -> Result<Vec<lash_core_execution::PendingTurnInput>, StoreError> {
        use lash_core_execution::store_backend_support as support;
        let session_id = batch.session_id();
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        ensure_session_not_closing_tx(&mut tx, session_id).await?;
        // The session's write authority, held to the commit: every ingress
        // producer takes it before it allocates, so the absences read below
        // hold and the block allocated below is contiguous (FIG-3842).
        super::lock_session_history_mutation_tx(&mut tx, session_id).await?;
        let now = self.clock.timestamp_ms();
        let sql = crate::turn_ingress::turn_ingress_sql();
        let mut interned = std::collections::BTreeSet::new();
        let mut admitted = Vec::with_capacity(batch.drafts().len());
        for draft in batch.drafts() {
            let submission_digest = support::turn_input_submission_digest(draft)?;
            let by_source_key: Option<(String, String)> = match draft.source_key.as_deref() {
                Some(source_key) => {
                    sqlx::query_as(sql.pending_inputs.select_id_by_source_key.sql())
                        .bind(session_id.as_str())
                        .bind(source_key)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(store_sqlx_error)?
                }
                None => None,
            };
            let by_input_id: Option<(String, String)> =
                match (&by_source_key, draft.input_id.as_deref()) {
                    (None, Some(input_id)) => {
                        sqlx::query_as(sql.pending_inputs.select_session_by_input_id.sql())
                            .bind(input_id)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(store_sqlx_error)?
                    }
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
                    let enqueue_seq =
                        super::allocate_ingress_sequence_tx(&mut tx, session_id).await?;
                    let input_id = match draft.input_id.clone() {
                        Some(input_id) => input_id,
                        None => support::derive_pending_turn_input_id(
                            session_id,
                            draft.source_key.as_deref(),
                            now,
                            u64_from_sql("PendingTurnInput", "enqueue_seq", enqueue_seq)?,
                        ),
                    };
                    let state = lash_core_execution::TurnInputState::open(draft.ingress.clone());
                    let run_spec = admit_run_spec_tx(&mut tx, draft, &mut interned).await?;
                    sqlx::query(sql.pending_inputs.insert_new.sql())
                        .bind(enqueue_seq)
                        .bind(&input_id)
                        .bind(session_id.as_str())
                        .bind(&draft.source_key)
                        .bind(encode_json(&draft.ingress)?)
                        .bind(state.as_str())
                        .bind(encode_json(&draft.input)?)
                        .bind(&submission_digest)
                        .bind(now as i64)
                        .bind(run_spec.column())
                        .execute(&mut *tx)
                        .await
                        .map_err(|err| {
                            pending_turn_input_insert_error(err, session_id, &input_id)
                        })?;
                    // The admitted input owes its session a drive (ADR 0109
                    // §3): the row is armed in the transaction that admits
                    // it. A row an earlier submission admitted already
                    // carries its obligation and is left as it stands.
                    crate::ingress_obligation::arm_turn_input_tx(
                        &mut tx,
                        session_id,
                        input_id.as_str(),
                        now,
                    )
                    .await?;
                    input_id
                }
            };
            admitted.push(
                load_pending_turn_input(&mut tx, session_id, &input_id)
                    .await?
                    .ok_or_else(|| {
                        StoreError::Backend("admitted pending turn input disappeared".to_string())
                    })?,
            );
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(admitted)
    }

    /// This backend does not fold the follow-ups (FIG-3975): the probe, the
    /// enqueue, the relay's claim and the head read stay separate round-trips.
    async fn admit_pending_turn_inputs(
        &self,
        batch: lash_core_execution::PendingTurnInputBatch,
        _ingress_claim_ttl_ms: u64,
    ) -> Result<lash_core_execution::TurnInputAdmission, StoreError> {
        self.read_session_state_version().await?;
        self.enqueue_pending_turn_inputs(batch)
            .await
            .map(lash_core_execution::TurnInputAdmission::Enqueued)
    }

    async fn load_run_spec(
        &self,
        session_id: &SessionId,
        hash: &lash_core_execution::RunSpecHash,
    ) -> Result<Option<lash_core_execution::RunSpec>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let stored: Option<String> = sqlx::query_scalar(
            crate::turn_ingress::turn_ingress_sql()
                .run_specs
                .select_spec
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(hash.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        stored
            .map(|json| {
                lash_core_execution::RunSpec::from_canonical_json(&json).map_err(|error| {
                    StoreError::StoredDataCorrupt {
                        record_kind: "RunSpec",
                        message: error.to_string(),
                    }
                })
            })
            .transpose()
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::PendingTurnInputRead>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        // Open and admitted rows, and the rows a checkpoint accepted into a
        // running root, read in one snapshot and listed in `enqueue_seq`
        // order (FIG-4044). The isolation level must precede every other
        // statement in the transaction.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
        let mut inputs = Vec::new();
        for sql in [
            statements.list_undelivered.sql(),
            statements.list_accepted.sql(),
        ] {
            let rows = sqlx::query(sql)
                .bind(session_id.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            for row in rows {
                inputs.push(pending_turn_input_read_from_row(row)?);
            }
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        inputs.sort_by_key(|read| read.input.enqueue_seq);
        Ok(inputs)
    }

    async fn pending_turn_input(
        &self,
        session_id: &SessionId,
        input_id: &lash_core_execution::InputId,
    ) -> Result<Option<lash_core_execution::PendingTurnInputRead>, StoreError> {
        // One point read by primary key; the list's lifecycle filter is
        // applied to the one row here: a row is listed until it is completed
        // or cancelled, open or admitted to its root alike.
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let row = sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .select_by_id
                .sql(),
        )
        .bind(session_id.as_str())
        .bind(input_id.as_str())
        .fetch_optional(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
        Ok(row
            .map(pending_turn_input_read_from_row)
            .transpose()?
            .filter(|read| !read.input.state.is_terminal()))
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
                    Some(row) => cancel_pending_turn_input_row_tx(&mut tx, row).await?,
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
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            outcomes.push(cancel_pending_turn_input_row_tx(&mut tx, row).await?);
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

    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        self.enqueue_queued_work_pg(batch).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkEnqueueOutcome, StoreError> {
        self.enqueue_queued_work_with_outcome_pg(batch).await
    }

    async fn open_session_command_run(
        &self,
        fence: &lash_core_execution::store::DriveFence,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        super::open_session_command_run_postgres(self, fence).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        self.cancel_queued_work_batch_pg(session_id, batch_id).await
    }

    async fn queued_work_batch_completed(
        &self,
        session_id: &SessionId,
        batch_id: &str,
    ) -> Result<bool, StoreError> {
        self.queued_work_batch_completed_pg(session_id, batch_id)
            .await
    }

    async fn pending_session_work_ordering(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::PendingSessionWorkOrdering, StoreError> {
        self.pending_session_work_ordering_pg(session_id).await
    }

    async fn list_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.list_queued_work_pg(session_id).await
    }

    async fn list_open_queued_work(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.list_open_queued_work_pg(session_id).await
    }
}

/// Admit `draft`'s run spec inside its enqueue transaction (FIG-3838): refuse
/// a steering spec that differs from its running turn's, then intern a
/// non-default spec once per hash and refuse different bytes under an
/// interned hash. Both refusals roll the whole admission back. A hash this
/// batch's transaction already interned (`interned`) is not interned again.
///
/// The session lock the enqueue holds serializes concurrent interns of one
/// session, and a spec row is never rewritten, so the read-back sees the
/// bytes this or an earlier admission interned.
async fn admit_run_spec_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    draft: &lash_core_execution::PendingTurnInputDraft,
    interned: &mut std::collections::BTreeSet<String>,
) -> Result<lash_core_execution::store_backend_support::RunSpecAdmission, StoreError> {
    use lash_core_execution::store_backend_support as support;
    let spec = support::RunSpecAdmission::of(draft)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    if let Some(turn_id) = support::steering_run_spec_target(draft) {
        let addressed = sqlx::query(sql.pending_inputs.select_run_spec_by_source_key.sql())
            .bind(draft.session_id.as_str())
            .bind(turn_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .map(|row| {
                (
                    row.get::<String, _>("state"),
                    row.get::<Option<String>, _>("run_spec_hash"),
                )
            });
        match addressed {
            Some((state, hash)) => support::check_steering_run_spec(
                &draft.session_id,
                turn_id,
                &spec,
                Some((state.as_str(), hash.as_deref())),
            )?,
            // No input started `turn_id` under a source key: the addressed
            // turn may still be a running root of another kind (FIG-3877).
            None => check_unsourced_steering_run_spec_tx(tx, draft, turn_id, &spec).await?,
        }
    }
    if let Some((hash, canonical)) = spec.interned()
        && !interned.contains(hash)
    {
        sqlx::query(sql.run_specs.intern.sql())
            .bind(draft.session_id.as_str())
            .bind(hash)
            .bind(canonical)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let stored: String = sqlx::query_scalar(sql.run_specs.select_spec.sql())
            .bind(draft.session_id.as_str())
            .bind(hash)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
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
///   queued-headed parent leaves the unfinished root to decide.
/// * `turn_id` is a physical turn of the unfinished queued-headed root: it
///   started from no input, so it runs the default spec.
/// * Otherwise nothing running names `turn_id`: the steering input is a
///   next-turn root under its own spec.
async fn check_unsourced_steering_run_spec_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    draft: &lash_core_execution::PendingTurnInputDraft,
    turn_id: &lash_core_execution::TurnId,
    spec: &lash_core_execution::store_backend_support::RunSpecAdmission,
) -> Result<(), StoreError> {
    use lash_core_execution::store_backend_support as support;
    let sql = crate::turn_ingress::turn_ingress_sql();
    // `Some(hash)` is the shape the running root resolved under (`None` =
    // the default spec); `None` means the evidence did not decide.
    let mut running: Option<Option<String>> = None;
    if let Some(owed) = pending_follow_on_tx(tx, &draft.session_id, false)
        .await?
        .filter(|owed| owed.is_turn(turn_id))
    {
        running = match &owed.resolved_run {
            Some(resolved) => Some(resolved.spec.as_ref().map(|hash| hash.as_str().to_string())),
            // A fact written before the shape was recorded: the parent
            // root's own starting input names the shape instead.
            None => sqlx::query(sql.pending_inputs.select_run_spec_by_source_key.sql())
                .bind(draft.session_id.as_str())
                .bind(owed.root_turn_id().as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?
                .map(|row| row.get::<Option<String>, _>("run_spec_hash")),
        };
    }
    if running.is_none()
        && let Some(unfinished) =
            crate::session_roots::unfinished_root_conn(tx, &draft.session_id).await?
        && matches!(
            unfinished.head,
            lash_core_execution::store::AdmittedHead::Batch(_)
        )
        && lash_core_execution::store::PhysicalTurn::split_turn_id(turn_id).0 == unfinished.root
    {
        // A queued-headed root starts from no input, so it runs the default
        // spec.
        running = Some(None);
    }
    if let Some(hash) = running {
        support::check_running_root_run_spec(&draft.session_id, turn_id, spec, hash.as_deref())?;
    }
    Ok(())
}
