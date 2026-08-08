//! The lashlang module-artifact store and the attachment write-ahead manifest.
//!
//! Both traits in this module have synchronous-looking call sites in their
//! consumers but bridge to the async [`SqliteConnection`] underneath:
//!
//! * [`lashlang::LashlangArtifactStore`] is itself an `#[async_trait]`, so its
//!   methods `.await` the connection wrapper directly (matching the the prior store
//!   store's async surface byte-for-byte).
//! * [`AttachmentManifest`] is a *synchronous* trait. Its bodies therefore wrap
//!   the async store work in [`block_on_store`], exactly as the prior store did.
//!
//! Every DB body is a synchronous rusqlite closure handed to `conn.call`
//! (reads) or `conn.write` (read-then-write); only the wrapper call is awaited.

use super::*;

/// Logical keyspaces multiplexed onto the `artifact_refs` pointer table. Each
/// namespace owns its own half of the `(namespace, artifact_ref)` composite
/// primary key. The `blobs` table is content-addressed, but the `artifact_refs`
/// pointer is *not*: without the namespace column, a module ref that collides
/// with a process-execution-env ref would rewrite the same pointer row under
/// `INSERT OR REPLACE`, so content-addressing alone does not keep the namespaces
/// disjoint. The composite key does.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
pub(crate) const RAW_ARTIFACT_NAMESPACE: &str = "lashlang_artifact";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";
pub(crate) const CURRENT_TRIGGER_MANIFEST_NAMESPACE: &str = "lashlang_trigger_manifest";

