use crate::*;

/// Read the authoritative lease clock from PostgreSQL.
///
/// Distributed lease decisions must not depend on the wall clock of whichever
/// runtime happens to execute them. `transaction_timestamp()` is stable for the
/// transaction, so every comparison and derived expiry in that transaction is
/// based on one database-owned instant.
pub(crate) async fn postgres_transaction_epoch_ms(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, StoreError> {
    let now: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    u64::try_from(now)
        .map_err(|_| StoreError::Backend(format!("postgres returned invalid epoch millis `{now}`")))
}

pub(crate) fn current_timestamp_string() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}", now.as_secs())
}

pub(crate) fn store_sqlx_error(err: sqlx::Error) -> StoreError {
    if is_contention_error(&err) {
        StoreError::Contended
    } else {
        StoreError::Backend(err.to_string())
    }
}

pub(crate) fn graph_node_insert_error(
    err: sqlx::Error,
    session_id: &str,
    generation: u64,
    node_id: &str,
) -> StoreError {
    if let sqlx::Error::Database(database) = &err
        && database.code().as_deref() == Some("23505")
    {
        match database.constraint() {
            Some("lash_graph_nodes_session_id_generation_key") => {
                return StoreError::GraphGenerationCollision {
                    session_id: session_id.to_string(),
                    generation,
                };
            }
            Some("lash_graph_nodes_pkey") => {
                return StoreError::NodeIdCollision {
                    node_id: node_id.to_string(),
                };
            }
            _ => {}
        }
    }
    store_sqlx_error(err)
}

pub(crate) fn u64_from_sql(
    record_kind: &'static str,
    field: &'static str,
    value: i64,
) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind,
        message: format!("{field} must be non-negative, got {value}"),
    })
}

pub(crate) fn plugin_u64_from_sql(
    record_kind: &'static str,
    field: &'static str,
    value: i64,
) -> Result<u64, PluginError> {
    u64::try_from(value).map_err(|_| PluginError::StoredDataCorrupt {
        record_kind: record_kind.to_string(),
        message: format!("{field} must be non-negative, got {value}"),
    })
}

pub(crate) fn sql_monotonic_counter_value(
    counter: &'static str,
    current: u64,
    next: u64,
) -> Result<i64, StoreError> {
    i64::try_from(next).map_err(|_| StoreError::MonotonicCounterOverflow { counter, current })
}

pub(crate) fn sql_counter_value(counter: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::MonotonicCounterOverflow {
        counter,
        current: value,
    })
}

pub(crate) fn sql_session_lease_generation(value: u64) -> Result<i64, StoreError> {
    sql_counter_value("session_lease_generation", value)
}

pub(crate) fn sql_claim_fencing_tokens(
    counter: &'static str,
    currents: impl IntoIterator<Item = u64>,
) -> Result<Vec<i64>, StoreError> {
    currents
        .into_iter()
        .map(|current| {
            let next = StoreError::checked_monotonic_increment(counter, current)?;
            sql_monotonic_counter_value(counter, current, next)
        })
        .collect()
}

pub(crate) fn plugin_sql_monotonic_counter_value(
    counter: &'static str,
    current: u64,
    value: u64,
) -> Result<i64, PluginError> {
    i64::try_from(value).map_err(|_| PluginError::MonotonicCounterOverflow {
        counter: counter.to_string(),
        current,
    })
}

pub(crate) fn plugin_sql_counter_value(
    counter: &'static str,
    value: u64,
) -> Result<i64, PluginError> {
    i64::try_from(value).map_err(|_| PluginError::MonotonicCounterOverflow {
        counter: counter.to_string(),
        current: value,
    })
}

/// Postgres SQLSTATEs that signal transient write contention rather than a hard
/// failure: serialization failure, deadlock, and lock-acquisition timeout.
/// These mean the transaction can retry its identical commit unchanged.
pub(crate) fn is_contention_error(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| is_contention_sqlstate(&code))
}

