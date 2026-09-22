use super::*;
use lash_core::store::{
    BeginQueuedRun, QueuedRunAdmission, QueuedRunCommit, QueuedRunMember, QueuedRunProgress,
    QueuedRunRequest, QueuedRunTerminal, SelectedQueuedRun,
};
use lash_store_sql::turn_ingress::queued_runs::QueuedRunStatements;

fn run_sql() -> &'static QueuedRunStatements {
    &crate::turn_ingress::turn_ingress_sql().queued_runs
}
fn conflict(session_id: &SessionId) -> StoreError {
    StoreError::QueuedRunConflict {
        session_id: session_id.clone(),
    }
}
fn scope_key(scope: &lash_core::ExecutionScope) -> Result<String, StoreError> {
    scope
        .journal_identity()
        .map(|identity| identity.key().to_owned())
        .map_err(|error| StoreError::Backend(error.to_string()))
}

pub(super) async fn load_run_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    scope: Option<&lash_core::ExecutionScope>,
) -> Result<Option<QueuedRunAdmission>, StoreError> {
    let sql = run_sql();
    let mut query = sqlx::query(if scope.is_some() {
        sql.by_scope.sql()
    } else {
        sql.pending.sql()
    })
    .bind(session_id.as_str());
    if let Some(scope) = scope {
        query = query.bind(scope_key(scope)?);
    }
    let Some(row) = query
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
    else {
        return Ok(None);
    };
    let payload: String = row.try_get(0).map_err(store_sqlx_error)?;
    let status: String = row.try_get(1).map_err(store_sqlx_error)?;
    let revision: i64 = row.try_get(2).map_err(store_sqlx_error)?;
    let mut admission: QueuedRunAdmission = store_decode_json(&payload, "queued run admission")?;
    if admission.scope.session_id() != Some(session_id)
        || scope.is_some_and(|scope| scope != &admission.scope)
    {
        return Err(conflict(session_id));
    }
    if revision < 0
        || admission.revision != revision as u64
        || status
            != if admission.terminal.is_some() {
                "settled"
            } else {
                "pending"
            }
        || admission
            .members
            .as_ref()
            .is_some_and(|members| !members.is_empty())
        || admission
            .initial_members
            .as_ref()
            .is_some_and(|members| !members.is_empty())
        || !admission.withheld_members.is_empty()
        || !admission.assigned_members.is_empty()
        || admission.members.is_some() != admission.initial_members.is_some()
    {
        return Err(conflict(session_id));
    }
    let rows = sqlx::query(sql.members.sql())
        .bind(session_id.as_str())
        .bind(scope_key(&admission.scope)?)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    for row in rows {
        let collection: String = row.try_get(0).map_err(store_sqlx_error)?;
        let ordinal: i64 = row.try_get(1).map_err(store_sqlx_error)?;
        let kind: String = row.try_get(2).map_err(store_sqlx_error)?;
        let id: String = row.try_get(3).map_err(store_sqlx_error)?;
        let members = match collection.as_str() {
            "initial" => admission.initial_members.as_mut(),
            "current" => admission.members.as_mut(),
            "withheld" => Some(&mut admission.withheld_members),
            "assigned" => Some(&mut admission.assigned_members),
            _ => None,
        }
        .ok_or_else(|| conflict(session_id))?;
        if ordinal != members.len() as i64 {
            return Err(conflict(session_id));
        }
        members.push(match kind.as_str() {
            "input" => QueuedRunMember::Input(id.into()),
            "batch" => QueuedRunMember::Batch(id.into()),
            _ => return Err(conflict(session_id)),
        });
    }
    Ok(Some(admission))
}

