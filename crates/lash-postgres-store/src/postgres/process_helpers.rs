fn process_status_label(record: &ProcessRecord) -> &'static str {
    record.status.label()
}

async fn load_process_tx(
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

async fn load_process(
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

async fn save_process_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &ProcessRecord,
) -> Result<(), PluginError> {
    let change_seq = next_process_change_seq_tx(tx).await?;
    sqlx::query(
        "UPDATE lash_processes
         SET updated_at_ms = $2, change_seq = $3, status = $4, record_json = $5
         WHERE process_id = $1",
    )
    .bind(&record.id)
    .bind(record.updated_at_ms as i64)
    .bind(change_seq as i64)
    .bind(process_status_label(record))
    .bind(serde_json::to_string(record).map_err(process_decode_error)?)
    .execute(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(())
}

async fn next_process_change_seq_tx(
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

async fn load_event_by_key_tx(
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

async fn append_process_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &mut ProcessRecord,
    request: ProcessEventAppendRequest,
    occurred_at_ms: u64,
) -> Result<ProcessEventAppendResult, PluginError> {
    let process_id = record.id.clone();
    let replay_lookup =
        if let Some(replay_key) = request.replay.as_ref().map(|replay| replay.key.as_str()) {
            load_event_by_key_tx(tx, &process_id, replay_key).await?
        } else {
            None
        };
    let sequence = next_process_event_sequence_tx(tx, &process_id).await?;
    let prepared = lash_core::runtime::prepare_process_event_append(
        record,
        request,
        sequence,
        replay_lookup,
        occurred_at_ms,
    )?;
    match prepared {
        lash_core::ProcessEventAppendPlan::Replay {
            event,
            repair_record,
            wake_delivery,
            ..
        } => {
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
            Ok(ProcessEventAppendResult {
                event,
                wake_delivery,
            })
        }
    }
}

async fn load_process_lease_tx(
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
    let liveness_json: Option<String> = row.get(6);
    let (Some(owner_id), Some(lease_token)) = (owner_id, lease_token) else {
        return Ok(None);
    };
    Ok(Some(ProcessLease {
        schema_version: PROCESS_LEASE_SCHEMA_VERSION,
        process_id: process_id.to_string(),
        owner: LeaseOwnerIdentity {
            incarnation_id: incarnation_id.unwrap_or_else(|| owner_id.clone()),
            owner_id,
            liveness: liveness_json
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or(LeaseOwnerLiveness::Opaque),
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
async fn process_lease_now_epoch_ms_tx(
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
async fn acquire_process_lease_tx(
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
    .bind(encode_process_lease_liveness(&lease.owner.liveness)?)
    .bind(&lease.lease_token)
    .bind(lease.fencing_token as i64)
    .bind(lease.claimed_at_epoch_ms as i64)
    .bind(lease.expires_at_epoch_ms as i64)
    .execute(&mut **tx)
    .await
    .map_err(plugin_sqlx_error)?;
    Ok(lease)
}

async fn retained_process_lease_fencing_token(
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

fn encode_process_lease_liveness(liveness: &LeaseOwnerLiveness) -> Result<String, PluginError> {
    serde_json::to_string(liveness).map_err(|err| {
        PluginError::Session(format!("failed to encode process lease liveness: {err}"))
    })
}

fn process_lease_expired(process_id: &str) -> PluginError {
    PluginError::ProcessLeaseSuperseded {
        process_id: process_id.to_string(),
    }
}

async fn validate_process_execution_authority_tx(
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
                authority.validate_invocation_for_start(process_id, started)
            } else {
                authority.validate_invocation_for_write(process_id, record)
            }
        }
        ProcessExecutionWriteAuthority::Testing { .. } => {
            if let Some(started) = start {
                authority.validate_invocation_for_start(process_id, started)
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

fn guard_lease(current: Option<&ProcessLease>, lease_token: &str, now: u64) -> bool {
    current
        .map(|current| current.lease_token == lease_token && current.expires_at_epoch_ms > now)
        .unwrap_or(false)
}

async fn list_grants_for_scope(
    pool: &PgPool,
    session_scope: &SessionScope,
    live_only: bool,
) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
    let status_clause = if live_only {
        "AND p.status = 'running'"
    } else {
        ""
    };
    let sql = format!(
        "SELECT g.process_id, g.descriptor_json, p.record_json
         FROM lash_process_handle_grants g
         JOIN lash_processes p ON p.process_id = g.process_id
         WHERE g.scope_id = $1 {status_clause}
         ORDER BY g.process_id ASC"
    );
    let rows = sqlx::query(&sql)
        .bind(session_scope.id().as_str())
        .fetch_all(pool)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut entries = Vec::new();
    for row in rows {
        let process_id: String = row.get(0);
        let descriptor_json: String = row.get(1);
        let record_json: String = row.get(2);
        let descriptor: ProcessHandleDescriptor =
            serde_json::from_str(&descriptor_json).map_err(process_decode_error)?;
        let record: ProcessRecord =
            serde_json::from_str(&record_json).map_err(process_decode_error)?;
        entries.push((
            ProcessHandleGrant {
                session_id: session_scope.session_id.clone(),
                process_id,
                descriptor,
            },
            record,
        ));
    }
    Ok(entries)
}