fn is_contention_sqlstate(code: &str) -> bool {
    matches!(code, "40001" | "40P01" | "55P03")
}

pub(crate) fn plugin_sqlx_error(err: sqlx::Error) -> PluginError {
    PluginError::Session(err.to_string())
}

pub(crate) fn process_decode_error(err: serde_json::Error) -> PluginError {
    PluginError::Session(format!("failed to decode process registry row: {err}"))
}

pub(crate) fn store_decode_json<T: serde::de::DeserializeOwned>(
    json: &str,
    what: &str,
) -> Result<T, StoreError> {
    serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {what}: {err}")))
}

pub(crate) fn encode_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("persisted state should serialize")
}

fn encode_msgpack<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1024);
    rmp_serde::encode::write_named(&mut buf, value).expect("value should serialize");
    buf
}

pub(crate) fn decode_versioned_msgpack_record<T>(
    bytes: &[u8],
    record_kind: &'static str,
    expected: u32,
) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value: serde_json::Value = rmp_serde::from_slice(bytes)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))?;
    lash_core::store::ensure_supported_record_schema_version(record_kind, &value, expected)?;
    rmp_serde::from_slice(bytes)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))
}

pub(crate) fn block_on_detached<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("postgres manifest runtime")
            .block_on(future)
    })
    .join()
    .expect("postgres manifest thread")
}

async fn put_blob_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    content: &[u8],
) -> Result<BlobRef, StoreError> {
    let hash = format!("{:x}", Sha256::digest(content));
    sqlx::query(
        "INSERT INTO lash_blobs (hash, content)
         VALUES ($1, $2)
         ON CONFLICT (hash) DO NOTHING",
    )
    .bind(&hash)
    .bind(content)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(BlobRef(hash))
}

async fn get_blob_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_ref: &BlobRef,
) -> Result<Option<Vec<u8>>, StoreError> {
    sqlx::query_scalar("SELECT content FROM lash_blobs WHERE hash = $1")
        .bind(blob_ref.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

async fn put_checkpoint_component_tx<T: serde::Serialize>(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    component: &'static str,
    body: Option<&T>,
    existing_ref: Option<&BlobRef>,
) -> Result<Option<BlobRef>, StoreError> {
    if let Some(body) = body {
        return put_blob_tx(tx, &encode_msgpack(body)).await.map(Some);
    }
    let Some(blob_ref) = existing_ref else {
        return Ok(None);
    };
    if get_blob_tx(tx, blob_ref).await?.is_none() {
        return Err(StoreError::CheckpointComponentMissing {
            component,
            blob_ref: blob_ref.clone(),
        });
    }
    Ok(Some(blob_ref.clone()))
}

async fn get_checkpoint_component_tx<T: serde::de::DeserializeOwned>(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    component: &'static str,
    blob_ref: Option<&BlobRef>,
) -> Result<Option<T>, StoreError> {
    let Some(blob_ref) = blob_ref else {
        return Ok(None);
    };
    let bytes =
        get_blob_tx(tx, blob_ref)
            .await?
            .ok_or_else(|| StoreError::CheckpointComponentMissing {
                component,
                blob_ref: blob_ref.clone(),
            })?;
    rmp_serde::from_slice(&bytes)
        .map(Some)
        .map_err(|err| StoreError::Backend(format!("failed to decode {component}: {err}")))
}

pub(crate) async fn put_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint: &HydratedSessionCheckpoint,
) -> Result<(BlobRef, SessionCheckpoint), StoreError> {
    let tool_state_ref = put_checkpoint_component_tx(
        tx,
        "tool-state",
        checkpoint.tool_state.as_ref(),
        checkpoint.tool_state_ref.as_ref(),
    )
    .await?;
    let plugin_snapshot_ref = put_checkpoint_component_tx(
        tx,
        "plugin-snapshot",
        checkpoint.plugin_snapshot.as_ref(),
        checkpoint.plugin_snapshot_ref.as_ref(),
    )
    .await?;
    let execution_state_ref = put_checkpoint_component_tx(
        tx,
        "execution-state",
        checkpoint.execution_state.as_ref(),
        checkpoint.execution_state_ref.as_ref(),
    )
    .await?;
    let manifest = SessionCheckpoint::new(
        checkpoint.turn_state.clone(),
        tool_state_ref,
        plugin_snapshot_ref,
        checkpoint.plugin_snapshot_revision,
        execution_state_ref,
    );
    let bytes = encode_msgpack(&manifest);
    let checkpoint_ref = put_blob_tx(tx, &bytes).await?;
    Ok((checkpoint_ref, manifest))
}

pub(crate) async fn get_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_ref: &BlobRef,
) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
    let bytes = get_blob_tx(tx, blob_ref).await?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let manifest: SessionCheckpoint = decode_versioned_msgpack_record(
        &bytes,
        "SessionCheckpoint",
        lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
    )?;
    let tool_state =
        get_checkpoint_component_tx(tx, "tool-state", manifest.tool_state_ref.as_ref()).await?;
    let plugin_snapshot =
        get_checkpoint_component_tx(tx, "plugin-snapshot", manifest.plugin_snapshot_ref.as_ref())
            .await?;
    let execution_state =
        get_checkpoint_component_tx(tx, "execution-state", manifest.execution_state_ref.as_ref())
            .await?;
    Ok(Some(HydratedSessionCheckpoint {
        turn_state: manifest.turn_state,
        tool_state_ref: manifest.tool_state_ref,
        tool_state,
        plugin_snapshot_ref: manifest.plugin_snapshot_ref,
        plugin_snapshot,
        plugin_snapshot_revision: manifest.plugin_snapshot_revision,
        execution_state_ref: manifest.execution_state_ref,
        execution_state,
    }))
}

