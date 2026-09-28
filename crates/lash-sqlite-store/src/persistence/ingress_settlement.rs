//! A commit's ingress settlement (FIG-3927), inside its `BEGIN IMMEDIATE`
//! write transaction.
//!
//! Every row a commit names is settled under the root that admitted it: the
//! shared verdict decides over the row as read, and the write keeps the root
//! predicate as its backstop. A row the root does not hold refuses the whole
//! commit. The session-command run a commit applied settles bindlessly,
//! predicated on each row still being open.

use super::*;

/// Settle `commit`'s ingress and applied commands, and, for an interrupted
/// turn, re-defer or drop the open input addressed to it. Returns the cancel
/// outcome the commit's cancellation records.
pub(super) fn settle_commit_ingress_conn(
    tx: &Connection,
    commit: &RuntimeCommit,
) -> Result<lash_core_execution::TurnCancelInputOutcome, StoreError> {
    let session_id = &commit.session_id;
    if let Some(commands) = commit.applied_commands.as_ref() {
        for batch_id in &commands.batch_ids {
            crate::queued_work::settle_open_command_conn(tx, session_id, batch_id)?;
        }
    }
    let interrupted = commit.interrupted_turn_input_turn_id.as_ref();
    let cancellation = commit.interrupted_turn_input_cancellation.as_ref();
    if let Some(turn_id) = interrupted
        && let Some(evidence) = commit
            .turn_cancel_closure_settlement
            .as_ref()
            .and_then(lash_core_execution::TurnCancelClosureSettlement::base_cancellation)
    {
        let observed = commit
            .interrupted_turn_cancel_intent
            .as_ref()
            .ok_or_else(|| {
                StoreError::Backend(
                    "interrupted turn commit omitted cancellation intent predicate".to_string(),
                )
            })?;
        if !reconcile_turn_cancel_winner_conn(tx, session_id, turn_id, observed, evidence)? {
            return Err(StoreError::TurnCancelIntentChanged {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
            });
        }
    }
    let mut affected_inputs = Vec::new();
    let mut affected_wakes = Vec::new();
    if let Some(ingress) = commit.ingress.as_ref() {
        let root = &ingress.root;
        for completion in &ingress.completed_inputs {
            for input_id in &completion.input_ids {
                admitted_input_conn(tx, session_id, root, input_id)?;
                settle_admitted_input_conn(
                    tx,
                    session_id,
                    root,
                    input_id,
                    lash_core_execution::runtime::TurnInputStateKind::Completed,
                )?;
            }
        }
        for completion in &ingress.completed_batches {
            for batch_id in &completion.batch_ids {
                crate::queued_work::complete_admitted_batch_conn(tx, session_id, root, batch_id)?;
            }
        }
        for (rows, disposition) in [
            (
                &ingress.released,
                lash_core_execution::TurnCancelDisposition::Defer,
            ),
            (
                &ingress.dropped,
                lash_core_execution::TurnCancelDisposition::Drop,
            ),
        ] {
            for row in rows {
                match row {
                    lash_core_execution::store::IngressRowId::Input(input_id) => {
                        let held = admitted_input_conn(tx, session_id, root, input_id)?;
                        let payload = decode_stored_json(&held.input_json, "turn input")?;
                        match disposition {
                            lash_core_execution::TurnCancelDisposition::Defer => {
                                release_admitted_input_conn(tx, session_id, root, input_id)?;
                            }
                            lash_core_execution::TurnCancelDisposition::Drop => {
                                settle_admitted_input_conn(
                                    tx,
                                    session_id,
                                    root,
                                    input_id,
                                    lash_core_execution::runtime::TurnInputStateKind::Cancelled,
                                )?;
                            }
                        }
                        affected_inputs.push((
                            held.enqueue_seq,
                            lash_core_execution::TurnCancelAffectedInput {
                                input_id: input_id.clone(),
                                payload,
                                disposition,
                            },
                        ));
                    }
                    lash_core_execution::store::IngressRowId::Batch(batch_id) => {
                        match disposition {
                            // A released wake keeps its position and its
                            // redelivery floor; its record says it was
                            // deferred (FIG-3543).
                            lash_core_execution::TurnCancelDisposition::Defer => {
                                let batch = admitted_batch_conn(tx, session_id, root, batch_id)?;
                                release_admitted_batch_conn(tx, session_id, root, batch_id)?;
                                affected_wakes.extend(
                                    lash_core_execution::store_backend_support::deferred_wake_records(
                                        std::slice::from_ref(&batch),
                                    ),
                                );
                            }
                            lash_core_execution::TurnCancelDisposition::Drop => {
                                crate::queued_work::complete_admitted_batch_conn(
                                    tx, session_id, root, batch_id,
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    let mut outcome = lash_core_execution::TurnCancelInputOutcome::default();
    let Some(turn_id) = interrupted else {
        return Ok(outcome);
    };
    // The open input addressed to the interrupted turn that no checkpoint
    // admitted names a turn that is over: it is re-deferred, or dropped by
    // the cancellation's disposition, which governs host-authored input only.
    let disposition = cancellation.map_or(
        lash_core_execution::TurnCancelDisposition::Defer,
        |evidence| evidence.undelivered,
    );
    let sql = crate::turn_ingress::turn_ingress_sql();
    let open_rows = {
        let mut stmt = tx
            .prepare_cached(sql.pending_inputs_sqlite.select_pending_active.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session_id.as_str()],
                pending_turn_input_row_from_sql,
            )
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?
    };
    let deferred = lash_core_execution::TurnInputState::DeferredNextTurn;
    let deferred_ingress = encode_json(&deferred.ingress())?;
    for row in open_rows {
        let ingress = decode_turn_input_ingress(row.ingress_json.clone())?;
        if ingress.active_turn_id() != Some(turn_id) {
            continue;
        }
        let payload = decode_stored_json(&row.input_json, "turn input")?;
        // Two dispositions, two named statements: deferring rewrites the
        // ingress so the row stops naming a turn that is over, dropping is
        // the withdrawal this table already has.
        match disposition {
            lash_core_execution::TurnCancelDisposition::Defer => crate::conn::cached_execute(
                tx,
                sql.pending_inputs.defer_to_next_turn.sql(),
                params![
                    session_id.as_str(),
                    row.input_id.as_str(),
                    deferred.as_str(),
                    deferred_ingress.as_str(),
                ],
            ),
            lash_core_execution::TurnCancelDisposition::Drop => crate::conn::cached_execute(
                tx,
                sql.pending_inputs.cancel.sql(),
                params![
                    session_id.as_str(),
                    row.input_id.as_str(),
                    lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str(),
                ],
            ),
        }
        .map_err(sqlite_error)?;
        affected_inputs.push((
            row.enqueue_seq,
            lash_core_execution::TurnCancelAffectedInput {
                input_id: row.input_id.into(),
                payload,
                disposition,
            },
        ));
    }
    // The outcome reports every row the teardown moved; only a cancellation
    // has a request record to append them to.
    affected_inputs.sort_by_key(|(enqueue_seq, _)| *enqueue_seq);
    for (_, affected) in affected_inputs {
        if cancellation.is_some() {
            append_turn_cancel_outcome_conn(tx, session_id, turn_id, affected.clone())?;
        }
        outcome.affected_inputs.push(affected);
    }
    for affected in affected_wakes {
        if cancellation.is_some() {
            append_turn_cancel_wake_conn(tx, session_id, turn_id, affected.clone())?;
        }
        outcome.affected_wakes.push(affected);
    }
    Ok(outcome)
}

/// The row of input `input_id`, which root `root` must hold.
fn admitted_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<PendingTurnInputRow, StoreError> {
    let row = tx
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .pending_inputs
                .select_by_id
                .sql(),
            params![session_id.as_str(), input_id.as_str()],
            pending_turn_input_row_from_sql,
        )
        .optional()
        .map_err(sqlite_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_root(
        session_id,
        root,
        &lash_core_execution::store::IngressRowId::Input(input_id.clone()),
        row.as_ref().map(|row| row.admitted_root.as_deref()),
    )?;
    row.ok_or_else(|| StoreError::Backend(format!("admitted input `{input_id}` vanished")))
}

/// The hydrated batch `batch_id`, which root `root` must hold.
fn admitted_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<QueuedWorkBatch, StoreError> {
    let row = tx
        .query_row(
            crate::turn_ingress::turn_ingress_sql()
                .queued_batches
                .select_by_id
                .sql(),
            params![batch_id.as_str()],
            queued_batch_row_from_sql,
        )
        .optional()
        .map_err(sqlite_error)?
        .filter(|row| row.session_id == *session_id);
    lash_core_execution::store_backend_support::require_admitted_to_root(
        session_id,
        root,
        &lash_core_execution::store::IngressRowId::Batch(batch_id.clone()),
        row.as_ref().map(|row| row.admitted_root.as_deref()),
    )?;
    let row =
        row.ok_or_else(|| StoreError::Backend(format!("admitted batch `{batch_id}` vanished")))?;
    queued_work_batch_from_conn(tx, row)
}

/// Settle input `input_id`, held by `root`, into the terminal `state`.
fn settle_admitted_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    input_id: &lash_core_execution::InputId,
    state: lash_core_execution::runtime::TurnInputStateKind,
) -> Result<(), StoreError> {
    let settled = crate::conn::cached_execute(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .settle_admitted
            .sql(),
        params![
            session_id.as_str(),
            input_id.as_str(),
            state.as_str(),
            root.as_str()
        ],
    )
    .map_err(sqlite_error)?;
    require_settlement_applied(session_id, root, input_row(input_id), settled)
}

/// Hand input `input_id`, held by `root`, back open at its own position.
fn release_admitted_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<(), StoreError> {
    let deferred = lash_core_execution::TurnInputState::DeferredNextTurn;
    let released = crate::conn::cached_execute(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .release_admitted
            .sql(),
        params![
            session_id.as_str(),
            input_id.as_str(),
            root.as_str(),
            deferred.as_str(),
            encode_json(&deferred.ingress())?,
        ],
    )
    .map_err(sqlite_error)?;
    require_settlement_applied(session_id, root, input_row(input_id), released)
}

/// Hand batch `batch_id`, held by `root`, back open at its own position.
fn release_admitted_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    root: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let released = crate::conn::cached_execute(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .release_admitted
            .sql(),
        params![session_id.as_str(), batch_id.as_str(), root.as_str()],
    )
    .map_err(sqlite_error)?;
    require_settlement_applied(
        session_id,
        root,
        lash_core_execution::store::IngressRowId::Batch(batch_id.clone()),
        released,
    )
}

fn input_row(input_id: &lash_core_execution::InputId) -> lash_core_execution::store::IngressRowId {
    lash_core_execution::store::IngressRowId::Input(input_id.clone())
}

/// Backstop: the verdict was taken over this row earlier in the same write
/// transaction, so the root predicate cannot legitimately miss. A miss is
/// recorded as evidence and then fails closed.
fn require_settlement_applied(
    session_id: &SessionId,
    root: &TurnId,
    row: lash_core_execution::store::IngressRowId,
    rows_affected: usize,
) -> Result<(), StoreError> {
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::SQLITE_BACKEND,
        &row.to_string(),
        u64::try_from(rows_affected).unwrap_or(u64::MAX),
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            root: root.clone(),
            row: Box::new(row.clone()),
            admitted_root: None,
        },
    )
}
