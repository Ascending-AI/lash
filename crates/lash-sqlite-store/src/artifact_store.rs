//! The lashlang module-artifact store and the process-execution-env store.
//!
//! The SQLite owner of the artifact family: `artifact_refs` (this backend's
//! pointer from a namespaced reference to its bytes), `artifact_owners` (the
//! exact owner edges that keep an artifact alive) and
//! `artifact_owner_retirements` (the permanent publication fence). The bytes
//! themselves live in `blobs`, shared with checkpoint storage, which is why
//! reclaiming one is conditional on every other rooting relation.
//!
//! Both traits here are `#[async_trait]` surfaces over the async
//! [`SqliteConnection`]: every DB body is a synchronous rusqlite closure
//! handed to `conn.call` (reads) or `conn.write` (read-then-write), and only
//! the wrapper call is awaited.

use std::sync::LazyLock;

use crate::scope_fence::Schema;
use lash_store_sql::artifact::blobs::BlobStatements;
use lash_store_sql::artifact::owner_retirements::OwnerRetirementStatements;
use lash_store_sql::artifact::owners::OwnerStatements;

use super::*;
#[cfg(feature = "lashlang")]
use lash_sansio::sync::MutexExt;

lash_store_sql::statements! {
    /// `artifact_refs` statements only SQLite issues.
    ///
    /// Every one of them: PostgreSQL keeps artifact bytes inline in
    /// `lash_lashlang_artifacts` and has no pointer table, so this whole set
    /// exists on one backend only.
    pub(crate) struct RefSqliteStatements @ "artifact_ref" {
        /// Point `?1`/`?2` at blob `?3`. An artifact is immutable, so a
        /// second publication of the same reference keeps the first pointer
        /// and the caller compares what it reads back.
        insert_pointer = "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (namespace, artifact_ref) DO NOTHING";

        /// The blob `?1`/`?2` points at.
        select_blob_ref = "SELECT blob_ref FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2";

        /// Drop `?1`/`?2`'s pointer once its last owner edge is gone. The
        /// `NOT EXISTS` is the whole safety argument: an owner acquired
        /// between the release and this delete keeps the row.
        delete_unowned = "DELETE FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_owners
                   WHERE namespace = ?1 AND artifact_ref = ?2
               )";

        /// Every pointer row, as the collector's root set.
        select_gc_roots = "SELECT namespace, blob_ref FROM artifact_refs
             ORDER BY namespace, artifact_ref";

        /// One preflight page of published artifacts in namespace `?1`, after
        /// reference `?2`, at most `?3` rows.
        select_preflight_page = "SELECT refs.artifact_ref, refs.blob_ref, blobs.content
             FROM artifact_refs AS refs
             LEFT JOIN blobs ON blobs.hash = refs.blob_ref
             WHERE refs.namespace = ?1
               AND (?2 IS NULL OR refs.artifact_ref > ?2)
             ORDER BY refs.artifact_ref
             LIMIT ?3";
    }
}

lash_store_sql::statements! {
    /// `artifact_owners` statements only SQLite issues.
    pub(crate) struct OwnerSqliteStatements @ "artifact_owner" {
        /// Move artifact `?1`/`?2` from owner `?5`/`?6` to owner `?3`/`?4`,
        /// reporting whether the source edge existed.
        ///
        /// `INSERT … SELECT` is the fork: it makes "the destination edge
        /// appears only if the source edge was there" one statement under the
        /// write lock. PostgreSQL reads the source edge first because it
        /// takes a per-owner advisory lock anyway and wants to tell a missing
        /// staging edge from an already-completed transfer.
        transfer_edge = "INSERT INTO artifact_owners
             (namespace, artifact_ref, owner_kind, owner_id)
             SELECT namespace, artifact_ref, ?3, ?4
             FROM artifact_owners
             WHERE namespace = ?1 AND artifact_ref = ?2
               AND owner_kind = ?5 AND owner_id = ?6
             ON CONFLICT DO NOTHING";

        /// Every artifact owner `?2`/`?3` holds in namespace `?1`.
        ///
        /// Unordered: SQLite reclaims each reference in turn under one write
        /// lock, so no lock-acquisition order exists to respect. PostgreSQL
        /// orders the same read because it takes a per-artifact advisory lock
        /// for each row and must take them in a stable order.
        select_owned_refs = "SELECT artifact_ref FROM artifact_owners
             WHERE namespace = ?1 AND owner_kind = ?2 AND owner_id = ?3";
    }
}