pub(crate) async fn load_session_head_meta_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    for_update: bool,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let sql = if for_update {
        "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref
         FROM lash_sessions WHERE session_id = $1 FOR UPDATE"
    } else {
        "SELECT head_json, head_revision, leaf_node_id, checkpoint_ref
         FROM lash_sessions WHERE session_id = $1"
    };
    let row = sqlx::query(sql)
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let head_json: String = row.get(0);
    let head_revision: i64 = row.get(1);
    let leaf_node_id: Option<String> = row.get(2);
    let checkpoint_ref: Option<String> = row.get(3);
    let payload: SessionHeadPayload = lash_core::store::decode_versioned_json_record(
        &head_json,
        "SessionHeadMeta",
        lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
    )?;
    Ok(Some(SessionHeadMeta::assemble(
        payload,
        u64_from_sql("SessionHeadMeta", "head_revision", head_revision)?,
        checkpoint_ref.map(Into::into),
        leaf_node_id,
    )))
}

pub(crate) async fn load_usage_deltas_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
) -> Result<Vec<TokenLedgerEntry>, StoreError> {
    let rows = sqlx::query(
        "SELECT entry_json FROM lash_usage_deltas WHERE session_id = $1 ORDER BY seq ASC",
    )
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            let json: String = row.get(0);
            store_decode_json(&json, "usage delta")
        })
        .collect()
}

pub(crate) async fn load_graph_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    leaf_node_id: Option<String>,
) -> Result<lash_core::SessionGraph, StoreError> {
    let Some(leaf_node_id) = leaf_node_id else {
        return Ok(lash_core::SessionGraph::default());
    };
    let leaf_generation = sqlx::query_scalar::<_, i64>(
        "SELECT generation FROM lash_graph_nodes
         WHERE node_id = $1 AND tombstoned = FALSE",
    )
    .bind(&leaf_node_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?
    .ok_or_else(|| StoreError::StoredDataCorrupt {
        record_kind: "SessionGraph",
        message: format!("leaf `{leaf_node_id}` is missing or tombstoned"),
    })?;
    load_readable_graph_tx(tx, session_id, Some(leaf_generation), Some(leaf_node_id)).await
}

#[cfg(test)]
pub(crate) async fn load_whole_graph_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    leaf_node_id: Option<String>,
) -> Result<lash_core::SessionGraph, StoreError> {
    load_readable_graph_tx(tx, session_id, None, leaf_node_id).await
}

