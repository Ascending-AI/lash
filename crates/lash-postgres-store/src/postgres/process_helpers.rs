use crate::*;

pub(crate) fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status.label()
}

pub(crate) async fn load_process_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> = sqlx::query_scalar(
        "SELECT record_json
             FROM lash_processes
             WHERE process_id = $1
             FOR UPDATE",
    )
    .bind(process_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
        .transpose()
}

pub(crate) async fn load_process(
    pool: &PgPool,
    process_id: &str,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: Option<String> =
        sqlx::query_scalar("SELECT record_json FROM lash_processes WHERE process_id = $1")
            .bind(process_id)
            .fetch_optional(pool)
            .await
            .map_err(plugin_sqlx_error)?;
    json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
        .transpose()
}

pub(crate) fn decode_matching_process(
    row: sqlx::postgres::PgRow,
    filter: &lash_core::ProcessListFilter,
) -> Result<Option<ProcessRecord>, PluginError> {
    let json: String = row.get(0);
    let record = serde_json::from_str(&json).map_err(process_decode_error)?;
    // JSONB normalizes numeric representations (`1` equals `1.0`).
    // SQL is the coarse pushdown; this is the exact public Value contract.
    Ok(filter.matches_record(&record).then_some(record))
}

pub(crate) async fn wake_session_id_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
) -> Result<Option<String>, PluginError> {
    sqlx::query_scalar("SELECT wake_session_id FROM lash_processes WHERE process_id = $1")
        .bind(process_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(plugin_sqlx_error)
}

pub(crate) async fn save_process_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &ProcessRecord,
) -> Result<(), PluginError> {
    let change_seq = next_process_change_seq_tx(tx).await?;
    sqlx::query(
        "UPDATE lash_processes
         SET updated_at_ms = $2, change_seq = $3, status = $4,
             is_waiting = $5, record_json = $6
         WHERE process_id = $1",
    )
    .bind(&record.id)
    .bind(record.updated_at_ms as i64)
    .bind(change_seq as i64)
    .bind(process_status_label(record))
    .bind(record.wait.is_some())
    .bind(serde_json::to_string(record).map_err(process_decode_error)?)
    .execute(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(())
}

pub(crate) async fn next_process_change_seq_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, PluginError> {
    let seq: i64 = sqlx::query_scalar(
        "UPDATE lash_process_change_clock
         SET current_seq = current_seq + 1
         WHERE singleton = TRUE
         RETURNING current_seq",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(seq as u64)
}

pub(crate) async fn load_event_by_key_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
    replay_key: &str,
) -> Result<Option<(String, ProcessEvent)>, PluginError> {
    let row = sqlx::query(
        "SELECT payload_hash, event_json
         FROM lash_process_events
         WHERE process_id = $1 AND idempotency_key = $2",
    )
    .bind(process_id)
    .bind(replay_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    row.map(|row| {
        let hash: String = row.get(0);
        let json: String = row.get(1);
        serde_json::from_str(&json)
            .map(|event| (hash, event))
            .map_err(process_decode_error)
    })
    .transpose()
}

async fn next_process_event_sequence_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
) -> Result<u64, PluginError> {
    let sequence: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(sequence), 0) + 1
         FROM lash_process_events
         WHERE process_id = $1",
    )
    .bind(process_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(sequence as u64)
}