impl Store {
    async fn put_artifact_ref_blob(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        descriptor: BlobArtifactDescriptor,
        bytes: Vec<u8>,
    ) -> Result<(), StoreError> {
        let blob_profile = self.options.blob_profile;
        self.conn
            .write(move |tx| {
                let blob_ref =
                    Self::insert_artifact_blob_conn(tx, descriptor, &bytes, blob_profile)?;
                tx.execute(
                    "INSERT OR REPLACE INTO artifact_refs (namespace, artifact_ref, blob_ref)
                     VALUES (?1, ?2, ?3)",
                    params![namespace, artifact_ref, blob_ref.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn get_artifact_ref_blob(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        missing_diagnostic: String,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let resolved = self
            .conn
            .call(move |conn| {
                let blob_ref: Option<String> = conn
                    .query_row(
                        "SELECT blob_ref FROM artifact_refs
                         WHERE namespace = ?1 AND artifact_ref = ?2",
                        params![namespace, artifact_ref],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let Some(blob_ref) = blob_ref else {
                    return Ok(None);
                };
                Ok(Some(
                    Self::get_blob_conn(conn, &BlobRef(blob_ref))
                        .map_err(sqlite_conversion_error)?,
                ))
            })
            .await
            .map_err(sqlite_error)?;
        let Some(blob) = resolved else {
            return Ok(None);
        };
        blob.ok_or_else(|| {
            stored_data_corrupt(
                "artifact reference",
                format_args!("{missing_diagnostic} points at a missing blob"),
            )
        })
        .map(Some)
    }
}

#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for Store {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn put_module_artifact(
        &self,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        let bytes = artifact
            .to_store_bytes()
            .map_err(|err| lashlang::ArtifactStoreError::Encode(err.to_string()))?;
        let artifact_ref = artifact.module_ref.as_str().to_string();
        self.put_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            artifact_ref,
            BlobArtifactDescriptor::lashlang_module(),
            bytes,
        )
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        self.artifact_cache
            .lock()
            .map_err(|_| {
                lashlang::ArtifactStoreError::Backend("artifact cache lock poisoned".to_string())
            })?
            .insert(artifact.module_ref.clone(), Arc::new(artifact.clone()));
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
        if let Some(artifact) = self
            .artifact_cache
            .lock()
            .map_err(|_| {
                lashlang::ArtifactStoreError::Backend("artifact cache lock poisoned".to_string())
            })?
            .get(module_ref)
            .cloned()
        {
            return Ok(Some(artifact));
        }

        let artifact_ref = module_ref.as_str().to_string();
        let Some(bytes) = self
            .get_artifact_ref_blob(
                MODULE_ARTIFACT_NAMESPACE,
                artifact_ref,
                format!("lashlang module artifact `{module_ref}`"),
            )
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?
        else {
            return Ok(None);
        };
        let artifact = Arc::new(
            lashlang::ModuleArtifact::from_store_bytes(&bytes)
                .map_err(lashlang::ArtifactStoreError::from)?,
        );
        self.artifact_cache
            .lock()
            .map_err(|_| {
                lashlang::ArtifactStoreError::Backend("artifact cache lock poisoned".to_string())
            })?
            .insert(module_ref.clone(), artifact.clone());
        Ok(Some(artifact))
    }

    async fn replace_current_trigger_manifest(
        &self,
        owner_namespace: &str,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<lashlang::TriggerManifestReplacement, lashlang::ArtifactStoreError> {
        let current = lashlang::CurrentTriggerKeyManifest {
            module_ref: artifact.module_ref.clone(),
            manifest: artifact.trigger_key_manifest.clone(),
        };
        let bytes = serde_json::to_vec(&current)
            .map_err(|err| lashlang::ArtifactStoreError::Encode(err.to_string()))?;
        let owner_namespace = owner_namespace.to_string();
        let blob_profile = self.options.blob_profile;
        let previous_bytes = self
            .conn
            .write(move |tx| {
                let previous_blob_ref: Option<String> = tx
                    .query_row(
                        "SELECT blob_ref FROM artifact_refs
                         WHERE namespace = ?1 AND artifact_ref = ?2",
                        params![CURRENT_TRIGGER_MANIFEST_NAMESPACE, owner_namespace.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let previous_bytes = match previous_blob_ref {
                    // This pointer is GC-rooted, so a missing blob proves durable
                    // corruption. Keep the hard error: silently replacing it would
                    // recreate the self-healing defect class FIG-1031 removes. An
                    // operator can recover by deleting the corrupt artifact_refs row.
                    Some(blob_ref) => Some(
                        Self::get_blob_conn(tx, &BlobRef(blob_ref.clone()))
                            .map_err(sqlite_conversion_error)?
                            .ok_or_else(|| {
                                sqlite_conversion_error(stored_data_corrupt(
                                    "artifact reference",
                                    format_args!(
                                        "current trigger manifest points at missing blob `{blob_ref}`"
                                    ),
                                ))
                            })?,
                    ),
                    None => None,
                };
                let blob_ref = Self::insert_artifact_blob_conn(
                    tx,
                    BlobArtifactDescriptor::lashlang_module(),
                    &bytes,
                    blob_profile,
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO artifact_refs (namespace, artifact_ref, blob_ref)
                     VALUES (?1, ?2, ?3)",
                    params![
                        CURRENT_TRIGGER_MANIFEST_NAMESPACE,
                        owner_namespace.as_str(),
                        blob_ref.as_str()
                    ],
                )?;
                Ok(previous_bytes)
            })
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        let previous = previous_bytes
            .map(|bytes| {
                serde_json::from_slice::<lashlang::CurrentTriggerKeyManifest>(&bytes)
                    .map_err(|err| lashlang::ArtifactStoreError::Decode(err.to_string()))
            })
            .transpose()?;
        Ok(lashlang::TriggerManifestReplacement {
            previous_module_ref: previous.as_ref().map(|entry| entry.module_ref.clone()),
            current_module_ref: artifact.module_ref.clone(),
            diff: previous
                .map(|entry| entry.manifest.diff(&artifact.trigger_key_manifest))
                .unwrap_or_default(),
        })
    }

    async fn get_current_trigger_manifest(
        &self,
        owner_namespace: &str,
    ) -> Result<Option<lashlang::CurrentTriggerKeyManifest>, lashlang::ArtifactStoreError> {
        let bytes = self
            .get_artifact_ref_blob(
                CURRENT_TRIGGER_MANIFEST_NAMESPACE,
                owner_namespace.to_string(),
                format!("current trigger manifest `{owner_namespace}`"),
            )
            .await
            .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        bytes
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|err| lashlang::ArtifactStoreError::Decode(err.to_string()))
            })
            .transpose()
    }

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), lashlang::ArtifactStoreError> {
        let artifact_ref = artifact_ref.to_string();
        let descriptor = match descriptor {
            "process_execution_env" => BlobArtifactDescriptor::process_execution_env(),
            _ => BlobArtifactDescriptor::new(PersistedArtifactKind::GenericBlob, Vec::new()),
        };
        self.put_artifact_ref_blob(
            RAW_ARTIFACT_NAMESPACE,
            artifact_ref,
            descriptor,
            bytes.to_vec(),
        )
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }

    async fn get_artifact_bytes(
        &self,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, lashlang::ArtifactStoreError> {
        let artifact_ref = artifact_ref.to_string();
        self.get_artifact_ref_blob(
            RAW_ARTIFACT_NAMESPACE,
            artifact_ref.clone(),
            format!("artifact `{artifact_ref}`"),
        )
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for Store {
    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        let artifact_ref = env_ref.as_str().to_string();
        self.put_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            artifact_ref,
            BlobArtifactDescriptor::process_execution_env(),
            bytes.to_vec(),
        )
        .await
        .map_err(|err| lash_core::PluginError::Session(err.to_string()))
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        let artifact_ref = env_ref.as_str().to_string();
        self.get_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            artifact_ref.clone(),
            format!("process execution env `{artifact_ref}`"),
        )
        .await
        .map_err(|err| lash_core::PluginError::Session(err.to_string()))
    }
}

impl AttachmentManifest for Store {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        block_on_store(async {
            let attachment_id = intent.attachment_id.as_str().to_string();
            let session_id = intent.session_id.as_str().to_string();
            let canonical_uri = intent.canonical_uri.as_str().to_string();
            let intent_at_ms = intent.intent_at_epoch_ms as i64;
            let owner_kind = intent.owner_kind.map(AttachmentOwnerKind::as_str);
            let owner_id = intent.owner_id;
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                        // Re-recording refreshes the timestamp and durable owner
                        // together. GC later composes this age with owner-death proof.
                        tx.execute(
                            "INSERT INTO attachment_manifest
                            (attachment_id, session_id, canonical_uri, intent_at_ms,
                             committed_at_ms, owner_kind, owner_id)
                         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)
                         ON CONFLICT(session_id, attachment_id) DO UPDATE SET
                            canonical_uri = excluded.canonical_uri,
                            intent_at_ms = excluded.intent_at_ms,
                            owner_kind = excluded.owner_kind,
                            owner_id = excluded.owner_id",
                            params![
                                attachment_id,
                                session_id,
                                canonical_uri,
                                intent_at_ms,
                                owner_kind,
                                owner_id
                            ],
                        )
                        .map_err(sqlite_error)?;
                        Ok(())
                    })();
                    Ok(match outcome {
                        Ok(()) => TxOutcome::Commit(Ok(())),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        })
    }