pub(super) async fn write_run_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    admission: &QueuedRunAdmission,
    insert: bool,
) -> Result<(), StoreError> {
    let session_id = admission
        .scope
        .session_id()
        .ok_or_else(|| StoreError::Backend("queued run missing session".into()))?;
    let key = scope_key(&admission.scope)?;
    let mut metadata = admission.clone();
    metadata.members = metadata.members.map(|_| Vec::new());
    metadata.initial_members = metadata.initial_members.map(|_| Vec::new());
    metadata.withheld_members.clear();
    metadata.assigned_members.clear();
    let json = encode_json(&metadata)?;
    let sql = run_sql();
    if insert {
        sqlx::query(sql.insert.sql())
            .bind(session_id.as_str())
            .bind(&key)
            .bind(json)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    } else {
        sqlx::query(sql.update.sql())
            .bind(session_id.as_str())
            .bind(&key)
            .bind(json)
            .bind(if admission.terminal.is_some() {
                "settled"
            } else {
                "pending"
            })
            .bind(sql_counter_value(
                "queued_run_revision",
                admission.revision,
            )?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        sqlx::query(sql.clear_members.sql())
            .bind(session_id.as_str())
            .bind(&key)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    for (collection, members) in [
        ("initial", &admission.initial_members),
        ("current", &admission.members),
        ("withheld", &Some(admission.withheld_members.clone())),
        ("assigned", &Some(admission.assigned_members.clone())),
    ] {
        for (ordinal, member) in members.iter().flatten().enumerate() {
            let (kind, id) = match member {
                QueuedRunMember::Input(id) => ("input", id.as_str()),
                QueuedRunMember::Batch(id) => ("batch", id.as_str()),
            };
            sqlx::query(sql.insert_member.sql())
                .bind(session_id.as_str())
                .bind(&key)
                .bind(collection)
                .bind(ordinal as i64)
                .bind(kind)
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    Ok(())
}

impl PostgresSessionStore {
    pub(super) async fn begin_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        request: BeginQueuedRun,
    ) -> Result<QueuedRunAdmission, StoreError> {
        request.validate(fence)?;
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, &request.session_id).await?;
        ensure_session_execution_lease_tx(&mut tx, &request.session_id, fence).await?;
        if request.identity.is_some()
            && let Some(admission) =
                load_run_tx(&mut tx, &request.session_id, request.identity.as_ref()).await?
        {
            return request.resume(&admission);
        }
        if let Some(admission) = load_run_tx(&mut tx, &request.session_id, None).await? {
            return request.resume(&admission);
        }
        let actual = load_session_head_meta_tx(&mut tx, &request.session_id, false)
            .await?
            .map_or(0, |head| head.head_revision);
        if actual != request.expected_head_revision {
            return Err(StoreError::HeadRevisionConflict {
                expected: request.expected_head_revision,
                actual,
            });
        }
        let admission = request.admit(uuid::Uuid::new_v4().to_string());
        write_run_tx(&mut tx, &admission, true).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(admission)
    }
    pub(super) async fn pending_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<QueuedRunAdmission>, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        ensure_session_not_deleted_tx(&mut tx, session_id).await?;
        load_run_tx(&mut tx, session_id, None).await
    }
    pub(super) async fn settle_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        settlement: QueuedRunCommit,
    ) -> Result<QueuedRunAdmission, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut tx)
            .await?;
        ensure_session_not_deleted_tx(&mut tx, &fence.session_id).await?;
        ensure_session_execution_lease_tx(&mut tx, &fence.session_id, fence).await?;
        if !matches!(settlement.progress, QueuedRunProgress::Settle { .. }) {
            return Err(conflict(&fence.session_id));
        }
        let admission = load_run_tx(&mut tx, &fence.session_id, Some(&settlement.scope))
            .await?
            .ok_or_else(|| conflict(&fence.session_id))?;
        match &settlement.progress {
            QueuedRunProgress::Settle {
                terminal: QueuedRunTerminal::Failed { .. },
            } => {}
            QueuedRunProgress::Settle {
                terminal: QueuedRunTerminal::Empty,
            } if admission.members.as_ref().is_some_and(Vec::is_empty)
                && admission.withheld_members.is_empty()
                && admission.assigned_members.is_empty() => {}
            _ => return Err(conflict(&fence.session_id)),
        }
        let next = admission.advance(&settlement, &[])?;
        if admission.terminal.is_some() {
            return Ok(next);
        }
        settle_run_members_tx(&mut tx, fence, &settlement.scope).await?;
        write_run_tx(&mut tx, &next, false).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(next)
    }
    pub(super) async fn select_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        scope: &lash_core::ExecutionScope,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &lash_core::PersistedSessionConfig,
        policy: QueuedWorkClaimPolicy,
    ) -> Result<SelectedQueuedRun, StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut transaction = connection.begin().await.map_err(store_sqlx_error)?;
        #[cfg(any(test, feature = "testing"))]
        self.set_transaction_lease_clock_for_testing(&mut transaction)
            .await?;
        let result = {
            let tx = &mut transaction;
            async {
                let now = postgres_transaction_epoch_ms(tx).await?;
                ensure_session_not_deleted_tx(tx, &fence.session_id).await?;
                ensure_session_execution_lease_tx(tx, &fence.session_id, fence).await?;
                let mut admission = load_run_tx(tx, &fence.session_id, Some(scope))
                    .await?
                    .ok_or_else(|| conflict(&fence.session_id))?;
                if admission.terminal.is_some() {
                    return Err(conflict(&fence.session_id));
                }
                if let Some(members) = &admission.members {
                    if &admission.configuration != configuration {
                        return Err(StoreError::QueuedRunConfigurationChanged {
                            session_id: fence.session_id.clone(),
                        });
                    }
                    let (inputs, queued) =
                        reclaim_run_members_tx(tx, now, fence, owner, members).await?;
                    let already_satisfied = admission.already_satisfied_batch_ids();
                    return Ok(SelectedQueuedRun {
                        admission,
                        inputs,
                        queued,
                        already_satisfied,
                        refusal: None,
                    });
                }
                let inputs = if matches!(admission.request, QueuedRunRequest::Automatic) {
                    require_claim(
                        claim_pending_turn_inputs_postgres_tx(
                            tx,
                            &fence.session_id,
                            fence,
                            owner,
                            max_inputs,
                            lash_core::TurnInputClaimMode::NextTurn,
                        )
                        .await?,
                        &fence.session_id,
                    )?
                } else {
                    None
                };
                let (queued, already_satisfied, refusal) = match &admission.request {
                    QueuedRunRequest::Automatic if inputs.is_some() => (None, Vec::new(), None),
                    QueuedRunRequest::Automatic => {
                        let queued = require_claim(
                            claim_ready_queued_work_postgres_tx(
                                tx,
                                &fence.session_id,
                                fence,
                                owner,
                                QueuedWorkClaimBoundary::Idle,
                                policy.clone(),
                            )
                            .await?,
                            &fence.session_id,
                        )?;
                        let refusal = if queued.is_none() {
                            Some(
                                match postgres_refusal_for_empty_scan(
                                    tx,
                                    &fence.session_id,
                                    fence.fencing_token,
                                    QueuedWorkClaimBoundary::Idle,
                                    &policy,
                                )
                                .await?
                                {
                                    TurnWorkEmptyScanDiagnostic::Refused { reason } => reason,
                                    _ => QueuedWorkClaimRefusal::Empty,
                                },
                            )
                        } else {
                            None
                        };
                        (queued, Vec::new(), refusal)
                    }
                    QueuedRunRequest::Selected { batch_ids } => {
                        let outcome = claim_selected_queued_work_postgres_tx(
                            tx,
                            &fence.session_id,
                            fence,
                            owner,
                            QueuedWorkClaimBoundary::Idle,
                            batch_ids,
                            policy,
                        )
                        .await?;
                        let unclaimed_batch_ids = batch_ids
                            .iter()
                            .filter(|id| {
                                !outcome.already_satisfied_batch_ids.contains(id)
                                    && !outcome.claim.as_ref().is_some_and(|claim| {
                                        claim.batches.iter().any(|batch| batch.batch_id == *id)
                                    })
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        if !unclaimed_batch_ids.is_empty() {
                            return Err(StoreError::SelectedQueuedRunIncomplete {
                                unclaimed_batch_ids,
                            });
                        }
                        (outcome.claim, outcome.already_satisfied_batch_ids, None)
                    }
                };
                if inputs.is_none()
                    && queued.is_none()
                    && refusal.is_some_and(|reason| reason != QueuedWorkClaimRefusal::Empty)
                {
                    return Ok(SelectedQueuedRun {
                        admission,
                        inputs: inputs.into_iter().collect(),
                        queued: queued.into_iter().collect(),
                        already_satisfied,
                        refusal,
                    });
                }
                let members = inputs
                    .iter()
                    .flat_map(|claim| {
                        claim
                            .inputs
                            .iter()
                            .map(|input| QueuedRunMember::Input(input.input_id.clone()))
                    })
                    .chain(queued.iter().flat_map(|claim| {
                        claim
                            .batches
                            .iter()
                            .map(|batch| QueuedRunMember::Batch(batch.batch_id.clone()))
                    }))
                    .collect::<Vec<_>>();
                admission.members = Some(members.clone());
                admission.initial_members = Some(members);
                admission.configuration = configuration.clone();
                admission.revision = StoreError::checked_monotonic_increment(
                    "queued_run_revision",
                    admission.revision,
                )?;
                write_run_tx(tx, &admission, false).await?;
                Ok(SelectedQueuedRun {
                    admission,
                    inputs: inputs.into_iter().collect(),
                    queued: queued.into_iter().collect(),
                    already_satisfied,
                    refusal,
                })
            }
            .await
        }?;
        transaction.commit().await.map_err(store_sqlx_error)?;
        Ok(result)
    }
}