/// Every artifact-family statement, rendered once.
pub(crate) struct ArtifactSql {
    /// `artifact_owners` statements both backends issue verbatim.
    pub(crate) owners: OwnerStatements,
    /// `artifact_owners` statements only SQLite issues.
    pub(crate) owners_sqlite: OwnerSqliteStatements,
    /// `artifact_owner_retirements` statements both backends issue verbatim.
    pub(crate) retirements: OwnerRetirementStatements,
    /// `artifact_refs` statements, all of which only SQLite issues.
    pub(crate) refs: RefSqliteStatements,
    /// `blobs` statements both backends issue verbatim.
    pub(crate) blobs: BlobStatements,
    /// `blobs` statements only SQLite issues.
    pub(crate) blobs_sqlite: crate::blobs::BlobSqliteStatements,
}

/// The artifact family lives entirely in the session catalog's own database
/// and is never reached through an `ATTACH`ed name, so one dialect renders it.
static ARTIFACT_SQL: LazyLock<ArtifactSql> = LazyLock::new(|| {
    let dialect = Schema::Main.dialect();
    ArtifactSql {
        owners: OwnerStatements::render(dialect),
        owners_sqlite: OwnerSqliteStatements::render(dialect),
        retirements: OwnerRetirementStatements::render(dialect),
        refs: RefSqliteStatements::render(dialect),
        blobs: BlobStatements::render(dialect),
        blobs_sqlite: crate::blobs::BlobSqliteStatements::render(dialect),
    }
});

/// The artifact-family statements, rendered once at first use.
pub(crate) fn artifact_sql() -> &'static ArtifactSql {
    &ARTIFACT_SQL
}

/// Logical keyspaces multiplexed onto the `artifact_refs` pointer table. Each
/// namespace owns its own half of the `(namespace, artifact_ref)` composite
/// primary key. The `blobs` table is content-addressed, but the `artifact_refs`
/// pointer is *not*: without the namespace column, a module ref that collides
/// with a process-execution-env ref would rewrite the same pointer row under
/// `INSERT OR REPLACE`, so content-addressing alone does not keep the namespaces
/// disjoint. The composite key does.
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "lashlang_module";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";

/// The [`PersistedArtifactKind`] a pointer-table row carries, derived from the
/// row's own namespace key — the namespace is the sole owner of the
/// payload-family fact (FIG-1949). A new namespace must extend this match; an
/// unknown namespace fails the caller rather than inheriting a sibling's label.
pub(crate) fn artifact_namespace_kind(
    namespace: &str,
) -> Result<PersistedArtifactKind, StoreError> {
    match namespace {
        MODULE_ARTIFACT_NAMESPACE => Ok(PersistedArtifactKind::LashlangModule),
        PROCESS_ENV_NAMESPACE => Ok(PersistedArtifactKind::ProcessExecutionEnv),
        unknown => Err(stored_data_corrupt(
            "artifact_refs namespace",
            format!("unknown artifact namespace `{unknown}`"),
        )),
    }
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
                    artifact_sql().retirements.select_is_retired.sql(),
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
                    artifact_sql().refs.insert_pointer.sql(),
                    params![namespace, artifact_ref, blob_ref.as_str()],
                )?;
                let stored_blob_ref: String = tx.query_row(
                    artifact_sql().refs.select_blob_ref.sql(),
                    params![namespace, artifact_ref],
                    |row| row.get(0),
                )?;
                if stored_blob_ref != blob_ref.as_str() {
                    return Err(rusqlite::Error::InvalidParameterName(format!(
                        "artifact `{artifact_ref}` in namespace `{namespace}` is immutable"
                    )));
                }
                tx.execute(
                    artifact_sql().owners.insert_edge.sql(),
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
                    artifact_sql().retirements.select_is_retired.sql(),
                    params![to_kind, to_id],
                    |row| row.get::<_, bool>(0),
                )?;
                if retired {
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        StoreError::ArtifactDestinationOwnerRetired,
                    )));
                }
                let inserted = tx.execute(
                    artifact_sql().owners_sqlite.transfer_edge.sql(),
                    params![namespace, artifact_ref, to_kind, to_id, from_kind, from_id],
                )?;
                if inserted == 0 {
                    let destination_exists = tx.query_row(
                        artifact_sql().owners.select_edge_exists.sql(),
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
                    artifact_sql().owners.delete_edge.sql(),
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
                artifact_sql().refs.select_blob_ref.sql(),
                params![namespace, artifact_ref],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(blob_ref) = blob_ref else {
            return Ok(());
        };
        tx.execute(
            artifact_sql().refs.delete_unowned.sql(),
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
                    artifact_sql().owners.delete_edge.sql(),
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
                    artifact_sql().retirements.insert_retirement.sql(),
                    params![owner_kind, owner_id],
                )?;
                let refs = {
                    let mut stmt =
                        tx.prepare(artifact_sql().owners_sqlite.select_owned_refs.sql())?;
                    stmt.query_map(params![namespace, owner_kind, owner_id], |row| row.get(0))?
                        .collect::<Result<Vec<String>, _>>()?
                };
                tx.execute(
                    artifact_sql().owners.delete_owner_edges.sql(),
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
                        artifact_sql().refs.select_blob_ref.sql(),
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
