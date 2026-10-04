//! Admission (FIG-3927): the fenced writes that bind open rows of both
//! admission tables to a run, and the bindless read of the command lane.
//!
//! Every admission runs in one `BEGIN IMMEDIATE` write transaction: it checks
//! the shift fence, reads back what an earlier execution of the same step
//! already bound, and otherwise composes from open rows and binds them, each
//! write predicated on the row still being open.

use super::*;
use lash_core_execution::store::queued_work::TurnWorkPrefix;
use lash_core_execution::store::{
    AdmittedHead, CheckpointAdmission, CheckpointAdmissionRequest, FollowOnAdmission,
    RUN_ADMISSION_STEP, RunAdmission, TurnLaneStop,
};

/// Lower a transaction body's outcome into the write flow's commit decision:
/// an error rolls back and carries the typed error to the caller.
fn flow<T>(
    outcome: Result<TxOutcome<T>, StoreError>,
) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match outcome {
        Ok(TxOutcome::Commit(value)) => TxOutcome::Commit(Ok(value)),
        Ok(TxOutcome::Rollback(value)) => TxOutcome::Rollback(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}

/// Whether the head's pending follow-on refuses `admission` (ADR 0101 §3):
/// every admission but the follow-on's own is blocked while it is set.
pub(super) fn follow_on_blocks_admission_conn(
    conn: &Connection,
    session_id: &SessionId,
    admission: FollowOnAdmission<'_>,
) -> Result<bool, StoreError> {
    Ok(lash_core_execution::store::follow_on_blocks_admission(
        super::turn_cancel::pending_follow_on_conn(conn, session_id)?.as_ref(),
        admission,
    )
    .is_some())
}

/// Admit the run's turn-lane run and record it with its rows, bindings and
/// base, in one transaction ([`RunSqliteStore::admit_run`]).
///
/// A recorded admission is returned unchanged, whatever fence or incarnation
/// asks: a re-execution of the step reads back what it chose and never
/// widens (FIG-3840). An executor the recorded one excludes is refused
/// instead (FIG-4765).
///
/// [`RunSqliteStore::admit_run`]: lash_core_execution::store::RunSqliteStore::admit_run
pub(crate) async fn admit_run_sqlite(
    store: &crate::SqliteStore,
    request: &lash_core_execution::store::AdmitRunRequest,
    prepared: Option<&lash_core_execution::store::PreparedRunAdmission>,
    anchor: &lash_core_execution::TraceAnchor,
) -> Result<Option<RunAdmission>, StoreError> {
    let request = request.clone();
    let prepared = prepared.cloned();
    let anchor = anchor.clone();
    let now = store.clock.timestamp_ms();
    let preparing = prepared.is_none();
    let fleet = store.conn.fleet();
    let compose = move |tx: &rusqlite::Connection, fleet| {
        admit_run_conn(tx, fleet, &request, prepared.as_ref(), &anchor, now)
    };
    if preparing {
        store
            .conn
            .read(move |tx| Ok(compose(tx, fleet)))
            .await
            .map_err(sqlite_error)?
            .map(|outcome| match outcome {
                TxOutcome::Commit(value) | TxOutcome::Rollback(value) => value,
            })
    } else {
        store
            .conn
            .write_flow(move |tx| flow(compose(tx, tx.fleet())))
            .await
            .map_err(sqlite_error)?
    }
}

pub(crate) fn admit_run_conn(
    tx: &Connection,
    fleet: lash_core_execution::FleetFormat,
    request: &lash_core_execution::store::AdmitRunRequest,
    prepared: Option<&lash_core_execution::store::PreparedRunAdmission>,
    anchor: &lash_core_execution::TraceAnchor,
    now: u64,
) -> Result<TxOutcome<Option<RunAdmission>>, StoreError> {
    let session_id = request.session_id();
    if let Some(epoch) = request.unsealed_epoch {
        let stored = super::shift_epoch::shift_epoch_conn(tx, session_id)?;
        if stored.epoch != epoch || stored.closing.is_some() || stored.control_pending {
            return Err(StoreError::PreparedRunAdmissionStale {
                session_id: session_id.clone(),
                run: request.run.clone(),
            });
        }
    } else {
        super::shift_epoch::require_fence_conn(tx, session_id, &request.fence)?;
    }
    if let Some(binding) = &request.turn_cancellation {
        super::turn_input::check_turn_cancellation_conn(
            tx,
            session_id,
            &binding.binding_id,
            &binding.admitted_scope,
        )?;
    }
    let runs = crate::session_runs::session_runs_sql();
    let existing: Option<Option<String>> = tx
        .query_row(
            runs.runs.select_admission.sql(),
            params![session_id.as_str(), request.run.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    if let Some(Some(json)) = existing {
        let admission = crate::session_runs::decode_run_admission(&json)?;
        // The recorded executor decides who executes the run
        // (FIG-4765).
        if admission.executor.excludes(&request.executor) {
            return Err(StoreError::RunHeldByAnotherExecutor {
                session_id: session_id.clone(),
                run: request.run.clone(),
                recorded: Box::new(admission.executor),
                admitting: Box::new(request.executor.clone()),
            });
        }
        if prepared.is_some()
            && let Some(binding) = &request.turn_cancellation
        {
            super::turn_input::bind_turn_cancellation_conn(
                tx,
                session_id,
                &binding.binding_id,
                &binding.admitted_scope,
            )?;
        }
        return Ok(TxOutcome::Commit(Some(admission)));
    }
    if follow_on_blocks_admission_conn(tx, session_id, FollowOnAdmission::Idle)? {
        if prepared.is_some() {
            return Err(StoreError::PreparedRunAdmissionStale {
                session_id: session_id.clone(),
                run: request.run.clone(),
            });
        }
        return Ok(TxOutcome::Commit(None));
    }
    if let Some(unfinished) = crate::session_runs::unfinished_run_conn(tx, session_id)? {
        return Err(StoreError::UnfinishedRunConflict {
            session_id: session_id.clone(),
            run: unfinished.run,
        });
    }
    let (inputs, queued) = match &request.head {
        AdmittedHead::Input(head) => {
            let Some(inputs) = compose_next_turn_inputs_conn(
                tx,
                now,
                session_id,
                request.max_inputs,
                &request.policy,
            )?
            else {
                if prepared.is_some() {
                    return Err(StoreError::PreparedRunAdmissionStale {
                        session_id: session_id.clone(),
                        run: request.run.clone(),
                    });
                }
                return Ok(TxOutcome::Commit(None));
            };
            if !inputs.inputs.iter().any(|input| input.input_id == *head) {
                if prepared.is_some() {
                    return Err(StoreError::PreparedRunAdmissionStale {
                        session_id: session_id.clone(),
                        run: request.run.clone(),
                    });
                }
                return Ok(TxOutcome::Commit(None));
            }
            (Some(Box::new(inputs)), None)
        }
        AdmittedHead::Batch(head) => {
            let batches = compose_turn_lane_batches_conn(
                tx,
                now,
                session_id,
                AdmissionBoundary::Idle,
                None,
                &request.policy,
            )?;
            if !batches.iter().any(|batch| batch.batch_id == *head) {
                if prepared.is_some() {
                    return Err(StoreError::PreparedRunAdmissionStale {
                        session_id: session_id.clone(),
                        run: request.run.clone(),
                    });
                }
                return Ok(TxOutcome::Commit(None));
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
    base.generation = read_session_state_version_conn(tx, session_id, fleet)?;
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
        executor: request.executor.clone(),
        plugins: request.plugins.clone(),
        trace: Some(trace),
        cancel_intent: Some(super::turn_cancel::load_turn_cancel_intent_snapshot_conn(
            tx,
            session_id,
            &request.run,
        )?),
        recorded_by_this_call: prepared.is_some(),
    };
    let Some(prepared) = prepared else {
        return Ok(TxOutcome::Commit(Some(admission)));
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
    // Revalidate the exact trace proposal before binding either the
    // cancellation authority or admitted rows.
    if let Some(binding) = &request.turn_cancellation {
        super::turn_input::bind_turn_cancellation_conn(
            tx,
            session_id,
            &binding.binding_id,
            &binding.admitted_scope,
        )?;
    }
    if let Some(inputs) = admission.inputs.as_deref() {
        bind_turn_inputs_conn(tx, now, &request.run, RUN_ADMISSION_STEP, inputs)?;
    }
    if let Some(queued) = admission.queued.as_deref() {
        bind_batches_conn(
            tx,
            now,
            session_id,
            &request.run,
            RUN_ADMISSION_STEP,
            &queued.batches,
        )?;
    }
    crate::session_meta::retain_admission_base_conn(
        tx,
        session_id,
        admission.base.checkpoint.as_ref(),
    )?;
    crate::session_runs::bind_run_inputs_conn(
        tx,
        session_id,
        &request.run,
        &admission.input_ids(),
    )?;
    let json = encode_json(&admission)?;
    let changed = crate::conn::cached_execute(
        tx,
        runs.runs.write_admission.sql(),
        params![
            session_id.as_str(),
            request.run.as_str(),
            json,
            request.admitted_generation.as_str()
        ],
    )
    .map_err(sqlite_error)?;
    if changed != 1 {
        return Err(StoreError::Backend(
            "run admission was already recorded".into(),
        ));
    }
    Ok(TxOutcome::Commit(Some(admission)))
}

/// Admit the checkpoint work of `request`'s run, keyed by its step
/// ([`RunSqliteStore::admit_at_checkpoint`]).
///
/// A read-only probe answers the common empty checkpoint without a write
/// transaction. It refuses a stale fence first, whatever the caps and
/// whatever is pending (FIG-3927 N4), and it also reports rows the step
/// already bound, so a re-executed step always reaches the read-back.
///
/// [`RunSqliteStore::admit_at_checkpoint`]: lash_core_execution::store::RunSqliteStore::admit_at_checkpoint
pub(crate) async fn admit_at_checkpoint_sqlite(
    store: &crate::SqliteStore,
    request: &CheckpointAdmissionRequest,
) -> Result<CheckpointAdmission, StoreError> {
    #[cfg(test)]
    store
        .checkpoint_probe_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !checkpoint_work_pending_sqlite(&store.conn, request).await? {
        return Ok(CheckpointAdmission::default());
    }
    #[cfg(test)]
    store
        .checkpoint_write_transaction_count
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let request = request.clone();
    let now = store.clock.timestamp_ms();
    store
        .conn
        .write_flow(move |tx| {
            flow((|| {
                let session_id = request.session_id();
                super::shift_epoch::require_fence_conn(tx, session_id, &request.fence)?;
                let mode = lash_core_execution::TurnInputAdmissionMode::ActiveTurn {
                    turn_id: request.turn_id.clone(),
                    checkpoint: request.checkpoint,
                };
                let recorded = read_step_admission_conn(
                    tx,
                    session_id,
                    &request.run,
                    &request.step,
                    mode.clone(),
                )?;
                if !recorded.is_empty() {
                    return Ok(TxOutcome::Commit(recorded));
                }
                if follow_on_blocks_admission_conn(
                    tx,
                    session_id,
                    FollowOnAdmission::Checkpoint {
                        turn_id: &request.turn_id,
                    },
                )? {
                    return Ok(TxOutcome::Commit(CheckpointAdmission::default()));
                }
                let inputs = if request.max_inputs == 0 {
                    None
                } else {
                    compose_active_turn_inputs_conn(
                        tx,
                        session_id,
                        &request.turn_id,
                        request.checkpoint,
                        request.max_inputs,
                    )?
                };
                if let Some(inputs) = inputs.as_ref() {
                    bind_turn_inputs_conn(tx, now, &request.run, &request.step, inputs)?;
                }
                let batches = compose_turn_lane_batches_conn(
                    tx,
                    now,
                    session_id,
                    AdmissionBoundary::ActiveTurnCheckpoint,
                    Some(&request.turn_id),
                    &request.policy,
                )?;
                bind_batches_conn(tx, now, session_id, &request.run, &request.step, &batches)?;
                let queued = (!batches.is_empty()).then(|| {
                    lash_core_execution::runtime::AdmittedQueuedWork {
                        session_id: session_id.clone(),
                        batches,
                    }
                });
                Ok(TxOutcome::Commit(CheckpointAdmission { inputs, queued }))
            })())
        })
        .await
        .map_err(sqlite_error)?
}

/// The leading open session-command run the shift applies next (ADR 0101
/// §4, FIG-3927 §2.7), with each row's ingress obligation acknowledged
/// delivered in the same fenced write. The command lane takes no admission:
/// the commit that applies the run settles it, predicated on each row still
/// being open.
pub(crate) async fn open_session_command_run_sqlite(
    store: &crate::SqliteStore,
    fence: &lash_core_execution::store::ShiftFence,
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    let fence = fence.clone();
    let now = store.clock.timestamp_ms();
    store
        .conn
        .write_flow(move |tx| {
            flow((|| {
                let session_id = fence.session();
                super::shift_epoch::require_fence_conn(tx, session_id, &fence)?;
                if follow_on_blocks_admission_conn(tx, session_id, FollowOnAdmission::Idle)? {
                    return Ok(TxOutcome::Commit(Vec::new()));
                }
                let (_, mut batches, candidates) = scan_queued_work_candidates_sqlite(
                    tx,
                    session_id,
                    crate::turn_ingress::turn_ingress_sql()
                        .queued_batches_sqlite
                        .admission_candidates_idle
                        .sql(),
                    SESSION_COMMAND_BATCHES_PER_RUN,
                )?;
                let run = select_leading_session_command(&candidates);
                batches.truncate(run);
                let sql = crate::turn_ingress::turn_ingress_sql();
                for batch in &batches {
                    let delivered = crate::conn::cached_execute(
                        tx,
                        sql.queued_batches.deliver_open_command.sql(),
                        params![
                            session_id.as_str(),
                            batch.batch_id.as_str(),
                            i64::try_from(now).unwrap_or(i64::MAX)
                        ],
                    )
                    .map_err(sqlite_error)?;
                    lash_core_execution::store_backend_support::require_fenced_write_applied(
                        lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
                        crate::SQLITE_BACKEND,
                        batch.batch_id.as_str(),
                        u64::try_from(delivered).unwrap_or(u64::MAX),
                        || StoreError::Contended,
                    )?;
                }
                Ok(TxOutcome::Commit(batches))
            })())
        })
        .await
        .map_err(sqlite_error)?
}

/// Whether `request`'s checkpoint has anything to admit or read back,
/// answered by one read-only probe once its fence is known current.
async fn checkpoint_work_pending_sqlite(
    conn: &SqliteConnection,
    request: &CheckpointAdmissionRequest,
) -> Result<bool, StoreError> {
    let fence = request.fence.clone();
    let session_id = request.session_id().clone();
    let turn_id = request.turn_id.clone();
    let run = request.run.clone();
    let step = request.step.clone();
    let checkpoint = request.checkpoint;
    let max_inputs = request.max_inputs;
    let max_batches = request.policy.max_rows;
    conn.call(move |conn| {
        let outcome: Result<bool, StoreError> = (|| {
            super::shift_epoch::require_fence_conn(conn, &session_id, &fence)?;
            let family = &crate::turn_ingress::turn_ingress_sql().family_sqlite;
            // One statement per checkpoint, chosen exhaustively: the admitted
            // minimum-boundary set is what the checkpoint decides, and an
            // optional predicate over a bound boundary cannot seek an index.
            let sql = match checkpoint {
                lash_core_execution::CheckpointKind::AfterWork => {
                    family.checkpoint_work_pending_after_work.sql()
                }
                lash_core_execution::CheckpointKind::BeforeCompletion => {
                    family.checkpoint_work_pending_before_completion.sql()
                }
            };
            let pending: i64 = conn
                .query_row(
                    sql,
                    params![
                        session_id.as_str(),
                        turn_id.as_str(),
                        i64::try_from(max_inputs).unwrap_or(i64::MAX),
                        i64::try_from(max_batches).unwrap_or(i64::MAX),
                        run.as_str(),
                        step.as_str(),
                    ],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            Ok(pending != 0)
        })();
        Ok(outcome)
    })
    .await
    .map_err(sqlite_error)?
}

/// What `run` already bound under `step`, both families in `enqueue_seq`
/// order: a re-executed admission step answers exactly this.
fn read_step_admission_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    step: &str,
    mode: lash_core_execution::TurnInputAdmissionMode,
) -> Result<CheckpointAdmission, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let input_rows = {
        let mut stmt = tx
            .prepare_cached(sql.pending_inputs.select_admitted_by_step.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session_id.as_str(), run.as_str(), step],
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let batch_rows = {
        let mut stmt = tx
            .prepare_cached(sql.queued_batches.select_admitted_by_step.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session_id.as_str(), run.as_str(), step],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let inputs = input_rows
        .into_iter()
        .map(pending_turn_input_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let batches = queued_work_batches_from_rows(&batch_rows)?;
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
fn compose_next_turn_inputs_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    max_inputs: usize,
    policy: &lash_core_execution::TurnLaneAdmissionPolicy,
) -> Result<Option<lash_core_execution::AdmittedTurnInputs>, StoreError> {
    if max_inputs == 0 {
        return Ok(None);
    }
    let rows = {
        let mut stmt = tx
            .prepare_cached(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs_sqlite
                    .admission_candidates_next_turn
                    .sql(),
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    i64::try_from(max_inputs).unwrap_or(i64::MAX)
                ],
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let inputs = rows
        .into_iter()
        .map(pending_turn_input_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(lash_core_execution::store::plan_next_turn_input_admission(
        session_id, inputs, max_inputs, policy, now,
    ))
}

/// The open input addressed to active turn `turn_id` that `checkpoint`
/// admits, up to `max_inputs`.
fn compose_active_turn_inputs_conn(
    tx: &Connection,
    session_id: &SessionId,
    turn_id: &TurnId,
    checkpoint: lash_core_execution::CheckpointKind,
    max_inputs: usize,
) -> Result<Option<lash_core_execution::AdmittedTurnInputs>, StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_sqlite;
    let statement = match checkpoint {
        lash_core_execution::CheckpointKind::AfterWork => {
            statements.admission_candidates_active_turn_after_work.sql()
        }
        lash_core_execution::CheckpointKind::BeforeCompletion => statements
            .admission_candidates_active_turn_before_completion
            .sql(),
    };
    let rows = {
        let mut stmt = tx.prepare_cached(statement).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![
                    session_id.as_str(),
                    i64::try_from(max_inputs).unwrap_or(i64::MAX),
                    turn_id.as_str()
                ],
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let inputs = rows
        .into_iter()
        .map(pending_turn_input_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(lash_core_execution::store::plan_checkpoint_input_admission(
        session_id, turn_id, checkpoint, inputs,
    ))
}

/// Bind every input of `admitted` to `run` under `step`, delivering each
/// row's ingress obligation in the same write.
fn bind_turn_inputs_conn(
    tx: &Connection,
    now: u64,
    run: &TurnId,
    step: &str,
    admitted: &lash_core_execution::AdmittedTurnInputs,
) -> Result<(), StoreError> {
    let state = lash_core_execution::store::turn_input_state_after_admission(&admitted.mode)
        .map(|state| state.as_str());
    let statement = &crate::turn_ingress::turn_ingress_sql().pending_inputs.admit;
    for input in &admitted.inputs {
        let bound = crate::conn::cached_execute(
            tx,
            statement.sql(),
            params![
                admitted.session_id.as_str(),
                input.input_id.as_str(),
                state,
                run.as_str(),
                step,
                i64::try_from(now).unwrap_or(i64::MAX),
            ],
        )
        .map_err(sqlite_error)?;
        // Backstop: the composition read this row open in the same write
        // transaction, so the open predicate cannot legitimately miss.
        lash_core_execution::store_backend_support::require_fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
            crate::SQLITE_BACKEND,
            input.input_id.as_str(),
            u64::try_from(bound).unwrap_or(u64::MAX),
            || StoreError::Contended,
        )?;
    }
    Ok(())
}

/// Bind every batch of `batches` to `run` under `step`, delivering each
/// row's ingress obligation in the same write.
fn bind_batches_conn(
    tx: &Connection,
    now: u64,
    session_id: &SessionId,
    run: &TurnId,
    step: &str,
    batches: &[QueuedWorkBatch],
) -> Result<(), StoreError> {
    let statement = &crate::turn_ingress::turn_ingress_sql().queued_batches.admit;
    for batch in batches {
        let bound = crate::conn::cached_execute(
            tx,
            statement.sql(),
            params![
                session_id.as_str(),
                batch.batch_id.as_str(),
                run.as_str(),
                step,
                i64::try_from(now).unwrap_or(i64::MAX),
            ],
        )
        .map_err(sqlite_error)?;
        lash_core_execution::store_backend_support::require_fenced_write_applied(
            lash_core_execution::store_backend_support::FencedWrite::IngressAdmission,
            crate::SQLITE_BACKEND,
            batch.batch_id.as_str(),
            u64::try_from(bound).unwrap_or(u64::MAX),
            || StoreError::Contended,
        )?;
    }
    Ok(())
}

/// The open queued turn work one composition at `boundary` takes, by the
/// shared prefix rule: stopped before the earliest open next-turn input
/// while `running_turn` runs (`None` at idle) (ADR 0101 §5) and bounded by
/// `policy`.
fn compose_turn_lane_batches_conn(
    tx: &Connection,
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
    // cannot seek the `(session_id, enqueue_seq)` primary key cleanly, and
    // this query is the admission path's hottest. An idle run's execution is the
    // turn lane's, which a command enqueued since the shift chose it never
    // holds back (ADR 0101 §4).
    let sql = &crate::turn_ingress::turn_ingress_sql().queued_batches_sqlite;
    let statement = match boundary {
        AdmissionBoundary::Idle => sql.admission_candidates_turn_lane.sql(),
        AdmissionBoundary::ActiveTurnCheckpoint => sql.admission_candidates_boundary.sql(),
    };
    let (_, mut batches, candidates) =
        scan_queued_work_candidates_sqlite(tx, session_id, statement, policy.max_rows)?;
    let admitted = TurnLaneStop::before(earliest_next_turn_candidate_seq_conn(
        tx,
        session_id,
        running_turn,
    )?)
    .queued_prefix(&candidates);
    let selected = match select_turn_work_prefix(&candidates[..admitted], boundary, policy, now)? {
        TurnWorkPrefix::Selected { len } => len,
        TurnWorkPrefix::Refused { .. } => 0,
    };
    batches.truncate(selected);
    Ok(batches)
}

/// The rows as read, the batches they hydrate to, and the candidates the
/// shared prefix rule decides over.
type QueuedWorkCandidateScan = (
    Vec<QueuedBatchRow>,
    Vec<QueuedWorkBatch>,
    Vec<TurnLaneCandidate>,
);

/// One candidate scan of the open queued work by `statement`, one of the
/// named admission-candidate scans.
fn scan_queued_work_candidates_sqlite(
    tx: &Connection,
    session_id: &SessionId,
    statement: &str,
    max_rows: usize,
) -> Result<QueuedWorkCandidateScan, StoreError> {
    let rows = {
        let mut stmt = tx.prepare_cached(statement).map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session_id.as_str(), admission_scan_limit(max_rows)],
                queued_batch_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let batches = queued_work_batches_from_rows(&rows)?;
    let candidates = batches.iter().map(turn_lane_candidate).collect();
    Ok((rows, batches, candidates))
}

/// The `enqueue_seq` of session `session_id`'s earliest open next-turn
/// input while `running_turn` runs (`None` at idle): the turn-lane head of
/// the input table.
fn earliest_next_turn_candidate_seq_conn(
    tx: &Connection,
    session_id: &SessionId,
    running_turn: Option<&TurnId>,
) -> Result<Option<u64>, StoreError> {
    let seq: Option<i64> = tx
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .earliest_next_turn_candidate_seq
                .sql(),
            params![session_id.as_str(), running_turn.map(TurnId::as_str)],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    seq.map(|seq| u64_from_sql("turn_lane", "enqueue_seq", seq).map_err(sqlite_error))
        .transpose()
}