fn require_claim<T>(
    outcome: ClaimTransactionOutcome<T>,
    session_id: &SessionId,
) -> Result<T, StoreError> {
    match outcome {
        ClaimTransactionOutcome::Commit(claim) => Ok(claim),
        ClaimTransactionOutcome::Rollback(_) => Err(conflict(session_id)),
    }
}

async fn reclaim_run_members_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    now: u64,
    fence: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    members: &[QueuedRunMember],
) -> Result<(Vec<lash_core::TurnInputClaim>, Vec<QueuedWorkClaim>), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut inputs = Vec::new();
    let mut batches = Vec::new();
    for member in members {
        match member {
            QueuedRunMember::Input(id) => {
                let row = sqlx::query(sql.pending_inputs.select_by_id.sql())
                    .bind(fence.session_id.as_str())
                    .bind(id.as_str())
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .ok_or_else(|| conflict(&fence.session_id))?;
                let row = pending_turn_input_row(row)?;
                let input = pending_turn_input_from_row(row.clone())?;
                if input.state.is_terminal() {
                    return Err(conflict(&fence.session_id));
                }
                inputs.push((row, input));
            }
            QueuedRunMember::Batch(id) => {
                let row = sqlx::query(sql.queued_batches.select_by_id.sql())
                    .bind(id.as_str())
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .ok_or_else(|| conflict(&fence.session_id))?;
                let row = queued_batch_row(row)?;
                batches.push(row);
            }
        }
    }
    let mut input_claims = Vec::new();
    let mut remaining_inputs = inputs.into_iter().peekable();
    while let Some(first) = remaining_inputs.next() {
        let identity = |row: &PendingTurnInputRow| {
            (row.claim_session_lease_generation() == fence.fencing_token && row.is_claimed()).then(
                || {
                    row.claim_identity()
                        .map(|(id, token, owner)| (id.to_owned(), token.to_owned(), owner.clone()))
                },
            )
        };
        let head_identity = identity(&first.0);
        let mut inputs = vec![first];
        while let Some(row) = remaining_inputs.next_if(|(row, _)| identity(row) == head_identity) {
            inputs.push(row);
        }
        let input_claim = match inputs.first() {
            Some((head, _))
                if head.claim_session_lease_generation() == fence.fencing_token
                    && head.is_claimed() =>
            {
                let (claim_id, claim_token, claim_owner) = head
                    .claim_identity()
                    .ok_or_else(|| conflict(&fence.session_id))?;
                if claim_owner != owner
                    || inputs.iter().any(|(row, _)| {
                        row.claim_identity() != Some((claim_id, claim_token, owner))
                            || row.claim_session_lease_generation() != fence.fencing_token
                    })
                {
                    return Err(conflict(&fence.session_id));
                }
                Some(lash_core::TurnInputClaim {
                    session_id: fence.session_id.clone(),
                    owner: owner.clone(),
                    claim_id: claim_id.to_owned(),
                    lease_token: claim_token.to_owned(),
                    fencing_token: head.claim_fencing_token,
                    session_lease_generation: fence.fencing_token,
                    data: lash_core::runtime::TurnInputClaimData {
                        mode: lash_core::TurnInputClaimMode::NextTurn,
                        inputs: inputs.into_iter().map(|(_, input)| input).collect(),
                        applications: Vec::new(),
                    },
                })
            }
            _ => require_claim(
                claim_turn_input_rows_postgres_tx(
                    tx,
                    now,
                    &fence.session_id,
                    fence,
                    owner,
                    lash_core::TurnInputClaimMode::NextTurn,
                    inputs,
                )
                .await?,
                &fence.session_id,
            )?,
        };
        if let Some(claim) = input_claim {
            input_claims.push(claim);
        }
    }
    let mut hydrated = Vec::with_capacity(batches.len());
    for row in &batches {
        let batch = queued_work_batch_from_row(tx, row.clone()).await?;
        if batch.session_id != fence.session_id {
            return Err(conflict(&fence.session_id));
        }
        hydrated.push(batch);
    }
    let mut queued_claims = Vec::new();
    let mut remaining_batches = batches.into_iter().zip(hydrated).peekable();
    while let Some((first_row, first_batch)) = remaining_batches.next() {
        let identity = |row: &QueuedBatchRow| {
            (row.claim_session_lease_generation == fence.fencing_token && row.claim_token.is_some())
                .then(|| (row.claim_id.clone(), row.claim_token.clone()))
        };
        let head_identity = identity(&first_row);
        let mut batches = vec![first_row];
        let mut hydrated = vec![first_batch];
        while let Some((row, batch)) =
            remaining_batches.next_if(|(row, _)| identity(row) == head_identity)
        {
            batches.push(row);
            hydrated.push(batch);
        }
        let queued_claim = match batches.first() {
            Some(head)
                if head.claim_session_lease_generation == fence.fencing_token
                    && head.claim_token.is_some() =>
            {
                if batches.iter().any(|row| {
                    row.claim_id != head.claim_id
                        || row.claim_token != head.claim_token
                        || row.claim_session_lease_generation != fence.fencing_token
                }) {
                    return Err(conflict(&fence.session_id));
                }
                Some(QueuedWorkClaim {
                    session_id: fence.session_id.clone(),
                    owner: owner.clone(),
                    claim_id: head
                        .claim_id
                        .clone()
                        .ok_or_else(|| conflict(&fence.session_id))?,
                    lease_token: head
                        .claim_token
                        .clone()
                        .ok_or_else(|| conflict(&fence.session_id))?,
                    fencing_token: head.claim_fencing_token,
                    session_lease_generation: fence.fencing_token,
                    data: lash_core::store_backend_support::queued_work_claim_data(
                        hydrated, None, None,
                    )?,
                })
            }
            _ => {
                let candidates = batches
                    .iter()
                    .zip(hydrated.iter())
                    .map(|(row, batch)| claim_candidate_from_row(row, batch))
                    .collect::<Vec<_>>();
                require_claim(
                    claim_queued_work_rows_postgres(
                        tx,
                        now,
                        &fence.session_id,
                        owner,
                        fence.fencing_token,
                        &batches,
                        hydrated,
                        &candidates,
                    )
                    .await?,
                    &fence.session_id,
                )?
            }
        };
        if let Some(claim) = queued_claim {
            queued_claims.push(claim);
        }
    }
    Ok((input_claims, queued_claims))
}

