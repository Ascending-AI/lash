use super::*;

pub(super) async fn complete_queued_work_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed_claims: &[QueuedWorkCompletion],
) -> Result<(), StoreError> {
    for completed in completed_claims {
        for batch_id in &completed.batch_ids {
            let source_key: Option<String> = sqlx::query_scalar(
                "SELECT source_key
                 FROM lash_queued_work_batches
                 WHERE session_id = $1
                   AND batch_id = $2
                   AND claim_id = $3
                   AND claim_token = $4",
            )
            .bind(completed.session_id.as_str())
            .bind(batch_id)
            .bind(&completed.claim_id)
            .bind(&completed.lease_token)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?
            .flatten();
            let payload_json: Option<String> = sqlx::query_scalar(
                "SELECT item.payload_json
                 FROM lash_queued_work_batches AS batch
                 JOIN lash_queued_work_items AS item ON item.batch_id = batch.batch_id
                 WHERE batch.session_id = $1
                   AND batch.batch_id = $2
                   AND batch.claim_id = $3
                   AND batch.claim_token = $4
                 ORDER BY item.item_index ASC
                 LIMIT 1",
            )
            .bind(completed.session_id.as_str())
            .bind(batch_id)
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
                    "INSERT INTO lash_wake_redelivery_fences (
                        session_id, process_id, allocation_floor
                     ) VALUES ($1, $2, $3)
                     ON CONFLICT (session_id, process_id) DO UPDATE
                     SET allocation_floor = GREATEST(
                         lash_wake_redelivery_fences.allocation_floor,
                         EXCLUDED.allocation_floor
                     )",
                )
                .bind(completed.session_id.as_str())
                .bind(process_id.as_str())
                .bind(sequence as i64)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            }
            let completion = sqlx::query(
                "DELETE FROM lash_queued_work_batches
                 WHERE session_id = $1 AND batch_id = $2 AND claim_id = $3 AND claim_token = $4",
            )
            .bind(completed.session_id.as_str())
            .bind(batch_id)
            .bind(&completed.claim_id)
            .bind(&completed.lease_token)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            if completion.rows_affected() != 1 {
                return Err(StoreError::QueuedWorkClaimSuperseded {
                    session_id: completed.session_id.clone(),
                    claim_id: completed.claim_id.clone(),
                    row_id: Some(batch_id.clone().into_boxed_str()),
                    superseding_claim_id: None,
                    superseding_session_lease_generation: None,
                });
            }
        }
    }
    Ok(())
}

pub(crate) async fn complete_turn_input_claims_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    completed_claims: &[lash_core::TurnInputCompletion],
) -> Result<(), StoreError> {
    let unclaimed_settlement_statement = format!(
        "UPDATE lash_pending_turn_inputs
                     SET state = $3,
                         claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = $1
                       AND input_id = $2
                       AND claim_id IS NULL
                       AND state NOT IN ({terminal_states})",
        terminal_states = super::turn_input_settlement::unclaimed_turn_input_terminal_states_sql()
    );
    for completed in completed_claims {
        for input_id in &completed.input_ids {
            // One conditional write for both settlement regimes: the claim
            // fields are an optional predicate strengthener, and either way
            // exactly one row must change (ADR 0069 §5).
            let settlement = match completed.claim.as_ref() {
                Some(claim) => sqlx::query(
                    "UPDATE lash_pending_turn_inputs
                     SET state = $3,
                         claim_id = NULL,
                         claim_owner_id = NULL,
                         claim_owner_incarnation_id = NULL,
                         claim_token = NULL,
                         claim_session_lease_generation = 0
                     WHERE session_id = $1
                       AND input_id = $2
                       AND claim_id = $4
                       AND claim_token = $5",
                )
                .bind(completed.session_id.as_str())
                .bind(input_id)
                .bind(lash_core::TurnInputState::Completed.as_str())
                .bind(&claim.claim_id)
                .bind(&claim.lease_token),
                None => sqlx::query(&unclaimed_settlement_statement)
                    .bind(completed.session_id.as_str())
                    .bind(input_id)
                    .bind(lash_core::TurnInputState::Completed.as_str()),
            }
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            if settlement.rows_affected() != 1 {
                return Err(match completed.claim.as_ref() {
                    Some(claim) => StoreError::TurnInputClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: claim.claim_id.clone(),
                        row_id: Some(input_id.clone().into_boxed_str()),
                        superseding_claim_id: None,
                        superseding_session_lease_generation: None,
                    },
                    None => StoreError::UnclaimedTurnInputSettlementSuperseded {
                        session_id: completed.session_id.clone(),
                        input_id: input_id.clone(),
                        observed_state: None,
                        superseding_claim_id: None,
                    },
                });
            }
        }
    }
    Ok(())
}