pub(crate) async fn append_process_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
    wake_delivery_config: lash_core::WakeDeliveryConfig,
) -> Result<ProcessEventAppendResult, PluginError> {
    let process_id = record.id.clone();
    let replay_lookup =
        if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str()) {
            load_event_by_key_tx(tx, &process_id, replay_key).await?
        } else {
            None
        };
    let sequence = next_process_event_sequence_tx(tx, &process_id).await?;
    let wake_session_id = wake_session_id_tx(tx, &process_id).await?;
    let prepared = lash_core::runtime::prepare_process_event_append(
        record,
        request,
        sequence,
        replay_lookup,
        occurred_at_ms,
        wake_session_id.as_deref(),
    )?;
    match prepared {
        lash_core::ProcessEventAppendPlan::Replay {
            event,
            repair_record,
            wake_delivery,
            ..
        } => {
            insert_wake_delivery_tx(tx, wake_delivery.as_ref(), wake_delivery_config).await?;
            if let Some(repaired) = repair_record {
                *record = repaired;
                save_process_tx(tx, record).await?;
            }
            Ok(ProcessEventAppendResult {
                event,
                wake_delivery,
            })
        }
        lash_core::ProcessEventAppendPlan::Insert {
            event,
            payload_hash,
            projected_record,
            wake_delivery,
            occurred_at_ms,
        } => {
            sqlx::query(
                "INSERT INTO lash_process_events (
                    process_id, sequence, event_type, payload_hash, idempotency_key,
                    occurred_at_ms, event_json
                 )
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(&process_id)
            .bind(sequence as i64)
            .bind(event.event_type.as_str())
            .bind(&payload_hash)
            .bind(event.invocation.replay_key())
            .bind(occurred_at_ms as i64)
            .bind(serde_json::to_string(&event).map_err(process_decode_error)?)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
            *record = projected_record;
            save_process_tx(tx, record).await?;
            insert_wake_delivery_tx(tx, wake_delivery.as_ref(), wake_delivery_config).await?;
            Ok(ProcessEventAppendResult {
                event,
                wake_delivery,
            })
        }
    }
}

pub(crate) async fn insert_wake_delivery_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    wake: Option<&lash_core::ProcessWakeDelivery>,
    config: lash_core::WakeDeliveryConfig,
) -> Result<(), PluginError> {
    let Some(wake) = wake else {
        return Ok(());
    };
    let delivery = lash_core::WakeDelivery::pending(wake.clone(), config)?;
    sqlx::query(
        "INSERT INTO lash_process_wake_deliveries (
            delivery_id, process_id, target_session_id, sequence, state,
            claim_token, attempts, first_attempt_ms, next_attempt_at_ms, expires_at_ms,
            discard_reason, delivery_json
         ) VALUES ($1, $2, $3, $4, 'pending', NULL, 0, NULL, $5, $6, NULL, $7)
         ON CONFLICT (delivery_id) DO NOTHING",
    )
    .bind(&delivery.delivery_id)
    .bind(&delivery.wake.process_id)
    .bind(&delivery.wake.target_session_id)
    .bind(delivery.wake.sequence as i64)
    .bind(delivery.next_attempt_at_ms as i64)
    .bind(delivery.expires_at_ms as i64)
    .bind(serde_json::to_string(&delivery.wake).map_err(process_decode_error)?)
    .execute(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(())
}

pub(crate) async fn load_process_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
) -> Result<Option<ProcessLease>, PluginError> {
    let row = sqlx::query(
        "SELECT lease_owner_id, lease_token, lease_fencing_token,
                lease_claimed_at_ms, lease_expires_at_ms,
                lease_owner_incarnation_id, lease_owner_liveness_json
         FROM lash_process_leases
         WHERE process_id = $1
         FOR UPDATE",
    )
    .bind(process_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let owner_id: Option<String> = row.get(0);
    let lease_token: Option<String> = row.get(1);
    let incarnation_id: Option<String> = row.get(5);
    let (Some(owner_id), Some(lease_token)) = (owner_id, lease_token) else {
        return Ok(None);
    };
    Ok(Some(ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: process_id.to_string(),
        owner: LeaseOwnerIdentity {
            incarnation_id: incarnation_id.unwrap_or_else(|| owner_id.clone()),
            owner_id,
        },
        lease_token,
        fencing_token: row.get::<i64, _>(2) as u64,
        claimed_at_epoch_ms: row.get::<i64, _>(3) as u64,
        expires_at_epoch_ms: row.get::<i64, _>(4) as u64,
    }))
}

