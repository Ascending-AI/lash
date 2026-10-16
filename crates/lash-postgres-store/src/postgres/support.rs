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
        let injected: Option<String> = crate::observed_sql::query_scalar(
            crate::connection_sql::connection_sql()
                .select_injected_lease_epoch_ms
                .sql(),
        )
        .fetch_one(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(store_sqlx_error)?;
        if let Some(injected) = injected {
            return injected
                .parse()
                .map_err(|error| StoreError::Backend(format!("invalid test lease time: {error}")));
        }
    }
    let now: i64 = crate::observed_sql::query_scalar(
        crate::connection_sql::connection_sql()
            .select_transaction_epoch_ms
            .sql(),
    )
    .fetch_one(crate::observed_sql::executor(&mut **tx))
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

/// Why session `session_id`'s graph node `node_id`, planned at
/// `generation`, was left out of its batch insert: another row holds its
/// id, or else its session's generation.
pub(crate) async fn graph_node_collision(
    connection: &mut sqlx::PgConnection,
    session_id: &SessionId,
    generation: u64,
    node_id: &lash_core_execution::NodeId,
) -> StoreError {
    let taken: Result<Vec<String>, _> = crate::observed_sql::query_scalar(
        crate::session_sql::session_sql()
            .graph_postgres
            .select_occupied
            .sql(),
    )
    .bind(vec![node_id.to_string()])
    .fetch_all(crate::observed_sql::executor(connection))
    .await;
    match taken {
        Err(error) => store_sqlx_error(error),
        Ok(taken) if taken.is_empty() => StoreError::GraphGenerationCollision {
            session_id: session_id.clone(),
            generation,
        },
        Ok(_) => StoreError::NodeIdCollision {
            node_id: node_id.clone(),
        },
    }
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
    if matches!(
        err,
        sqlx::Error::Decode(_) | sqlx::Error::ColumnDecode { .. }
    ) {
        return PluginError::StoredDataCorrupt {
            record_kind: "PostgreSQL row".to_owned(),
            message: err.to_string(),
        };
    }
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

async fn put_checkpoint_blobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blobs: &std::collections::BTreeMap<String, std::sync::Arc<[u8]>>,
    chunk_size: usize,
) -> Result<(), StoreError> {
    let blobs = blobs.iter().collect::<Vec<_>>();
    for chunk in blobs.chunks(chunk_size) {
        let hashes = chunk
            .iter()
            .map(|(hash, _)| hash.as_str())
            .collect::<Vec<_>>();
        let contents = chunk
            .iter()
            .map(|(_, content)| &content[..])
            .collect::<Vec<_>>();
        crate::observed_sql::query(crate::blobs::blob_sql().postgres.insert_chunk.sql())
            .bind(hashes)
            .bind(contents)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

pub(crate) async fn lock_checkpoint_blob_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blob_ref: &str,
    component_key: Option<&str>,
) -> Result<(), StoreError> {
    let exists = crate::observed_sql::query_scalar::<_, bool>(
        crate::blobs::blob_sql().postgres.lock_one.sql(),
    )
    .bind(blob_ref)
    .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
    chunk_size: usize,
) -> Result<(), StoreError> {
    let blob_refs = acquisition_order
        .iter()
        .map(|(blob_ref, _)| blob_ref.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut locked = std::collections::HashSet::with_capacity(blob_refs.len());
    for chunk in blob_refs.chunks(chunk_size) {
        locked.extend(
            crate::observed_sql::query_scalar::<_, String>(
                crate::blobs::blob_sql().postgres.lock_existing_hashes.sql(),
            )
            .bind(chunk)
            .fetch_all(crate::observed_sql::executor(&mut **tx))
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
    tx: &mut sqlx::PgConnection,
    manifest: &SessionCheckpoint,
    chunk_size: usize,
) -> Result<std::collections::HashMap<String, std::sync::Arc<[u8]>>, StoreError> {
    let blob_refs = manifest
        .components
        .values()
        .map(|descriptor| descriptor.blob_ref.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let mut bodies = std::collections::HashMap::with_capacity(blob_refs.len());
    let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
    for chunk in blob_refs.chunks(chunk_size) {
        let rows = crate::observed_sql::query(
            crate::blobs::blob_sql()
                .postgres
                .select_bodies_by_hash
                .sql(),
        )
        .bind(chunk)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
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

/// What a checkpoint's put writes: its manifest and the manifest's root,
/// the component bodies the checkpoint supplies by digest, and the graph
/// the put installs (the manifest's nodes beside the execution's).
pub(crate) struct CheckpointParts {
    pub(crate) checkpoint_ref: BlobRef,
    pub(crate) manifest: SessionCheckpoint,
    pub(crate) supplied: std::collections::BTreeMap<String, std::sync::Arc<[u8]>>,
    pub(crate) nodes: std::collections::BTreeMap<
        lash_core_store::checkpoint_graph::NodeId,
        lash_core_store::checkpoint_graph::EncodedNode,
    >,
}

/// A checkpoint's manifest with the component bodies it supplies and its
/// components' lock order: what its put writes before its graph.
type CheckpointComponents = (
    SessionCheckpoint,
    std::collections::BTreeMap<String, std::sync::Arc<[u8]>>,
    Vec<(String, Option<String>)>,
);

/// The parts of `checkpoint`'s put under `fleet_format`.
pub(crate) fn checkpoint_parts(
    checkpoint: &HydratedSessionCheckpoint,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<CheckpointParts, StoreError> {
    checkpoint_graph_parts(checkpoint, checkpoint_components(checkpoint, fleet_format)?)
}

fn checkpoint_components(
    checkpoint: &HydratedSessionCheckpoint,
    fleet_format: lash_core_execution::FleetFormat,
) -> Result<CheckpointComponents, StoreError> {
    let manifest = checkpoint.manifest(fleet_format)?;
    let mut acquisition_order = manifest
        .components
        .iter()
        .map(|(key, descriptor)| (descriptor.blob_ref.as_str().to_string(), Some(key.clone())))
        .collect::<Vec<_>>();
    acquisition_order.sort();
    let mut supplied = std::collections::BTreeMap::new();
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
        supplied
            .entry(stored_ref.0)
            .or_insert_with(|| std::sync::Arc::clone(body));
    }
    Ok((manifest, supplied, acquisition_order))
}

fn checkpoint_graph_parts(
    checkpoint: &HydratedSessionCheckpoint,
    (manifest, supplied, _): CheckpointComponents,
) -> Result<CheckpointParts, StoreError> {
    // The manifest is a SessionManifest node over its components' opaque
    // blobs and its SessionExecution root, staged and installed with its
    // edges once every component exists, beside the execution's descriptor
    // when the checkpoint holds it: a missing component is refused as one.
    let (root, mut nodes) = manifest.to_graph().map_err(graph_store_error)?;
    if let Some(node) = checkpoint
        .execution
        .as_ref()
        .and_then(lash_core_execution::store::CheckpointExecution::node)
    {
        nodes.insert(node.id, node);
    }
    Ok(CheckpointParts {
        checkpoint_ref: BlobRef(root.to_hex()),
        manifest,
        supplied,
        nodes,
    })
}

/// Persist the complete checkpoint root and every changed leaf inside the
/// caller's commit transaction, answering what it put. Keeping both writes
/// under the same transaction is the GC-safety argument: no orphan audit can
/// observe a git-loose-object-style leaf that is not yet reachable from its
/// root, or a root whose leaf is absent.
pub(crate) async fn put_checkpoint_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint: &HydratedSessionCheckpoint,
    fleet_format: lash_core_execution::FleetFormat,
    chunk_size: usize,
) -> Result<CheckpointParts, StoreError> {
    // Global PostgreSQL checkpoint lock order:
    // 1. session-history advisory locks, ascending by session id;
    // 2. component blob rows, ascending by content hash;
    // 3. the manifest's graph nodes and edges, then session heads.
    //
    // This transaction bulk-inserts every supplied body in hash order, then acquires the
    // complete component set with one ordered FOR KEY SHARE query.
    // Concurrent reclaim that wins first is a typed missing-component refusal, never a
    // resurrection or a later foreign-key error.
    let components = checkpoint_components(checkpoint, fleet_format)?;
    put_checkpoint_blobs_tx(tx, &components.1, chunk_size).await?;
    lock_checkpoint_blobs_tx(tx, &components.2, chunk_size).await?;
    let parts = checkpoint_graph_parts(checkpoint, components)?;
    // The install key-share locks a root that is already stored, and stores
    // it again from these bytes when a reclaim took it first (FIG-5949).
    crate::durable::install_staged(tx, &parts.nodes)
        .await
        .map_err(graph_store_error)?;
    Ok(parts)
}

/// A checkpoint graph refusal on the session path, as the store reports it.
pub(crate) fn graph_store_error(
    refusal: lash_core_store::checkpoint_graph::GraphRefusal,
) -> StoreError {
    match refusal {
        lash_core_store::checkpoint_graph::GraphRefusal::Backend(failure) => {
            StoreError::Backend(failure.message)
        }
        refusal => StoreError::StoredDataCorrupt {
            record_kind: "SessionCheckpoint",
            message: refusal.to_string(),
        },
    }
}

/// Session manifest `blob_ref`'s graph, decoded, with every graph node it
/// reaches: its SessionExecution graph among them.
async fn checkpoint_graph_tx(
    tx: &mut sqlx::PgConnection,
    blob_ref: &BlobRef,
) -> Result<Option<(SessionCheckpoint, crate::durable::GraphClosure)>, StoreError> {
    let root = lash_core_store::checkpoint_graph::NodeId::from_hex(blob_ref.as_str()).ok_or_else(
        || StoreError::StoredDataCorrupt {
            record_kind: "SessionCheckpoint",
            message: format!("`{blob_ref}` is not a node reference"),
        },
    )?;
    let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 32);
    let closure = match crate::durable::graph_closure(tx, root, &limits).await {
        Ok(closure) => closure,
        Err(lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(missing))
            if missing == root =>
        {
            return Ok(None);
        }
        Err(refusal) => return Err(graph_store_error(refusal)),
    };
    let manifest = SessionCheckpoint::load(&closure, root, limits).map_err(graph_store_error)?;
    Ok(Some((manifest, closure)))
}

/// `fleet` is the store's recorded `F`: the manifest and its component
/// encodings admit the `[N-1, N]` reader window `F` names (FIG-3796).
pub(crate) async fn get_checkpoint_tx(
    tx: &mut sqlx::PgConnection,
    blob_ref: &BlobRef,
    fleet: lash_core_execution::FleetFormat,
    chunk_size: usize,
) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
    let Some((manifest, closure)) = checkpoint_graph_tx(tx, blob_ref).await? else {
        return Ok(None);
    };
    manifest.validate_component_encoding_versions_for_fleet(fleet)?;
    let execution = manifest
        .execution_ref
        .as_ref()
        .map(|root| {
            let root = lash_core_store::checkpoint_graph::NodeId::from_hex(root.as_str())
                .ok_or_else(|| StoreError::StoredDataCorrupt {
                    record_kind: "SessionCheckpoint",
                    message: format!("`{root}` is not a node reference"),
                })?;
            // The descriptor alone: its heap pages are read on demand
            // under a root lease (ADR 0145 §10).
            let descriptor = lash_core_store::checkpoint_graph::NodeSource::fetch(&closure, &root)
                .map(|node| std::sync::Arc::from(node.bytes))
                .ok_or_else(|| {
                    graph_store_error(
                        lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(root),
                    )
                })?;
            Ok::<_, StoreError>(lash_core_execution::store::CheckpointExecution {
                root,
                descriptor: Some(descriptor),
            })
        })
        .transpose()?;
    let bodies = checkpoint_component_bodies_tx(tx, &manifest, chunk_size).await?;
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
        execution,
    }))
}

/// `fleet` is the store's recorded `F`: the head-meta JSON admits the
/// `[N-1, N]` reader window `F` names (FIG-3796).
pub(crate) async fn load_session_head_meta_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    for_update: bool,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let sql = if for_update {
        session_sql().head_postgres.select_meta_for_update.sql()
    } else {
        session_sql().head.select_meta.sql()
    };
    let row = crate::observed_sql::query(sql)
        .bind(session_id.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(store_sqlx_error)?;
    decode_session_head_meta_row(tx, session_id, row, fleet).await
}

/// The head that run `run`'s terminal named as its window (FIG-5931), read
/// like the live head: its head document, revision, leaf and checkpoint, and
/// the frame its leaf row names. The terminal's row roots the leaf and the
/// checkpoint, so later commits and collections leave them readable. `None`
/// when the run has no terminal or its terminal names no window.
pub(crate) async fn load_terminal_head_meta_tx(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    run: &lash_core_execution::TurnId,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let row = crate::observed_sql::query(
        crate::session_runs::session_runs_sql()
            .runs
            .select_terminal_window
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(run.as_str())
    .fetch_optional(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(store_sqlx_error)?;
    decode_session_head_meta_row(tx, session_id, row, fleet).await
}

async fn decode_session_head_meta_row(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    row: Option<sqlx::postgres::PgRow>,
    fleet: lash_core_execution::FleetFormat,
) -> Result<Option<SessionHeadMeta>, StoreError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let head_revision = u64_from_sql("SessionHeadMeta", "head_revision", row.get(1))?;
    let config_ref: Option<String> = row.get(0);
    let payload = load_config_tx(tx, config_ref.as_deref(), fleet).await?;
    let current_frame_node_id = row
        .try_get::<Option<String>, _>(4)
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
    let leaf_node_id: Option<String> = row.get(2);
    let checkpoint_ref: Option<String> = row.get(3);
    Ok(Some(SessionHeadMeta::assemble(
        session_id,
        payload,
        head_revision,
        checkpoint_ref.map(Into::into),
        leaf_node_id
            .map(lash_core_execution::NodeId::parse)
            .transpose()?,
        current_frame_node_id,
    )?))
}

/// Read a revision's canonical configuration graph in the caller's snapshot.
pub(crate) async fn load_config_tx(
    tx: &mut sqlx::PgConnection,
    reference: Option<&str>,
    fleet: lash_core_execution::FleetFormat,
) -> Result<SessionHeadPayload, StoreError> {
    use lash_core_store::checkpoint_graph::{DecodeLimits, GraphDecoder, NodeId, config};
    let corrupt = |message: String| StoreError::StoredDataCorrupt {
        record_kind: "SessionHeadMeta",
        message,
    };
    let root = reference
        .and_then(NodeId::from_hex)
        .ok_or_else(|| corrupt("revision names a missing config reference".to_owned()))?;
    let limits = DecodeLimits::within_total(1 << 32);
    let graph = crate::durable::graph_closure(tx, root, &limits)
        .await
        .map_err(config_store_error)?;
    let value: serde_json::Value =
        config::load(&mut GraphDecoder::new(&graph, limits), root).map_err(corrupt)?;
    SessionHeadPayload::decode_for_fleet(&value.to_string(), fleet)
}

/// Configuration admission and reads preserve database failures as store failures.
pub(crate) fn config_store_error(
    error: lash_core_store::checkpoint_graph::GraphRefusal,
) -> StoreError {
    use lash_core_store::checkpoint_graph::GraphRefusal;
    match error {
        GraphRefusal::Backend(failure)
            if failure.kind == lash_durable::StoreFailureKind::Contended =>
        {
            StoreError::Contended
        }
        GraphRefusal::Backend(failure)
            if failure.kind == lash_durable::StoreFailureKind::Corrupt =>
        {
            StoreError::StoredDataCorrupt {
                record_kind: "SessionHeadMeta",
                message: failure.message,
            }
        }
        GraphRefusal::Backend(failure) => StoreError::StorageFailure {
            backend: "postgres",
            message: failure.message,
        },
        error => StoreError::StoredDataCorrupt {
            record_kind: "SessionHeadMeta",
            message: error.to_string(),
        },
    }
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
