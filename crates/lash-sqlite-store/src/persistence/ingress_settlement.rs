//! A commit's ingress settlement (FIG-3927), inside its `BEGIN IMMEDIATE`
//! write transaction.
//!
//! Every row a commit names is settled under the run that admitted it: the
//! shared verdict decides over the row as read, and the write keeps the run
//! predicate as its backstop. A row the run does not hold refuses the whole
//! commit. The session-command run a commit applied settles bindlessly,
//! predicated on each row still being open.

use super::*;

/// Settle `commit`'s ingress and applied commands at `now`. Returns the rows
/// it released or dropped.
pub(super) fn settle_commit_ingress_conn(
    tx: &Connection,
    commit: &RuntimeCommit,
    now: u64,
) -> Result<lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome, StoreError> {
    let session_id = &commit.session_id;
    if let Some(commands) = commit.applied_commands.as_ref() {
        for batch_id in &commands.batch_ids {
            crate::queued_work::settle_open_command_conn(tx, commit, batch_id, now)?;
        }
    }
    let mut affected_inputs = Vec::new();
    if let Some(ingress) = commit.ingress.as_ref() {
        let run = &ingress.run;
        for completion in &ingress.completed_inputs {
            for input_id in &completion.input_ids {
                admitted_input_conn(tx, session_id, run, input_id)?;
                settle_admitted_input_conn(
                    tx,
                    session_id,
                    run,
                    input_id,
                    lash_core_store::turn_input_vocabulary::TurnInputStateKind::Completed,
                    now,
                )?;
            }
        }
        for completion in &ingress.completed_batches {
            for batch_id in &completion.batch_ids {
                crate::queued_work::complete_admitted_batch_conn(
                    tx,
                    session_id,
                    run,
                    batch_id,
                    lash_core_store::store::IngressTerminal {
                        cause: lash_core_store::store::IngressTerminalCause::Delivered,
                        at_ms: now,
                    },
                )?;
            }
        }
        for (rows, disposition) in [
            (
                &ingress.released,
                lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Defer,
            ),
            (
                &ingress.dropped,
                lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Drop,
            ),
        ] {
            for row in rows {
                match row {
                    lash_core_store::store::IngressRowId::Input(input_id) => {
                        let held = admitted_input_conn(tx, session_id, run, input_id)?;
                        let payload =
                            decode_turn_input(held.input_json)?.into_live("PendingTurnInput")?;
                        match disposition {
                            lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Defer => {
                                release_admitted_input_conn(tx, session_id, run, input_id)?;
                            }
                            lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Drop => {
                                settle_admitted_input_conn(
                                    tx,
                                    session_id,
                                    run,
                                    input_id,
                                    lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled,
                                    now,
                                )?;
                            }
                        }
                        affected_inputs.push((
                            held.enqueue_seq,
                            lash_core_store::turn_control_vocabulary::TurnCancelAffectedInput {
                                input_id: input_id.clone(),
                                payload,
                                disposition,
                            },
                        ));
                    }
                    lash_core_store::store::IngressRowId::Batch(batch_id) => {
                        match disposition {
                            // A deferred command keeps its ingress position.
                            lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Defer => {
                                admitted_batch_conn(tx, session_id, run, batch_id)?;
                                release_admitted_batch_conn(tx, session_id, run, batch_id)?;
                            }
                            lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Drop => {
                                crate::queued_work::complete_admitted_batch_conn(
                                    tx,
                                    session_id,
                                    run,
                                    batch_id,
                                    lash_core_store::store::IngressTerminal {
                                        cause:
                                            lash_core_store::store::IngressTerminalCause::Cancelled,
                                        at_ms: now,
                                    },
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    // The outcome reports every row the commit released or dropped, in the
    // order they were submitted.
    affected_inputs.sort_by_key(|(enqueue_seq, _)| *enqueue_seq);
    Ok(
        lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome {
            affected_inputs: affected_inputs
                .into_iter()
                .map(|(_, affected)| affected)
                .collect(),
        },
    )
}

/// The row of input `input_id`, which run `run` must hold.
fn admitted_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_protocol_types::InputId,
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
    lash_core_store::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &lash_core_store::store::IngressRowId::Input(input_id.clone()),
        row.as_ref().map(|row| row.admitted_run.as_deref()),
    )?;
    row.ok_or_else(|| StoreError::Backend(format!("admitted input `{input_id}` vanished")))
}

/// The hydrated batch `batch_id`, which run `run` must hold.
fn admitted_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    batch_id: &lash_protocol_types::BatchId,
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
    lash_core_store::store_backend_support::require_admitted_to_run(
        session_id,
        run,
        &lash_core_store::store::IngressRowId::Batch(batch_id.clone()),
        row.as_ref().map(|row| row.admitted_run.as_deref()),
    )?;
    let row =
        row.ok_or_else(|| StoreError::Backend(format!("admitted batch `{batch_id}` vanished")))?;
    queued_work_batch_from_row(row)
}

/// Settle input `input_id`, held by `run`, into the terminal `state` at
/// `now`.
fn settle_admitted_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_protocol_types::InputId,
    state: lash_core_store::turn_input_vocabulary::TurnInputStateKind,
    now: u64,
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
            run.as_str(),
            crate::clamp_epoch_ms(now),
        ],
    )
    .map_err(sqlite_error)?;
    require_settlement_applied(session_id, run, input_row(input_id), settled)
}

/// Hand input `input_id`, held by `run`, back open at its own position.
fn release_admitted_input_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    input_id: &lash_protocol_types::InputId,
) -> Result<(), StoreError> {
    let released = crate::conn::cached_execute(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .release_admitted
            .sql(),
        params![session_id.as_str(), input_id.as_str(), run.as_str()],
    )
    .map_err(sqlite_error)?;
    require_settlement_applied(session_id, run, input_row(input_id), released)
}

/// Hand batch `batch_id`, held by `run`, back open at its own position.
fn release_admitted_batch_conn(
    tx: &Connection,
    session_id: &SessionId,
    run: &TurnId,
    batch_id: &lash_protocol_types::BatchId,
) -> Result<(), StoreError> {
    let released = crate::conn::cached_execute(
        tx,
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .release_admitted
            .sql(),
        params![session_id.as_str(), batch_id.as_str(), run.as_str()],
    )
    .map_err(sqlite_error)?;
    require_settlement_applied(
        session_id,
        run,
        lash_core_store::store::IngressRowId::Batch(batch_id.clone()),
        released,
    )
}

fn input_row(input_id: &lash_protocol_types::InputId) -> lash_core_store::store::IngressRowId {
    lash_core_store::store::IngressRowId::Input(input_id.clone())
}

/// Backstop: the verdict was taken over this row earlier in the same write
/// transaction, so the run predicate cannot legitimately miss. A miss is
/// recorded as evidence and then fails closed.
fn require_settlement_applied(
    session_id: &SessionId,
    run: &TurnId,
    row: lash_core_store::store::IngressRowId,
    rows_affected: usize,
) -> Result<(), StoreError> {
    lash_core_store::store_backend_support::require_fenced_write_applied(
        lash_core_store::store_backend_support::FencedWrite::IngressSettlement,
        crate::SQLITE_BACKEND,
        &row.to_string(),
        u64::try_from(rows_affected).unwrap_or(u64::MAX),
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            run: run.clone(),
            row: Box::new(row.clone()),
            admitted_run: None,
        },
    )
}
