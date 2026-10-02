//! Admission (FIG-3927): the fenced writes that bind open rows of both
//! admission tables to a run, and the bindless read of the command lane.
//!
//! Every admission runs in one transaction: it checks the shift fence, reads
//! back what an earlier execution of the same step already bound, and
//! otherwise composes from open rows under `FOR UPDATE` and binds them, each
//! write predicated on the row still being open.

use super::*;
use lash_core_execution::store::queued_work::TurnWorkPrefix;
use lash_core_execution::store::{
    AdmittedHead, CheckpointAdmission, CheckpointAdmissionRequest, FollowOnAdmission,
    RUN_ADMISSION_STEP, RunAdmission, TurnLaneStop,
};

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// Whether the head's pending follow-on refuses `admission` (ADR 0101 §3):
/// every admission but the follow-on's own is blocked while it is set.
async fn follow_on_blocks_admission_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    admission: FollowOnAdmission<'_>,
) -> Result<bool, StoreError> {
    Ok(lash_core_execution::store::follow_on_blocks_admission(
        pending_follow_on_tx(tx, session_id, false).await?.as_ref(),
        admission,
    )
    .is_some())
}

/// Admit the run's turn-lane run and record it with its rows, bindings and
/// base, in one transaction ([`RunStore::admit_run`]).
///
/// A recorded admission is returned unchanged, whatever fence or incarnation
/// asks: a re-execution of the step reads back what it chose and never
/// widens (FIG-3840). An executor the recorded one excludes is refused
/// instead (FIG-4765).
///
/// [`RunStore::admit_run`]: lash_core_execution::store::RunStore::admit_run
pub(crate) async fn admit_run_postgres(
    store: &crate::PostgresStore,
    request: &lash_core_execution::store::AdmitRunRequest,
    prepared: Option<&lash_core_execution::store::PreparedRunAdmission>,
    anchor: &lash_core_execution::TraceAnchor,
) -> Result<Option<RunAdmission>, StoreError> {
    let session_id = request.session_id();
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    #[cfg(any(test, feature = "testing"))]
    store
        .set_transaction_lease_clock_for_testing(&mut tx)
        .await?;
    require_shift_fence_tx(&mut tx, &request.fence).await?;
    let runs = crate::session_runs::session_runs_sql();
    let existing: Option<Option<String>> = sqlx::query_scalar(runs.runs.select_admission.sql())
        .bind(session_id.as_str())
        .bind(request.run.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if let Some(Some(json)) = existing {
        let admission = crate::session_runs::decode_run_admission(&json)?;
        // The recorded executor decides who executes the run (FIG-4765).
        if admission.executor.excludes(&request.executor) {
            return Err(StoreError::RunHeldByAnotherExecutor {
                session_id: session_id.clone(),
                run: request.run.clone(),
                recorded: Box::new(admission.executor),
                admitting: Box::new(request.executor.clone()),
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(Some(admission));
    }
    if follow_on_blocks_admission_tx(&mut tx, session_id, FollowOnAdmission::Idle).await? {
        tx.rollback().await.map_err(store_sqlx_error)?;
        if prepared.is_some() {
            return Err(StoreError::PreparedRunAdmissionStale {
                session_id: session_id.clone(),
                run: request.run.clone(),
            });
        }
        return Ok(None);
    }
    if let Some(unfinished) = crate::session_runs::unfinished_run_conn(&mut tx, session_id).await? {
        return Err(StoreError::UnfinishedRunConflict {
            session_id: session_id.clone(),
            run: unfinished.run,
        });
    }
    let now = postgres_transaction_epoch_ms(&mut tx).await?;
    let (inputs, queued) = match &request.head {
        AdmittedHead::Input(head) => {
            let inputs = compose_next_turn_inputs_tx(
                &mut tx,
                now,
                session_id,
                request.max_inputs,
                &request.policy,
            )
            .await?;
            // A composition that misses the head takes nothing.
            let Some(inputs) =
                inputs.filter(|inputs| inputs.inputs.iter().any(|input| input.input_id == *head))
            else {
                tx.rollback().await.map_err(store_sqlx_error)?;
                if prepared.is_some() {
                    return Err(StoreError::PreparedRunAdmissionStale {
                        session_id: session_id.clone(),
                        run: request.run.clone(),
                    });
                }
                return Ok(None);
            };
            (Some(Box::new(inputs)), None)
        }
        AdmittedHead::Batch(head) => {
            let batches = compose_turn_lane_batches_tx(
                &mut tx,
                now,
                session_id,
                AdmissionBoundary::Idle,
                None,
                &request.policy,
            )
            .await?;
            if !batches.iter().any(|batch| batch.batch_id == *head) {
                tx.rollback().await.map_err(store_sqlx_error)?;
                if prepared.is_some() {
                    return Err(StoreError::PreparedRunAdmissionStale {
                        session_id: session_id.clone(),
                        run: request.run.clone(),
                    });
                }
                return Ok(None);
            }
            (
                None,
                Some(Box::new(lash_core_execution::runtime::AdmittedQueuedWork {
                    session_id: session_id.clone(),
                    batches,
                })),
            )
        }
    };
    let mut base = request.base.clone();
    base.generation =
        read_session_state_version_tx(&mut tx, session_id, true, store.fence.fleet()).await?;
    let trace = RunAdmission::trace_scope_of(
        session_id,
        &request.run,
        inputs.as_deref(),
        queued.as_deref(),
        anchor.clone(),
        now,
    );
    let admission = RunAdmission {
        head: request.head.clone(),
        inputs,
        queued,
        base,
        turn_index: request.turn_index,
        generation: request.generation.clone(),
        executor: request.executor.clone(),
        plugins: request.plugins.clone(),
        trace: Some(trace),
        recorded_by_this_call: prepared.is_some(),
    };
    let Some(prepared) = prepared else {
        tx.rollback().await.map_err(store_sqlx_error)?;
        return Ok(Some(admission));
    };
    if !prepared.matches(
        admission.inputs.as_deref(),
        admission.queued.as_deref(),
        &admission.base,
    )? {
        return Err(StoreError::PreparedRunAdmissionStale {
            session_id: session_id.clone(),
            run: request.run.clone(),
        });
    }
    if let Some(inputs) = admission.inputs.as_deref() {
        bind_turn_inputs_tx(&mut tx, now, &request.run, RUN_ADMISSION_STEP, inputs).await?;
    }
    if let Some(queued) = admission.queued.as_deref() {
        bind_batches_tx(
            &mut tx,
            now,
            session_id,
            &request.run,
            RUN_ADMISSION_STEP,
            &queued.batches,
        )
        .await?;
    }
    sqlx::query(session_sql().meta.retain_admission_base.sql())
        .bind(session_id.as_str())
        .bind(
            admission
                .base
                .checkpoint
                .as_ref()
                .map(|blob_ref| blob_ref.as_str()),
        )
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    crate::session_runs::bind_run_inputs_conn(
        &mut tx,
        session_id,
        &request.run,
        &admission.input_ids(),
    )
    .await?;
    let json = serde_json::to_string(&admission)
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    let changed = sqlx::query(runs.runs.write_admission.sql())
        .bind(session_id.as_str())
        .bind(request.run.as_str())
        .bind(json)
        .bind(request.admitted_generation.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
    if changed != 1 {
        return Err(StoreError::Backend(
            "run admission was already recorded".into(),
        ));
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(Some(admission))
}

/// Admit the checkpoint work of `request`'s run, keyed by its step
/// ([`RunStore::admit_at_checkpoint`]).
///
/// A read-only probe answers the common empty checkpoint without a write
/// transaction. It refuses a stale fence first, whatever the caps and
/// whatever is pending (FIG-3927 N4), and it also reports rows the step
/// already bound, so a re-executed step always reaches the read-back.
///
/// [`RunStore::admit_at_checkpoint`]: lash_core_execution::store::RunStore::admit_at_checkpoint
pub(crate) async fn admit_at_checkpoint_postgres(
    store: &crate::PostgresStore,
    request: &CheckpointAdmissionRequest,
) -> Result<CheckpointAdmission, StoreError> {
    #[cfg(test)]
    store
        .checkpoint_probe_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !checkpoint_work_pending_postgres(&store.pool, request, &store.observer).await? {
        return Ok(CheckpointAdmission::default());
    }
    #[cfg(test)]
    store
        .checkpoint_write_transaction_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let session_id = request.session_id();
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    #[cfg(any(test, feature = "testing"))]
    store
        .set_transaction_lease_clock_for_testing(&mut tx)
        .await?;
    require_shift_fence_tx(&mut tx, &request.fence).await?;
    let mode = lash_core_execution::TurnInputAdmissionMode::ActiveTurn {
        turn_id: request.turn_id.clone(),
        checkpoint: request.checkpoint,
    };
    let recorded =
        read_step_admission_tx(&mut tx, session_id, &request.run, &request.step, mode).await?;
    if !recorded.is_empty() {
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(recorded);
    }
    if follow_on_blocks_admission_tx(
        &mut tx,
        session_id,
        FollowOnAdmission::Checkpoint {
            turn_id: &request.turn_id,
        },
    )
    .await?
    {
        tx.rollback().await.map_err(store_sqlx_error)?;
        return Ok(CheckpointAdmission::default());
    }
    let now = postgres_transaction_epoch_ms(&mut tx).await?;
    let inputs = if request.max_inputs == 0 {
        None
    } else {
        compose_active_turn_inputs_tx(
            &mut tx,
            session_id,
            &request.turn_id,
            request.checkpoint,
            request.max_inputs,
        )
        .await?
    };
    if let Some(inputs) = inputs.as_ref() {
        bind_turn_inputs_tx(&mut tx, now, &request.run, &request.step, inputs).await?;
    }
    let batches = compose_turn_lane_batches_tx(
        &mut tx,
        now,
        session_id,
        AdmissionBoundary::ActiveTurnCheckpoint,
        Some(&request.turn_id),
        &request.policy,
    )
    .await?;
    bind_batches_tx(
        &mut tx,
        now,
        session_id,
        &request.run,
        &request.step,
        &batches,
    )
    .await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(CheckpointAdmission {
        inputs,
        queued: (!batches.is_empty()).then(|| lash_core_execution::runtime::AdmittedQueuedWork {
            session_id: session_id.clone(),
            batches,
        }),
    })
}

/// The leading open session-command run the shift applies next (ADR 0101
/// §4, FIG-3927 §2.7), with each row's ingress obligation acknowledged
/// delivered in the same fenced write. The command lane takes no admission:
/// the commit that applies the run settles it, predicated on each row still
/// being open.
pub(crate) async fn open_session_command_run_postgres(
    store: &crate::PostgresStore,
    fence: &lash_core_execution::store::ShiftFence,
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    let session_id = fence.session();
    let mut connection = acquire_runtime_connection(&store.pool, &store.observer).await?;
    let mut tx = begin_guarded(&mut *connection, &store.fence).await?;
    #[cfg(any(test, feature = "testing"))]
    store
        .set_transaction_lease_clock_for_testing(&mut tx)
        .await?;
    require_shift_fence_tx(&mut tx, fence).await?;
    if follow_on_blocks_admission_tx(&mut tx, session_id, FollowOnAdmission::Idle).await? {
        tx.rollback().await.map_err(store_sqlx_error)?;
        return Ok(Vec::new());
    }
    let now = postgres_transaction_epoch_ms(&mut tx).await?;
    let (mut batches, candidates) = scan_queued_work_candidates_tx(
        &mut tx,
        session_id,
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches_postgres
            .admission_candidates_idle
            .sql(),
        SESSION_COMMAND_BATCHES_PER_RUN,
    )
    .await?;
    batches.truncate(select_leading_session_command(&candidates));
    let sql = crate::turn_ingress::turn_ingress_sql();
    for batch in &batches {
        let delivered = sqlx::query(sql.queued_batches.deliver_open_command.sql())
            .bind(session_id.as_str())
            .bind(batch.batch_id.as_str())
            .bind(i64::try_from(now).unwrap_or(i64::MAX))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        lash_core_execution::store_backend_support::require_fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
            crate::POSTGRES_BACKEND,
            batch.batch_id.as_str(),
            delivered,
            || StoreError::Contended,
        )?;
    }
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(batches)
}

/// Whether `request`'s checkpoint has anything to admit or read back,
/// answered by one read-only probe once its fence is known current.
async fn checkpoint_work_pending_postgres(
    pool: &PgPool,
    request: &CheckpointAdmissionRequest,
    observer: &crate::StoreObserver,
) -> Result<bool, StoreError> {
    let mut connection = acquire_runtime_connection(pool, observer).await?;
    super::shift_epoch::require_fence_conn(&mut connection, request.session_id(), &request.fence)
        .await?;
    // One statement per checkpoint, chosen exhaustively: the admitted
    // minimum-boundary set is what the checkpoint decides, and an optional
    // predicate over a bound boundary cannot seek an index.
    let family = &crate::turn_ingress::turn_ingress_sql().family_postgres;
    let sql = match request.checkpoint {
        lash_core_execution::CheckpointKind::AfterWork => {
            family.checkpoint_work_pending_after_work.sql()
        }
        lash_core_execution::CheckpointKind::BeforeCompletion => {
            family.checkpoint_work_pending_before_completion.sql()
        }
    };
    sqlx::query_scalar(sql)
        .bind(request.session_id().as_str())
        .bind(request.turn_id.as_str())
        .bind(i64::try_from(request.max_inputs).unwrap_or(i64::MAX))
        .bind(i64::try_from(request.policy.max_rows).unwrap_or(i64::MAX))
        .bind(request.run.as_str())
        .bind(request.step.as_str())
        .fetch_one(&mut *connection)
        .await
        .map_err(store_sqlx_error)
}

/// What `run` already bound under `step`, both families in `enqueue_seq`
/// order: a re-executed admission step answers exactly this.
async fn read_step_admission_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    run: &TurnId,
    step: &str,
    mode: lash_core_execution::TurnInputAdmissionMode,
) -> Result<CheckpointAdmission, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let input_rows = sqlx::query(sql.pending_inputs.select_admitted_by_step.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .bind(step)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let inputs = input_rows
        .into_iter()
        .map(|row| pending_turn_input_from_row(pending_turn_input_row(row)?))
        .collect::<Result<Vec<_>, _>>()?;
    let batch_rows = sqlx::query(sql.queued_batches.select_admitted_by_step.sql())
        .bind(session_id.as_str())
        .bind(run.as_str())
        .bind(step)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut batches = Vec::with_capacity(batch_rows.len());
    for row in batch_rows {
        batches.push(queued_work_batch_from_row(queued_batch_row(row)?)?);
    }
    Ok(CheckpointAdmission {
        inputs: (!inputs.is_empty()).then(|| lash_core_execution::AdmittedTurnInputs {
            session_id: session_id.clone(),
            mode,
            inputs,
            applications: Vec::new(),
        }),
        queued: (!batches.is_empty()).then(|| lash_core_execution::runtime::AdmittedQueuedWork {
            session_id: session_id.clone(),
            batches,
        }),
    })
}

/// The open next-turn inputs a run's admission takes, up to `max_inputs`,
/// composed by the shared rule under the host's drain `policy`.
async fn compose_next_turn_inputs_tx(
    tx: &mut PgTx<'_>,
    now: u64,
    session_id: &SessionId,
    max_inputs: usize,
    policy: &TurnLaneAdmissionPolicy,
) -> Result<Option<lash_core_execution::AdmittedTurnInputs>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let rows = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .admission_candidates_next_turn
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(i64::try_from(max_inputs).unwrap_or(i64::MAX))
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let inputs = rows
        .into_iter()
        .map(|row| pending_turn_input_from_row(pending_turn_input_row(row)?))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(lash_core_execution::store::plan_next_turn_input_admission(
        session_id, inputs, max_inputs, policy, now,
    ))
}

/// The open input addressed to active turn `turn_id` that `checkpoint`
/// admits, up to `max_inputs`.
async fn compose_active_turn_inputs_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    turn_id: &TurnId,
    checkpoint: lash_core_execution::CheckpointKind,
    max_inputs: usize,
) -> Result<Option<lash_core_execution::AdmittedTurnInputs>, StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_postgres;
    let statement = match checkpoint {
        lash_core_execution::CheckpointKind::AfterWork => {
            statements.admission_candidates_active_turn_after_work.sql()
        }
        lash_core_execution::CheckpointKind::BeforeCompletion => statements
            .admission_candidates_active_turn_before_completion
            .sql(),
    };
    let rows = sqlx::query(statement)
        .bind(session_id.as_str())
        .bind(i64::try_from(max_inputs).unwrap_or(i64::MAX))
        .bind(turn_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let inputs = rows
        .into_iter()
        .map(|row| pending_turn_input_from_row(pending_turn_input_row(row)?))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(lash_core_execution::store::plan_checkpoint_input_admission(
        session_id, turn_id, checkpoint, inputs,
    ))
}

/// Bind every input of `admitted` to `run` under `step`, delivering each
/// row's ingress obligation in the same write.
async fn bind_turn_inputs_tx(
    tx: &mut PgTx<'_>,
    now: u64,
    run: &TurnId,
    step: &str,
    admitted: &lash_core_execution::AdmittedTurnInputs,
) -> Result<(), StoreError> {
    let state = lash_core_execution::store::turn_input_state_after_admission(&admitted.mode)
        .map(|state| state.as_str());
    let statement = crate::turn_ingress::turn_ingress_sql()
        .pending_inputs
        .admit
        .sql();
    for input in &admitted.inputs {
        let bound = sqlx::query(statement)
            .bind(admitted.session_id.as_str())
            .bind(input.input_id.as_str())
            .bind(state)
            .bind(run.as_str())
            .bind(step)
            .bind(i64::try_from(now).unwrap_or(i64::MAX))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        // Backstop: the composition locked this row open in the same
        // transaction, so the open predicate cannot legitimately miss.
        lash_core_execution::store_backend_support::require_fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
            crate::POSTGRES_BACKEND,
            input.input_id.as_str(),
            bound,
            || StoreError::Contended,
        )?;
    }
    Ok(())
}

/// Bind every batch of `batches` to `run` under `step`, delivering each
/// row's ingress obligation in the same write.
async fn bind_batches_tx(
    tx: &mut PgTx<'_>,
    now: u64,
    session_id: &SessionId,
    run: &TurnId,
    step: &str,
    batches: &[QueuedWorkBatch],
) -> Result<(), StoreError> {
    let statement = crate::turn_ingress::turn_ingress_sql()
        .queued_batches
        .admit
        .sql();
    for batch in batches {
        let bound = sqlx::query(statement)
            .bind(session_id.as_str())
            .bind(batch.batch_id.as_str())
            .bind(run.as_str())
            .bind(step)
            .bind(i64::try_from(now).unwrap_or(i64::MAX))
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
        lash_core_execution::store_backend_support::require_fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
            crate::POSTGRES_BACKEND,
            batch.batch_id.as_str(),
            bound,
            || StoreError::Contended,
        )?;
    }
    Ok(())
}

