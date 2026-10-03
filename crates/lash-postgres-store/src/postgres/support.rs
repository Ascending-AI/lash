use crate::session_sql::session_sql;
use crate::*;
use lash_sansio::SessionId;

/// Read the authoritative lease clock from PostgreSQL.
///
/// Distributed lease decisions must not depend on the wall clock of whichever
/// runtime happens to execute them. `transaction_timestamp()` is stable for the
/// transaction, so every comparison and derived expiry in that transaction is
/// based on one database-owned instant.
pub(crate) async fn postgres_transaction_epoch_ms(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64, StoreError> {
    #[cfg(any(test, feature = "testing"))]
    {
        let injected: Option<String> = sqlx::query_scalar(
            crate::connection_sql::connection_sql()
                .select_injected_lease_epoch_ms
                .sql(),
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if let Some(injected) = injected {
            return injected
                .parse()
                .map_err(|error| StoreError::Backend(format!("invalid test lease time: {error}")));
        }
    }
    let now: i64 = sqlx::query_scalar(
        crate::connection_sql::connection_sql()
            .select_transaction_epoch_ms
            .sql(),
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

/// Clamps a caller-supplied sequence, rank, or offset bound to the `i64` range
/// of the SQL sequence columns.
///
/// Every stored sequence is a signed 64-bit integer, so a bound at or above
/// `i64::MAX` already means "through every stored row". Saturating keeps that
/// meaning for the rest of the unsigned range; a raw `as i64` cast wraps
/// (`u64::MAX as i64 == -1`), so a "through the end" count counted nothing and
/// an offset past the end read the first row (FIG-3601).
pub(crate) fn clamp_sequence_bound(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
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
    session_id: &SessionId,
    generation: u64,
    node_id: &lash_core_execution::NodeId,
) -> StoreError {
    if let sqlx::Error::Database(database) = &err
        && database.code().as_deref() == Some("23505")
    {
        match database.constraint() {
            Some("lash_graph_nodes_session_id_generation_key") => {
                return StoreError::GraphGenerationCollision {
                    session_id: session_id.clone(),
                    generation,
                };
            }
            Some("lash_graph_nodes_pkey") => {
                return StoreError::NodeIdCollision {
                    node_id: node_id.clone(),
                };
            }
            _ => {}
        }
    }
    store_sqlx_error(err)
}

/// The `pending_turn_inputs.input_id` column is globally `UNIQUE`: a draft
/// naming an id any row already carries fails the insert, and that violation
/// is the typed id-conflict refusal, not an opaque storage failure. The
/// `(session_id, source_key)` unique constraint surfaces the same way: the
/// admission verdict reads both names first, so reaching the constraint means
/// the row materialized after that read — a race or a skipped verdict — and
/// the refused draft's provisioned id is the identity that cannot be filed.
/// SQLite's seam maps both violations to the same refusal.
pub(crate) fn pending_turn_input_insert_error(
    err: sqlx::Error,
    session_id: &SessionId,
    input_id: &lash_core_execution::InputId,
) -> StoreError {
    if let sqlx::Error::Database(database) = &err
        && database.code().as_deref() == Some("23505")
        && matches!(
            database.constraint(),
            Some("lash_pending_turn_inputs_input_id_key")
                | Some("lash_pending_turn_inputs_session_id_source_key_key")
        )
    {
        return StoreError::PendingTurnInputIdConflict {
            session_id: session_id.clone(),
            input_id: input_id.clone(),
        };
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
    PluginError::from(store_sqlx_error(err))
}

/// A store refusal met by a registry-facing write, the writer fence's
/// included, as the registry's error.
pub(crate) fn plugin_store_error(err: StoreError) -> PluginError {
    PluginError::from(err)
}

pub(crate) fn process_decode_error(err: serde_json::Error) -> PluginError {
    PluginError::Session(format!("failed to decode process registry row: {err}"))
}

pub(crate) fn store_decode_json<T: serde::de::DeserializeOwned>(
    json: &str,
    record_kind: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(json).map_err(|err| StoreError::StoredDataCorrupt {
        record_kind,
        message: format!("failed to decode {record_kind}: {err}"),
    })
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

async fn put_checkpoint_blobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blobs: &std::collections::BTreeMap<String, std::sync::Arc<[u8]>>,
) -> Result<(), StoreError> {
    let blobs = blobs.iter().collect::<Vec<_>>();
    for chunk in blobs.chunks(CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
        let hashes = chunk
            .iter()
            .map(|(hash, _)| hash.as_str())
            .collect::<Vec<_>>();
        let contents = chunk
            .iter()
            .map(|(_, content)| &content[..])
            .collect::<Vec<_>>();
        sqlx::query(crate::blobs::blob_sql().postgres.insert_chunk.sql())
            .bind(hashes)
            .bind(contents)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

async fn get_blob_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_ref: &BlobRef,
) -> Result<Option<Vec<u8>>, StoreError> {
    sqlx::query_scalar(crate::blobs::blob_sql().shared.select_content.sql())
        .bind(blob_ref.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

// One array bind avoids PostgreSQL's scalar-parameter ceiling. A 16,384-ref
// chunk is four times the largest required depth while bounding each encoded
// request to roughly one MiB of SHA-256 text plus array framing.
const CHECKPOINT_COMPONENT_REF_CHUNK_SIZE: usize = 16_384;

pub(crate) async fn lock_checkpoint_blob_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_ref: &str,
    component_key: Option<&str>,
) -> Result<(), StoreError> {
    let exists = sqlx::query_scalar::<_, bool>(crate::blobs::blob_sql().postgres.lock_one.sql())
        .bind(blob_ref)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if exists.is_some() {
        return Ok(());
    }
    let blob_ref = BlobRef(blob_ref.to_string());
    match component_key {
        Some(key) => Err(StoreError::CheckpointComponentMissing {
            key: key.to_string(),
            blob_ref,
        }),
        None => Err(StoreError::CheckpointRootMissing { blob_ref }),
    }
}

async fn lock_checkpoint_blobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    acquisition_order: &[(String, Option<String>)],
) -> Result<(), StoreError> {
    let blob_refs = acquisition_order
        .iter()
        .map(|(blob_ref, _)| blob_ref.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut locked = std::collections::HashSet::with_capacity(blob_refs.len());
    for chunk in blob_refs.chunks(CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
        locked.extend(
            sqlx::query_scalar::<_, String>(
                crate::blobs::blob_sql().postgres.lock_existing_hashes.sql(),
            )
            .bind(chunk)
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?,
        );
    }
    for (blob_ref, component_key) in acquisition_order {
        if locked.contains(blob_ref) {
            continue;
        }
        let blob_ref = BlobRef(blob_ref.clone());
        return match component_key {
            Some(key) => Err(StoreError::CheckpointComponentMissing {
                key: key.clone(),
                blob_ref,
            }),
            None => Err(StoreError::CheckpointRootMissing { blob_ref }),
        };
    }
    Ok(())
}

async fn checkpoint_component_bodies_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    manifest: &SessionCheckpoint,
) -> Result<std::collections::HashMap<String, std::sync::Arc<[u8]>>, StoreError> {
    let blob_refs = manifest
        .components
        .values()
        .map(|descriptor| descriptor.blob_ref.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let mut bodies = std::collections::HashMap::with_capacity(blob_refs.len());
    let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
    for chunk in blob_refs.chunks(CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
        let rows = sqlx::query(
            crate::blobs::blob_sql()
                .postgres
                .select_bodies_by_hash
                .sql(),
        )
        .bind(chunk)
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        for row in rows {
            bodies.insert(
                row.get::<String, _>(0),
                std::sync::Arc::from(row.get::<Vec<u8>, _>(1)),
            );
        }
    }
    Ok(bodies)
}

/// Persist the complete checkpoint root and every changed leaf inside the
/// caller's commit transaction. Keeping both writes under the same transaction
/// is the GC-safety argument: no collector can observe a git-loose-object-style
/// leaf that is not yet reachable from its root, or a root whose leaf is absent.
pub(crate) async fn put_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint: &HydratedSessionCheckpoint,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<(BlobRef, SessionCheckpoint), StoreError> {
    let manifest = checkpoint.manifest(fleet_format)?;
    let bytes = encode_msgpack(&manifest, "checkpoint root")?;
    let checkpoint_ref = BlobRef::for_content(&bytes);

    // Global PostgreSQL checkpoint lock order:
    // 1. session-history advisory locks, ascending by session id;
    // 2. manifest-root and component blob rows, ascending by content hash;
    // 3. checkpoint-owner edges, graph rows, and session heads.
    //
    // This transaction bulk-inserts every supplied body in hash order, then acquires the
    // complete root/component union with one ordered FOR KEY SHARE query.
    // Concurrent reclaim that wins first is a typed missing-root/component refusal, never a
    // resurrection or a later foreign-key error.
    let mut acquisition_order = manifest
        .components
        .iter()
        .map(|(key, descriptor)| (descriptor.blob_ref.as_str().to_string(), Some(key.clone())))
        .collect::<Vec<_>>();
    acquisition_order.push((checkpoint_ref.as_str().to_string(), None));
    acquisition_order.sort();
    let mut supplied_blobs = std::collections::BTreeMap::new();
    supplied_blobs.insert(checkpoint_ref.as_str().to_string(), bytes.into());
    for (key, descriptor) in &manifest.components {
        let component =
            checkpoint
                .components
                .get(key)
                .ok_or_else(|| StoreError::StoredDataCorrupt {
                    record_kind: "HydratedSessionCheckpoint",
                    message: format!("manifest projection lost component `{key}`"),
                })?;
        let (stored_ref, body) = match component {
            HydratedCheckpointComponent::Changed { body_ref, body, .. } => (body_ref.clone(), body),
            HydratedCheckpointComponent::Hydrated { body, .. } => {
                let stored_ref = BlobRef::for_content(body);
                #[cfg(feature = "perf-witness")]
                lash_core_execution::perf_witness::record_hash_pass(body.len());
                (stored_ref, body)
            }
            HydratedCheckpointComponent::Unchanged { .. } => continue,
        };
        lash_core_execution::store::ensure_checkpoint_component_hash_agreement(
            key,
            &stored_ref,
            &descriptor.blob_ref,
        )?;
        supplied_blobs
            .entry(stored_ref.0)
            .or_insert_with(|| std::sync::Arc::clone(body));
    }
    put_checkpoint_blobs_tx(tx, &supplied_blobs).await?;
    lock_checkpoint_blobs_tx(tx, &acquisition_order).await?;
    let component_refs = manifest
        .components
        .values()
        .map(|descriptor| descriptor.blob_ref.as_str())
        .collect::<Vec<_>>();
    sqlx::query(session_sql().checkpoint_edges.insert_batch.sql())
        .bind(checkpoint_ref.as_str())
        .bind(component_refs)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok((checkpoint_ref, manifest))
}

/// `fleet` is the store's recorded `F`: the manifest and its component
/// encodings admit the `[N-1, N]` reader window `F` names (FIG-3796).
pub(crate) async fn get_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_ref: &BlobRef,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
    let bytes = get_blob_tx(tx, blob_ref).await?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let manifest: SessionCheckpoint =
        lash_core_execution::store::decode_versioned_msgpack_record_for_fleet(
            &bytes,
            "SessionCheckpoint",
            lash_core_execution::surface_format!(
                lash_core_execution::store::SESSION_CHECKPOINT_SCHEMA_VERSION
            ),
            fleet,
        )?;
    manifest.validate_component_encoding_versions_for_fleet(fleet)?;
    let bodies = checkpoint_component_bodies_tx(tx, &manifest).await?;
    let mut components = std::collections::BTreeMap::new();
    for (key, descriptor) in &manifest.components {
        let body = bodies.get(descriptor.blob_ref.as_str()).ok_or_else(|| {
            StoreError::CheckpointComponentMissing {
                key: key.clone(),
                blob_ref: descriptor.blob_ref.clone(),
            }
        })?;
        components.insert(
            key.clone(),
            lash_core_execution::HydratedCheckpointComponent::hydrated(
                descriptor.clone(),
                std::sync::Arc::clone(body),
            ),
        );
    }
    Ok(Some(HydratedSessionCheckpoint {
        turn_state: manifest.turn_state,
        components,
    }))
}

/// `fleet` is the store's recorded `F`: the head-meta JSON admits the
/// `[N-1, N]` reader window `F` names (FIG-3796).
pub(crate) async fn load_session_head_meta_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    for_update: bool,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let sql = if for_update {
        session_sql().head_postgres.select_meta_for_update.sql()
    } else {
        session_sql().head.select_meta.sql()
    };
    let row = sqlx::query(sql)
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    decode_session_head_meta_row(session_id, row, fleet)
}

fn decode_session_head_meta_row(
    session_id: &SessionId,
    row: Option<sqlx::postgres::PgRow>,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let current_frame_node_id = row
        .try_get::<Option<String>, _>(5)
        .map_err(store_sqlx_error)?;
    let leaf = row
        .try_get::<Option<String>, _>(2)
        .map_err(store_sqlx_error)?;
    if let Some(leaf) = leaf
        && current_frame_node_id.is_none()
    {
        return Err(StoreError::MissingFrameOpenAncestor {
            leaf_node_id: leaf.try_into()?,
        });
    }
    let current_frame_node_id = current_frame_node_id
        .map(lash_core_execution::FrameNodeId::new)
        .transpose()
        .map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: "SessionGraph",
            message: error.to_string(),
        })?;
    let head_json: String = row.get(0);
    let head_revision: i64 = row.get(1);
    let leaf_node_id: Option<String> = row.get(2);
    let checkpoint_ref: Option<String> = row.get(3);
    let pending_follow_on: Option<String> = row.get(4);
    let pending_follow_on =
        lash_core_execution::store::pending_follow_on::decode_pending_follow_on(
            session_id,
            pending_follow_on.as_deref(),
        )?;
    let payload: SessionHeadPayload =
        lash_core_execution::store::decode_versioned_json_record_for_fleet(
            &head_json,
            "SessionHeadMeta",
            lash_core_execution::surface_format!(
                lash_core_execution::store::SESSION_HEAD_META_SCHEMA_VERSION
            ),
            fleet,
        )?;
    Ok(Some(
        SessionHeadMeta::assemble(
            session_id,
            payload,
            u64_from_sql("SessionHeadMeta", "head_revision", head_revision)?,
            checkpoint_ref.map(Into::into),
            leaf_node_id
                .map(lash_core_execution::NodeId::parse)
                .transpose()?,
            current_frame_node_id,
        )?
        .with_pending_follow_on(pending_follow_on),
    ))
}

#[cfg(test)]
mod contention_tests {
    use super::{is_contention_sqlstate, plugin_sqlx_error, store_sqlx_error};
    use crate::StoreError;
    use lash_core_execution::PluginError;
    use lash_core_execution::store::StoreFault;

    /// A database error carrying only a SQLSTATE.
    #[derive(Debug)]
    struct SqlState(&'static str);

    impl std::fmt::Display for SqlState {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "SQLSTATE {}", self.0)
        }
    }

    impl std::error::Error for SqlState {}

    impl sqlx::error::DatabaseError for SqlState {
        fn message(&self) -> &str {
            "injected database error"
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    /// FIG-4649: a registry or trigger SQL fault reaches the plugin boundary
    /// through the store's mapper, so it keeps its class: contention and a
    /// failed substrate are retried, never a session error a recorded step
    /// would journal as its answer.
    #[test]
    fn a_registry_sql_fault_is_a_retryable_store_fault_at_the_plugin_boundary() {
        for code in ["40001", "40P01", "55P03"] {
            let fault = plugin_sqlx_error(sqlx::Error::Database(Box::new(SqlState(code))));
            assert!(
                matches!(
                    fault,
                    PluginError::StoreUnavailable {
                        fault: StoreFault::Contended
                    }
                ),
                "{code}: {fault:?}"
            );
            assert!(fault.is_retryable() && !fault.is_terminal(), "{code}");
        }
        for error in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::Protocol("broken wire frame".to_string()),
            sqlx::Error::Database(Box::new(SqlState("08006"))),
        ] {
            let fault = plugin_sqlx_error(error);
            assert!(
                matches!(
                    &fault,
                    PluginError::StoreUnavailable {
                        fault: StoreFault::StorageFailure { backend, .. }
                    } if backend == "postgres"
                ),
                "{fault:?}"
            );
            assert!(fault.is_retryable() && !fault.is_terminal(), "{fault:?}");
        }
    }

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