async fn load_readable_graph_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    generation_ceiling: Option<i64>,
    leaf_node_id: Option<String>,
) -> Result<lash_core::SessionGraph, StoreError> {
    let rows = sqlx::query(
        "SELECT node.node_id, node.parent_node_id, node.node_json,
                node.generation, node.frame_node_id
         FROM lash_graph_nodes AS node
         WHERE node.tombstoned = FALSE
           AND ($2::BIGINT IS NULL OR node.generation <= $2)
           AND (
               node.session_id = $1
               OR EXISTS (
                   SELECT 1 FROM lash_fork_lineage AS lineage
                   WHERE lineage.session_id = $1
                     AND lineage.ancestor_session_id = node.session_id
                     AND node.generation <= lineage.fork_generation
               )
           )
         ORDER BY node.generation ASC",
    )
    .bind(session_id)
    .bind(generation_ceiling)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let mut nodes = Vec::<SessionNodeRecord>::new();
    let mut prior_node_id: Option<String> = None;
    let mut expected_generation = 0_i64;
    let mut expected_frame_node_id: Option<String> = None;
    for row in rows {
        let node_id: String = row.get(0);
        let parent_node_id: Option<String> = row.get(1);
        let json: String = row.get(2);
        let generation: i64 = row.get(3);
        let frame_node_id: String = row.get(4);
        if generation != expected_generation || parent_node_id != prior_node_id {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: format!(
                    "generation/parent gap at `{node_id}`: generation {generation}, expected {expected_generation}"
                ),
            });
        }
        let node = SessionNodeRecord::decode_storage_body(node_id.clone(), parent_node_id, &json)
            .map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph node",
            message: error.to_string(),
        })?;
        if matches!(
            node.payload,
            lash_core::SessionNodePayload::FrameOpen { .. }
        ) {
            expected_frame_node_id = Some(node_id.clone());
        }
        if expected_frame_node_id.as_deref() != Some(frame_node_id.as_str()) {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: format!("frame pointer mismatch at `{node_id}`"),
            });
        }
        prior_node_id = Some(node_id);
        expected_generation =
            expected_generation
                .checked_add(1)
                .ok_or_else(|| StoreError::StoredDataCorrupt {
                    record_kind: "SessionGraph",
                    message: "generation overflow".to_string(),
                })?;
        nodes.push(node);
    }
    if let Some(leaf_node_id) = leaf_node_id.as_deref()
        && prior_node_id.as_deref() != Some(leaf_node_id)
    {
        return Err(StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph",
            message: format!("readable path does not end at leaf `{leaf_node_id}`"),
        });
    }
    lash_core::SessionGraph::from_nodes(nodes, leaf_node_id).map_err(|error| {
        StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph",
            message: error.to_string(),
        }
    })
}

pub(crate) async fn commit_attachment_refs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &str,
    attachment_ids: &[AttachmentId],
    now_epoch_ms: u64,
) -> Result<(), StoreError> {
    if attachment_ids.is_empty() {
        return Ok(());
    }
    for id in attachment_ids {
        sqlx::query(
            "UPDATE lash_attachment_manifest
             SET committed_at_ms = COALESCE(committed_at_ms, $1)
             WHERE attachment_id = $2 AND session_id = $3",
        )
        .bind(now_epoch_ms as i64)
        .bind(id.as_str())
        .bind(session_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod contention_tests {
    use super::is_contention_sqlstate;

    #[test]
    fn only_retry_unchanged_sqlstates_are_contention() {
        for code in ["40001", "40P01", "55P03"] {
            assert!(is_contention_sqlstate(code), "{code}");
        }
        for code in ["23505", "57014", "08006"] {
            assert!(!is_contention_sqlstate(code), "{code}");
        }
    }
}
