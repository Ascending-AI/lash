//! A commit's ingress settlement (FIG-3927), inside its write transaction.
//!
//! Every row a commit names is settled under the root that admitted it: the
//! shared verdict decides over the row as read under `FOR UPDATE`, and the
//! write keeps the root predicate as its backstop. A row the root does not
//! hold refuses the whole commit. The session-command run a commit applied
//! settles bindlessly, predicated on each row still being open.

use super::*;

type PgTx<'c> = sqlx::Transaction<'c, sqlx::Postgres>;

/// Settle `commit`'s ingress and applied commands, and, for an interrupted
/// turn, re-defer or drop the open input addressed to it, at `now`. Returns
/// the cancel outcome the commit's cancellation records.
pub(super) async fn settle_commit_ingress_tx(
    tx: &mut PgTx<'_>,
    commit: &RuntimeCommit,
    now: u64,
) -> Result<lash_core_execution::TurnCancelInputOutcome, StoreError> {
    let session_id = &commit.session_id;
    if let Some(commands) = commit.applied_commands.as_ref() {
        for batch_id in &commands.batch_ids {
            crate::queued_work::settle_open_command_tx(tx, session_id, batch_id).await?;
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
        if !reconcile_turn_cancel_winner_tx(tx, session_id, turn_id, observed, evidence).await? {
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
                admitted_input_tx(tx, session_id, root, input_id).await?;
                settle_admitted_input_tx(
                    tx,
                    session_id,
                    root,
                    input_id,
                    lash_core_execution::runtime::TurnInputStateKind::Completed,
                )
                .await?;
                // The completing commit binds the input to the root that
                // applied it: a checkpoint-admitted input carries no binding
                // until now, and a root-admitted input's is this same row.
                crate::session_roots::bind_applied_input_tx(tx, session_id, input_id, root).await?;
            }
        }
        for completion in &ingress.completed_batches {
            for batch_id in &completion.batch_ids {
                crate::queued_work::complete_admitted_batch_tx(tx, session_id, root, batch_id)
                    .await?;
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
                        let held = admitted_input_tx(tx, session_id, root, input_id).await?;
                        match disposition {
                            lash_core_execution::TurnCancelDisposition::Defer => {
                                release_admitted_input_tx(tx, session_id, root, input_id).await?;
                            }
                            lash_core_execution::TurnCancelDisposition::Drop => {
                                settle_admitted_input_tx(
                                    tx,
                                    session_id,
                                    root,
                                    input_id,
                                    lash_core_execution::runtime::TurnInputStateKind::Cancelled,
                                )
                                .await?;
                            }
                        }
                        affected_inputs.push((
                            held.enqueue_seq,
                            lash_core_execution::TurnCancelAffectedInput {
                                input_id: input_id.clone(),
                                payload: held.input,
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
                                let batch =
                                    admitted_batch_tx(tx, session_id, root, batch_id).await?;
                                release_admitted_batch_tx(tx, session_id, root, batch_id).await?;
                                affected_wakes.extend(
                                    lash_core_execution::store_backend_support::deferred_wake_records(
                                        std::slice::from_ref(&batch),
                                    ),
                                );
                            }
                            lash_core_execution::TurnCancelDisposition::Drop => {
                                crate::queued_work::complete_admitted_batch_tx(
                                    tx, session_id, root, batch_id,
                                )
                                .await?;
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
    let rows = sqlx::query(sql.pending_inputs_postgres.select_pending_active.sql())
        .bind(session_id.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut open = Vec::new();
    for row in rows {
        let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
        if input.state.active_turn_id() == Some(turn_id) {
            open.push(input);
        }
    }
    let deferred = lash_core_execution::TurnInputState::DeferredNextTurn;
    let deferred_ingress = encode_json(&deferred.ingress())?;
    for input in open {
        // Two dispositions, two named statements: deferring rewrites the
        // ingress so the row stops naming a turn that is over, dropping is
        // the withdrawal this table already has.
        match disposition {
            lash_core_execution::TurnCancelDisposition::Defer => {
                sqlx::query(sql.pending_inputs.defer_to_next_turn.sql())
                    .bind(session_id.as_str())
                    .bind(input.input_id.as_str())
                    .bind(deferred.as_str())
                    .bind(&deferred_ingress)
            }
            lash_core_execution::TurnCancelDisposition::Drop => {
                sqlx::query(sql.pending_inputs.cancel.sql())
                    .bind(session_id.as_str())
                    .bind(input.input_id.as_str())
                    .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
                    .bind(crate::support::clamp_epoch_ms(now))
            }
        }
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        affected_inputs.push((
            input.enqueue_seq,
            lash_core_execution::TurnCancelAffectedInput {
                input_id: input.input_id,
                payload: input.input,
                disposition,
            },
        ));
    }
    // The outcome reports every row the teardown moved; only a cancellation
    // has a request record to append them to.
    affected_inputs.sort_by_key(|(enqueue_seq, _)| *enqueue_seq);
    for (_, affected) in affected_inputs {
        if cancellation.is_some() {
            append_turn_cancel_outcome_conn(tx, session_id, turn_id, affected.clone()).await?;
        }
        outcome.affected_inputs.push(affected);
    }
    for affected in affected_wakes {
        if cancellation.is_some() {
            append_turn_cancel_wake_tx(tx, session_id, turn_id, &affected).await?;
        }
        outcome.affected_wakes.push(affected);
    }
    Ok(outcome)
}

/// Input `input_id`, which root `root` must hold, locked for the commit.
async fn admitted_input_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    root: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs_postgres
            .select_by_id_for_update
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .map(pending_turn_input_row)
    .transpose()?;
    lash_core_execution::store_backend_support::require_admitted_to_root(
        session_id,
        root,
        &input_row(input_id),
        row.as_ref().map(|row| row.admitted_root.as_deref()),
    )?;
    let row =
        row.ok_or_else(|| StoreError::Backend(format!("admitted input `{input_id}` vanished")))?;
    pending_turn_input_from_row(row)
}

/// The hydrated batch `batch_id`, which root `root` must hold, locked for
/// the commit.
async fn admitted_batch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    root: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<QueuedWorkBatch, StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    let observed: Option<Option<String>> =
        sqlx::query_scalar(sql.queued_batches_postgres.settlement_facts.sql())
            .bind(session_id.as_str())
            .bind(batch_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    lash_core_execution::store_backend_support::require_admitted_to_root(
        session_id,
        root,
        &lash_core_execution::store::IngressRowId::Batch(batch_id.clone()),
        observed.as_ref().map(Option::as_deref),
    )?;
    load_queued_batch(tx, batch_id.as_str())
        .await?
        .ok_or_else(|| StoreError::Backend(format!("admitted batch `{batch_id}` vanished")))
}

/// Settle input `input_id`, held by `root`, into the terminal `state`.
async fn settle_admitted_input_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    root: &TurnId,
    input_id: &lash_core_execution::InputId,
    state: lash_core_execution::runtime::TurnInputStateKind,
) -> Result<(), StoreError> {
    let settled = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .settle_admitted
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .bind(state.as_str())
    .bind(root.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
    require_settlement_applied(session_id, root, input_row(input_id), settled)
}

/// Hand input `input_id`, held by `root`, back open at its own position.
async fn release_admitted_input_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    root: &TurnId,
    input_id: &lash_core_execution::InputId,
) -> Result<(), StoreError> {
    let deferred = lash_core_execution::TurnInputState::DeferredNextTurn;
    let released = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .release_admitted
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id.as_str())
    .bind(root.as_str())
    .bind(deferred.as_str())
    .bind(encode_json(&deferred.ingress())?)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
    require_settlement_applied(session_id, root, input_row(input_id), released)
}

/// Hand batch `batch_id`, held by `root`, back open at its own position.
async fn release_admitted_batch_tx(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    root: &TurnId,
    batch_id: &lash_core_execution::BatchId,
) -> Result<(), StoreError> {
    let released = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .queued_batches
            .release_admitted
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(batch_id.as_str())
    .bind(root.as_str())
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .rows_affected();
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

/// Backstop: the verdict was taken over this row under its lock earlier in
/// the same transaction, so the root predicate cannot legitimately miss. A
/// miss is recorded as evidence and then fails closed.
fn require_settlement_applied(
    session_id: &SessionId,
    root: &TurnId,
    row: lash_core_execution::store::IngressRowId,
    rows_affected: u64,
) -> Result<(), StoreError> {
    lash_core_execution::store_backend_support::require_fenced_write_applied(
        lash_core_execution::store_backend_support::FencedWrite::IngressSettlement,
        crate::POSTGRES_BACKEND,
        &row.to_string(),
        rows_affected,
        || StoreError::IngressRowNotAdmitted {
            session_id: session_id.clone(),
            root: root.clone(),
            row: Box::new(row.clone()),
            admitted_root: None,
        },
    )
}
