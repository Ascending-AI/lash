//! The lashlang module-artifact store and the attachment write-ahead manifest.
//!
//! Both traits in this module are `#[async_trait]` surfaces over the async
//! [`SqliteConnection`]: their bodies `.await` the connection wrapper directly
//! on the caller's runtime, with no `block_on` and no thread hop.
//!
//! Every DB body is a synchronous rusqlite closure handed to `conn.call`
//! (reads) or `conn.write` (read-then-write); only the wrapper call is awaited.

use lash_sansio::SessionId;
/// FIG-653: graph retention is a prune precondition for committed attachment roots.
/// Owner-level retention deliberately includes suffix attachments: the manifest
/// has no node edge. Forks and pins keep these rows until their final prefix dies.
pub(crate) const RECLAIM_DELETED_ATTACHMENT_ROOTS: &str =
    "DELETE FROM attachment_manifest AS manifest
 WHERE EXISTS (SELECT 1 FROM deleted_sessions AS deleted
               WHERE deleted.session_id = manifest.session_id)
   AND (manifest.committed_at_ms IS NULL OR NOT EXISTS (
       SELECT 1 FROM graph_nodes AS node
       WHERE node.session_id = manifest.session_id AND node.tombstoned = 0
   ))";

use super::*;
#[cfg(feature = "lashlang")]
use lash_sansio::sync::MutexExt;

/// Logical keyspaces multiplexed onto the `artifact_refs` pointer table. Each
/// namespace owns its own half of the `(namespace, artifact_ref)` composite
/// primary key. The `blobs` table is content-addressed, but the `artifact_refs`
/// pointer is *not*: without the namespace column, a module ref that collides
/// with a process-execution-env ref would rewrite the same pointer row under
/// `INSERT OR REPLACE`, so content-addressing alone does not keep the namespaces
/// disjoint. The composite key does.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