/// One authoritative wall-clock sample for every process-lease transaction.
/// Using the database clock prevents worker clock skew from stealing or
/// spuriously expiring a lease in multi-host Postgres deployments.
pub(crate) async fn process_lease_now_epoch_ms_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, PluginError> {
    let now: i64 =
        sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT")
            .fetch_one(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
    Ok(now.max(0) as u64)
}

/// Insert-or-replace the persisted lease row for `process_id` with a fresh
/// lease owned by `owner` at `fencing_token`.
pub(crate) async fn acquire_process_lease_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
    owner: &LeaseOwnerIdentity,
    fencing_token: u64,
    now: u64,
    lease_ttl_ms: u64,
) -> Result<ProcessLease, PluginError> {
    let lease = ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: process_id.to_string(),
        owner: owner.clone(),
        lease_token: format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{process_id}:{}:{}:{now}:{fencing_token}",
                    owner.owner_id, owner.incarnation_id
                )
                .as_bytes()
            )
        ),
        fencing_token,
        claimed_at_epoch_ms: now,
        expires_at_epoch_ms: now.saturating_add(lease_ttl_ms),
    };
    sqlx::query(
        "INSERT INTO lash_process_leases (
            process_id, lease_owner_id, lease_owner_incarnation_id,
            lease_owner_liveness_json, lease_token, lease_fencing_token,
            lease_claimed_at_ms, lease_expires_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (process_id) DO UPDATE SET
            lease_owner_id = EXCLUDED.lease_owner_id,
            lease_owner_incarnation_id = EXCLUDED.lease_owner_incarnation_id,
            lease_owner_liveness_json = EXCLUDED.lease_owner_liveness_json,
            lease_token = EXCLUDED.lease_token,
            lease_fencing_token = EXCLUDED.lease_fencing_token,
            lease_claimed_at_ms = EXCLUDED.lease_claimed_at_ms,
            lease_expires_at_ms = EXCLUDED.lease_expires_at_ms",
    )
    .bind(&lease.process_id)
    .bind(&lease.owner.owner_id)
    .bind(&lease.owner.incarnation_id)
    .bind(Option::<&str>::None)
    .bind(&lease.lease_token)
    .bind(lease.fencing_token as i64)
    .bind(lease.claimed_at_epoch_ms as i64)
    .bind(lease.expires_at_epoch_ms as i64)
    .execute(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(lease)
}

pub(crate) async fn retained_process_lease_fencing_token(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
) -> Result<u64, PluginError> {
    let existing_fence: Option<i64> = sqlx::query_scalar(
        "SELECT lease_fencing_token FROM lash_process_leases WHERE process_id = $1 FOR UPDATE",
    )
    .bind(process_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(existing_fence.unwrap_or(0) as u64)
}

pub(crate) fn process_lease_expired(process_id: &str) -> PluginError {
    PluginError::ProcessLeaseSuperseded {
        process_id: process_id.to_string(),
    }
}

pub(crate) async fn validate_process_execution_authority_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    process_id: &str,
    record: &ProcessRecord,
    authority: &ProcessExecutionWriteAuthority,
    start: Option<&ProcessStarted>,
    now: u64,
) -> Result<(), PluginError> {
    match authority {
        ProcessExecutionWriteAuthority::Invocation { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(
                    process_id,
                    started,
                    record.first_started.as_deref(),
                )
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Lease(lease) => {
            if lease.process_id != process_id {
                return Err(process_lease_expired(process_id));
            }
            let current = load_process_lease_tx(tx, process_id).await?;
            if guard_lease(current.as_ref(), &lease.lease_token, now)
                && current.as_ref().is_some_and(|current| {
                    current.owner.same_incarnation(&lease.owner)
                        && current.fencing_token == lease.fencing_token
                })
            {
                Ok(())
            } else {
                Err(process_lease_expired(process_id))
            }
        }
    }
}

pub(crate) fn guard_lease(current: Option<&ProcessLease>, lease_token: &str, now: u64) -> bool {
    current
        .map(|current| current.lease_token == lease_token && current.expires_at_epoch_ms > now)
        .unwrap_or(false)
}
