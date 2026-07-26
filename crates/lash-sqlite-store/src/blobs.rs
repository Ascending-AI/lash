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

    pub(crate) fn get_blob_conn(conn: &Connection, blob_ref: &BlobRef) -> Option<Vec<u8>> {
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT content FROM blobs WHERE hash = ?1",
                params![blob_ref.as_str()],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()?;
        decode_artifact_blob(&bytes).or(Some(bytes))
    }

    pub(crate) fn get_typed_blob_conn<T: serde::de::DeserializeOwned>(
        conn: &Connection,
        blob_ref: &BlobRef,
    ) -> Option<T> {
        let bytes = Self::get_blob_conn(conn, blob_ref)?;
        decode_msgpack(&bytes)
    }

    pub(crate) fn get_checkpoint_conn(
        conn: &Connection,
        blob_ref: &BlobRef,
    ) -> Result<Option<HydratedSessionCheckpoint>, StoreError> {
        let Some(bytes) = Self::get_blob_conn(conn, blob_ref) else {
            return Ok(None);
        };
        let record = decode_checkpoint(&bytes)?;
        Ok(Some(HydratedSessionCheckpoint {
            turn_state: record.turn_state,
            tool_state_ref: record.tool_state_ref.clone(),
            tool_state: record
                .tool_state_ref
                .as_ref()
                .and_then(|blob_ref| Self::get_typed_blob_conn(conn, blob_ref)),
            plugin_snapshot_ref: record.plugin_snapshot_ref.clone(),
            plugin_snapshot: record
                .plugin_snapshot_ref
                .as_ref()
                .and_then(|blob_ref| Self::get_typed_blob_conn(conn, blob_ref)),
            plugin_snapshot_revision: record.plugin_snapshot_revision,
            execution_state_ref: record.execution_state_ref.clone(),
            execution_state: record
                .execution_state_ref
                .as_ref()
                .and_then(|blob_ref| Self::get_typed_blob_conn(conn, blob_ref)),
        }))
    }

    pub(crate) fn load_usage_deltas_conn(
        conn: &Connection,
        session_id: &str,
    ) -> Vec<lash_core::TokenLedgerEntry> {
        let mut stmt = match conn.prepare(
            "SELECT source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens
             FROM usage_deltas WHERE session_id = ?1 ORDER BY seq ASC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![session_id], |row| {
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
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(Result::ok).collect()
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

    pub async fn get_blob(&self, blob_ref: &BlobRef) -> Option<Vec<u8>> {
        let blob_ref = blob_ref.clone();
        self.conn
            .call(move |conn| Ok(Self::get_blob_conn(conn, &blob_ref)))
            .await
            .ok()
            .flatten()
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
    ) -> Option<T> {
        let bytes = self.get_blob(blob_ref).await?;
        decode_msgpack(&bytes)
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

    pub async fn get_checkpoint(&self, blob_ref: &BlobRef) -> Option<HydratedSessionCheckpoint> {
        let blob_ref = blob_ref.clone();
        self.conn
            .call(move |conn| Ok(Self::get_checkpoint_conn(conn, &blob_ref)))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    pub async fn append_usage_deltas(&self, entries: &[lash_core::TokenLedgerEntry]) {
        if entries.is_empty() {
            return;
        }
        let Ok(session_id) = self.selected_session_id() else {
            tracing::warn!("cannot append usage on an unbound SQLite session store");
            return;
        };
        let entries = entries.to_vec();
        let result = self
            .conn
            .write(move |tx| {
                let mut stmt = tx.prepare(
                    "INSERT INTO usage_deltas (
                        session_id, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )?;
                for entry in &entries {
                    stmt.execute(params![
                        session_id,
                        entry.source,
                        entry.model,
                        entry.usage.input_tokens,
                        entry.usage.output_tokens,
                        entry.usage.cache_read_input_tokens,
                        entry.usage.cache_write_input_tokens,
                        entry.usage.reasoning_output_tokens,
                    ])?;
                }
                Ok(())
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "failed to persist usage deltas");
        }
    }

    pub async fn load_usage_deltas(&self) -> Vec<lash_core::TokenLedgerEntry> {
        let Ok(session_id) = self.selected_session_id() else {
            return Vec::new();
        };
        self.conn
            .call(move |conn| Ok(Self::load_usage_deltas_conn(conn, &session_id)))
            .await
            .unwrap_or_default()
    }
}
