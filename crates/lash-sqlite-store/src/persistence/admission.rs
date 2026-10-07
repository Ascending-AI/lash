//! Checkpoint admission (FIG-3927): the writes that bind open rows of both
//! admission tables to a running turn's run, and the bindless read of the
//! command lane. A run's own admission is the session actor's mail drain.
//!
//! Every admission runs in one `BEGIN IMMEDIATE` write transaction: it reads
//! back what an earlier execution of the same step already bound, and
//! otherwise composes from open rows and binds them, each write predicated on
//! the row still being open.

use super::*;
use lash_core_execution::store::queued_work::TurnWorkPrefix;
use lash_core_execution::store::{
    CheckpointAdmission, CheckpointAdmissionRequest, FollowOnAdmission, TurnLaneStop,
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

/// Admit the checkpoint work of `request`'s run, keyed by its step
/// ([`RunStore::admit_at_checkpoint`]).
///
/// A read-only probe answers the common empty checkpoint without a write
/// transaction. It also reports rows the step already bound, so a
/// re-executed step always reaches the read-back.
///
/// [`RunStore::admit_at_checkpoint`]: lash_core_execution::store::RunStore::admit_at_checkpoint
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
                let session_id = &request.session_id;
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
                    bind_turn_inputs_conn(tx, &request.run, &request.step, inputs)?;
                }
                let batches = compose_turn_lane_batches_conn(
                    tx,
                    now,
                    session_id,
                    AdmissionBoundary::ActiveTurnCheckpoint,
                    Some(&request.turn_id),
                    &request.policy,
                )?;
                bind_batches_conn(tx, session_id, &request.run, &request.step, &batches)?;
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

/// The leading open session-command run the session applies next (ADR 0101
/// §4, FIG-3927 §2.7). The command lane takes no admission: the commit that
/// applies the run settles it, predicated on each row still being open.
pub(crate) async fn open_session_command_run_sqlite(
    store: &crate::SqliteStore,
    session_id: &SessionId,
) -> Result<Vec<QueuedWorkBatch>, StoreError> {
    let session_id = session_id.clone();
    store
        .conn
        .read(move |tx| {
            Ok((|| {
                let session_id = &session_id;
                if follow_on_blocks_admission_conn(tx, session_id, FollowOnAdmission::Idle)? {
                    return Ok(Vec::new());
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
                Ok(batches)
            })())
        })
        .await
        .map_err(sqlite_error)?
}

/// Whether `request`'s checkpoint has anything to admit or read back,
/// answered by one read-only probe.
async fn checkpoint_work_pending_sqlite(
    conn: &SqliteConnection,
    request: &CheckpointAdmissionRequest,
) -> Result<bool, StoreError> {
    let session_id = request.session_id.clone();
    let turn_id = request.turn_id.clone();
    let run = request.run.clone();
    let step = request.step.clone();
    let checkpoint = request.checkpoint;
    let max_inputs = request.max_inputs;
    let max_batches = request.policy.max_rows;
    conn.call(move |conn| {
        let outcome: Result<bool, StoreError> = (|| {
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

/// Bind every input of `admitted` to `run` under `step`.
fn bind_turn_inputs_conn(
    tx: &Connection,
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

/// Bind every batch of `batches` to `run` under `step`.
fn bind_batches_conn(
    tx: &Connection,
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
    // turn lane's, which a command enqueued since the session chose it never
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
