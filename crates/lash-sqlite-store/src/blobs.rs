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
    #[doc(hidden)]
    #[cfg(any(test, feature = "testing"))]
    pub fn raw_checkpoint_from_path_for_testing(
        path: &std::path::Path,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let connection = Connection::open(path).map_err(sqlite_error)?;
        Self::get_checkpoint_conn(&connection, blob_ref)
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
        let hash = blob_content_hash(content);
        let stored = encode_artifact_blob(&descriptor, profile, content)?;
        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, ?2)",
            params![hash, stored],
        )
        .map_err(sqlite_error)?;
        Ok(BlobRef(hash))
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
    #[doc(hidden)]
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
    ) -> Result<StoredSessionCheckpoint, StoreError> {
        Self::validate_checkpoint_component_refs_conn(conn, checkpoint)?;
        let manifest = checkpoint.manifest()?;
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
                let stored_ref = Self::insert_artifact_blob_conn_typed(
                    conn,
                    BlobArtifactDescriptor::checkpoint_component(),
                    body,
                    profile,
                )?;
                #[cfg(feature = "perf-witness")]
                lash_core::perf_witness::record_hash_pass(body.len());
                lash_core::store::ensure_checkpoint_component_hash_agreement(
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
            "INSERT OR IGNORE INTO checkpoint_blob_refs (checkpoint_ref, blob_ref)
             SELECT ?1, CAST(value AS TEXT) FROM json_each(?2)",
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
            lash_core::store::ensure_checkpoint_component_encoding_version(
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
                .prepare(
                    "SELECT hash FROM blobs
                     WHERE hash IN (SELECT value FROM json_each(?1))",
                )
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
    ) -> Result<std::collections::HashMap<String, Vec<u8>>, StoreError> {
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
                .prepare(
                    "SELECT hash, content FROM blobs
                     WHERE hash IN (SELECT value FROM json_each(?1))",
                )
                .map_err(sqlite_error)?;
            let rows = statement
                .query_map(params![encoded], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(sqlite_error)?;
            for row in rows {
                let (hash, bytes) = row.map_err(sqlite_error)?;
                let body = decode_artifact_blob(&bytes)?;
                bodies.insert(hash, body);
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
                "SELECT content FROM blobs WHERE hash = ?1",
                params![blob_ref.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        bytes.map(|bytes| decode_artifact_blob(&bytes)).transpose()
    }

    pub(crate) fn get_checkpoint_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let Some(bytes) = Self::get_blob_conn(conn, blob_ref)? else {
            return Ok(None);
        };
        let record = decode_checkpoint(&bytes)?;
        record.validate_component_encoding_versions()?;
        let bodies = Self::checkpoint_component_bodies_conn(conn, &record)?;
        let mut components = std::collections::BTreeMap::new();
        for (key, descriptor) in &record.components {
            let body = bodies.get(descriptor.blob_ref.as_str()).ok_or_else(|| {
                StoreError::CheckpointComponentMissing {
                    key: key.clone(),
                    blob_ref: descriptor.blob_ref.clone(),
                }
            })?;
            let bytes = body.clone();
            #[cfg(feature = "perf-witness")]
            lash_core::perf_witness::record_body_copy(body.len());
            components.insert(
                key.clone(),
                lash_core::HydratedCheckpointComponent::hydrated(descriptor.clone(), bytes),
            );
        }
        Ok(Some(HydratedSessionCheckpoint {
            turn_state: record.turn_state,
            components,
            plugin_snapshot_revision: record.plugin_snapshot_revision,
        }))
    }

    pub(crate) fn load_usage_deltas_conn(
        conn: &Connection,
        session_id: &str,
    ) -> Result<Vec<lash_core::TokenLedgerEntry>, StoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens
             FROM usage_deltas WHERE session_id = ?1 ORDER BY seq ASC",
            )
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                let usage = lash_core::TokenUsage {
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    cache_read_input_tokens: row.get(4)?,
                    cache_write_input_tokens: row.get(5)?,
                    reasoning_output_tokens: row.get(6)?,
                };
                Ok(lash_core::TokenLedgerEntry {
                    source: row.get(0)?,
                    model: row.get(1)?,
                    usage,
                })
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
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
                Ok(match Self::put_checkpoint_conn(tx, &checkpoint, profile) {
                    Ok(stored) => TxOutcome::Commit(Ok(stored)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    pub async fn get_checkpoint(
        &self,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let blob_ref = blob_ref.clone();
        self.conn
            .call(move |conn| {
                Self::get_checkpoint_conn(conn, &blob_ref).map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }

    pub async fn load_usage_deltas(&self) -> Result<Vec<lash_core::TokenLedgerEntry>, StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .call(move |conn| {
                Self::load_usage_deltas_conn(conn, &session_id).map_err(sqlite_conversion_error)
            })
            .await
            .map_err(sqlite_error)
    }
}
