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
#[cfg(any(feature = "lashlang", test))]
pub(crate) const RAW_ARTIFACT_NAMESPACE: &str = "lashlang_artifact";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

/// Adopt stored references under the boundary transaction, including a new
/// receiver root when only another session has ever put the bytes.
pub(crate) fn commit_attachment_refs_conn(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    attachment_ids: &[AttachmentId],
    now: i64,
) -> Result<(), StoreError> {
    for id in attachment_ids {
        let condemnation = tx
            .query_row(
                "SELECT phase, write_token FROM attachment_condemnations WHERE attachment_id = ?1",
                params![id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        match condemnation
            .as_ref()
            .map(|(phase, token)| (phase.as_str(), token.is_some()))
        {
            Some(("deleting", _)) => {
                return Err(StoreError::Backend(format!(
                    "cannot adopt attachment `{id}` while physical deletion is in flight"
                )));
            }
            Some(("reclaimed", _)) => {
                return Err(StoreError::AttachmentBytesReclaimed { digest: id.clone() });
            }
            Some(("condemned", true)) => {
                return Err(StoreError::Backend(format!(
                    "cannot adopt attachment `{id}` while its bytes are being restored"
                )));
            }
            None | Some(("condemned", false)) => {}
            Some((phase, _)) => {
                return Err(StoreError::Backend(format!(
                    "attachment `{id}` has unknown condemnation phase `{phase}`"
                )));
            }
        }
        tx.execute(
            "DELETE FROM attachment_condemnations
             WHERE attachment_id = ?1 AND phase = 'condemned' AND write_token IS NULL",
            params![id.as_str()],
        )
        .map_err(sqlite_error)?;
        tx.execute(
            "INSERT INTO attachment_manifest
             (attachment_id, session_id, canonical_uri, intent_at_ms, committed_at_ms)
             VALUES (?2, ?3, ?4, ?1, ?1)
             ON CONFLICT (session_id, attachment_id) DO UPDATE
             SET committed_at_ms = COALESCE(attachment_manifest.committed_at_ms, excluded.committed_at_ms)",
            params![now, id.as_str(), session_id.as_str(), format!("lash-attachment://blake3/{id}")],
        ).map_err(sqlite_error)?;
    }
    Ok(())
}

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

#[cfg(feature = "lashlang")]
#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for Store {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn put_module_artifact(
        &self,
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
        self.put_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            artifact_ref,
            BlobArtifactDescriptor::lashlang_module(),
            bytes,
        )
        .await
        .map_err(|err| lashlang::ArtifactStoreError::Backend(err.to_string()))?;
        self.artifact_cache
            .lock_recover()
            .insert(artifact.module_ref.clone(), Arc::new(artifact.clone()));
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
        if let Some(artifact) = self.artifact_cache.lock_recover().get(module_ref).cloned() {
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
            .lock_recover()
            .insert(module_ref.clone(), artifact.clone());
        Ok(Some(artifact))
    }

    async fn put_artifact_bytes(
        &self,
        artifact_ref: &str,
        descriptor: &str,
        bytes: &[u8],
    ) -> Result<(), lashlang::ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(artifact_ref) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid artifact namespace key".into(),
            ));
        }
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
        if !crate::namespace::is_valid_opaque_key(artifact_ref) {
            return Err(lashlang::ArtifactStoreError::Backend(
                "invalid artifact namespace key".into(),
            ));
        }
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
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(lash_core::PluginError::Invoke(
                "invalid process execution environment reference".into(),
            ));
        }
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
        .map_err(|err| lash_core::PluginError::Session(err.to_string()))
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
    /// Preserve `Reclaimed`; retire `Condemned` only when its associated intent
    /// became committed, otherwise preserve it after removing that intent.
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
                               AND phase IN ('condemned', 'reclaimed')
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
                           AND committed_at_ms IS NULL",
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

    /// `Deleting -> Reclaimed` after the physical delete succeeds.
    pub(crate) async fn reclaim_attachment_condemnation(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    "UPDATE attachment_condemnations SET phase = 'reclaimed'
                     WHERE attachment_id = ?1 AND phase = 'deleting'",
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }
}

