//! Content-addressed blob, artifact, checkpoint, and usage-ledger storage on
//! [`Store`].
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
use lash_sansio::SessionId;

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
        insert_ignore = "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, ?2)";

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

        /// Reclaim the blob at `?1` once the artifact pointer that named it
        /// is gone, if nothing else roots it.
        ///
        /// Every predicate is an indexed `NOT EXISTS` over exact edges; no
        /// whole-catalog mark/sweep runs in this transaction. PostgreSQL has
        /// no counterpart: its artifact bytes live inline in
        /// `lash_lashlang_artifacts` and never reach this table.
        reclaim_unowned_artifact = "DELETE FROM blobs AS candidate
             WHERE candidate.hash = ?1
               AND NOT EXISTS (SELECT 1 FROM artifact_refs WHERE blob_ref = candidate.hash)
               AND NOT EXISTS (SELECT 1 FROM session_head WHERE checkpoint_ref = candidate.hash)
               AND NOT EXISTS (SELECT 1 FROM node_anchors WHERE checkpoint_ref = candidate.hash)
               AND NOT EXISTS (SELECT 1 FROM checkpoint_blob_refs WHERE blob_ref = candidate.hash)";

        /// Reclaim the session-delete candidate `?1` if nothing still roots it.
        ///
        /// Forks from PostgreSQL's counterpart on the artifact clause: only
        /// SQLite keeps an `artifact_refs` pointer table, so only SQLite has a
        /// fourth kind of root to rule out. The head table also forks by name,
        /// `session_head` here and `lash_sessions` there.
        reclaim_session_candidate = "DELETE FROM blobs AS candidate
             WHERE candidate.hash = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM session_head AS head
                   WHERE head.checkpoint_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM node_anchors AS anchor
                   WHERE anchor.checkpoint_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_refs AS artifact
                   WHERE artifact.blob_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM checkpoint_blob_refs AS edge
                   WHERE edge.blob_ref = candidate.hash
                     AND (
                         EXISTS (
                             SELECT 1 FROM session_head AS head
                             WHERE head.checkpoint_ref = edge.checkpoint_ref
                         )
                         OR EXISTS (
                             SELECT 1 FROM node_anchors AS anchor
                             WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                         )
                     )
               )";

        /// One preflight page of sessions that have published a checkpoint
        /// root, after session `?1`, at most `?2` rows.
        ///
        /// The join is `LEFT` on purpose: an inner join would make a session
        /// whose manifest blob has gone missing simply disappear from the
        /// walk — the single most alarming finding a preflight can make,
        /// rendered as "no such session". PostgreSQL's walk reads its own
        /// head table, `lash_sessions`, so the two texts fork on the table
        /// name alone.
        select_session_checkpoint_page = "SELECT session_head.session_id, session_head.checkpoint_ref, blobs.content
             FROM session_head
             LEFT JOIN blobs ON blobs.hash = session_head.checkpoint_ref
             WHERE session_head.checkpoint_ref IS NOT NULL
               AND (?1 IS NULL OR session_head.session_id > ?1)
             ORDER BY session_head.session_id
             LIMIT ?2";

        /// SQLite alone asks this: it is the session-delete sweep's proof that
        /// an enumerated reference is not already dangling, taken under the
        /// write lock. PostgreSQL gets the same proof from the row lock its
        /// candidate read takes, so it has no separate existence check.
        select_exists = "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)";
    }
}

/// Versioned BLAKE3 content address that keys every row in the `blobs` table.
fn blob_content_hash(content: &[u8]) -> String {
    BlobRef::for_content(content).0
}

impl Store {
    // One JSON-array bind avoids SQLite's scalar-parameter ceiling. A
    // 16,384-ref chunk is four times the largest required depth while bounding
    // each encoded request to roughly one MiB of SHA-256 text plus JSON framing.
    const CHECKPOINT_COMPONENT_REF_CHUNK_SIZE: usize = 16_384;

