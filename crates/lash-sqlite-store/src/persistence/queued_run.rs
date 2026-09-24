use super::*;
use lash_core_execution::store::{
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

fn scope_key(scope: &lash_core_execution::ExecutionScope) -> Result<String, StoreError> {
    scope
        .journal_identity()
        .map(|identity| identity.key().to_owned())
        .map_err(|error| StoreError::Backend(error.to_string()))
}

pub(super) fn load_run_conn(
    tx: &Connection,
    session_id: &SessionId,
    scope: Option<&lash_core_execution::ExecutionScope>,
) -> Result<Option<QueuedRunAdmission>, StoreError> {
    let sql = run_sql();
    let payload: Option<(String, String, i64)> = match scope {
        Some(scope) => tx.query_row(
            sql.by_scope.sql(),
            params![session_id.as_str(), scope_key(scope)?],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ),
        None => tx.query_row(sql.pending.sql(), params![session_id.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        }),
    }
    .optional()
    .map_err(sqlite_error)?;
    let Some((payload, status, revision)) = payload else {
        return Ok(None);
    };
    let mut admission: QueuedRunAdmission =
        serde_json::from_str(&payload).map_err(|error| StoreError::Backend(error.to_string()))?;
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
    let mut stmt = tx.prepare(sql.members.sql()).map_err(sqlite_error)?;
    let rows = stmt
        .query_map(
            params![session_id.as_str(), scope_key(&admission.scope)?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(sqlite_error)?;
    for row in rows {
        let (collection, ordinal, kind, id) = row.map_err(sqlite_error)?;
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

pub(super) fn write_run_conn(
    tx: &Connection,
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
        tx.execute(sql.insert.sql(), params![session_id.as_str(), key, json])
            .map_err(sqlite_error)?;
    } else {
        tx.execute(
            sql.update.sql(),
            params![
                session_id.as_str(),
                key,
                json,
                if admission.terminal.is_some() {
                    "settled"
                } else {
                    "pending"
                },
                sql_counter_value("queued_run_revision", admission.revision)?
            ],
        )
        .map_err(sqlite_error)?;
        tx.execute(sql.clear_members.sql(), params![session_id.as_str(), key])
            .map_err(sqlite_error)?;
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
            tx.execute(
                sql.insert_member.sql(),
                params![
                    session_id.as_str(),
                    key,
                    collection,
                    ordinal as i64,
                    kind,
                    id
                ],
            )
            .map_err(sqlite_error)?;
        }
    }
    Ok(())
}

impl Store {
    pub(super) async fn begin_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        request: BeginQueuedRun,
    ) -> Result<QueuedRunAdmission, StoreError> {
        request.validate(fence)?;
        let fence = fence.clone();
        let clock = self.clock.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_not_deleted_conn(tx, &request.session_id)?;
                    ensure_session_execution_lease_conn(
                        tx,
                        &request.session_id,
                        &fence,
                        clock.timestamp_ms(),
                    )?;
                    if request.identity.is_some()
                        && let Some(admission) =
                            load_run_conn(tx, &request.session_id, request.identity.as_ref())?
                    {
                        let resumed = request.resume(&admission)?;
                        if resumed.origin != admission.origin {
                            write_run_conn(tx, &resumed, false)?;
                        }
                        return Ok(resumed);
                    }
                    if let Some(admission) = load_run_conn(tx, &request.session_id, None)? {
                        let resumed = request.resume(&admission)?;
                        if resumed.origin != admission.origin {
                            write_run_conn(tx, &resumed, false)?;
                        }
                        return Ok(resumed);
                    }
                    let actual = try_load_session_head_meta_from_conn(tx, &request.session_id)?
                        .map_or(0, |head| head.head_revision);
                    if actual != request.expected_head_revision {
                        return Err(StoreError::HeadRevisionConflict {
                            expected: request.expected_head_revision,
                            actual,
                        });
                    }
                    let admission = request.admit(uuid::Uuid::new_v4().to_string());
                    write_run_conn(tx, &admission, true)?;
                    Ok(admission)
                })();
                Ok(match outcome {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }
    pub(super) async fn pending_run(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<QueuedRunAdmission>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .read(move |tx| {
                Ok(ensure_session_not_deleted_conn(tx, &session_id)
                    .and_then(|()| load_run_conn(tx, &session_id, None)))
            })
            .await
            .map_err(sqlite_error)?
    }
    pub(super) async fn run_by_scope(
        &self,
        scope: &lash_core_execution::ExecutionScope,
    ) -> Result<Option<QueuedRunAdmission>, StoreError> {
        let Some(session_id) = scope.session_id().cloned() else {
            return Ok(None);
        };
        let scope = scope.clone();
        self.conn
            .read(move |tx| {
                Ok(ensure_session_not_deleted_conn(tx, &session_id)
                    .and_then(|()| load_run_conn(tx, &session_id, Some(&scope))))
            })
            .await
            .map_err(sqlite_error)?
    }
    pub(super) async fn settle_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        settlement: QueuedRunCommit,
    ) -> Result<QueuedRunAdmission, StoreError> {
        let fence = fence.clone();
        let clock = self.clock.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_not_deleted_conn(tx, &fence.session_id)?;
                    ensure_session_execution_lease_conn(
                        tx,
                        &fence.session_id,
                        &fence,
                        clock.timestamp_ms(),
                    )?;
                    if !matches!(
                        settlement.progress,
                        QueuedRunProgress::Settle { .. } | QueuedRunProgress::ForgetUnworked
                    ) {
                        return Err(conflict(&fence.session_id));
                    }
                    let admission = load_run_conn(tx, &fence.session_id, Some(&settlement.scope))?
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
                        QueuedRunProgress::ForgetUnworked if admission.can_forget_unworked() => {}
                        _ => return Err(conflict(&fence.session_id)),
                    }
                    let next = admission.advance(&settlement, &[])?;
                    if admission.terminal.is_some() {
                        return Ok(next);
                    }
                    settle_run_members_conn(tx, &fence, &settlement.scope)?;
                    // Settling the run settles the turn it had parked (FIG-3586).
                    let released: Option<(String, i64)> = tx
                        .query_row(
                            crate::turn_ingress::turn_ingress_sql()
                                .turn_parks
                                .delete_by_session_returning
                                .sql(),
                            params![fence.session_id.as_str()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    if let Some((released_turn_id, released_park_id)) = released {
                        crate::persistence::turn_park_feed::log_turn_park_closed_conn(
                            tx,
                            &fence.session_id,
                            &released_turn_id,
                            released_park_id,
                            &lash_core_execution::store::TurnParkEventKind::Unparked {
                                cause: lash_core_execution::store::UnparkCause::RunSettled,
                            },
                            crate::clamp_epoch_ms(clock.timestamp_ms()),
                        )?;
                    }
                    if matches!(settlement.progress, QueuedRunProgress::ForgetUnworked) {
                        let scope_key = scope_key(&settlement.scope)?;
                        tx.execute(
                            run_sql().clear_members.sql(),
                            params![fence.session_id.as_str(), &scope_key],
                        )
                        .map_err(sqlite_error)?;
                        tx.execute(
                            run_sql().delete_scope.sql(),
                            params![fence.session_id.as_str(), &scope_key],
                        )
                        .map_err(sqlite_error)?;
                    } else {
                        write_run_conn(tx, &next, false)?;
                    }
                    Ok(next)
                })();
                Ok(match outcome {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }
    pub(super) async fn select_run(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        scope: &lash_core_execution::ExecutionScope,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
        configuration: &lash_core_execution::PersistedSessionConfig,
        policy: QueuedWorkClaimPolicy,
    ) -> Result<SelectedQueuedRun, StoreError> {
        let fence = fence.clone();
        let scope = scope.clone();
        let owner = owner.clone();
        let configuration = configuration.clone();
        let clock = self.clock.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    let now = clock.timestamp_ms();
                    ensure_session_not_deleted_conn(tx, &fence.session_id)?;
                    ensure_session_execution_lease_conn(tx, &fence.session_id, &fence, now)?;
                    let mut admission = load_run_conn(tx, &fence.session_id, Some(&scope))?
                        .ok_or_else(|| conflict(&fence.session_id))?;
                    if admission.terminal.is_some() {
                        return Err(conflict(&fence.session_id));
                    }
                    if let Some(members) = &admission.members {
                        if admission.configuration != configuration {
                            return Err(StoreError::QueuedRunConfigurationChanged {
                                session_id: fence.session_id.clone(),
                            });
                        }
                        let (inputs, queued) =
                            reclaim_run_members_conn(tx, now, &fence, &owner, members)?;
                        let assigned = open_assigned_members_conn(tx, &fence, &admission)?;
                        let (reacquired_inputs, reacquired_queued) =
                            reclaim_run_members_conn(tx, now, &fence, &owner, &assigned)?;
                        let already_satisfied = admission.already_satisfied_batch_ids();
                        return Ok(SelectedQueuedRun {
                            admission,
                            inputs,
                            queued,
                            already_satisfied,
                            refusal: None,
                            reacquired_inputs,
                            reacquired_queued,
                        });
                    }
                    let inputs = if matches!(admission.request, QueuedRunRequest::Automatic) {
                        require_claim(
                            claim_pending_turn_inputs_sqlite_conn(
                                tx,
                                now,
                                &fence.session_id,
                                &fence,
                                &owner,
                                max_inputs,
                                lash_core_execution::TurnInputClaimMode::NextTurn,
                            )?,
                            &fence.session_id,
                        )?
                    } else {
                        None
                    };
                    let (queued, already_satisfied, refusal) = match &admission.request {
                        QueuedRunRequest::Automatic if inputs.is_some() => (None, Vec::new(), None),
                        QueuedRunRequest::Automatic => {
                            let queued = require_claim(
                                claim_ready_queued_work_sqlite_conn(
                                    tx,
                                    now,
                                    &fence.session_id,
                                    &fence,
                                    &owner,
                                    QueuedWorkClaimBoundary::Idle,
                                    policy.clone(),
                                )?,
                                &fence.session_id,
                            )?;
                            let refusal = if queued.is_none() {
                                Some(
                                    match sqlite_refusal_for_empty_scan(
                                        tx,
                                        &fence.session_id,
                                        now,
                                        fence.fencing_token,
                                        QueuedWorkClaimBoundary::Idle,
                                        &policy,
                                    )? {
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
                            let outcome = claim_selected_queued_work_sqlite_conn(
                                tx,
                                now,
                                &fence.session_id,
                                &fence,
                                &owner,
                                QueuedWorkClaimBoundary::Idle,
                                batch_ids,
                                policy,
                            )?;
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
                    admission.configuration = configuration;
                    admission.revision = StoreError::checked_monotonic_increment(
                        "queued_run_revision",
                        admission.revision,
                    )?;
                    write_run_conn(tx, &admission, false)?;
                    Ok(SelectedQueuedRun {
                        admission,
                        inputs: inputs.into_iter().collect(),
                        queued: queued.into_iter().collect(),
                        already_satisfied,
                        refusal,
                        reacquired_inputs: Vec::new(),
                        reacquired_queued: Vec::new(),
                    })
                })();
                Ok(match outcome {
                    Ok(value) => TxOutcome::Commit(Ok(value)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }
}

fn require_claim<T>(outcome: TxOutcome<T>, session_id: &SessionId) -> Result<T, StoreError> {
    match outcome {
        TxOutcome::Commit(claim) => Ok(claim),
        TxOutcome::Rollback(_) => Err(conflict(session_id)),
    }
}

/// The run's checkpoint-assigned rows, beyond its members, that are still
/// open and held by a claim. Settled rows are gone or terminal; a row whose
/// claim was released went back to the queue. Neither is retaken.
fn open_assigned_members_conn(
    tx: &Connection,
    fence: &SessionExecutionLeaseAuthority,
    admission: &QueuedRunAdmission,
) -> Result<Vec<QueuedRunMember>, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut open = Vec::new();
    for member in admission.assigned_non_members() {
        let is_open = match member {
            QueuedRunMember::Input(id) => tx
                .query_row(
                    sql.pending_inputs.select_by_id.sql(),
                    params![fence.session_id.as_str(), id.as_str()],
                    pending_turn_input_row_from_sql,
                )
                .optional()
                .map_err(sqlite_error)?
                .filter(|row| row.claim_token.is_some())
                .map(pending_turn_input_from_row)
                .transpose()?
                .is_some_and(|input| !input.state.is_terminal()),
            QueuedRunMember::Batch(id) => tx
                .query_row(
                    sql.queued_batches.select_by_id.sql(),
                    params![id.as_str()],
                    queued_batch_row_from_sql,
                )
                .optional()
                .map_err(sqlite_error)?
                .is_some_and(|row| row.session_id == fence.session_id && row.claim_token.is_some()),
        };
        if is_open {
            open.push(member.clone());
        }
    }
    Ok(open)
}

fn reclaim_run_members_conn(
    tx: &Connection,
    now: u64,
    fence: &SessionExecutionLeaseAuthority,
    owner: &LeaseOwnerIdentity,
    members: &[QueuedRunMember],
) -> Result<
    (
        Vec<lash_core_execution::TurnInputClaim>,
        Vec<QueuedWorkClaim>,
    ),
    StoreError,
> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let mut inputs = Vec::new();
    let mut batches = Vec::new();
    for member in members {
        match member {
            QueuedRunMember::Input(id) => {
                let row = tx
                    .query_row(
                        sql.pending_inputs.select_by_id.sql(),
                        params![fence.session_id.as_str(), id.as_str()],
                        pending_turn_input_row_from_sql,
                    )
                    .optional()
                    .map_err(sqlite_error)?
                    .ok_or_else(|| conflict(&fence.session_id))?;
                let input = pending_turn_input_from_row(row.clone())?;
                if input.state.is_terminal() {
                    return Err(conflict(&fence.session_id));
                }
                inputs.push((row, input));
            }
            QueuedRunMember::Batch(id) => {
                let row = tx
                    .query_row(
                        sql.queued_batches.select_by_id.sql(),
                        params![id.as_str()],
                        queued_batch_row_from_sql,
                    )
                    .optional()
                    .map_err(sqlite_error)?
                    .ok_or_else(|| conflict(&fence.session_id))?;
                if row.session_id != fence.session_id {
                    return Err(conflict(&fence.session_id));
                }
                batches.push(row);
            }
        }
    }
    let mut input_claims = Vec::new();
    let mut remaining_inputs = inputs.into_iter().peekable();
    while let Some(first) = remaining_inputs.next() {
        let identity = |row: &PendingTurnInputRow| {
            (row.claim_session_lease_generation == fence.fencing_token && row.claim_token.is_some())
                .then(|| {
                    (
                        row.claim_id.clone(),
                        row.claim_token.clone(),
                        row.claim_owner.clone(),
                    )
                })
        };
        let head_identity = identity(&first.0);
        let mut inputs = vec![first];
        while let Some(row) = remaining_inputs.next_if(|(row, _)| identity(row) == head_identity) {
            inputs.push(row);
        }
        let input_claim = match inputs.first() {
            Some((head, _))
                if head.claim_session_lease_generation == fence.fencing_token
                    && head.claim_token.is_some() =>
            {
                if inputs.iter().any(|(row, _)| {
                    row.claim_id != head.claim_id
                        || row.claim_token != head.claim_token
                        || row.claim_owner.as_ref() != Some(owner)
                        || row.claim_session_lease_generation != fence.fencing_token
                }) {
                    return Err(conflict(&fence.session_id));
                }
                Some(lash_core_execution::TurnInputClaim {
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
                    data: lash_core_execution::runtime::TurnInputClaimData {
                        mode: lash_core_execution::TurnInputClaimMode::NextTurn,
                        inputs: inputs.into_iter().map(|(_, input)| input).collect(),
                        applications: Vec::new(),
                    },
                })
            }
            _ => require_claim(
                claim_turn_input_rows_sqlite_conn(
                    tx,
                    now,
                    &fence.session_id,
                    fence,
                    owner,
                    lash_core_execution::TurnInputClaimMode::NextTurn,
                    inputs,
                    None,
                )?,
                &fence.session_id,
            )?,
        };
        if let Some(claim) = input_claim {
            input_claims.push(claim);
        }
    }
    let hydrated = queued_work_batches_from_conn(tx, &batches)?;
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
                    data: lash_core_execution::store_backend_support::queued_work_claim_data(
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
                    claim_queued_work_rows_sqlite(
                        tx,
                        now,
                        &fence.session_id,
                        owner,
                        fence.fencing_token,
                        &batches,
                        hydrated,
                        &candidates,
                    )?,
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

pub(super) fn settle_run_members_conn(
    tx: &Connection,
    fence: &SessionExecutionLeaseAuthority,
    scope: &lash_core_execution::ExecutionScope,
) -> Result<(), StoreError> {
    tx.execute(
        run_sql().cancel_inputs.sql(),
        params![
            fence.session_id.as_str(),
            scope_key(scope)?,
            lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str()
        ],
    )
    .map_err(sqlite_error)?;
    for statement in [run_sql().delete_items.sql(), run_sql().delete_batches.sql()] {
        tx.execute(
            statement,
            params![fence.session_id.as_str(), scope_key(scope)?],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

pub(super) fn validate_run_members_conn(
    tx: &Connection,
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
        let authorized: bool = tx
            .query_row(
                run_sql().authorized_member.sql(),
                params![fence.session_id.as_str(), key, kind, id],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        if !authorized {
            return Err(conflict(&fence.session_id));
        }
    }
    Ok(())
}
