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

/// Clamps an epoch-milliseconds bound to the `i64` range of the SQL time columns.
///
/// Every stored `*_at_ms` column is a `BIGINT`, so a `u64` bound above
/// `i64::MAX` is outside the representable range. Saturating keeps SQL
/// comparisons ordered the way the in-memory predicates order them; a raw
/// `as i64` cast wraps (`u64::MAX as i64 == -1`) and inverts every comparison,
/// so a host-supplied huge cutoff would silently select the opposite row set
/// from the in-memory backend.
///
/// Bounds are compared against stored `i64` timestamps, so saturating is exact
/// for every timestamp below `i64::MAX`.
pub(crate) fn clamp_epoch_ms(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(crate) fn store_sqlx_error(err: sqlx::Error) -> StoreError {
    if is_contention_error(&err) {
        StoreError::Contended
    } else {
        StoreError::StorageFailure {
            backend: "postgres",
            message: err.to_string(),
        }
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

/// Rebuild a stored attachment id, refusing a row that no longer satisfies the
/// id rule. A malformed stored id is corrupt data, not an id: it must surface
/// as a read failure rather than travel on as a well-formed-looking value.
pub(crate) fn attachment_id_from_sql(
    record_kind: &'static str,
    field: &'static str,
    value: String,
) -> Result<AttachmentId, StoreError> {
    AttachmentId::parse(&value).map_err(|err| StoreError::StoredDataCorrupt {
        record_kind,
        message: format!("{field} is not a valid attachment id: {err}"),
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

pub(crate) fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::RecordEncodingFailed {
        record_kind: "persisted JSON record".to_string(),
        message: error.to_string(),
    })
}

fn encode_msgpack<T: serde::Serialize>(
    value: &T,
    record_kind: &str,
) -> Result<Vec<u8>, StoreError> {
    let mut buf = Vec::with_capacity(1024);
    rmp_serde::encode::write_named(&mut buf, value).map_err(|error| {
        StoreError::RecordEncodingFailed {
            record_kind: record_kind.to_string(),
            message: error.to_string(),
        }
    })?;
    Ok(buf)
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

// One array bind avoids PostgreSQL's scalar-parameter ceiling. A 16,384-ref
// chunk is four times the largest required depth while bounding each encoded
// request to roughly one MiB of SHA-256 text plus array framing.
const CHECKPOINT_COMPONENT_REF_CHUNK_SIZE: usize = 16_384;

async fn existing_checkpoint_component_refs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_refs: &std::collections::BTreeSet<String>,
) -> Result<std::collections::HashSet<String>, StoreError> {
    let mut existing = std::collections::HashSet::with_capacity(blob_refs.len());
    let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
    for chunk in blob_refs.chunks(CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT hash FROM lash_blobs WHERE hash = ANY($1::text[])",
        )
        .bind(chunk)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        existing.extend(rows);
    }
    Ok(existing)
}

async fn checkpoint_component_bodies_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    manifest: &SessionCheckpoint,
) -> Result<std::collections::HashMap<String, Vec<u8>>, StoreError> {
    let blob_refs = manifest
        .components
        .values()
        .map(|descriptor| descriptor.blob_ref.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let mut bodies = std::collections::HashMap::with_capacity(blob_refs.len());
    let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
    for chunk in blob_refs.chunks(CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
        let rows = sqlx::query("SELECT hash, content FROM lash_blobs WHERE hash = ANY($1::text[])")
            .bind(chunk)
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        for row in rows {
            bodies.insert(row.get::<String, _>(0), row.get::<Vec<u8>, _>(1));
        }
    }
    Ok(bodies)
}

async fn validate_checkpoint_component_refs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint: &HydratedSessionCheckpoint,
) -> Result<(), StoreError> {
    let mut referenced = std::collections::BTreeSet::new();
    for (key, component) in &checkpoint.components {
        lash_core::store::ensure_checkpoint_component_encoding_version(
            key,
            component.encoding_version(),
        )?;
        let Some(blob_ref) = component.blob_ref().filter(|_| component.body().is_none()) else {
            continue;
        };
        referenced.insert(blob_ref.as_str().to_string());
    }
    let existing = existing_checkpoint_component_refs_tx(tx, &referenced).await?;
    for (key, component) in &checkpoint.components {
        let Some(blob_ref) = component.blob_ref().filter(|_| component.body().is_none()) else {
            continue;
        };
        if !existing.contains(blob_ref.as_str()) {
            return Err(StoreError::CheckpointComponentMissing {
                key: key.clone(),
                blob_ref: blob_ref.clone(),
            });
        }
    }
    Ok(())
}

/// Persist the complete checkpoint root and every changed leaf inside the
/// caller's commit transaction. Keeping both writes under the same transaction
/// is the GC-safety argument: no collector can observe a git-loose-object-style
/// leaf that is not yet reachable from its root, or a root whose leaf is absent.
pub(crate) async fn put_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint: &HydratedSessionCheckpoint,
) -> Result<(BlobRef, SessionCheckpoint), StoreError> {
    let manifest = checkpoint.manifest()?;
    validate_checkpoint_component_refs_tx(tx, checkpoint).await?;
    for (key, descriptor) in &manifest.components {
        let component =
            checkpoint
                .components
                .get(key)
                .ok_or_else(|| StoreError::StoredDataCorrupt {
                    record_kind: "HydratedSessionCheckpoint",
                    message: format!("manifest projection lost component `{key}`"),
                })?;
        if let Some(body) = component.body() {
            let stored_ref = put_blob_tx(tx, body).await?;
            lash_core::store::ensure_checkpoint_component_hash_agreement(
                key,
                &stored_ref,
                &descriptor.blob_ref,
            )?;
        }
    }
    let bytes = encode_msgpack(&manifest, "checkpoint root")?;
    let checkpoint_ref = put_blob_tx(tx, &bytes).await?;
    let component_refs = manifest
        .components
        .values()
        .map(|descriptor| descriptor.blob_ref.as_str())
        .collect::<Vec<_>>();
    sqlx::query(
        "INSERT INTO lash_checkpoint_blob_refs (checkpoint_ref, blob_ref)
         SELECT $1, component_ref
           FROM unnest($2::text[]) AS component_ref
         ON CONFLICT (checkpoint_ref, blob_ref) DO NOTHING",
    )
    .bind(checkpoint_ref.as_str())
    .bind(component_refs)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
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
    manifest.validate_component_encoding_versions()?;
    let bodies = checkpoint_component_bodies_tx(tx, &manifest).await?;
    let mut components = std::collections::BTreeMap::new();
    for (key, descriptor) in &manifest.components {
        let bytes = bodies
            .get(descriptor.blob_ref.as_str())
            .cloned()
            .ok_or_else(|| StoreError::CheckpointComponentMissing {
                key: key.clone(),
                blob_ref: descriptor.blob_ref.clone(),
            })?;
        components.insert(
            key.clone(),
            lash_core::HydratedCheckpointComponent::hydrated(descriptor.clone(), bytes),
        );
    }
    Ok(Some(HydratedSessionCheckpoint {
        turn_state: manifest.turn_state,
        components,
        plugin_snapshot_revision: manifest.plugin_snapshot_revision,
    }))
}

#[cfg(test)]
pub(crate) async fn count_checkpoint_data_statements<F: std::future::Future>(
    stats_pool: &sqlx::postgres::PgPool,
    future: F,
) -> (F::Output, usize) {
    sqlx::query("SELECT pg_stat_statements_reset()")
        .execute(stats_pool)
        .await
        .expect("reset PostgreSQL statement statistics");

    let output = future.await;
    let calls = sqlx::query_scalar::<_, i64>(
        "SELECT calls
         FROM pg_stat_statements
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND query NOT LIKE '%pg_stat_statements%'",
    )
    .fetch_all(stats_pool)
    .await
    .expect("read PostgreSQL statement statistics");
    let count = calls
        .into_iter()
        .try_fold(0usize, |total, calls| {
            let calls =
                usize::try_from(calls).expect("PostgreSQL statement calls are non-negative");
            total
                .checked_add(calls)
                .ok_or("PostgreSQL statement count fits usize")
        })
        .expect("PostgreSQL statement count fits usize");
    (output, count)
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
    decode_session_head_meta_row(row)
}

fn decode_session_head_meta_row(
    row: Option<sqlx::postgres::PgRow>,
) -> Result<Option<SessionHeadMeta>, StoreError> {
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
    use super::{is_contention_sqlstate, store_sqlx_error};
    use crate::StoreError;

    #[test]
    fn only_retry_unchanged_sqlstates_are_contention() {
        for code in ["40001", "40P01", "55P03"] {
            assert!(is_contention_sqlstate(code), "{code}");
        }
        for code in ["23505", "57014", "08006"] {
            assert!(!is_contention_sqlstate(code), "{code}");
        }
    }

    #[test]
    fn non_contention_sqlx_errors_are_typed_postgres_storage_failures() {
        let error = store_sqlx_error(sqlx::Error::Protocol("broken wire frame".to_string()));

        assert!(matches!(
            error,
            StoreError::StorageFailure {
                backend: "postgres",
                ref message,
            } if message.contains("broken wire frame")
        ));
    }
}
