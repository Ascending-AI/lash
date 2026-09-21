use super::*;

pub(super) async fn complete_queued_work_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed_claims: &[QueuedWorkCompletion],
) -> Result<(), StoreError> {
    for completed in completed_claims {
        for batch_id in &completed.batch_ids {
            let sql = crate::turn_ingress::turn_ingress_sql();
            let source_key: Option<String> =
                sqlx::query_scalar(sql.family_postgres.select_claimed_batch_source_key.sql())
                    .bind(completed.session_id.as_str())
                    .bind(batch_id.as_str())
                    .bind(&completed.claim_id)
                    .bind(&completed.lease_token)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?
                    .flatten();
            let payload_json: Option<String> =
                sqlx::query_scalar(sql.family_postgres.select_claimed_batch_head_payload.sql())
                    .bind(completed.session_id.as_str())
                    .bind(batch_id.as_str())
                    .bind(&completed.claim_id)
                    .bind(&completed.lease_token)
                    .fetch_optional(&mut **tx)
                    .await
                    .map_err(store_sqlx_error)?;
            let wake_source = payload_json
                .as_deref()
                .map(|json| {
                    store_decode_json::<lash_core::runtime::QueuedWorkPayload>(
                        json,
                        "queued work payload",
                    )
                })
                .transpose()?
                .and_then(|payload| match payload {
                    lash_core::runtime::QueuedWorkPayload::ProcessWake { wake } => {
                        Some((wake.process_id, wake.sequence))
                    }
                    _ => None,
                });
            if let (Some(source_key), Some(_)) = (source_key.as_deref(), wake_source.as_ref()) {
                lock_process_wake_source_tx(tx, &completed.session_id, source_key).await?;
            }
            if let Some((process_id, sequence)) = wake_source {
                sqlx::query(
                    crate::process_sql::process_sql()
                        .fence_postgres
                        .upsert_max
                        .sql(),
                )
                .bind(completed.session_id.as_str())
                .bind(process_id.as_str())
                .bind(sequence as i64)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            }
            let completion = sqlx::query(sql.queued_batches.settle_claimed.sql())
                .bind(completed.session_id.as_str())
                .bind(batch_id.as_str())
                .bind(&completed.claim_id)
                .bind(&completed.lease_token)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            // Backstop: `ensure_queued_work_completion_tx` already took the
            // verdict over this row under `FOR UPDATE` earlier in this same
            // transaction, so the predicate cannot legitimately miss. A miss is
            // recorded as evidence and then fails closed with the same
            // supersession this site has always returned.
            lash_core::store_backend_support::require_fenced_write_applied(
                lash_core::store_backend_support::FencedWrite::QueuedWorkClaimSettlement,
                crate::POSTGRES_BACKEND,
                batch_id.as_str(),
                completion.rows_affected(),
                || StoreError::QueuedWorkClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                    row_id: Some(batch_id.as_str().to_string().into_boxed_str()),
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                },
            )?;
        }
    }
    Ok(())
}

pub(crate) async fn complete_turn_input_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed_claims: &[lash_core::TurnInputCompletion],
) -> Result<(), StoreError> {
    let pending_inputs = &crate::turn_ingress::turn_ingress_sql().pending_inputs;
    let unclaimed_settlement_statement = pending_inputs.settle_unclaimed.sql();
    let claimed_settlement_statement = pending_inputs.settle_claimed.sql();
    for completed in completed_claims {
        for input_id in &completed.input_ids {
            // One conditional write for both settlement regimes: the claim
            // fields are an optional predicate strengthener, and either way
            // exactly one row must change (ADR 0069 §5).
            let settlement = match completed.claim.as_ref() {
                Some(claim) => sqlx::query(claimed_settlement_statement)
                    .bind(completed.session_id.as_str())
                    .bind(input_id.as_str())
                    .bind(lash_core::TurnInputStateKind::Completed.as_str())
                    .bind(&claim.claim_id)
                    .bind(&claim.lease_token),
                None => sqlx::query(unclaimed_settlement_statement)
                    .bind(completed.session_id.as_str())
                    .bind(input_id.as_str())
                    .bind(lash_core::TurnInputStateKind::Completed.as_str()),
            }
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            // Backstop: `ensure_turn_input_completion_tx` already took the
            // verdict over this row under `FOR UPDATE` earlier in this same
            // transaction, so the predicate cannot legitimately miss. A miss
            // is recorded as evidence and then fails closed with the same
            // supersession this site has always returned.
            lash_core::store_backend_support::require_fenced_write_applied(
                match completed.claim.as_ref() {
                    Some(_) => {
                        lash_core::store_backend_support::FencedWrite::TurnInputClaimSettlement
                    }
                    None => {
                        lash_core::store_backend_support::FencedWrite::UnclaimedTurnInputSettlement
                    }
                },
                crate::POSTGRES_BACKEND,
                input_id.as_str(),
                settlement.rows_affected(),
                || match completed.claim.as_ref() {
                    Some(claim) => StoreError::TurnInputClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: claim.claim_id.clone(),
                        row_id: Some(input_id.as_str().to_string().into_boxed_str()),
                        superseding_claim_id: None,
                        superseding_session_lease_generation: None,
                    },
                    None => StoreError::UnclaimedTurnInputSettlementSuperseded {
                        session_id: completed.session_id.clone(),
                        input_id: input_id.clone(),
                        observed_state: None,
                        superseding_claim_id: None,
                    },
                },
            )?;
        }
    }
    Ok(())
}