/// Adopt stored references under the boundary transaction.
///
/// Validate every digest for upload evidence first, so a batch containing one
/// unknown digest writes nothing at all, then acquire this session's roots.
pub(crate) fn commit_attachment_refs_conn(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    attachment_ids: &[AttachmentId],
    now: i64,
) -> Result<(), StoreError> {
    let mut evidence = std::collections::BTreeMap::new();
    for id in attachment_ids {
        let deleting = tx
            .query_row(
                "SELECT 1 FROM attachment_condemnations
                 WHERE attachment_id = ?1 AND phase = 'deleting'",
                params![id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sqlite_error)?
            .is_some();
        if deleting {
            return Err(StoreError::UnknownAttachment { digest: id.clone() });
        }
        // Evidence from any session: the uploader and the adopter need not be
        // the same, and the earliest proven upload is the one that is copied.
        let written_at_ms = tx
            .query_row(
                "SELECT MIN(written_at_ms) FROM attachment_manifest
                 WHERE attachment_id = ?1 AND written_at_ms IS NOT NULL",
                params![id.as_str()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(sqlite_error)?
            .flatten();
        let Some(written_at_ms) = written_at_ms else {
            return Err(StoreError::UnknownAttachment { digest: id.clone() });
        };
        evidence.insert(id.clone(), written_at_ms);
    }
    for id in attachment_ids {
        // The fresh committed root supersedes an unarmed, unclaimed
        // condemnation. A restoring writer's claim is left for that writer to
        // settle.
        tx.execute(
            "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND phase = 'condemned' AND write_token IS NULL",
            params![id.as_str()],
        )
        .map_err(sqlite_error)?;
        // Copy the evidence onto the adopter's row so it outlives the
        // uploader's intent being forgotten.
        tx.execute(
            "INSERT INTO attachment_manifest
             (attachment_id, session_id, canonical_uri, intent_at_ms, written_at_ms, committed_at_ms)
             VALUES (?2, ?3, ?4, ?1, ?5, ?1)
             ON CONFLICT (session_id, attachment_id) DO UPDATE
             SET committed_at_ms = COALESCE(attachment_manifest.committed_at_ms, excluded.committed_at_ms),
                 written_at_ms = COALESCE(attachment_manifest.written_at_ms, excluded.written_at_ms)",
            params![
                now,
                id.as_str(),
                session_id.as_str(),
                format!("lash-attachment://blake3/{id}"),
                evidence.get(id).copied(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

impl Store {
    async fn publish_artifact_ref_blob(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        descriptor: BlobArtifactDescriptor,
        bytes: Vec<u8>,
        owner: lash_core::ArtifactOwner,
    ) -> Result<(), StoreError> {
        let blob_profile = self.options.blob_profile;
        self.conn
            .write(move |tx| {
                let (owner_kind, owner_id) = owner
                    .storage_parts()
                    .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
                let retired = tx.query_row(
                    "SELECT EXISTS (
                         SELECT 1 FROM artifact_owner_retirements
                         WHERE owner_kind = ?1 AND owner_id = ?2
                     )",
                    params![owner_kind, owner_id],
                    |row| row.get::<_, bool>(0),
                )?;
                if retired {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        StoreError::ArtifactOwnerRetired,
                    )));
                }
                let blob_ref =
                    Self::insert_artifact_blob_conn(tx, descriptor, &bytes, blob_profile)?;
                tx.execute(
                    "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT (namespace, artifact_ref) DO NOTHING",
                    params![namespace, artifact_ref, blob_ref.as_str()],
                )?;
                let stored_blob_ref: String = tx.query_row(
                    "SELECT blob_ref FROM artifact_refs
                     WHERE namespace = ?1 AND artifact_ref = ?2",
                    params![namespace, artifact_ref],
                    |row| row.get(0),
                )?;
                if stored_blob_ref != blob_ref.as_str() {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "artifact `{artifact_ref}` in namespace `{namespace}` is immutable"
                    )));
                }
                tx.execute(
                    "INSERT INTO artifact_owners
                     (namespace, artifact_ref, owner_kind, owner_id)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT DO NOTHING",
                    params![namespace, artifact_ref, owner_kind, owner_id],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn transfer_artifact_ref_owner(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        from: lash_core::ArtifactOwner,
        to: lash_core::ArtifactOwner,
    ) -> Result<(), StoreError> {
        self.conn
            .write(move |tx| {
                let (from_kind, from_id) = from
                    .storage_parts()
                    .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
                let (to_kind, to_id) = to
                    .storage_parts()
                    .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
                let retired = tx.query_row(
                    "SELECT EXISTS (
                         SELECT 1 FROM artifact_owner_retirements
                         WHERE owner_kind = ?1 AND owner_id = ?2
                     )",
                    params![to_kind, to_id],
                    |row| row.get::<_, bool>(0),
                )?;
                if retired {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        StoreError::ArtifactDestinationOwnerRetired,
                    )));
                }
                let inserted = tx.execute(
                    "INSERT INTO artifact_owners
                     (namespace, artifact_ref, owner_kind, owner_id)
                     SELECT namespace, artifact_ref, ?3, ?4
                     FROM artifact_owners
                     WHERE namespace = ?1 AND artifact_ref = ?2
                       AND owner_kind = ?5 AND owner_id = ?6
                     ON CONFLICT DO NOTHING",
                    params![namespace, artifact_ref, to_kind, to_id, from_kind, from_id],
                )?;
                if inserted == 0 {
                    let destination_exists = tx.query_row(
                        "SELECT EXISTS (
                             SELECT 1 FROM artifact_owners
                             WHERE namespace = ?1 AND artifact_ref = ?2
                               AND owner_kind = ?3 AND owner_id = ?4
                         )",
                        params![namespace, artifact_ref, to_kind, to_id],
                        |row| row.get::<_, bool>(0),
                    )?;
                    if !destination_exists {
                        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                            StoreError::ArtifactStagingEdgeMissing {
                                artifact: format!("artifact `{artifact_ref}`"),
                            },
                        )));
                    }
                }
                tx.execute(
                    "DELETE FROM artifact_owners
                     WHERE namespace = ?1 AND artifact_ref = ?2
                       AND owner_kind = ?3 AND owner_id = ?4",
                    params![namespace, artifact_ref, from_kind, from_id],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    fn reclaim_unowned_artifact_conn(
        tx: &rusqlite::Connection,
        namespace: &str,
        artifact_ref: &str,
    ) -> rusqlite::Result<()> {
        let blob_ref = tx
            .query_row(
                "SELECT blob_ref FROM artifact_refs
                 WHERE namespace = ?1 AND artifact_ref = ?2",
                params![namespace, artifact_ref],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(blob_ref) = blob_ref else {
            return Ok(());
        };
        tx.execute(
            "DELETE FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_owners
                   WHERE namespace = ?1 AND artifact_ref = ?2
               )",
            params![namespace, artifact_ref],
        )?;
        tx.execute(
            "DELETE FROM blobs AS candidate
             WHERE candidate.hash = ?1
               AND NOT EXISTS (SELECT 1 FROM artifact_refs WHERE blob_ref = candidate.hash)
               AND NOT EXISTS (SELECT 1 FROM session_head WHERE checkpoint_ref = candidate.hash)
               AND NOT EXISTS (SELECT 1 FROM node_anchors WHERE checkpoint_ref = candidate.hash)
               AND NOT EXISTS (SELECT 1 FROM checkpoint_blob_refs WHERE blob_ref = candidate.hash)",
            params![blob_ref],
        )?;
        Ok(())
    }

    async fn release_artifact_ref_owner(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        owner: lash_core::ArtifactOwner,
    ) -> Result<(), StoreError> {
        self.conn
            .write(move |tx| {
                let (owner_kind, owner_id) = owner
                    .storage_parts()
                    .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
                tx.execute(
                    "DELETE FROM artifact_owners
                     WHERE namespace = ?1 AND artifact_ref = ?2
                       AND owner_kind = ?3 AND owner_id = ?4",
                    params![namespace, artifact_ref, owner_kind, owner_id],
                )?;
                Self::reclaim_unowned_artifact_conn(tx, namespace, &artifact_ref)
            })
            .await
            .map_err(sqlite_error)
    }

    async fn retire_artifact_owner(
        &self,
        namespace: &'static str,
        owner: lash_core::ArtifactOwner,
    ) -> Result<(), StoreError> {
        self.conn
            .write(move |tx| {
                if !matches!(owner, lash_core::ArtifactOwner::Execution(_)) {
                    return Err(rusqlite::Error::InvalidParameterName(
                        "only execution artifact owners can be retired".to_string(),
                    ));
                }
                let (owner_kind, owner_id) = owner
                    .storage_parts()
                    .map_err(|error| rusqlite::Error::InvalidParameterName(error.to_string()))?;
                tx.execute(
                    "INSERT INTO artifact_owner_retirements (owner_kind, owner_id)
                     VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                    params![owner_kind, owner_id],
                )?;
                let refs = {
                    let mut stmt = tx.prepare(
                        "SELECT artifact_ref FROM artifact_owners
                         WHERE namespace = ?1 AND owner_kind = ?2 AND owner_id = ?3",
                    )?;
                    stmt.query_map(params![namespace, owner_kind, owner_id], |row| row.get(0))?
                        .collect::<Result<Vec<String>, _>>()?
                };
                tx.execute(
                    "DELETE FROM artifact_owners
                     WHERE namespace = ?1 AND owner_kind = ?2 AND owner_id = ?3",
                    params![namespace, owner_kind, owner_id],
                )?;
                for artifact_ref in refs {
                    Self::reclaim_unowned_artifact_conn(tx, namespace, &artifact_ref)?;
                }
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

#[cfg(feature = "lashlang")]
#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for Store {
    fn pause_next_publication_for_testing(&self) -> Option<lashlang::ArtifactPublicationPause> {
        let pause = lashlang::ArtifactPublicationPause::default();
        *self.artifact_publication_pause.lock_recover() = Some(pause.clone());
        Some(pause)
    }

    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact.module_ref.as_str()) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let bytes = artifact
            .to_store_bytes()
            .map_err(|err| lashlang::ArtifactStoreError::Encode(err.to_string()))?;
        let artifact_ref = artifact.module_ref.as_str().to_string();
        let publication_pause = self.artifact_publication_pause.lock_recover().take();
        if let Some(pause) = publication_pause {
            pause.pause().await;
        }
        self.publish_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            artifact_ref,
            BlobArtifactDescriptor::lashlang_module(),
            bytes,
            owner.clone(),
        )
        .await
        .map_err(lashlang::ArtifactStoreError::from)?;
        self.artifact_cache
            .lock_recover()
            .insert(artifact.module_ref.clone(), Arc::new(artifact.clone()));
        Ok(())
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        let bytes = self
            .get_artifact_ref_blob(
                MODULE_ARTIFACT_NAMESPACE,
                module_ref.as_str().to_string(),
                format!("lashlang module artifact `{module_ref}`"),
            )
            .await
            .map_err(lashlang::ArtifactStoreError::from)?
            .ok_or_else(|| {
                lashlang::ArtifactStoreError::Backend(format!(
                    "missing module artifact `{module_ref}`"
                ))
            })?;
        self.publish_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.as_str().to_string(),
            BlobArtifactDescriptor::lashlang_module(),
            bytes,
            owner.clone(),
        )
        .await
        .map_err(lashlang::ArtifactStoreError::from)
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.transfer_artifact_ref_owner(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.as_str().to_string(),
            from.clone(),
            to.clone(),
        )
        .await
        .map_err(lashlang::ArtifactStoreError::from)
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.release_artifact_ref_owner(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.as_str().to_string(),
            owner.clone(),
        )
        .await
        .map_err(lashlang::ArtifactStoreError::from)?;
        self.artifact_cache.lock_recover().remove(module_ref);
        Ok(())
    }

    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        self.retire_artifact_owner(MODULE_ARTIFACT_NAMESPACE, owner.clone())
            .await
            .map_err(lashlang::ArtifactStoreError::from)?;
        self.artifact_cache.lock_recover().clear();
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref.as_str()) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid module reference".into(),
            ));
        }
        let artifact_ref = module_ref.as_str().to_string();
        let Some(bytes) = self
            .get_artifact_ref_blob(
                MODULE_ARTIFACT_NAMESPACE,
                artifact_ref,
                format!("lashlang module artifact `{module_ref}`"),
            )
            .await
            .map_err(lashlang::ArtifactStoreError::from)?
        else {
            self.artifact_cache.lock_recover().remove(module_ref);
            return Ok(None);
        };
        if let Some(artifact) = self.artifact_cache.lock_recover().get(module_ref).cloned() {
            return Ok(Some(artifact));
        }
        let artifact = Arc::new(
            lashlang::ModuleArtifact::from_store_bytes(&bytes)
                .map_err(lashlang::ArtifactStoreError::from)?,
        );
        self.artifact_cache
            .lock_recover()
            .insert(module_ref.clone(), artifact.clone());
        Ok(Some(artifact))
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for Store {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        if !env_ref.matches_store_bytes(bytes) {
            return Err(lash_core::PluginError::Session(format!(
                "process execution environment bytes do not match `{env_ref}`"
            )));
        }
        let artifact_ref = env_ref.as_str().to_string();
        self.publish_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            artifact_ref,
            BlobArtifactDescriptor::process_execution_env(),
            bytes.to_vec(),
            owner.clone(),
        )
        .await
        .map_err(lash_core::artifact_store_plugin_error)
    }

    async fn transfer_process_execution_env(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.transfer_artifact_ref_owner(
            PROCESS_ENV_NAMESPACE,
            env_ref.as_str().to_string(),
            from.clone(),
            to.clone(),
        )
        .await
        .map_err(lash_core::artifact_store_plugin_error)
    }

    async fn release_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<(), lash_core::PluginError> {
        self.release_artifact_ref_owner(
            PROCESS_ENV_NAMESPACE,
            env_ref.as_str().to_string(),
            owner.clone(),
        )
        .await
        .map_err(lash_core::artifact_store_plugin_error)
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> Result<(), lash_core::PluginError> {
        self.retire_artifact_owner(PROCESS_ENV_NAMESPACE, owner.clone())
            .await
            .map_err(lash_core::artifact_store_plugin_error)
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::PluginError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
        let artifact_ref = env_ref.as_str().to_string();
        self.get_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            artifact_ref.clone(),
            format!("process execution env `{artifact_ref}`"),
        )
        .await
        .map_err(lash_core::artifact_store_plugin_error)
    }
}