/// The open queued turn work one composition at `boundary` takes, by the
/// shared prefix rule: stopped before the earliest open next-turn input
/// while `running_turn` runs (`None` at idle) (ADR 0101 §5) and bounded by
/// `policy`.
async fn compose_turn_lane_batches_tx(
    tx: &mut PgTx<'_>,
    now: u64,
    session_id: &SessionId,
    boundary: AdmissionBoundary,
    running_turn: Option<&TurnId>,
    policy: &TurnLaneAdmissionPolicy,
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    if policy.max_rows == 0 {
        return Ok(Vec::new());
    }
    // The boundary is a closed two-variant choice, so it selects a named
    // statement rather than splicing a predicate: an optional boundary filter
    // cannot seek the `(session_id, enqueue_seq)` primary key cleanly. An
    // idle run's execution is the turn lane's, which a command enqueued since the
    // shift chose it never holds back (ADR 0101 §4).
    let sql = &crate::turn_ingress::turn_ingress_sql().queued_batches_postgres;
    let statement = match boundary {
        AdmissionBoundary::Idle => sql.admission_candidates_turn_lane.sql(),
        AdmissionBoundary::ActiveTurnCheckpoint => sql.admission_candidates_boundary.sql(),
    };
    let (mut batches, candidates) =
        scan_queued_work_candidates_tx(tx, session_id, statement, policy.max_rows).await?;
    // Read after the scan: an input committed before a scanned row took its
    // sequence first, so this read sees it.
    let admitted = TurnLaneStop::before(
        earliest_next_turn_candidate_seq_tx(tx, session_id, running_turn).await?,
    )
    .queued_prefix(&candidates);
    let selected = match select_turn_work_prefix(&candidates[..admitted], boundary, policy, now)? {
        TurnWorkPrefix::Selected { len } => len,
        TurnWorkPrefix::Refused { .. } => 0,
    };
    batches.truncate(selected);
    Ok(batches)
}