    fn commit_refs(
        &self,
        session_id: &str,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        if attachment_ids.is_empty() {
            return Ok(());
        }
        block_on_store(async {
            let session_id = session_id.to_string();
            let attachment_ids: Vec<String> = attachment_ids
                .iter()
                .map(|id| id.as_str().to_string())
                .collect();
            let now = self.clock.timestamp_ms() as i64;
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                        let mut stmt = tx
                            .prepare(
                                "UPDATE attachment_manifest
                         SET committed_at_ms = COALESCE(committed_at_ms, ?1)
                         WHERE attachment_id = ?2 AND session_id = ?3",
                            )
                            .map_err(sqlite_error)?;
                        for id in &attachment_ids {
                            stmt.execute(params![now, id, session_id])
                                .map_err(sqlite_error)?;
                        }
                        Ok(())
                    })();
                    Ok(match outcome {
                        Ok(()) => TxOutcome::Commit(Ok(())),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        })
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        block_on_store(async {
            let older_than = older_than_epoch_ms as i64;
            self.conn
                .call(move |conn| {
                    let mut stmt = conn.prepare(
                        "SELECT attachment_id, session_id, canonical_uri, intent_at_ms,
                                committed_at_ms, owner_kind, owner_id
                         FROM attachment_manifest
                         WHERE committed_at_ms IS NULL AND intent_at_ms <= ?1
                         ORDER BY intent_at_ms ASC",
                    )?;
                    let rows = stmt.query_map(params![older_than], |row| {
                        let id: String = row.get(0)?;
                        let session_id: String = row.get(1)?;
                        let canonical_uri: String = row.get(2)?;
                        let intent_at_ms: i64 = row.get(3)?;
                        let committed_at_ms: Option<i64> = row.get(4)?;
                        let owner_kind: Option<String> = row.get(5)?;
                        let owner_id: Option<String> = row.get(6)?;
                        Ok(AttachmentManifestEntry {
                            attachment_id: AttachmentId::new(id),
                            session_id,
                            canonical_uri,
                            intent_at_epoch_ms: intent_at_ms as u64,
                            committed_at_epoch_ms: committed_at_ms.map(|v| v as u64),
                            owner_kind: match owner_kind.as_deref() {
                                Some("turn") => Some(AttachmentOwnerKind::Turn),
                                Some("process") => Some(AttachmentOwnerKind::Process),
                                None => None,
                                Some(value) => {
                                    return Err(sqlite_conversion_error(stored_data_corrupt(
                                        "AttachmentManifest owner kind",
                                        format_args!("unknown value `{value}`"),
                                    )));
                                }
                            },
                            owner_id,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        })
    }

    fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        block_on_store(async {
            let cutoff = intent_grace_cutoff_epoch_ms as i64;
            let process_registry_attached = self.process_registry_attached;
            self.conn
                .write(move |tx| {
                    // One conditional DELETE composes age with owner-death proof.
                    // The attached process DB makes the NOT EXISTS predicate part
                    // of this same SQLite statement/transaction, avoiding a
                    // read-process-then-forget race across the per-session topology.
                    let process_dead = if process_registry_attached {
                        "OR (
                            manifest.owner_kind = 'process'
                            AND NOT EXISTS (
                                SELECT 1 FROM process_registry.processes AS process
                                WHERE process.process_id = manifest.owner_id
                            )
                        )"
                    } else {
                        // Without a configured process registry, conservatively
                        // retain process-owned rows rather than guess liveness.
                        ""
                    };
                    let sql = format!(
                        "DELETE FROM attachment_manifest AS manifest
                         WHERE manifest.committed_at_ms IS NULL
                           AND manifest.intent_at_ms <= ?1
                           AND (
                                manifest.owner_kind IS NULL
                                OR (
                                    manifest.owner_kind = 'turn'
                                    AND EXISTS (
                                        SELECT 1 FROM runtime_turn_commits AS turn_commit
                                        WHERE turn_commit.session_id = manifest.session_id
                                          AND turn_commit.turn_id <> manifest.owner_id
                                          AND turn_commit.committed_at_ms > manifest.intent_at_ms
                                    )
                                )
                                {process_dead}
                           )"
                    );
                    tx.execute(&sql, params![cutoff])?;
                    Ok(())
                })
                .await
                .map_err(sqlite_error)?;
            Ok(())
        })
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        block_on_store(async {
            let attachment_id = attachment_id.as_str().to_string();
            let cutoff = intent_grace_cutoff_epoch_ms as i64;
            let process_registry_attached = self.process_registry_attached;
            self.conn
                .call(move |conn| {
                    let process_dead = if process_registry_attached {
                        "OR (
                            manifest.owner_kind = 'process'
                            AND NOT EXISTS (
                                SELECT 1 FROM process_registry.processes AS process
                                WHERE process.process_id = manifest.owner_id
                            )
                        )"
                    } else {
                        ""
                    };
                    let sql = format!(
                        "SELECT 1 FROM attachment_manifest AS manifest
                         WHERE manifest.attachment_id = ?1
                           AND NOT (
                                manifest.committed_at_ms IS NULL
                                AND manifest.intent_at_ms <= ?2
                                AND (
                                    manifest.owner_kind IS NULL
                                    OR (
                                        manifest.owner_kind = 'turn'
                                        AND EXISTS (
                                            SELECT 1 FROM runtime_turn_commits AS turn_commit
                                            WHERE turn_commit.session_id = manifest.session_id
                                              AND turn_commit.turn_id <> manifest.owner_id
                                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                                        )
                                    )
                                    {process_dead}
                                )
                           )
                         LIMIT 1"
                    );
                    conn.query_row(
                        &sql,
                        params![attachment_id, cutoff],
                        |_| Ok(()),
                    )
                    .optional()
                    .map(|found| found.is_some())
                })
                .await
                .map_err(sqlite_error)
        })
    }

    fn forget(&self, session_id: &str, attachment_id: &AttachmentId) -> Result<(), StoreError> {
        block_on_store(async {
            let session_id = session_id.to_string();
            let attachment_id = attachment_id.as_str().to_string();
            self.conn
                .call(move |conn| {
                    conn.execute(
                        "DELETE FROM attachment_manifest
                         WHERE session_id = ?1 AND attachment_id = ?2",
                        params![session_id, attachment_id],
                    )
                })
                .await
                .map_err(sqlite_error)?;
            Ok(())
        })
    }

    fn holds_ref(
        &self,
        session_id: &str,
        attachment_id: &AttachmentId,
    ) -> Result<bool, StoreError> {
        block_on_store(async {
            let session_id = session_id.to_string();
            let attachment_id = attachment_id.as_str().to_string();
            self.conn
                .call(move |conn| {
                    conn.query_row(
                        "SELECT 1 FROM attachment_manifest
                         WHERE session_id = ?1 AND attachment_id = ?2",
                        params![session_id, attachment_id],
                        |_| Ok(()),
                    )
                    .optional()
                    .map(|found| found.is_some())
                })
                .await
                .map_err(sqlite_error)
        })
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        block_on_store(async {
            self.conn
                .call(move |conn| {
                    let mut stmt =
                        conn.prepare("SELECT DISTINCT attachment_id FROM attachment_manifest")?;
                    let rows = stmt.query_map([], |row| {
                        let id: String = row.get(0)?;
                        Ok(AttachmentId::new(id))
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        })
    }
}
