//! Pending turn-input row projection and turn-input claim leases.
//!
//! The durable representation of queued turn inputs, mirroring the SQLite
//! backend's `pending_turn_inputs` module. Originated in `session_factory.rs`;
//! every item keeps its previous path through the crate-root glob.

use crate::*;

#[derive(Clone, Debug)]
pub(crate) struct PendingTurnInputRow {
    pub(crate) enqueue_seq: u64,
    pub(crate) input_id: String,
    session_id: SessionId,
    source_key: Option<String>,
    state: lash_core_execution::TurnInputState,
    input_json: String,
    enqueued_at_ms: u64,
    claim_id: Option<String>,
    pub(crate) claim_fencing_token: u64,
    claim_owner: Option<LeaseOwnerIdentity>,
    claim_token: Option<String>,
    claim_session_lease_generation: u64,
}

impl PendingTurnInputRow {
    pub(crate) fn claim_identity(&self) -> Option<(&str, &str, &LeaseOwnerIdentity)> {
        Some((
            self.claim_id.as_deref()?,
            self.claim_token.as_deref()?,
            self.claim_owner.as_ref()?,
        ))
    }

    /// The claim columns the shared claimability verdict consults.
    ///
    /// Exposed as one value rather than two fields so a call site cannot pass
    /// a generation that belongs to a different row's token.
    pub(crate) fn claim_facts(
        &self,
    ) -> lash_core_execution::store_backend_support::WorkRowClaimFacts<'_> {
        lash_core_execution::store_backend_support::WorkRowClaimFacts {
            claim_token: self.claim_token.as_deref(),
            claim_session_lease_generation: self.claim_session_lease_generation,
        }
    }

    /// The decoded lifecycle state this row carries.
    pub(crate) fn state(&self) -> &lash_core_execution::TurnInputState {
        &self.state
    }

    /// Whether a claim token names this row, which is the only way its
    /// generation means anything.
    pub(crate) fn is_claimed(&self) -> bool {
        self.claim_token.is_some()
    }

    /// The session-execution-lease generation this row's claim is pinned to.
    pub(crate) fn claim_session_lease_generation(&self) -> u64 {
        self.claim_session_lease_generation
    }
}

pub(crate) fn pending_turn_input_row(row: PgRow) -> Result<PendingTurnInputRow, StoreError> {
    let ingress_json: String = row.get("ingress_json");
    let ingress: lash_core_execution::TurnInputIngress =
        store_decode_json(&ingress_json, "turn-input ingress")?;
    let state = lash_core_execution::TurnInputState::from_persisted(
        row.get::<String, _>("state").as_str(),
        ingress,
    )
    .ok_or_else(|| StoreError::Backend("invalid pending turn-input state".to_string()))?;
    Ok(PendingTurnInputRow {
        enqueue_seq: u64_from_sql("PendingTurnInput", "enqueue_seq", row.get("enqueue_seq"))?,
        input_id: row.get("input_id"),
        session_id: SessionId::from(row.get::<String, _>("session_id")),
        source_key: row.get("source_key"),
        state,
        input_json: row.get("input_json"),
        enqueued_at_ms: u64_from_sql(
            "PendingTurnInput",
            "enqueued_at_ms",
            row.get("enqueued_at_ms"),
        )?,
        claim_id: row.get("claim_id"),
        claim_fencing_token: u64_from_sql(
            "PendingTurnInput",
            "claim_fencing_token",
            row.get("claim_fencing_token"),
        )?,
        claim_owner: lease_owner_from_columns(
            row.get("claim_owner_id"),
            row.get("claim_owner_incarnation_id"),
        )?,
        claim_token: row.get("claim_token"),
        claim_session_lease_generation: u64_from_sql(
            "PendingTurnInput",
            "claim_session_lease_generation",
            row.get("claim_session_lease_generation"),
        )?,
    })
}

pub(crate) fn pending_turn_input_from_row(
    row: PendingTurnInputRow,
) -> Result<lash_core_execution::PendingTurnInput, StoreError> {
    Ok(lash_core_execution::PendingTurnInput {
        input_id: row.input_id.into(),
        session_id: row.session_id,
        enqueue_seq: row.enqueue_seq,
        source_key: row.source_key,
        state: row.state,
        enqueued_at_ms: row.enqueued_at_ms,
        input: store_decode_json(&row.input_json, "turn input")?,
    })
}

