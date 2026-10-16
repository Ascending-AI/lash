//! Content-addressed blob, artifact, checkpoint, and usage-ledger storage on
//! [`SqliteStore`].
//!
//! Reference module (with `lifecycle.rs`) for the translation pattern. The
//! `*_conn` helpers here are **synchronous** and take a `&rusqlite::Connection`
//! so they can be reused from inside any `conn.call`/`conn.write` closure (the
//! checkpoint/persistence/graph modules call them while already on the
//! connection thread). Public reads bridge onto that connection thread, while
//! production blob writes stay inside the transaction that publishes their
//! durable root.

use super::*;
use crate::artifact_store::artifact_sql;
use lash_core_execution::FleetFormat;

lash_store_sql::statements! {
    /// `blobs` statements only SQLite issues.
    pub(crate) struct BlobSqliteStatements @ "blob" {
        /// Store `?2` at content address `?1`, keeping what is already there.
        ///
        /// `INSERT OR IGNORE` is the fork, and it is a fork of shape rather
        /// than of meaning: PostgreSQL writes a whole chunk in one round trip
        /// through `unnest` and spells the same idempotence as
        /// `ON CONFLICT DO NOTHING`. The bytes are content-addressed, so a
        /// conflict is always the same bytes.
        /// Every blob this writer stores is opaque (encoding 0): checkpoint
        /// graph nodes are installed by the graph commit alone.
        insert_ignore = "INSERT OR IGNORE INTO blobs (hash, content, encoding) VALUES (?1, ?2, 0)";

        /// Which of the content addresses in the JSON array `?1` exist.
        ///
        /// The `json_each` table-valued function is the fork: it is how SQLite
        /// binds a list to one statement, where PostgreSQL binds a text array.
        /// PostgreSQL's counterpart also takes `FOR KEY SHARE`, which SQLite
        /// does not need under `BEGIN IMMEDIATE`.
        select_existing_hashes = "SELECT hash FROM blobs
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// The stored bytes for every content address in the JSON array `?1`.
        /// Same `json_each` fork as select_existing_hashes.
        select_bodies_by_hash = "SELECT hash, content FROM blobs
             WHERE hash IN (SELECT value FROM json_each(?1))";

        /// One preflight page of sessions that have published a checkpoint
        /// root, after session `?1`, at most `?2` rows.
        ///
        /// The join is `LEFT` on purpose: an inner join would make a session
        /// whose manifest blob has gone missing simply disappear from the
        /// walk — the single most alarming finding a preflight can make,
        /// rendered as "no such session". PostgreSQL's walk reads its own
        /// head table, `lash_session_head`, so the two texts fork on the table
        /// name alone.
        select_session_checkpoint_page = "SELECT session_head.session_id, revision.checkpoint_ref, blobs.content, blobs.encoding
             FROM session_head JOIN session_revisions AS revision
               ON revision.session_id = session_head.session_id
              AND revision.head_revision = session_head.head_revision
             LEFT JOIN blobs ON blobs.hash = revision.checkpoint_ref
             WHERE revision.checkpoint_ref IS NOT NULL
               AND (?1 IS NULL OR session_head.session_id > ?1)
             ORDER BY session_head.session_id
             LIMIT ?2";

        /// SQLite alone asks this: it is a fork's proof that the checkpoint
        /// it copies is stored, taken under the write lock. PostgreSQL gets
        /// the same proof from the row lock it takes on the checkpoint.
        select_exists = "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)";
    }
}

/// A checkpoint graph refusal on the session path, as the store reports it.
pub(crate) fn graph_store_error(
    refusal: lash_core_store::checkpoint_graph::GraphRefusal,
) -> StoreError {
    match refusal {
        lash_core_store::checkpoint_graph::GraphRefusal::Backend(failure) => {
            StoreError::Backend(failure.message)
        }
        refusal => stored_data_corrupt("SessionCheckpoint", refusal),
    }
}

/// Versioned BLAKE3 content address that keys every row in the `blobs` table.
fn blob_content_hash(content: &[u8]) -> String {
    BlobRef::for_content(content).0
}

