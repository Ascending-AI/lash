use super::*;

pub(super) fn load_wake_delivery_conn(
    conn: &Connection,
    delivery_id: &str,
) -> Result<lash_core_execution::WakeDelivery, lash_core_execution::PluginError> {
    let row = conn
        .query_row(
            process_sql().wake_sqlite.select_report.sql(),
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
        .ok_or_else(|| registry_transitions::unknown_wake_delivery(delivery_id))?;
    registry_transitions::WakeDeliveryRow {
        delivery_id: delivery_id.to_string(),
        state_label: row.0,
        claim_token: row.1,
        attempts: row.2,
        first_attempt_ms: row.3,
        next_attempt_at_ms: row.4,
        expires_at_ms: row.5,
        discard_reason_label: row.6,
        delivery_json: row.7,
    }
    .project()
}

pub(super) fn wake_delivery_report<'a>(
    deliveries: impl IntoIterator<Item = &'a lash_core_execution::WakeDelivery>,
) -> lash_core_execution::WakeDeliveryReport {
    lash_core_execution::WakeDeliveryReport::from_deliveries(deliveries)
}

pub(super) async fn update_wake_delivery_state(
    conn: &SqliteConnection,
    delivery_id: &str,
    claim_token: &str,
    disposition: lash_core_execution::WakeDeliveryDisposition,
) -> Result<lash_core_execution::WakeDeliveryClaimOutcome, lash_core_execution::PluginError> {
    let state = disposition.state();
    let reason = disposition.discard_reason();
    let delivery_id = delivery_id.to_string();
    let claim_token = claim_token.to_string();
    conn.write_flow(move |tx| {
        Ok(tx_outcome((|| {
            let changed = tx
                .execute(
                    process_sql().wake.settle_claim.sql(),
                    params![
                        delivery_id,
                        claim_token,
                        state.as_str(),
                        reason.map(lash_core_execution::WakeDiscardReason::as_str)
                    ],
                )
                .map_err(process_sqlite_error)?;
            // The statement's own predicate is the fence (`state` is
            // enqueuing and the claim token still matches), so exactly one row
            // must change; the shared backstop records the disagreement if not
            // and the existing branch classifies the loss.
            if !lash_core_execution::store_backend_support::fenced_write_applied(
                lash_core_execution::store_backend_support::FencedWrite::WakeDeliverySettlement,
                crate::SQLITE_BACKEND,
                &delivery_id,
                u64::try_from(changed).unwrap_or(u64::MAX),
            ) {
                let current = tx
                    .query_row(
                        process_sql().wake.select_state.sql(),
                        params![delivery_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(process_sqlite_error)?
                    .ok_or_else(|| registry_transitions::unknown_wake_delivery(&delivery_id))?;
                let state =
                    registry_transitions::wake_delivery_state_from_label(&delivery_id, &current)?;
                return Ok(lash_core_execution::WakeDeliveryClaimOutcome::ClaimLost { state });
            }
            Ok(lash_core_execution::WakeDeliveryClaimOutcome::Applied)
        })()))
    })
    .await
    .map_err(process_sqlite_error)?
}
