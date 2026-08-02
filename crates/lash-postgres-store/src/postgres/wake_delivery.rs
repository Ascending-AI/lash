use super::*;

pub(super) async fn claim_pending_wake_deliveries(
    registry: &PostgresProcessRegistry,
    limit: usize,
) -> Result<Vec<lash_core::WakeDelivery>, PluginError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let now = registry.clock.timestamp_ms() as i64;
    sqlx::query(
        "UPDATE lash_process_wake_deliveries
         SET state = 'pending', claim_token = NULL
         WHERE state = 'enqueuing' AND next_attempt_at_ms <= $1",
    )
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT candidate.delivery_id
         FROM lash_process_wake_deliveries AS candidate
         WHERE candidate.state = 'pending'
           AND candidate.next_attempt_at_ms <= $2
           AND NOT EXISTS (
               SELECT 1
               FROM lash_process_wake_deliveries AS earlier
               WHERE earlier.state <> 'enqueued'
                 AND NOT (
                     earlier.state = 'discarded'
                     AND earlier.discard_reason = 'sequence_rewound'
                 )
                 AND earlier.target_session_id = candidate.target_session_id
                 AND earlier.process_id = candidate.process_id
                 AND earlier.sequence < candidate.sequence
           )
         ORDER BY candidate.next_attempt_at_ms ASC,
                  candidate.target_session_id ASC,
                  candidate.process_id ASC,
                  candidate.sequence ASC
         LIMIT $1
         FOR UPDATE OF candidate SKIP LOCKED",
    )
    .bind(limit as i64)
    .bind(now)
    .fetch_all(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let mut deliveries = Vec::with_capacity(ids.len());
    for id in ids {
        let claim_token = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "UPDATE lash_process_wake_deliveries
             SET state = 'enqueuing',
                 claim_token = $4,
                 attempts = attempts + 1,
                 first_attempt_ms = COALESCE(first_attempt_ms, $2),
                 next_attempt_at_ms = $3
             WHERE delivery_id = $1 AND state = 'pending'",
        )
        .bind(&id)
        .bind(now)
        .bind(now.saturating_add(registry.wake_delivery_config.enqueuing_stale_after_ms as i64))
        .bind(claim_token)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        deliveries.push(load_wake_delivery_tx(&mut tx, &id).await?);
    }
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(deliveries)
}

pub(super) async fn load_wake_delivery_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    delivery_id: &str,
) -> Result<lash_core::WakeDelivery, PluginError> {
    let row = sqlx::query(
        "SELECT delivery_id, state, claim_token, attempts, first_attempt_ms, next_attempt_at_ms,
                expires_at_ms, discard_reason, delivery_json
         FROM lash_process_wake_deliveries WHERE delivery_id = $1",
    )
    .bind(delivery_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?
    .ok_or_else(|| PluginError::Session(format!("unknown wake delivery `{delivery_id}`")))?;
    decode_wake_delivery_row(row)
}

pub(super) fn decode_wake_delivery_row(
    row: sqlx::postgres::PgRow,
) -> Result<lash_core::WakeDelivery, PluginError> {
    let delivery_id: String = row.get(0);
    let state_value: String = row.get(1);
    let state = match state_value.as_str() {
        "pending" => lash_core::WakeDeliveryState::Pending,
        "enqueuing" => lash_core::WakeDeliveryState::Enqueuing,
        "enqueued" => lash_core::WakeDeliveryState::Enqueued,
        "discarded" => lash_core::WakeDeliveryState::Discarded,
        state => {
            return Err(PluginError::Session(format!(
                "wake delivery `{delivery_id}` has unknown state `{state}`"
            )));
        }
    };
    let discard_value: Option<String> = row.get(7);
    let discard_reason = match discard_value.as_deref() {
        None => None,
        Some("expired") => Some(lash_core::WakeDiscardReason::Expired),
        Some("target_gone") => Some(lash_core::WakeDiscardReason::TargetGone),
        Some("retargeted") => Some(lash_core::WakeDiscardReason::Retargeted),
        Some("sequence_rewound") => Some(lash_core::WakeDiscardReason::SequenceRewound),
        Some(reason) => {
            return Err(PluginError::Session(format!(
                "wake delivery `{delivery_id}` has unknown discard reason `{reason}`"
            )));
        }
    };
    let delivery_json: String = row.get(8);
    Ok(lash_core::WakeDelivery {
        delivery_id,
        wake: serde_json::from_str(&delivery_json).map_err(process_decode_error)?,
        state,
        claim_token: row.get(2),
        attempts: row.get::<i64, _>(3) as u64,
        first_attempt_ms: row.get::<Option<i64>, _>(4).map(|value| value as u64),
        next_attempt_at_ms: row.get::<i64, _>(5) as u64,
        expires_at_ms: row.get::<i64, _>(6) as u64,
        discard_reason,
    })
}

pub(super) fn wake_delivery_report<'a>(
    deliveries: impl IntoIterator<Item = &'a lash_core::WakeDelivery>,
) -> lash_core::WakeDeliveryReport {
    lash_core::WakeDeliveryReport::from_deliveries(deliveries)
}

pub(super) async fn update_wake_delivery_state(
    pool: &PgPool,
    delivery_id: &str,
    claim_token: &str,
    state: lash_core::WakeDeliveryState,
    reason: Option<lash_core::WakeDiscardReason>,
) -> Result<lash_core::WakeDeliveryClaimOutcome, PluginError> {
    let changed = sqlx::query(
        "UPDATE lash_process_wake_deliveries
         SET state = $3, claim_token = NULL, discard_reason = $4
         WHERE delivery_id = $1 AND state = 'enqueuing' AND claim_token = $2",
    )
    .bind(delivery_id)
    .bind(claim_token)
    .bind(state.as_str())
    .bind(reason.map(lash_core::WakeDiscardReason::as_str))
    .execute(pool)
    .await
    .map_err(plugin_sqlx_error)?
    .rows_affected();
    if changed == 0 {
        let current: Option<String> = sqlx::query_scalar(
            "SELECT state FROM lash_process_wake_deliveries WHERE delivery_id = $1",
        )
        .bind(delivery_id)
        .fetch_optional(pool)
        .await
        .map_err(plugin_sqlx_error)?;
        let current = current.ok_or_else(|| {
            PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
        })?;
        let state = match current.as_str() {
            "pending" => lash_core::WakeDeliveryState::Pending,
            "enqueuing" => lash_core::WakeDeliveryState::Enqueuing,
            "enqueued" => lash_core::WakeDeliveryState::Enqueued,
            "discarded" => lash_core::WakeDeliveryState::Discarded,
            _ => {
                return Err(PluginError::Session(format!(
                    "wake delivery `{delivery_id}` has unknown state `{current}`"
                )));
            }
        };
        return Ok(lash_core::WakeDeliveryClaimOutcome::ClaimLost { state });
    }
    Ok(lash_core::WakeDeliveryClaimOutcome::Applied)
}