/// The `EXISTS (...)` body that decides whether one digest still has a live
/// root, parameterised `?1 = attachment_id`, `?2 = intent_grace_cutoff_ms`.
/// Shared by the targeted probe and the condemn CAS so the fence and the probe
/// cannot drift apart.
fn live_ref_exists_sql(process_registry_attached: bool) -> String {
    let turn_owner_kind = AttachmentOwnerKind::Turn.as_str();
    let process_dead = if process_registry_attached {
        let process_owner_kind = AttachmentOwnerKind::Process.as_str();
        format!(
            "OR (
            manifest.owner_kind = '{process_owner_kind}'
            AND NOT EXISTS (
                SELECT 1 FROM process_registry.processes AS process
                WHERE process.process_id = manifest.owner_id
                  AND process.incarnation = manifest.owner_incarnation
            )
        )"
        )
    } else {
        String::new()
    };
    format!(
        "SELECT 1 FROM attachment_manifest AS manifest
         WHERE manifest.attachment_id = ?1
           AND NOT (
                manifest.committed_at_ms IS NULL
                AND manifest.intent_at_ms <= ?2
                AND (
                    manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                    OR (
                        manifest.owner_kind = '{turn_owner_kind}'
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
    )
}

impl Store {
    /// Enumerate the durable condemnation authority without exposing write
    /// tokens. Persisted phase/provenance combinations are decoded strictly so
    /// a corrupt row cannot be mistaken for sweep-owned maintenance work.
    pub(crate) async fn list_attachment_condemnations(
        &self,
    ) -> Result<Vec<lash_core::AttachmentCondemnationRecord>, StoreError> {
        let rows = self
            .conn
            .call(|conn| {
                let mut statement = conn.prepare(
                    "SELECT attachment_id, phase, write_token, write_session_id
                     FROM attachment_condemnations",
                )?;
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        let mut condemnations = rows
            .into_iter()
            .map(|(digest, phase, write_token, write_session_id)| {
                let digest = AttachmentId::parse(&digest).map_err(|error| {
                    stored_data_corrupt(
                        "attachment condemnation",
                        format!("attachment_id is not a valid attachment id: {error}"),
                    )
                })?;
                lash_core::store::decode_attachment_condemnation_record(
                    digest,
                    &phase,
                    write_token.is_some(),
                    write_session_id.map(SessionId::from),
                )
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        condemnations.sort_by(|left, right| left.digest.cmp(&right.digest));
        Ok(condemnations)
    }

    /// `Free -> Condemned` for one digest, conditional on there being no live
    /// root. The root predicate, the existing-condemnation check, and the insert
    /// share one SQLite transaction, so this is one CAS against every concurrent
    /// [`AttachmentManifest::begin_attachment_write`].
    pub(crate) async fn condemn_attachment(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core::AttachmentCondemnation, StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        let cutoff = crate::clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        let live_ref_sql = live_ref_exists_sql(self.process_registry_attached);
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::AttachmentCondemnation, StoreError> = (|| {
                    let rooted = tx
                        .query_row(&live_ref_sql, params![attachment_id, cutoff], |_| Ok(()))
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    if rooted {
                        return Ok(lash_core::AttachmentCondemnation::RootPresent);
                    }
                    let condemned = tx
                        .query_row(
                            "SELECT 1 FROM attachment_condemnations WHERE attachment_id = ?1",
                            params![attachment_id],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    if condemned {
                        return Ok(lash_core::AttachmentCondemnation::AlreadyCondemned);
                    }
                    tx.execute(
                        "INSERT INTO attachment_condemnations (attachment_id, phase)
                         VALUES (?1, 'condemned')",
                        params![attachment_id],
                    )
                    .map_err(sqlite_error)?;
                    // The digest is proven unrooted, so every remaining manifest
                    // row for it is stale evidence of an upload whose bytes this
                    // sweep is about to delete. Clearing them here is what makes
                    // a negative byte-absence tombstone unnecessary.
                    tx.execute(
                        "DELETE FROM attachment_manifest WHERE attachment_id = ?1",
                        params![attachment_id],
                    )
                    .map_err(sqlite_error)?;
                    Ok(lash_core::AttachmentCondemnation::Condemned)
                })(
                );
                Ok(match outcome {
                    Ok(condemnation) => TxOutcome::Commit(Ok(condemnation)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    /// `Condemned -> Deleting`: the CAS that authorizes the physical delete. A
    /// writer that revoked the condemnation removed the row, so the conditional
    /// UPDATE matches nothing and the delete is never issued.
    pub(crate) async fn arm_attachment_delete(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<lash_core::AttachmentDeleteArming, StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        let armed = self
            .conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE attachment_condemnations SET phase = 'deleting'
                     WHERE attachment_id = ?1 AND phase = 'condemned' AND write_token IS NULL",
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(if armed == 1 {
            lash_core::AttachmentDeleteArming::Armed
        } else {
            lash_core::AttachmentDeleteArming::Revoked
        })
    }

    /// Return an abandoned sweep's un-tokened `Condemned` or `Deleting` digest
    /// to `Free`. A stale sweep cannot clear a restoring writer's token.
    pub(crate) async fn release_attachment_condemnation(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM attachment_condemnations
                     WHERE attachment_id = ?1
                       AND (phase = 'deleting'
                            OR (phase = 'condemned' AND write_token IS NULL))",
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Clear an abandoned restoring writer under explicit host quiescence.
    /// Retire `Condemned` only when its associated intent became committed,
    /// otherwise preserve it after removing that unstamped intent.
    pub(crate) async fn recover_abandoned_attachment_write(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    let claim = tx
                        .query_row(
                            "SELECT write_token, write_session_id
                             FROM attachment_condemnations
                             WHERE attachment_id = ?1
                               AND phase = 'condemned'
                               AND write_token IS NOT NULL",
                            params![attachment_id],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some((token, session_id)) = claim else {
                        return Ok(());
                    };
                    tx.execute(
                        "DELETE FROM attachment_manifest
                         WHERE attachment_id = ?1 AND session_id = ?2
                           AND written_at_ms IS NULL AND committed_at_ms IS NULL",
                        params![attachment_id, session_id],
                    )
                    .map_err(sqlite_error)?;
                    let condemned_superseded = tx
                        .execute(
                            "DELETE FROM attachment_condemnations
                             WHERE attachment_id = ?1 AND write_token = ?2
                               AND phase = 'condemned'
                               AND EXISTS (
                                   SELECT 1 FROM attachment_manifest
                                    WHERE attachment_id = ?1 AND session_id = ?3
                                      AND committed_at_ms IS NOT NULL
                               )",
                            params![attachment_id, token, session_id],
                        )
                        .map_err(sqlite_error)?;
                    if condemned_superseded == 0 {
                        tx.execute(
                            "UPDATE attachment_condemnations
                         SET write_token = NULL, write_session_id = NULL
                         WHERE attachment_id = ?1 AND write_token = ?2",
                            params![attachment_id, token],
                        )
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
    }

    /// Delete the condemnation row after the physical delete succeeds: the
    /// digest returns to `Free` holding no upload evidence, because the
    /// condemnation already cleared every manifest row for it.
    pub(crate) async fn retire_attachment_condemnation(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM attachment_condemnations
                     WHERE attachment_id = ?1 AND phase = 'deleting'",
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl AttachmentManifest for Store {
    /// The writer half of the GC fence: the condemnation check, the claim, and
    /// the intent upsert are one SQLite transaction, so a sweeper's condemn CAS
    /// either precedes this whole mutation or fails against the intent it wrote.
    async fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<lash_core::AttachmentWriteFence, StoreError> {
        {
            let attachment_id = intent.attachment_id.as_str().to_string();
            let session_id = intent.session_id.clone();
            let canonical_uri = intent.canonical_uri.as_str().to_string();
            let intent_at_ms = intent.intent_at_epoch_ms as i64;
            let owner_kind = intent.owner.as_ref().map(|owner| owner.kind().as_str());
            let owner_id = intent.owner.as_ref().map(|owner| owner.id().to_string());
            let owner_incarnation = intent
                .owner
                .as_ref()
                .and_then(lash_core::AttachmentOwner::incarnation)
                .map(|incarnation| i64::try_from(incarnation.registration_sequence()))
                .transpose()
                .map_err(|_| {
                    StoreError::Backend("attachment owner incarnation exceeds i64".to_string())
                })?;
            let write_id = lash_core::AttachmentWriteToken::new();
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<lash_core::AttachmentWriteFence, StoreError> = (|| {
                        crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                        let condemnation = tx
                            .query_row(
                                "SELECT phase, write_token FROM attachment_condemnations
                                 WHERE attachment_id = ?1",
                                params![attachment_id],
                                |row| {
                                    Ok((
                                        row.get::<_, String>(0)?,
                                        row.get::<_, Option<String>>(1)?,
                                    ))
                                },
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        match condemnation
                            .as_ref()
                            .map(|(phase, token)| (phase.as_str(), token.is_some()))
                        {
                            // The physical delete is already in flight: record
                            // nothing, so these bytes cannot land inside it.
                            Some(("deleting", _)) | Some(("condemned", true)) => {
                                return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                            }
                            // Keep the condemnation present and own it with this
                            // attempt's identity until the backend put settles.
                            Some(("condemned", false)) => {
                                let claimed = tx
                                    .execute(
                                        "UPDATE attachment_condemnations
                                         SET write_token = ?2, write_session_id = ?3
                                         WHERE attachment_id = ?1
                                           AND phase = 'condemned'
                                           AND write_token IS NULL",
                                        params![
                                            attachment_id,
                                            write_id.as_hex(),
                                            session_id.as_str()
                                        ],
                                    )
                                    .map_err(sqlite_error)?;
                                if claimed == 0 {
                                    return Ok(
                                        lash_core::AttachmentWriteFence::ReclamationInFlight,
                                    );
                                }
                            }
                            None => {}
                            Some((phase, _)) => {
                                return Err(StoreError::Backend(format!(
                                    "attachment `{attachment_id}` has unknown condemnation phase `{phase}`"
                                )));
                            }
                        }
                        // A fresh attempt has proven nothing, so it takes the row
                        // with no upload stamp. Evidence and commitment already on
                        // the row were earned by earlier attempts and are kept.
                        tx.execute(
                            "INSERT INTO attachment_manifest
                            (attachment_id, session_id, canonical_uri, intent_at_ms, write_id,
                             written_at_ms, committed_at_ms, owner_kind, owner_id, owner_incarnation)
                         VALUES (?1, ?2, ?3, ?4, ?8, NULL, NULL, ?5, ?6, ?7)
                         ON CONFLICT(session_id, attachment_id) DO UPDATE SET
                            canonical_uri = excluded.canonical_uri,
                            intent_at_ms = excluded.intent_at_ms,
                            write_id = excluded.write_id,
                            owner_kind = excluded.owner_kind,
                            owner_id = excluded.owner_id,
                            owner_incarnation = excluded.owner_incarnation",
                            params![
                                attachment_id,
                                session_id.as_str(),
                                canonical_uri,
                                intent_at_ms,
                                owner_kind,
                                owner_id,
                                owner_incarnation,
                                write_id.as_hex()
                            ],
                        )
                        .map_err(sqlite_error)?;
                        Ok(lash_core::AttachmentWriteFence::Granted(
                            lash_core::AttachmentWritePermit::new(write_id),
                        ))
                    })(
                    );
                    Ok(match outcome {
                        Ok(fence) => TxOutcome::Commit(Ok(fence)),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        }
    }

    async fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let digest = intent.attachment_id.clone();
        let attachment_id = intent.attachment_id.as_str().to_string();
        let session_id = intent.session_id.clone();
        let write_id = permit.write_id().as_hex();
        let written_at_ms = crate::clamp_epoch_ms(self.clock.timestamp_ms());
        {
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        // Id-matched: only the row this attempt still owns is
                        // stamped, and the first proven upload is kept.
                        let stamped = tx
                            .execute(
                                "UPDATE attachment_manifest
                                 SET written_at_ms = COALESCE(written_at_ms, ?4)
                                 WHERE attachment_id = ?1 AND session_id = ?2
                                   AND write_id = ?3",
                                params![
                                    attachment_id,
                                    session_id.as_str(),
                                    write_id,
                                    written_at_ms
                                ],
                            )
                            .map_err(sqlite_error)?;
                        if stamped == 0 {
                            return Err(StoreError::StaleWritePermit { digest });
                        }
                        // The bytes exist now, so this attempt's claim on the
                        // condemnation is released with the condemnation itself.
                        tx.execute(
                            "DELETE FROM attachment_condemnations
                             WHERE attachment_id = ?1 AND write_token = ?2",
                            params![attachment_id, write_id],
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
        }
    }

    async fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let attachment_id = intent.attachment_id.as_str().to_string();
        let session_id = intent.session_id.clone();
        let write_id = permit.write_id().as_hex();
        {
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        // Only this attempt's own unstamped, uncommitted row. A
                        // superseded permit matches nothing and deletes nothing.
                        tx.execute(
                            "DELETE FROM attachment_manifest
                             WHERE attachment_id = ?1 AND session_id = ?2
                               AND write_id = ?3
                               AND written_at_ms IS NULL AND committed_at_ms IS NULL",
                            params![attachment_id, session_id.as_str(), write_id],
                        )
                        .map_err(sqlite_error)?;
                        let condemned_superseded = tx
                            .execute(
                                "DELETE FROM attachment_condemnations
                                 WHERE attachment_id = ?1 AND write_token = ?2
                                   AND phase = 'condemned'
                                   AND EXISTS (
                                       SELECT 1 FROM attachment_manifest
                                        WHERE attachment_id = ?1 AND session_id = ?3
                                          AND committed_at_ms IS NOT NULL
                                   )",
                                params![attachment_id, write_id, session_id.as_str()],
                            )
                            .map_err(sqlite_error)?;
                        if condemned_superseded == 0 {
                            tx.execute(
                                "UPDATE attachment_condemnations
                                 SET write_token = NULL, write_session_id = NULL
                                 WHERE attachment_id = ?1 AND write_token = ?2",
                                params![attachment_id, write_id],
                            )
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
        }
    }

    async fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        if attachment_ids.is_empty() {
            return Ok(());
        }
        {
            let session_id = SessionId::from(session_id.to_string());
            let attachment_ids = attachment_ids.to_vec();
            let now = self.clock.timestamp_ms() as i64;
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                        commit_attachment_refs_conn(tx, &session_id, &attachment_ids, now)
                    })();
                    Ok(match outcome {
                        Ok(()) => TxOutcome::Commit(Ok(())),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        }
    }

    async fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        {
            let older_than = crate::clamp_epoch_ms(older_than_epoch_ms);
            self.conn
                .call(move |conn| {
                    let mut stmt = conn.prepare(
                        "SELECT attachment_id, session_id, canonical_uri, intent_at_ms,
                                committed_at_ms, owner_kind, owner_id, owner_incarnation,
                                written_at_ms
                         FROM attachment_manifest
                         WHERE committed_at_ms IS NULL AND intent_at_ms <= ?1
                         ORDER BY intent_at_ms ASC",
                    )?;
                    let rows = stmt.query_map(params![older_than], |row| {
                        let id: String = row.get(0)?;
                        let session_id: SessionId = SessionId::from(row.get::<_, String>(1)?);
                        let canonical_uri: String = row.get(2)?;
                        let intent_at_ms: i64 = row.get(3)?;
                        let committed_at_ms: Option<i64> = row.get(4)?;
                        let owner_kind: Option<String> = row.get(5)?;
                        let owner_id: Option<String> = row.get(6)?;
                        let owner_incarnation = row
                            .get::<_, Option<i64>>(7)?
                            .map(|value| {
                                u64_from_sql("AttachmentManifest", "owner_incarnation", value)
                            })
                            .transpose()?;
                        let written_at_ms: Option<i64> = row.get(8)?;
                        let owner = lash_core::store::decode_attachment_owner(
                            owner_kind.as_deref(),
                            owner_id,
                            owner_incarnation,
                        )
                        .map_err(sqlite_conversion_error)?;
                        Ok(AttachmentManifestEntry {
                            attachment_id: crate::attachment_id_from_sql(
                                "AttachmentManifest",
                                "attachment_id",
                                id,
                            )?,
                            session_id,
                            canonical_uri,
                            intent_at_epoch_ms: u64_from_sql(
                                "AttachmentManifest",
                                "intent_at_ms",
                                intent_at_ms,
                            )?,
                            written_at_epoch_ms: written_at_ms
                                .map(|value| {
                                    u64_from_sql("AttachmentManifest", "written_at_ms", value)
                                })
                                .transpose()?,
                            committed_at_epoch_ms: committed_at_ms
                                .map(|value| {
                                    u64_from_sql("AttachmentManifest", "committed_at_ms", value)
                                })
                                .transpose()?,
                            owner,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        }
    }

    async fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        {
            let cutoff = crate::clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
            let process_registry_attached = self.process_registry_attached;
            self.conn
                .write(move |tx| {
                    tx.execute(RECLAIM_DELETED_ATTACHMENT_ROOTS, [])?;
                    // One conditional DELETE composes age with owner-death proof.
                    // The attached process DB makes the NOT EXISTS predicate part
                    // of this same SQLite statement/transaction, avoiding a
                    // read-process-then-forget race across the per-session topology.
                    let turn_owner_kind = AttachmentOwnerKind::Turn.as_str();
                    let process_dead = if process_registry_attached {
                        let process_owner_kind = AttachmentOwnerKind::Process.as_str();
                        format!(
                            "OR (
                            manifest.owner_kind = '{process_owner_kind}'
                            AND NOT EXISTS (
                                SELECT 1 FROM process_registry.processes AS process
                                WHERE process.process_id = manifest.owner_id
                                  AND process.incarnation = manifest.owner_incarnation
                            )
                        )"
                        )
                    } else {
                        // Without a configured process registry, conservatively
                        // retain process-owned rows rather than guess liveness.
                        String::new()
                    };
                    let sql = format!(
                        "DELETE FROM attachment_manifest AS manifest
                         WHERE manifest.committed_at_ms IS NULL
                           AND manifest.intent_at_ms <= ?1
                           AND (
                                manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                                OR (
                                    manifest.owner_kind = '{turn_owner_kind}'
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
        }
    }

    async fn has_live_ref_for_id(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        {
            let attachment_id = attachment_id.as_str().to_string();
            let cutoff = crate::clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
            let sql = live_ref_exists_sql(self.process_registry_attached);
            self.conn
                .call(move |conn| {
                    conn.query_row(&sql, params![attachment_id, cutoff], |_| Ok(()))
                        .optional()
                        .map(|found| found.is_some())
                })
                .await
                .map_err(sqlite_error)
        }
    }

    async fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        {
            let session_id = SessionId::from(session_id.to_string());
            let attachment_id = attachment_id.as_str().to_string();
            self.conn
                .call(move |conn| {
                    conn.execute(
                        "DELETE FROM attachment_manifest
                         WHERE session_id = ?1 AND attachment_id = ?2 AND (
                             committed_at_ms IS NULL OR NOT EXISTS (
                                 SELECT 1 FROM graph_nodes AS node
                                 WHERE node.session_id = attachment_manifest.session_id
                                   AND node.tombstoned = 0
                             ))",
                        params![session_id.as_str(), attachment_id.as_str()],
                    )
                })
                .await
                .map_err(sqlite_error)?;
            Ok(())
        }
    }

    async fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        {
            self.conn
                .call(move |conn| {
                    let mut stmt =
                        conn.prepare("SELECT DISTINCT attachment_id FROM attachment_manifest")?;
                    let rows = stmt.query_map([], |row| {
                        let id: String = row.get(0)?;
                        crate::attachment_id_from_sql("AttachmentManifest", "attachment_id", id)
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        }
    }
}