impl SqliteStore {
    /// Decode a checkpoint from a fresh durable connection without calling
    /// the session history read path.
    #[cfg(any(test, feature = "testing"))]
    pub fn raw_checkpoint_from_path_for_testing(
        path: &std::path::Path,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let connection = Connection::open(path).map_err(sqlite_error)?;
        let fleet = crate::compat::recorded_or_current(&connection).map_err(sqlite_error)?;
        Self::get_checkpoint_conn(
            &connection,
            blob_ref,
            fleet,
            crate::SqliteOperationalSettings::standard()
                .checkpoint_ref_chunk
                .get(),
        )
    }

    pub(crate) fn insert_artifact_blob_conn(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
        profile: BuiltinBlobProfile,
        fleet: FleetFormat,
    ) -> rusqlite::Result<BlobRef> {
        Self::insert_artifact_blob_conn_typed(conn, descriptor, content, profile, fleet)
            .map_err(sqlite_conversion_error)
    }

    fn insert_artifact_blob_conn_typed(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
        profile: BuiltinBlobProfile,
        fleet: FleetFormat,
    ) -> Result<BlobRef, StoreError> {
        let blob_ref = BlobRef(blob_content_hash(content));
        Self::insert_artifact_blob_conn_typed_with_ref(
            conn, descriptor, content, profile, &blob_ref, fleet,
        )?;
        Ok(blob_ref)
    }