/// One locked candidate scan of the open queued work by `statement`, one of
/// the named admission-candidate scans: the batches it hydrates to and the
/// candidates the shared prefix rule decides over.
async fn scan_queued_work_candidates_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    statement: &str,
    max_rows: usize,
) -> Result<(Vec<QueuedWorkBatch>, Vec<TurnLaneCandidate>), StoreError> {
    let rows = sqlx::query(statement)
        .bind(session_id.as_str())
        .bind(admission_scan_limit(max_rows))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut batches = Vec::with_capacity(rows.len());
    for row in rows {
        batches.push(queued_work_batch_from_row(queued_batch_row(row)?)?);
    }
    let candidates = batches.iter().map(turn_lane_candidate).collect();
    Ok((batches, candidates))
}

/// The `enqueue_seq` of session `session_id`'s earliest open next-turn
/// input while `running_turn` runs (`None` at idle): the turn-lane head of
/// the input table.
async fn earliest_next_turn_candidate_seq_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    running_turn: Option<&TurnId>,
) -> Result<Option<u64>, StoreError> {
    let seq: Option<i64> = sqlx::query_scalar(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .earliest_next_turn_candidate_seq
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(running_turn.map(TurnId::as_str))
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    seq.map(|seq| u64_from_sql("turn_lane", "enqueue_seq", seq))
        .transpose()
}
