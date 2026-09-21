use super::*;

use crate::process_sql::process_sql;

pub(super) async fn claim_pending_wake_deliveries(
    registry: &PostgresProcessRegistry,
    limit: usize,
) -> Result<Vec<lash_core::WakeDelivery>, PluginError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let now = registry.clock.timestamp_ms() as i64;
    sqlx::query(process_sql().wake.reclaim_lapsed_claims.sql())
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    let ids = sqlx::query_scalar::<_, String>(process_sql().wake_postgres.select_claimable.sql())
        .bind(limit as i64)
        .bind(now)
        .bind(lash_core::WakeDiscardReason::NON_BLOCKING_ORDERING_GROUP_LABELS)
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut deliveries = Vec::with_capacity(ids.len());
    for id in ids {
        let claim_token = uuid::Uuid::new_v4().to_string();
        sqlx::query(process_sql().wake.start_enqueuing.sql())
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
    let row = sqlx::query(process_sql().wake_postgres.select_report.sql())
        .bind(delivery_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)?
        .ok_or_else(|| registry_transitions::unknown_wake_delivery(delivery_id))?;
    decode_wake_delivery_row(row)
}

pub(super) fn decode_wake_delivery_row(
    row: sqlx::postgres::PgRow,
) -> Result<lash_core::WakeDelivery, PluginError> {
    registry_transitions::WakeDeliveryRow {
        delivery_id: row.get(0),
        state_label: row.get(1),
        claim_token: row.get(2),
        attempts: row.get(3),
        first_attempt_ms: row.get(4),
        next_attempt_at_ms: row.get(5),
        expires_at_ms: row.get(6),
        discard_reason_label: row.get(7),
        delivery_json: row.get(8),
    }
    .project()
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
    disposition: lash_core::WakeDeliveryDisposition,
) -> Result<lash_core::WakeDeliveryClaimOutcome, PluginError> {
    let state = disposition.state();
    let reason = disposition.discard_reason();
    let changed = sqlx::query(process_sql().wake.settle_claim.sql())
        .bind(delivery_id)
        .bind(claim_token)
        .bind(state.as_str())
        .bind(reason.map(lash_core::WakeDiscardReason::as_str))
        .execute(pool)
        .await
        .map_err(plugin_sqlx_error)?
        .rows_affected();
    // The statement's own predicate is the fence (`state` is enqueuing and the
    // claim token still matches), so exactly one row must change; the shared
    // backstop records the disagreement if not and the existing branch
    // classifies the loss.
    if !lash_core::store_backend_support::fenced_write_applied(
        lash_core::store_backend_support::FencedWrite::WakeDeliverySettlement,
        crate::POSTGRES_BACKEND,
        delivery_id,
        changed,
    ) {
        let current: Option<String> = sqlx::query_scalar(process_sql().wake.select_state.sql())
            .bind(delivery_id)
            .fetch_optional(pool)
            .await
            .map_err(plugin_sqlx_error)?;
        let current =
            current.ok_or_else(|| registry_transitions::unknown_wake_delivery(delivery_id))?;
        let state = registry_transitions::wake_delivery_state_from_label(delivery_id, &current)?;
        return Ok(lash_core::WakeDeliveryClaimOutcome::ClaimLost { state });
    }
    Ok(lash_core::WakeDeliveryClaimOutcome::Applied)
}