    fn insert_artifact_blob_conn_typed_with_ref(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
        profile: BuiltinBlobProfile,
        blob_ref: &BlobRef,
        fleet: FleetFormat,
    ) -> Result<(), StoreError> {
        let version = fleet.writer_version(lash_core_execution::surface_format!(
            SQLITE_BLOB_ENVELOPE_VERSION
        ));
        let stored = encode_artifact_blob(&descriptor, profile, content, version)?;
        crate::conn::cached_execute(
            conn,
            artifact_sql().blobs_sqlite.insert_ignore.sql(),
            params![blob_ref.as_str(), stored],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    /// Seed an intentionally unrooted artifact for GC and failure-path tests.
    #[cfg(any(test, feature = "testing"))]
    pub async fn put_unrooted_artifact_blob_for_testing(
        &self,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
    ) -> Result<BlobRef, StoreError> {
        let content = content.to_vec();
        let profile = self.options.blob_profile;
        self.conn
            .write(move |tx| {
                Self::insert_artifact_blob_conn(tx, descriptor, &content, profile, tx.fleet())
            })
            .await
            .map_err(sqlite_error)
    }

    /// Persist the complete checkpoint root and every changed leaf inside the
    /// caller's commit transaction. This is the GC-safety argument: no
    /// collector can observe the git-loose-object race where a leaf exists
    /// without its root, or a root becomes visible before all leaves exist.
    pub(crate) fn put_checkpoint_conn(
        conn: &crate::conn::FencedTx<'_>,
        checkpoint: &HydratedSessionCheckpoint,
        profile: BuiltinBlobProfile,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<StoredSessionCheckpoint, StoreError> {
        Self::validate_checkpoint_component_refs_conn(
            conn,
            checkpoint,
            conn.operational.checkpoint_ref_chunk.get(),
        )?;
        let manifest = checkpoint.manifest(fleet_format)?;
        for (key, descriptor) in &manifest.components {
            let component =
                checkpoint
                    .components
                    .get(key)
                    .ok_or_else(|| StoreError::StoredDataCorrupt {
                        record_kind: "HydratedSessionCheckpoint",
                        message: format!("manifest projection lost component `{key}`"),
                    })?;
            if let HydratedCheckpointComponent::Changed { body, body_ref, .. } = component {
                Self::insert_artifact_blob_conn_typed_with_ref(
                    conn,
                    BlobArtifactDescriptor::checkpoint_component(),
                    body,
                    profile,
                    body_ref,
                    fleet_format,
                )?;
                lash_core_execution::store::ensure_checkpoint_component_hash_agreement(
                    key,
                    body_ref,
                    &descriptor.blob_ref,
                )?;
            } else if let Some(body) = component.body() {
                let stored_ref = Self::insert_artifact_blob_conn_typed(
                    conn,
                    BlobArtifactDescriptor::checkpoint_component(),
                    body,
                    profile,
                    fleet_format,
                )?;
                #[cfg(feature = "perf-witness")]
                lash_core_execution::perf_witness::record_hash_pass(body.len());
                lash_core_execution::store::ensure_checkpoint_component_hash_agreement(
                    key,
                    &stored_ref,
                    &descriptor.blob_ref,
                )?;
            }
        }
        // The manifest is a SessionManifest node over its components' opaque
        // blobs and its SessionExecution root, installed with its edges once
        // every component exists, beside the execution's descriptor when
        // the checkpoint holds it.
        let (root, mut nodes) = manifest.to_graph().map_err(graph_store_error)?;
        if let Some(node) = checkpoint
            .execution
            .as_ref()
            .and_then(lash_core_execution::store::CheckpointExecution::node)
        {
            nodes.insert(node.id, node);
        }
        crate::durable::install_staged(conn, &nodes)
            .map_err(sqlite_error)?
            .map_err(graph_store_error)?;
        let checkpoint_ref = BlobRef(root.to_hex());
        Ok(StoredSessionCheckpoint {
            checkpoint_ref,
            manifest,
        })
    }

    pub(crate) fn validate_checkpoint_component_refs_conn(
        conn: &Connection,
        checkpoint: &HydratedSessionCheckpoint,
        chunk_size: usize,
    ) -> Result<(), StoreError> {
        let mut referenced = std::collections::BTreeSet::new();
        for (key, component) in &checkpoint.components {
            lash_core_execution::store::ensure_checkpoint_component_encoding_version(
                key,
                component.encoding_version(),
            )?;
            let Some(blob_ref) = component.blob_ref().filter(|_| component.body().is_none()) else {
                continue;
            };
            referenced.insert(blob_ref.as_str().to_string());
        }
        let existing =
            Self::existing_checkpoint_component_refs_conn(conn, &referenced, chunk_size)?;
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

    fn existing_checkpoint_component_refs_conn(
        conn: &Connection,
        blob_refs: &std::collections::BTreeSet<String>,
        chunk_size: usize,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut existing = std::collections::HashSet::with_capacity(blob_refs.len());
        let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
        for chunk in blob_refs.chunks(chunk_size) {
            let encoded = serde_json::to_string(chunk).map_err(|error| {
                StoreError::Backend(format!("failed to encode checkpoint ref batch: {error}"))
            })?;
            let mut statement = conn
                .prepare(artifact_sql().blobs_sqlite.select_existing_hashes.sql())
                .map_err(sqlite_error)?;
            let rows = statement
                .query_map(params![encoded], |row| row.get::<_, String>(0))
                .map_err(sqlite_error)?;
            existing.extend(rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?);
        }
        Ok(existing)
    }

    fn checkpoint_component_bodies_conn(
        conn: &Connection,
        checkpoint: &SessionCheckpoint,
        chunk_size: usize,
    ) -> Result<std::collections::HashMap<String, std::sync::Arc<[u8]>>, StoreError> {
        let blob_refs = checkpoint
            .components
            .values()
            .map(|descriptor| descriptor.blob_ref.as_str().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let mut bodies = std::collections::HashMap::with_capacity(blob_refs.len());
        let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
        for chunk in blob_refs.chunks(chunk_size) {
            let encoded = serde_json::to_string(chunk).map_err(|error| {
                StoreError::Backend(format!("failed to encode checkpoint ref batch: {error}"))
            })?;
            let mut statement = conn
                .prepare(artifact_sql().blobs_sqlite.select_bodies_by_hash.sql())
                .map_err(sqlite_error)?;
            let rows = statement
                .query_map(params![encoded], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (hash, bytes) = row.map_err(sqlite_error)?;
                let body = decode_artifact_blob(&bytes)?;
                bodies.insert(hash, std::sync::Arc::from(body));
            }
        }
        Ok(bodies)
    }

    pub(crate) fn get_blob_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                artifact_sql().blobs.select_content.sql(),
                params![blob_ref.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        bytes.map(|bytes| decode_artifact_blob(&bytes)).transpose()
    }

    /// The SessionManifest node `blob_ref` names with every graph node it
    /// reaches: its SessionExecution graph among them.
    fn checkpoint_graph_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
    ) -> Result<Option<(SessionCheckpoint, crate::durable::GraphClosure)>, StoreError> {
        let Some(root) = lash_core_store::checkpoint_graph::NodeId::from_hex(blob_ref.as_str())
        else {
            // No node is stored under a reference that is not a digest: no
            // row is an absent checkpoint, and a blob stored there is no
            // manifest.
            let stored = conn
                .query_row(
                    artifact_sql().blobs.select_content.sql(),
                    params![blob_ref.as_str()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sqlite_error)?;
            return match stored {
                None => Ok(None),
                Some(()) => Err(stored_data_corrupt(
                    "SessionCheckpoint",
                    format_args!("`{blob_ref}` is not a node reference"),
                )),
            };
        };
        let limits = lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 32);
        let closure =
            match crate::durable::graph_closure(conn, root, &limits).map_err(sqlite_error)? {
                Ok(closure) => closure,
                Err(lash_core_store::checkpoint_graph::GraphRefusal::MissingNode(missing))
                    if missing == root =>
                {
                    return Ok(None);
                }
                Err(refusal) => return Err(graph_store_error(refusal)),
            };
        let manifest =
            SessionCheckpoint::load(&closure, root, limits).map_err(graph_store_error)?;
        Ok(Some((manifest, closure)))
    }

    /// `fleet` is the store's recorded `F`: the component encodings admit
    /// the `[N-1, N]` reader window `F` names (FIG-3796).
    pub(crate) fn get_checkpoint_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
        fleet: lash_core_execution::FleetFormat,
        chunk_size: usize,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let Some((record, closure)) = Self::checkpoint_graph_conn(conn, blob_ref)? else {
            return Ok(None);
        };
        let execution = record
            .execution_ref
            .as_ref()
            .map(|root| {
                let root = lash_core_store::checkpoint_graph::NodeId::from_hex(root.as_str())
                    .ok_or_else(|| {
                        stored_data_corrupt(
                            "SessionCheckpoint",
                            format_args!("`{root}` is not a node reference"),
                        )
                    })?;
                // The descriptor alone: its heap pages are read on demand
                // under a root lease (ADR 0145 §10).
                let descriptor =
                    lash_core_store::checkpoint_graph::NodeSource::fetch(&closure, &root)
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
        record.validate_component_encoding_versions_for_fleet(fleet)?;
        let bodies = Self::checkpoint_component_bodies_conn(conn, &record, chunk_size)?;
        let mut components = std::collections::BTreeMap::new();
        for (key, descriptor) in &record.components {
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
            turn_state: record.turn_state,
            components,
            execution,
        }))
    }

    pub async fn get_blob(&self, blob_ref: &BlobRef) -> Result<Option<Vec<u8>>, StoreError> {
        let blob_ref = blob_ref.clone();
        self.conn
            .call(move |conn| Self::get_blob_conn(conn, &blob_ref).map_err(sqlite_conversion_error))
            .await
            .map_err(sqlite_error)
    }

    pub async fn get_typed_blob<T: serde::de::DeserializeOwned>(
        &self,
        blob_ref: &BlobRef,
    ) -> Result<Option<T>, StoreError> {
        let Some(bytes) = self.get_blob(blob_ref).await? else {
            return Ok(None);
        };
        decode_msgpack(&bytes).map(Some).ok_or_else(|| {
            stored_data_corrupt(
                "typed blob",
                format_args!("failed to decode blob `{blob_ref}`"),
            )
        })
    }

    #[cfg(test)]
    pub(crate) async fn put_checkpoint(
        &self,
        checkpoint: &HydratedSessionCheckpoint,
    ) -> Result<StoredSessionCheckpoint, StoreError> {
        let checkpoint = checkpoint.clone();
        let profile = self.options.blob_profile;
        self.conn
            .write_flow(move |tx| {
                let self_fleet = tx.fleet();
                Ok(
                    match Self::put_checkpoint_conn(tx, &checkpoint, profile, self_fleet) {
                        Ok(stored) => TxOutcome::Commit(Ok(stored)),
                        Err(error) => TxOutcome::Rollback(Err(error)),
                    },
                )
            })
            .await
            .map_err(sqlite_error)?
    }

    pub async fn get_checkpoint(
        &self,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let blob_ref = blob_ref.clone();
        let chunk_size = self.conn.operational.checkpoint_ref_chunk.get();
        let fleet = self.conn.fleet();
        self.conn
            .call(move |conn| {
                Self::get_checkpoint_conn(conn, &blob_ref, fleet, chunk_size)
                    .map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }
}