impl AttachmentManifest for Store {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        block_on_store(async {
            let digest = intent.attachment_id.clone();
            let attachment_id = intent.attachment_id.as_str().to_string();
            let session_id = intent.session_id.clone();
            let canonical_uri = intent.canonical_uri.as_str().to_string();
            let intent_at_ms = intent.intent_at_epoch_ms as i64;
            let owner_kind = intent.owner_kind.map(AttachmentOwnerKind::as_str);
            let owner_id = intent.owner_id;
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
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
                            Some(("deleting", _)) => {
                                return Err(StoreError::Backend(format!(
                                    "cannot record attachment `{attachment_id}` while physical deletion is in flight"
                                )));
                            }
                            Some(("reclaimed", false)) => {
                                return Err(StoreError::AttachmentBytesReclaimed {
                                    digest,
                                });
                            }
                            Some(("condemned", false)) => {
                                return Err(StoreError::Backend(format!(
                                    "cannot record attachment `{attachment_id}` through the unfenced manifest path while it is condemned; use begin_attachment_write"
                                )));
                            }
                            Some(("condemned" | "reclaimed", true)) => {
                                return Err(StoreError::Backend(format!(
                                    "cannot record attachment `{attachment_id}` while its bytes are being restored"
                                )));
                            }
                            None => {}
                            Some((phase, _)) => {
                                return Err(StoreError::Backend(format!(
                                    "attachment `{attachment_id}` has unknown condemnation phase `{phase}`"
                                )));
                            }
                        }
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
                                session_id.as_str(),
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

    /// The writer half of the GC fence: the condemnation check, the revoke, and
    /// the intent insert are one SQLite transaction, so a sweeper's condemn CAS
    /// either precedes this whole mutation or fails against the intent it wrote.
    fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<lash_core::AttachmentWriteFence, StoreError> {
        block_on_store(async {
            let attachment_id = intent.attachment_id.as_str().to_string();
            let session_id = intent.session_id.clone();
            let canonical_uri = intent.canonical_uri.as_str().to_string();
            let intent_at_ms = intent.intent_at_epoch_ms as i64;
            let owner_kind = intent.owner_kind.map(AttachmentOwnerKind::as_str);
            let owner_id = intent.owner_id;
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
                        let permit = match condemnation
                            .as_ref()
                            .map(|(phase, token)| (phase.as_str(), token.is_some()))
                        {
                            // The physical delete is already in flight: record
                            // nothing, so these bytes cannot land inside it.
                            Some(("deleting", _))
                            | Some(("condemned" | "reclaimed", true)) => {
                                return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                            }
                            // Keep the prior phase present and own it with an
                            // opaque token until the backend put settles.
                            Some(("condemned" | "reclaimed", false)) => {
                                let token = lash_core::AttachmentWriteToken::new();
                                let claimed = tx
                                    .execute(
                                        "UPDATE attachment_condemnations
                                         SET write_token = ?2, write_session_id = ?3
                                         WHERE attachment_id = ?1
                                           AND phase IN ('condemned', 'reclaimed')
                                           AND write_token IS NULL",
                                        params![attachment_id, token.as_hex(), session_id.as_str()],
                                    )
                                    .map_err(sqlite_error)?;
                                if claimed == 0 {
                                    return Ok(
                                        lash_core::AttachmentWriteFence::ReclamationInFlight,
                                    );
                                }
                                lash_core::AttachmentWritePermit::restoring(token)
                            }
                            None => lash_core::AttachmentWritePermit::ordinary(),
                            Some((phase, _)) => {
                                return Err(StoreError::Backend(format!(
                                    "attachment `{attachment_id}` has unknown condemnation phase `{phase}`"
                                )));
                            }
                        };
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
                                session_id.as_str(),
                                canonical_uri,
                                intent_at_ms,
                                owner_kind,
                                owner_id
                            ],
                        )
                        .map_err(sqlite_error)?;
                        Ok(lash_core::AttachmentWriteFence::Granted(permit))
                    })(
                    );
                    Ok(match outcome {
                        Ok(fence) => TxOutcome::Commit(Ok(fence)),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        })
    }

    fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let Some(token) = permit.rollback_token() else {
            return Ok(());
        };
        let attachment_id = intent.attachment_id.as_str().to_string();
        block_on_store(async move {
            self.conn
                .write(move |tx| {
                    tx.execute(
                        "DELETE FROM attachment_condemnations
                         WHERE attachment_id = ?1 AND write_token = ?2",
                        params![attachment_id, token.as_hex()],
                    )
                })
                .await
                .map_err(sqlite_error)?;
            Ok(())
        })
    }

    fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let Some(token) = permit.rollback_token() else {
            return Ok(());
        };
        let attachment_id = intent.attachment_id.as_str().to_string();
        let session_id = intent.session_id.clone();
        block_on_store(async move {
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        let token = token.as_hex();
                        let owns_phase = tx
                            .query_row(
                                "SELECT 1 FROM attachment_condemnations
                                 WHERE attachment_id = ?1 AND write_token = ?2",
                                params![attachment_id, token],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .is_some();
                        if owns_phase {
                            tx.execute(
                                "DELETE FROM attachment_manifest
                                 WHERE attachment_id = ?1 AND session_id = ?2
                                   AND committed_at_ms IS NULL",
                                params![attachment_id, session_id.as_str()],
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
                                    params![attachment_id, token, session_id.as_str()],
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

    fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        if attachment_ids.is_empty() {
            return Ok(());
        }
        block_on_store(async {
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
        })
    }

    fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        block_on_store(async {
            let older_than = crate::clamp_epoch_ms(older_than_epoch_ms);
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
                        let session_id: SessionId = SessionId::from(row.get::<_, String>(1)?);
                        let canonical_uri: String = row.get(2)?;
                        let intent_at_ms: i64 = row.get(3)?;
                        let committed_at_ms: Option<i64> = row.get(4)?;
                        let owner_kind: Option<String> = row.get(5)?;
                        let owner_id: Option<String> = row.get(6)?;
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
                            committed_at_epoch_ms: committed_at_ms
                                .map(|value| {
                                    u64_from_sql("AttachmentManifest", "committed_at_ms", value)
                                })
                                .transpose()?,
                            owner_kind: owner_kind
                                .as_deref()
                                .map(|value| {
                                    AttachmentOwnerKind::from_wire_str(value).ok_or_else(|| {
                                        sqlite_conversion_error(stored_data_corrupt(
                                            "AttachmentManifest owner kind",
                                            format_args!("unknown attachment owner kind `{value}`"),
                                        ))
                                    })
                                })
                                .transpose()?,
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
        })
    }

    fn has_live_ref_for_id(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        block_on_store(async {
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
        })
    }

    fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        block_on_store(async {
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
                        crate::attachment_id_from_sql("AttachmentManifest", "attachment_id", id)
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        })
    }
}