    /// Decode a checkpoint from a fresh durable connection without calling
    /// the `RuntimePersistence` session read path.
    #[cfg(any(test, feature = "testing"))]
    pub fn raw_checkpoint_from_path_for_testing(
        path: &std::path::Path,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let connection = Connection::open(path).map_err(sqlite_error)?;
        let fleet = crate::fleet_format::recorded_or_current(&connection).map_err(sqlite_error)?;
        Self::get_checkpoint_conn(&connection, blob_ref, fleet)
    }

    pub(crate) fn insert_artifact_blob_conn(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
        profile: BuiltinBlobProfile,
    ) -> rusqlite::Result<BlobRef> {
        Self::insert_artifact_blob_conn_typed(conn, descriptor, content, profile)
            .map_err(sqlite_conversion_error)
    }

    fn insert_artifact_blob_conn_typed(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
        profile: BuiltinBlobProfile,
    ) -> Result<BlobRef, StoreError> {
        let blob_ref = BlobRef(blob_content_hash(content));
        Self::insert_artifact_blob_conn_typed_with_ref(
            conn, descriptor, content, profile, &blob_ref,
        )?;
        Ok(blob_ref)
    }

    fn insert_artifact_blob_conn_typed_with_ref(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
        profile: BuiltinBlobProfile,
        blob_ref: &BlobRef,
    ) -> Result<(), StoreError> {
        let stored = encode_artifact_blob(&descriptor, profile, content)?;
        conn.execute(
            artifact_sql().blobs_sqlite.insert_ignore.sql(),
            params![blob_ref.as_str(), stored],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    pub(crate) fn put_typed_artifact_blob_conn<T: serde::Serialize>(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        value: &T,
        profile: BuiltinBlobProfile,
    ) -> Result<BlobRef, StoreError> {
        let bytes = encode_msgpack(value, "SQLite typed artifact blob")?;
        Self::insert_artifact_blob_conn_typed(conn, descriptor, &bytes, profile)
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
            .call(move |conn| Self::insert_artifact_blob_conn(conn, descriptor, &content, profile))
            .await
            .map_err(sqlite_error)
    }

    /// Persist the complete checkpoint root and every changed leaf inside the
    /// caller's commit transaction. This is the GC-safety argument: no
    /// collector can observe the git-loose-object race where a leaf exists
    /// without its root, or a root becomes visible before all leaves exist.
    pub(crate) fn put_checkpoint_conn(
        conn: &Connection,
        checkpoint: &HydratedSessionCheckpoint,
        profile: BuiltinBlobProfile,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<StoredSessionCheckpoint, StoreError> {
        Self::validate_checkpoint_component_refs_conn(conn, checkpoint)?;
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
        let checkpoint_ref = Self::put_typed_artifact_blob_conn(
            conn,
            BlobArtifactDescriptor::checkpoint_manifest(),
            &manifest,
            profile,
        )?;
        let component_refs_json = encode_json(
            &manifest
                .components
                .values()
                .map(|descriptor| descriptor.blob_ref.as_str())
                .collect::<Vec<_>>(),
        )?;
        conn.execute(
            crate::session_sql::session_sql()
                .checkpoint_edges
                .insert_batch
                .sql(),
            params![checkpoint_ref.as_str(), component_refs_json],
        )
        .map_err(sqlite_error)?;
        Ok(StoredSessionCheckpoint {
            checkpoint_ref,
            manifest,
        })
    }

    pub(crate) fn validate_checkpoint_component_refs_conn(
        conn: &Connection,
        checkpoint: &HydratedSessionCheckpoint,
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
        let existing = Self::existing_checkpoint_component_refs_conn(conn, &referenced)?;
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
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut existing = std::collections::HashSet::with_capacity(blob_refs.len());
        let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
        for chunk in blob_refs.chunks(Self::CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
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
    ) -> Result<std::collections::HashMap<String, std::sync::Arc<[u8]>>, StoreError> {
        let blob_refs = checkpoint
            .components
            .values()
            .map(|descriptor| descriptor.blob_ref.as_str().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let mut bodies = std::collections::HashMap::with_capacity(blob_refs.len());
        let blob_refs = blob_refs.iter().map(String::as_str).collect::<Vec<_>>();
        for chunk in blob_refs.chunks(Self::CHECKPOINT_COMPONENT_REF_CHUNK_SIZE) {
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

    /// `fleet` is the store's recorded `F`: the manifest and its component
    /// encodings admit the `[N-1, N]` reader window `F` names (FIG-3796).
    pub(crate) fn get_checkpoint_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
        fleet: lash_core_execution::FleetFormat,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let Some(bytes) = Self::get_blob_conn(conn, blob_ref)? else {
            return Ok(None);
        };
        let record = decode_checkpoint_for_fleet(&bytes, fleet)?;
        record.validate_component_encoding_versions_for_fleet(fleet)?;
        let bodies = Self::checkpoint_component_bodies_conn(conn, &record)?;
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
        }))
    }

    pub(crate) fn load_usage_deltas_conn(
        conn: &Connection,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::TokenLedgerEntry>, StoreError> {
        let mut stmt = conn
            .prepare(
                crate::session_sql::session_sql()
                    .usage
                    .select_for_session
                    .sql(),
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![session_id.as_str()], |row| {
                let usage = lash_core_execution::TokenUsage {
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    cache_read_input_tokens: row.get(4)?,
                    cache_write_input_tokens: row.get(5)?,
                    reasoning_output_tokens: row.get(6)?,
                };
                let usage_disposition_json: String = row.get(7)?;
                Ok((
                    lash_core_execution::TokenLedgerEntry {
                        source: row.get(0)?,
                        model: row.get(1)?,
                        usage,
                        usage_disposition: lash_core_execution::LedgerUsageDisposition::Reported,
                    },
                    usage_disposition_json,
                ))
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?
            .into_iter()
            .map(|(mut entry, usage_disposition_json)| {
                // Strict: a disposition we cannot decode is a refusal, never a
                // silent `Reported` — that downgrade turns a billed call free.
                entry.usage_disposition = decode_usage_disposition(&usage_disposition_json)?;
                Ok(entry)
            })
            .collect()
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
        let self_fleet = self.fleet_format;
        self.conn
            .write_flow(move |tx| {
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
        let fleet = self.fleet_format;
        self.conn
            .call(move |conn| {
                Self::get_checkpoint_conn(conn, &blob_ref, fleet).map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }

    pub async fn load_usage_deltas(
        &self,
    ) -> Result<Vec<lash_core_execution::TokenLedgerEntry>, StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .call(move |conn| {
                Self::load_usage_deltas_conn(conn, &session_id).map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }
}

/// Rows written before the column existed cannot exist: the column is `NOT NULL` and version
/// 52 catalogs are refused outright, so every value here was written by this encoding.
pub(crate) fn decode_usage_disposition(
    stored: &str,
) -> Result<lash_core_execution::LedgerUsageDisposition, StoreError> {
    let disposition: lash_core_execution::LedgerUsageDisposition = serde_json::from_str(stored)
        .map_err(|error| {
            stored_data_corrupt(
                "TokenLedgerEntry",
                format_args!("failed to decode usage disposition: {error}"),
            )
        })?;
    disposition.validate().map_err(|error| {
        stored_data_corrupt(
            "TokenLedgerEntry",
            format_args!("persisted usage disposition is invalid: {error}"),
        )
    })?;
    Ok(disposition)
}

/// Encode one usage disposition for the durable column.
pub(crate) fn encode_usage_disposition(
    disposition: &lash_core_execution::LedgerUsageDisposition,
) -> Result<String, StoreError> {
    serde_json::to_string(disposition).map_err(|error| {
        StoreError::Backend(format!("failed to encode usage disposition: {error}"))
    })
}