pub(crate) fn pending_turn_input_read_from_row(
    row: PgRow,
) -> Result<lash_core_execution::PendingTurnInputRead, StoreError> {
    let lease_expires_at_ms = row
        .get::<Option<i64>, _>("live_lease_expires_at_ms")
        .map(|value| u64_from_sql("PendingTurnInputRead", "lease_expires_at_ms", value))
        .transpose()?;
    let binding = row
        .get::<Option<String>, _>("claim_bound_turn_id")
        .zip(row.get::<Option<String>, _>("claim_bound_receipt_input_id"));
    let input = pending_turn_input_from_row(pending_turn_input_row(row)?)?;
    Ok(match (binding, lease_expires_at_ms) {
        (Some((turn_id, receipt_input_id)), _) => {
            lash_core_execution::PendingTurnInputRead::turn_bound(
                input,
                turn_id.into(),
                receipt_input_id.into(),
            )
        }
        (None, Some(lease_expires_at_ms)) => {
            lash_core_execution::PendingTurnInputRead::held(input, lease_expires_at_ms)
        }
        (None, None) => lash_core_execution::PendingTurnInputRead::pending(input),
    })
}

pub(crate) async fn load_pending_turn_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    input_id: &str,
) -> Result<Option<lash_core_execution::PendingTurnInput>, StoreError> {
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .select_by_id
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    row.map(pending_turn_input_row)
        .transpose()?
        .map(pending_turn_input_from_row)
        .transpose()
}

