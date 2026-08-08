//! Content-addressed blob, artifact, checkpoint, and usage-ledger storage on
//! [`Store`].
//!
//! Reference module (with `lifecycle.rs`) for the translation pattern. The
//! `*_conn` helpers here are **synchronous** and take a `&rusqlite::Connection`
//! so they can be reused from inside any `conn.call`/`conn.write` closure (the
//! checkpoint/persistence/graph modules call them while already on the
//! connection thread). The public async methods wrap a single helper call in
//! `self.conn.call(...)`.

use super::*;

/// Hex SHA-256 content address that keys every row in the `blobs` table.
fn blob_content_hash(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

impl Store {
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
        let hash = blob_content_hash(content);
        let stored = encode_artifact_blob(&descriptor, profile, content);
        conn.execute(
            "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, ?2)",
            params![hash, stored],
        )?;
        Ok(BlobRef(hash))
    }

    pub(crate) fn put_typed_artifact_blob_conn<T: serde::Serialize>(
        conn: &Connection,
        descriptor: BlobArtifactDescriptor,
        value: &T,
        profile: BuiltinBlobProfile,
    ) -> rusqlite::Result<BlobRef> {
        let bytes = encode_msgpack(value);
        Self::insert_artifact_blob_conn(conn, descriptor, &bytes, profile)
    }

    pub(crate) fn put_checkpoint_conn(
        conn: &Connection,
        checkpoint: &HydratedSessionCheckpoint,
        profile: BuiltinBlobProfile,
    ) -> rusqlite::Result<StoredSessionCheckpoint> {
        let tool_state_ref = match checkpoint.tool_state.as_ref() {
            Some(snapshot) => Some(Self::put_typed_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::tool_state_snapshot(),
                snapshot,
                profile,
            )?),
            None => checkpoint.tool_state_ref.clone(),
        };
        let plugin_snapshot_ref = match checkpoint.plugin_snapshot.as_ref() {
            Some(snapshot) => Some(Self::put_typed_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::plugin_session_snapshot(),
                snapshot,
                profile,
            )?),
            None => checkpoint.plugin_snapshot_ref.clone(),
        };
        let execution_state_ref = match checkpoint.execution_state.as_ref() {
            Some(snapshot) => Some(Self::put_typed_artifact_blob_conn(
                conn,
                BlobArtifactDescriptor::execution_state_snapshot(),
                snapshot,
                profile,
            )?),
            None => checkpoint.execution_state_ref.clone(),
        };
        let manifest = SessionCheckpoint::new(
            checkpoint.turn_state.clone(),
            tool_state_ref,
            plugin_snapshot_ref,
            checkpoint.plugin_snapshot_revision,
            execution_state_ref,
        );
        let checkpoint_ref = Self::put_typed_artifact_blob_conn(
            conn,
            BlobArtifactDescriptor::checkpoint_manifest(),
            &manifest,
            profile,
        )?;
        Ok(StoredSessionCheckpoint {
            checkpoint_ref,
            manifest,
        })
    }

    pub(crate) fn validate_checkpoint_component_refs_conn(
        conn: &Connection,
        checkpoint: &HydratedSessionCheckpoint,
    ) -> Result<(), StoreError> {
        for (component, body_is_present, blob_ref) in [
            (
                "tool-state",
                checkpoint.tool_state.is_some(),
                checkpoint.tool_state_ref.as_ref(),
            ),
            (
                "plugin-snapshot",
                checkpoint.plugin_snapshot.is_some(),
                checkpoint.plugin_snapshot_ref.as_ref(),
            ),
            (
                "execution-state",
                checkpoint.execution_state.is_some(),
                checkpoint.execution_state_ref.as_ref(),
            ),
        ] {
            let Some(blob_ref) = blob_ref.filter(|_| !body_is_present) else {
                continue;
            };
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM blobs WHERE hash = ?1)",
                    params![blob_ref.as_str()],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            if !exists {
                return Err(StoreError::CheckpointComponentMissing {
                    component,
                    blob_ref: blob_ref.clone(),
                });
            }
        }
        Ok(())
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
        bytes
            .map(|bytes| decode_artifact_blob(&bytes).map(|decoded| decoded.unwrap_or(bytes)))
            .transpose()
    }

    fn get_checkpoint_component_conn<T: serde::de::DeserializeOwned>(
        conn: &Connection,
        component: &'static str,
        blob_ref: Option<&BlobRef>,
    ) -> Result<Option<T>, StoreError> {
        let Some(blob_ref) = blob_ref else {
            return Ok(None);
        };
        let bytes = Self::get_blob_conn(conn, blob_ref)?.ok_or_else(|| {
            StoreError::CheckpointComponentMissing {
                component,
                blob_ref: blob_ref.clone(),
            }
        })?;
        decode_msgpack(&bytes).map(Some).ok_or_else(|| {
            stored_data_corrupt(
                "SessionCheckpoint component",
                format_args!("failed to decode {component} component `{blob_ref}`"),
            )
        })
    }

    pub(crate) fn get_checkpoint_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let Some(bytes) = Self::get_blob_conn(conn, blob_ref)? else {
            return Ok(None);
        };
        let record = decode_checkpoint(&bytes)?;
        Ok(Some(HydratedSessionCheckpoint {
            turn_state: record.turn_state,
            tool_state_ref: record.tool_state_ref.clone(),
            tool_state: Self::get_checkpoint_component_conn(
                conn,
                "tool-state",
                record.tool_state_ref.as_ref(),
            )?,
            plugin_snapshot_ref: record.plugin_snapshot_ref.clone(),
            plugin_snapshot: Self::get_checkpoint_component_conn(
                conn,
                "plugin-snapshot",
                record.plugin_snapshot_ref.as_ref(),
            )?,
            plugin_snapshot_revision: record.plugin_snapshot_revision,
            execution_state_ref: record.execution_state_ref.clone(),
            execution_state: Self::get_checkpoint_component_conn(
                conn,
                "execution-state",
                record.execution_state_ref.as_ref(),
            )?,
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
                Ok(lash_core::TokenLedgerEntry {
                    source: row.get(0)?,
                    model: row.get(1)?,
                    usage: lash_core::TokenUsage {
                        input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        cache_read_input_tokens: row.get(4)?,
                        cache_write_input_tokens: row.get(5)?,
                        reasoning_output_tokens: row.get(6)?,
                    },
                })
            })
            .map_err(sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)
    }

    /// Persist `stored` bytes under `hash` in the `blobs` table, warning with
    /// `warn_label` (and dropping the row) if the write fails.
    async fn insert_blob_row(&self, hash: String, stored: Vec<u8>, warn_label: &str) -> BlobRef {
        let hash_for_row = hash.clone();
        let result = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, ?2)",
                    params![hash_for_row, stored],
                )
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, hash, "{warn_label}");
        }
        BlobRef(hash)
    }

    pub async fn put_blob(&self, content: &[u8]) -> BlobRef {
        let hash = blob_content_hash(content);
        self.insert_blob_row(hash, content.to_vec(), "failed to persist checkpoint blob")
            .await
    }

    pub async fn put_artifact_blob(
        &self,
        descriptor: BlobArtifactDescriptor,
        content: &[u8],
    ) -> BlobRef {
        let hash = blob_content_hash(content);
        let stored = encode_artifact_blob(&descriptor, self.options.blob_profile, content);
        self.insert_blob_row(hash, stored, "failed to persist artifact blob")
            .await
    }

    pub async fn get_blob(&self, blob_ref: &BlobRef) -> Result<Option<Vec<u8>>, StoreError> {
        let blob_ref = blob_ref.clone();
        self.conn
            .call(move |conn| Self::get_blob_conn(conn, &blob_ref).map_err(sqlite_conversion_error))
            .await
            .map_err(sqlite_error)
    }

    pub async fn put_typed_blob<T: serde::Serialize>(&self, value: &T) -> BlobRef {
        let bytes = encode_msgpack(value);
        self.put_blob(&bytes).await
    }

    pub async fn put_typed_artifact_blob<T: serde::Serialize>(
        &self,
        descriptor: BlobArtifactDescriptor,
        value: &T,
    ) -> BlobRef {
        let bytes = encode_msgpack(value);
        self.put_artifact_blob(descriptor, &bytes).await
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

    pub async fn put_checkpoint(
        &self,
        checkpoint: &HydratedSessionCheckpoint,
    ) -> StoredSessionCheckpoint {
        let checkpoint = checkpoint.clone();
        let profile = self.options.blob_profile;
        self.conn
            .write(move |tx| Self::put_checkpoint_conn(tx, &checkpoint, profile))
            .await
            .expect("checkpoint blob should persist")
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
