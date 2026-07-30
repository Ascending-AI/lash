use super::*;

pub(super) fn load_wake_delivery_conn(
    conn: &Connection,
    delivery_id: &str,
) -> Result<lash_core::WakeDelivery, lash_core::PluginError> {
    let row = conn
        .query_row(
            "SELECT state, claim_token, attempts, first_attempt_ms, next_attempt_at_ms,
                    expires_at_ms, discard_reason, delivery_json
             FROM process_wake_deliveries WHERE delivery_id = ?1",
            params![delivery_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(process_sqlite_error)?
        .ok_or_else(|| {
            lash_core::PluginError::Session(format!("unknown wake delivery `{delivery_id}`"))
        })?;
    let state = match row.0.as_str() {
        "pending" => lash_core::WakeDeliveryState::Pending,
        "enqueuing" => lash_core::WakeDeliveryState::Enqueuing,
        "enqueued" => lash_core::WakeDeliveryState::Enqueued,
        "discarded" => lash_core::WakeDeliveryState::Discarded,
        state => {
            return Err(lash_core::PluginError::Session(format!(
                "wake delivery `{delivery_id}` has unknown state `{state}`"
            )));
        }
    };
    let discard_reason = match row.6.as_deref() {
        None => None,
        Some("expired") => Some(lash_core::WakeDiscardReason::Expired),
        Some("target_gone") => Some(lash_core::WakeDiscardReason::TargetGone),
        Some("retargeted") => Some(lash_core::WakeDiscardReason::Retargeted),
        Some(reason) => {
            return Err(lash_core::PluginError::Session(format!(
                "wake delivery `{delivery_id}` has unknown discard reason `{reason}`"
            )));
        }
    };
    Ok(lash_core::WakeDelivery {
        delivery_id: delivery_id.to_string(),
        wake: serde_json::from_str(&row.7).map_err(process_decode_error)?,
        state,
        claim_token: row.1,
        attempts: row.2 as u64,
        first_attempt_ms: row.3.map(|value| value as u64),
        next_attempt_at_ms: row.4 as u64,
        expires_at_ms: row.5 as u64,
        discard_reason,
    })
}

pub(super) fn wake_delivery_report<'a>(
    deliveries: impl IntoIterator<Item = &'a lash_core::WakeDelivery>,
) -> lash_core::WakeDeliveryReport {
    lash_core::WakeDeliveryReport::from_deliveries(deliveries)
}

pub(super) async fn update_wake_delivery_state(
    conn: &SqliteConnection,
    delivery_id: &str,
    claim_token: &str,
    state: lash_core::WakeDeliveryState,
    reason: Option<lash_core::WakeDiscardReason>,
) -> Result<lash_core::WakeDeliveryClaimOutcome, lash_core::PluginError> {
    let delivery_id = delivery_id.to_string();
    let claim_token = claim_token.to_string();
    conn.write_flow(move |tx| {
        Ok(tx_outcome((|| {
            let changed = tx
                .execute(
                    "UPDATE process_wake_deliveries
                     SET state = ?3, claim_token = NULL, discard_reason = ?4
                     WHERE delivery_id = ?1 AND state = 'enqueuing' AND claim_token = ?2",
                    params![
                        delivery_id,
                        claim_token,
                        state.as_str(),
                        reason.map(lash_core::WakeDiscardReason::as_str)
                    ],
                )
                .map_err(process_sqlite_error)?;
            if changed == 0 {
                let current = tx
                    .query_row(
                        "SELECT state FROM process_wake_deliveries WHERE delivery_id = ?1",
                        params![delivery_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(process_sqlite_error)?
                    .ok_or_else(|| {
                        lash_core::PluginError::Session(format!(
                            "unknown wake delivery `{delivery_id}`"
                        ))
                    })?;
                let state = match current.as_str() {
                    "pending" => lash_core::WakeDeliveryState::Pending,
                    "enqueuing" => lash_core::WakeDeliveryState::Enqueuing,
                    "enqueued" => lash_core::WakeDeliveryState::Enqueued,
                    "discarded" => lash_core::WakeDeliveryState::Discarded,
                    _ => {
                        return Err(lash_core::PluginError::Session(format!(
                            "wake delivery `{delivery_id}` has unknown state `{current}`"
                        )));
                    }
                };
                return Ok(lash_core::WakeDeliveryClaimOutcome::ClaimLost { state });
            }
            Ok(lash_core::WakeDeliveryClaimOutcome::Applied)
        })()))
    })
    .await
    .map_err(process_sqlite_error)?
}