pub(crate) async fn load_pending_turn_input_row_by_target_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    target: &lash_core_execution::PendingTurnInputCancelTarget,
    for_update: bool,
) -> Result<Option<PendingTurnInputRow>, StoreError> {
    // Two lock regimes, two named statements each: a cancel that is about to
    // write takes the row lock, a read-only observation must not.
    let sql = crate::turn_ingress::turn_ingress_sql();
    let statement = match (target, for_update) {
        (lash_core_execution::PendingTurnInputCancelTarget::InputId(_), false) => {
            sql.pending_inputs.select_by_id.sql()
        }
        (lash_core_execution::PendingTurnInputCancelTarget::InputId(_), true) => {
            sql.pending_inputs_postgres.select_by_id_for_update.sql()
        }
        (lash_core_execution::PendingTurnInputCancelTarget::SourceKey(_), false) => {
            sql.pending_inputs.select_by_source_key.sql()
        }
        (lash_core_execution::PendingTurnInputCancelTarget::SourceKey(_), true) => sql
            .pending_inputs_postgres
            .select_by_source_key_for_update
            .sql(),
    };
    let key = match target {
        lash_core_execution::PendingTurnInputCancelTarget::InputId(input_id) => input_id.as_str(),
        lash_core_execution::PendingTurnInputCancelTarget::SourceKey(source_key) => {
            source_key.as_str()
        }
    };
    let row = sqlx::query(statement)
        .bind(session_id.as_str())
        .bind(key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    row.map(pending_turn_input_row).transpose()
}

fn pending_turn_input_claim_diagnostics_from_row(
    row: &PendingTurnInputRow,
) -> Option<lash_core_execution::PendingTurnInputClaimDiagnostics> {
    row.claim_token
        .is_some()
        .then(|| lash_core_execution::PendingTurnInputClaimDiagnostics {
            state: row.state.clone(),
            claim_id: row.claim_id.clone(),
            claim_owner: row.claim_owner.clone(),
            claim_session_lease_generation: row
                .claim_token
                .as_ref()
                .map(|_| row.claim_session_lease_generation),
            claim_fencing_token: row.claim_fencing_token,
        })
}

/// Which rows a cancel locks in queue order before it writes any (FIG-3589).
pub(crate) enum CancelLockScope<'a> {
    /// The resolved explicit targets.
    Targets(&'a std::collections::BTreeSet<lash_core_execution::InputId>),
    /// The suffix from this `enqueue_seq`.
    Suffix(u64),
}

/// Lock every row a cancel may write, targets plus the other rows of any bound
/// claim among them, in queue order: the order the redrive's commit and a
/// journal-less redrive's re-take lock the same rows in, so a concurrent cancel
/// and redrive cannot deadlock (FIG-3589).
pub(crate) async fn lock_cancel_rows_in_queue_order(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    scope: CancelLockScope<'_>,
) -> Result<(), StoreError> {
    let statements = &crate::turn_ingress::turn_ingress_sql().pending_inputs_postgres;
    let query = match scope {
        CancelLockScope::Targets(targets) => {
            let input_ids = targets
                .iter()
                .map(|input_id| input_id.as_str().to_string())
                .collect::<Vec<_>>();
            sqlx::query(statements.lock_cancel_targets_in_queue_order.sql())
                .bind(session_id.as_str())
                .bind(input_ids)
        }
        CancelLockScope::Suffix(anchor_seq) => {
            sqlx::query(statements.lock_cancel_suffix_in_queue_order.sql())
                .bind(session_id.as_str())
                .bind(i64::try_from(anchor_seq).unwrap_or(i64::MAX))
        }
    };
    query.fetch_all(&mut **tx).await.map_err(store_sqlx_error)?;
    Ok(())
}

/// The binding input `input_id` of `session_id` carries, if any (FIG-3589).
async fn turn_input_binding_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    input_id: &str,
) -> Result<Option<(lash_core_execution::TurnId, lash_core_execution::InputId)>, StoreError> {
    let (turn_id, receipt): (Option<String>, Option<String>) = sqlx::query_as(
        crate::turn_ingress::turn_ingress_sql()
            .pending_inputs
            .binding_facts
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(input_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(turn_id
        .zip(receipt)
        .map(|(turn_id, receipt)| (turn_id.into(), receipt.into())))
}

/// Cancel one locked row. `covered` is every input the same cancel operation
/// targets, which decides whether a row bound to an aborted turn may go
/// (FIG-3589).
pub(crate) async fn cancel_pending_turn_input_row_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    row: PendingTurnInputRow,
    now_epoch_ms: u64,
    covered: &std::collections::BTreeSet<lash_core_execution::InputId>,
) -> Result<lash_core_execution::PendingTurnInputCancelOutcome, StoreError> {
    let mut input = pending_turn_input_from_row(row.clone())?;
    match input.state.kind() {
        lash_core_execution::runtime::TurnInputStateKind::Cancelled => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCancelled(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::Completed => {
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::AlreadyCompleted(input))
        }
        lash_core_execution::runtime::TurnInputStateKind::PendingActive
        | lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn
        | lash_core_execution::runtime::TurnInputStateKind::Accepted => {
            let binding = if row.claim_token.is_some() {
                turn_input_binding_tx(tx, &row.session_id, &row.input_id).await?
            } else {
                None
            };
            let bound = lash_core_execution::store_backend_support::bound_turn_input_cancel(
                &input.input_id,
                binding,
                covered,
            );
            if let lash_core_execution::store_backend_support::BoundTurnInputCancel::Refused {
                turn_id,
                receipt_input_id,
            } = bound
            {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::TurnBound {
                        input,
                        turn_id,
                        receipt_input_id,
                    },
                );
            }
            // A claim is live only while the session-execution-lease generation it
            // pins still holds the session lease (ADR 0029).
            let live_claim = row.claim_token.is_some()
                && load_session_execution_lease_tx(tx, &row.session_id)
                    .await?
                    .is_some_and(|lease| {
                        lease.lease_token.is_some()
                            && lease.expires_at_ms > now_epoch_ms
                            && lease.fencing_token == row.claim_session_lease_generation
                    });
            if live_claim {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        input,
                        claim: pending_turn_input_claim_diagnostics_from_row(&row),
                    },
                );
            }
            let run_owns_input: bool = sqlx::query_scalar(
                crate::turn_ingress::turn_ingress_sql()
                    .queued_runs
                    .pending_member
                    .sql(),
            )
            .bind(row.session_id.as_str())
            .bind("input")
            .bind(row.input_id.as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            if run_owns_input {
                return Ok(
                    lash_core_execution::PendingTurnInputCancelOutcome::AlreadyClaimed {
                        input,
                        claim: pending_turn_input_claim_diagnostics_from_row(&row),
                    },
                );
            }
            sqlx::query(
                crate::turn_ingress::turn_ingress_sql()
                    .pending_inputs
                    .cancel
                    .sql(),
            )
            .bind(row.session_id.as_str())
            .bind(row.input_id.as_str())
            .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            // Cancelling the receipt's input leaves the aborted turn's redrive
            // nothing to settle, so the rest of its drive goes back to the
            // queue rather than stay bound (FIG-3589). The rows were locked in
            // queue order before this write.
            if bound == lash_core_execution::store_backend_support::BoundTurnInputCancel::Receipt
                && let (Some(claim_id), Some(claim_token)) = (&row.claim_id, &row.claim_token)
            {
                sqlx::query(
                    crate::turn_ingress::turn_ingress_sql()
                        .pending_inputs
                        .release_bound_claim
                        .sql(),
                )
                .bind(row.session_id.as_str())
                .bind(claim_id)
                .bind(claim_token)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            }
            input.state = lash_core_execution::TurnInputState::Cancelled(input.state.ingress());
            Ok(lash_core_execution::PendingTurnInputCancelOutcome::Cancelled(input))
        }
    }
}