pub(super) async fn settle_run_members_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &SessionExecutionLeaseAuthority,
    scope: &lash_core::ExecutionScope,
) -> Result<(), StoreError> {
    sqlx::query(run_sql().cancel_inputs.sql())
        .bind(fence.session_id.as_str())
        .bind(scope_key(scope)?)
        .bind(lash_core::TurnInputStateKind::Cancelled.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    for statement in [run_sql().delete_items.sql(), run_sql().delete_batches.sql()] {
        sqlx::query(statement)
            .bind(fence.session_id.as_str())
            .bind(scope_key(scope)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

pub(super) async fn validate_run_members_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &SessionExecutionLeaseAuthority,
    commit: &QueuedRunCommit,
) -> Result<(), StoreError> {
    let QueuedRunProgress::Advance {
        members,
        withheld_members,
        ..
    } = &commit.progress
    else {
        return Ok(());
    };
    let key = scope_key(&commit.scope)?;
    for member in members.iter().chain(withheld_members) {
        let (kind, id) = match member {
            QueuedRunMember::Input(id) => ("input", id.as_str()),
            QueuedRunMember::Batch(id) => ("batch", id.as_str()),
        };
        let authorized: bool = sqlx::query_scalar(run_sql().authorized_member.sql())
            .bind(fence.session_id.as_str())
            .bind(&key)
            .bind(kind)
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if !authorized {
            return Err(conflict(&fence.session_id));
        }
    }
    Ok(())
}
