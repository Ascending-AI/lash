//! The lashlang module-artifact store and the process-execution-env store.
//!
//! The SQLite owner of the artifact family: `artifact_refs` (this backend's
//! pointer from a namespaced reference to its bytes), `artifact_referrer_edges` (the
//! exact edges that keep an artifact alive) and
//! `artifact_referrer_fences` (the permanent publication fence). The bytes
//! themselves live in `blobs`, shared with checkpoint storage, which is why
//! reclaiming one is conditional on every other rooting relation.
//!
//! Both traits here are `#[async_trait]` surfaces over the async
//! [`SqliteConnection`]: every DB body is a synchronous rusqlite closure
//! handed to `conn.call` (reads) or `conn.write` (read-then-write), and only
//! the wrapper call is awaited.

use std::sync::LazyLock;

use crate::schema_layout::Schema;
use lash_core_execution::{
    ArtifactReferrer, ArtifactStoreError, ArtifactStoreId, ReferrerClaim, ResolvedArtifactCleanup,
};
use lash_store_sql::artifact::blobs::BlobStatements;
use lash_store_sql::artifact::referrer_edges::ReferrerEdgeStatements;
use lash_store_sql::artifact::referrer_fences::ReferrerFenceStatements;

use super::*;
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

        /// `NOT EXISTS` is the whole safety argument: a referrer acquired
        /// between the release and this delete keeps the row.
        delete_unreferenced = "DELETE FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2
               AND NOT EXISTS (
                   SELECT 1 FROM artifact_referrer_edges
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

/// Every artifact-family statement, rendered once.
pub(crate) struct ArtifactSql {
    pub(crate) edges: ReferrerEdgeStatements,
    pub(crate) fences: ReferrerFenceStatements,
    pub(crate) refs: RefSqliteStatements,
    pub(crate) blobs: BlobStatements,
    pub(crate) blobs_sqlite: crate::blobs::BlobSqliteStatements,
}

static ARTIFACT_SQL: LazyLock<ArtifactSql> = LazyLock::new(|| {
    let dialect = Schema::Main.dialect();
    ArtifactSql {
        edges: ReferrerEdgeStatements::render(dialect),
        fences: ReferrerFenceStatements::render(dialect),
        refs: RefSqliteStatements::render(dialect),
        blobs: BlobStatements::render(dialect),
        blobs_sqlite: crate::blobs::BlobSqliteStatements::render(dialect),
    }
});

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

/// Keep the typed refusals raised within a SQLite write transaction.
fn artifact_sqlite_error(error: rusqlite::Error) -> ArtifactStoreError {
    match error {
        rusqlite::Error::ToSqlConversionFailure(error) => {
            match error.downcast::<ArtifactStoreError>() {
                Ok(error) => *error,
                Err(error) => match error.downcast::<StoreError>() {
                    Ok(error) => ArtifactStoreError::from(*error),
                    Err(error) => ArtifactStoreError::Backend(error.to_string()),
                },
            }
        }
        error => ArtifactStoreError::from(sqlite_error(error)),
    }
}

fn artifact_failure(error: ArtifactStoreError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn decode_stored_edge(kind: &str, id: &str) -> Result<ArtifactReferrer, StoreError> {
    ArtifactReferrer::decode(kind, id).map_err(|error| match error {
        lash_core_execution::ArtifactReferrerError::UnknownKind(label) => {
            StoreError::Incompatible {
                refusal: lash_core_execution::compat::CompatRefusal::UnknownVocabulary {
                    surface: "artifact referrer edge kind".to_owned(),
                    label,
                },
            }
        }
        other => stored_data_corrupt("artifact referrer edge", other),
    })
}

pub(crate) fn artifact_fenced_tx(
    tx: &rusqlite::Connection,
    referrer: &ArtifactReferrer,
) -> rusqlite::Result<bool> {
    tx.query_row(
        artifact_sql().fences.select_is_fenced.sql(),
        params![referrer.kind().as_str(), referrer.canonical_id()],
        |row| row.get(0),
    )
}

pub(crate) fn fence_artifact_referrer_tx(
    tx: &rusqlite::Connection,
    referrer: &ArtifactReferrer,
    now_ms: u64,
) -> rusqlite::Result<()> {
    crate::conn::cached_execute(
        tx,
        artifact_sql().fences.insert_fence.sql(),
        params![
            referrer.kind().as_str(),
            referrer.canonical_id(),
            crate::clamp_epoch_ms(now_ms)
        ],
    )?;
    Ok(())
}

impl SqliteStore {
    async fn publish_artifact_ref_blob(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        descriptor: BlobArtifactDescriptor,
        bytes: Vec<u8>,
        claim: ReferrerClaim,
    ) -> Result<(), ArtifactStoreError> {
        let blob_profile = self.options.blob_profile;
        let now_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| {
                let referrer = claim.referrer();
                if artifact_fenced_tx(tx, referrer)? {
                    return Err(artifact_failure(ArtifactStoreError::ReferrerEnded {
                        referrer: referrer.clone(),
                    }));
                }
                if let Some(cleanup) = claim.guard_cleanup() {
                    crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms, "core")
                        .map_err(sqlite_conversion_error)?;
                }
                let blob_ref = Self::insert_artifact_blob_conn(
                    tx,
                    descriptor,
                    &bytes,
                    blob_profile,
                    tx.fleet(),
                )?;
                crate::conn::cached_execute(
                    tx,
                    artifact_sql().refs.insert_pointer.sql(),
                    params![namespace, artifact_ref, blob_ref.as_str()],
                )?;
                let stored_blob_ref: String = tx.query_row(
                    artifact_sql().refs.select_blob_ref.sql(),
                    params![namespace, artifact_ref],
                    |row| row.get(0),
                )?;
                if stored_blob_ref != blob_ref.as_str() {
                    return Err(artifact_failure(ArtifactStoreError::Immutable {
                        artifact_ref,
                    }));
                }
                crate::conn::cached_execute(
                    tx,
                    artifact_sql().edges.insert_edge.sql(),
                    params![
                        namespace,
                        artifact_ref,
                        referrer.kind().as_str(),
                        referrer.canonical_id()
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(artifact_sqlite_error)
    }

    async fn acquire_artifact_ref_blob(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        claim: ReferrerClaim,
    ) -> Result<(), ArtifactStoreError> {
        let now_ms = self.clock.timestamp_ms();
        self.conn.write(move |tx| {
            let referrer = claim.referrer();
            if artifact_fenced_tx(tx, referrer)? {
                return Err(artifact_failure(ArtifactStoreError::ReferrerEnded { referrer: referrer.clone() }));
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2)",
                params![namespace, artifact_ref], |row| row.get(0))?;
            if !exists {
                return Err(artifact_failure(ArtifactStoreError::ArtifactMissing { artifact_ref }));
            }
            if let Some(cleanup) = claim.guard_cleanup() {
                crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms, "core")
                    .map_err(sqlite_conversion_error)?;
            }
            crate::conn::cached_execute(tx, artifact_sql().edges.insert_edge.sql(),
                params![namespace, artifact_ref, referrer.kind().as_str(), referrer.canonical_id()])?;
            Ok(())
        }).await.map_err(artifact_sqlite_error)
    }

    fn reclaim_unreferenced_artifact_tx(
        tx: &rusqlite::Connection,
        namespace: &str,
        artifact_ref: &str,
    ) -> rusqlite::Result<()> {
        let blob_ref: Option<String> = tx
            .query_row(
                artifact_sql().refs.select_blob_ref.sql(),
                params![namespace, artifact_ref],
                |row| row.get(0),
            )
            .optional()?;
        let Some(blob_ref) = blob_ref else {
            return Ok(());
        };
        crate::conn::cached_execute(
            tx,
            artifact_sql().refs.delete_unreferenced.sql(),
            params![namespace, artifact_ref],
        )?;
        crate::conn::cached_execute(
            tx,
            artifact_sql()
                .blobs_sqlite
                .reclaim_unreferenced_artifact
                .sql(),
            params![blob_ref],
        )?;
        Ok(())
    }

    async fn end_artifact_referrer(
        &self,
        namespace: &'static str,
        cleanup: ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let now_ms = self.clock.timestamp_ms();
        let expected_store = match namespace {
            MODULE_ARTIFACT_NAMESPACE => ArtifactStoreId::LashlangModule,
            PROCESS_ENV_NAMESPACE => ArtifactStoreId::ProcessEnv,
            _ => {
                return Err(ArtifactStoreError::Backend(
                    "unknown artifact namespace".into(),
                ));
            }
        };
        if cleanup
            .carries
            .iter()
            .any(|carry| carry.artifact.store != expected_store)
        {
            return Err(ArtifactStoreError::Backend(
                "cleanup carries an artifact for a different store".into(),
            ));
        }
        self.conn.write(move |tx| {
            let refs: Vec<String> = {
                let mut stmt = tx.prepare_cached(artifact_sql().edges.select_referrer_edges_in_namespace.sql())?;
                stmt.query_map(params![namespace, cleanup.referrer.kind().as_str(), cleanup.referrer.canonical_id()],
                    |row| {
                        let artifact_ref = row.get(1)?;
                        let kind: String = row.get(2)?;
                        let id: String = row.get(3)?;
                        decode_stored_edge(&kind, &id).map_err(sqlite_conversion_error)?;
                        Ok(artifact_ref)
                    })?.collect::<rusqlite::Result<_>>()?
            };
            if refs.is_empty() && artifact_fenced_tx(tx, &cleanup.referrer)? {
                return Ok(());
            }
            fence_artifact_referrer_tx(tx, &cleanup.referrer, now_ms)?;
            let mut carries = cleanup.carries.clone();
            carries.sort_by(|left, right| left.artifact.artifact_ref.cmp(&right.artifact.artifact_ref));
            for carry in &carries {
                if artifact_fenced_tx(tx, &carry.to)? { continue; }
                let artifact_ref = &carry.artifact.artifact_ref;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2)",
                    params![namespace, artifact_ref], |row| row.get(0))?;
                if !exists {
                    return Err(artifact_failure(ArtifactStoreError::CarryArtifactMissing {
                        artifact_ref: artifact_ref.clone(), to: carry.to.clone(),
                    }));
                }
                crate::conn::cached_execute(tx, artifact_sql().edges.insert_edge.sql(),
                    params![namespace, artifact_ref, carry.to.kind().as_str(), carry.to.canonical_id()])?;
            }
            crate::conn::cached_execute(tx, artifact_sql().edges.delete_referrer_edges_in_namespace.sql(),
                params![namespace, cleanup.referrer.kind().as_str(), cleanup.referrer.canonical_id()])?;
            for artifact_ref in refs {
                Self::reclaim_unreferenced_artifact_tx(tx, namespace, &artifact_ref)?;
            }
            Ok(())
        }).await.map_err(artifact_sqlite_error)
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
                let mut edges =
                    conn.prepare_cached(artifact_sql().edges.select_artifact_edges.sql())?;
                let mut rows = edges.query(params![namespace, artifact_ref])?;
                while let Some(row) = rows.next()? {
                    let kind: String = row.get(0)?;
                    let id: String = row.get(1)?;
                    decode_stored_edge(&kind, &id).map_err(sqlite_conversion_error)?;
                }
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
impl lash_core_execution::ModuleArtifactStore for SqliteStore {
    fn pause_next_publication_for_testing(
        &self,
    ) -> Option<lash_core_execution::ArtifactPublicationPause> {
        let pause = lash_core_execution::ArtifactPublicationPause::default();
        *self.artifact_publication_pause.lock_recover() = Some(pause.clone());
        Some(pause)
    }

    fn durability_tier(&self) -> lash_core_execution::DurabilityTier {
        lash_core_execution::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        let publication_pause = self.artifact_publication_pause.lock_recover().take();
        if let Some(pause) = publication_pause {
            pause.pause().await;
        }
        self.publish_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.to_owned(),
            BlobArtifactDescriptor::lashlang_module(),
            bytes.to_vec(),
            claim.clone(),
        )
        .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.acquire_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.to_owned(),
            claim.clone(),
        )
        .await
    }

    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_artifact_referrer(MODULE_ARTIFACT_NAMESPACE, cleanup.clone())
            .await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Decode(
                "invalid module reference".into(),
            ));
        }
        self.get_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.to_owned(),
            format!("lashlang module artifact `{module_ref}`"),
        )
        .await
        .map_err(Into::into)
    }
}

#[async_trait::async_trait]
impl lash_core_execution::ProcessExecutionEnvStore for SqliteStore {
    async fn publish_process_execution_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        if !env_ref.matches_store_bytes(bytes) {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: env_ref.as_str().to_owned(),
            });
        }
        self.publish_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            env_ref.as_str().to_owned(),
            BlobArtifactDescriptor::process_execution_env(),
            bytes.to_vec(),
            claim.clone(),
        )
        .await
    }

    async fn acquire_process_execution_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.acquire_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            env_ref.as_str().to_owned(),
            claim.clone(),
        )
        .await
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_artifact_referrer(PROCESS_ENV_NAMESPACE, cleanup.clone())
            .await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core_execution::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Decode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.get_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            env_ref.as_str().to_owned(),
            format!("process execution env `{env_ref}`"),
        )
        .await
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_execution::{
        ArtifactCarry, ArtifactName, ArtifactStoreId, HostArtifactPin, ModuleArtifactStore,
    };

    fn pin() -> ArtifactReferrer {
        ArtifactReferrer::HostPin(HostArtifactPin::mint())
    }

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::open(&dir.path().join("artifact.db"))
            .await
            .expect("open store");
        (dir, store)
    }

    async fn edge_count(store: &SqliteStore, artifact_ref: &str) -> i64 {
        let artifact_ref = artifact_ref.to_owned();
        store.conn.call(move |conn| {
            conn.query_row("SELECT COUNT(*) FROM artifact_referrer_edges WHERE namespace = 'lashlang_module' AND artifact_ref = ?1",
                params![artifact_ref], |row| row.get(0))
        }).await.expect("edge count")
    }

    #[tokio::test]
    async fn publish_acquire_and_read_keep_exact_edges() {
        let (_dir, store) = store().await;
        let first = pin();
        let second = pin();
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(first.clone()).expect("valid test value"),
                "module-1",
                b"one",
            )
            .await
            .expect("publish");
        store
            .acquire_module_artifact(
                &ReferrerClaim::unguarded(second.clone()).expect("valid test value"),
                "module-1",
            )
            .await
            .expect("acquire");
        store
            .acquire_module_artifact(
                &ReferrerClaim::unguarded(second).expect("valid test value"),
                "module-1",
            )
            .await
            .expect("repeat acquire");
        assert_eq!(edge_count(&store, "module-1").await, 2);
        assert_eq!(
            store
                .get_module_artifact("module-1")
                .await
                .expect("valid test value"),
            Some(b"one".to_vec())
        );
        assert!(matches!(
            store
                .acquire_module_artifact(
                    &ReferrerClaim::unguarded(pin()).expect("valid test value"),
                    "absent"
                )
                .await,
            Err(ArtifactStoreError::ArtifactMissing { .. })
        ));
        assert!(matches!(
            store
                .publish_module_artifact(
                    &ReferrerClaim::unguarded(first).expect("valid test value"),
                    "module-1",
                    b"other"
                )
                .await,
            Err(ArtifactStoreError::Immutable { .. })
        ));
    }

    #[tokio::test]
    async fn guarded_publication_arms_cleanup_with_the_edge() {
        let (_dir, store) = store().await;
        let journal = lash_sansio::ExecutionScope::runtime_operation("guarded-publication")
            .journal_identity()
            .expect("journal identity");
        let claim = ReferrerClaim::guarded(
            ArtifactReferrer::Execution(journal),
            lash_core_execution::ArtifactCleanupPlan::AwaitJournal,
        )
        .expect("execution claim");
        store
            .publish_module_artifact(&claim, "guarded-module", b"guarded")
            .await
            .expect("publish with guard");
        let (edges, obligations): (i64, i64) = store.conn.call(|conn| {
            Ok((
                conn.query_row("SELECT COUNT(*) FROM artifact_referrer_edges WHERE artifact_ref = 'guarded-module'", [], |row| row.get(0))?,
                conn.query_row("SELECT COUNT(*) FROM artifact_cleanup_obligations WHERE referrer_kind = 'execution'", [], |row| row.get(0))?,
            ))
        }).await.expect("read edge and guard");
        assert_eq!((edges, obligations), (1, 1));
    }

    #[tokio::test]
    async fn process_environment_publish_acquire_and_end_preserve_other_namespace() {
        use lash_core_execution::ProcessExecutionEnvStore;
        let (_dir, store) = store().await;
        let spec = lash_core_execution::ProcessExecutionEnvSpec::new(
            lash_core_execution::PluginOptions::default(),
            lash_core_execution::SessionPolicy::new(lash_core_execution::TurnBudget::Unbounded),
        );
        let bytes = spec.to_store_bytes().expect("encode environment");
        let env_ref = spec.stable_ref().expect("environment reference");
        let source = pin();
        let other = pin();
        store
            .publish_process_execution_env(
                &ReferrerClaim::unguarded(source.clone()).expect("source claim"),
                &env_ref,
                &bytes,
            )
            .await
            .expect("publish environment");
        store
            .acquire_process_execution_env(
                &ReferrerClaim::unguarded(other.clone()).expect("other claim"),
                &env_ref,
            )
            .await
            .expect("acquire environment");
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(other).expect("module claim"),
                env_ref.as_str(),
                b"module",
            )
            .await
            .expect("publish same reference in module namespace");
        store
            .end_process_env_referrer(&ResolvedArtifactCleanup {
                referrer: source,
                carries: Vec::new(),
            })
            .await
            .expect("end first environment referrer");
        assert_eq!(
            store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read environment"),
            Some(bytes)
        );
        assert_eq!(
            store
                .get_module_artifact(env_ref.as_str())
                .await
                .expect("read module"),
            Some(b"module".to_vec())
        );
    }

    #[tokio::test]
    async fn end_carries_before_sever_and_reclaims_only_after_last_edge() {
        let (_dir, store) = store().await;
        let source = pin();
        let destination = pin();
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(source.clone()).expect("valid test value"),
                "module-2",
                b"two",
            )
            .await
            .expect("publish");
        let resolved = ResolvedArtifactCleanup {
            referrer: source.clone(),
            carries: vec![ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::LashlangModule,
                    artifact_ref: "module-2".into(),
                },
                to: destination.clone(),
            }],
        };
        store
            .end_module_referrer(&resolved)
            .await
            .expect("carry and end");
        store
            .end_module_referrer(&resolved)
            .await
            .expect("replay end");
        assert_eq!(edge_count(&store, "module-2").await, 1);
        assert!(matches!(
            store
                .acquire_module_artifact(
                    &ReferrerClaim::unguarded(source).expect("valid test value"),
                    "module-2"
                )
                .await,
            Err(ArtifactStoreError::ReferrerEnded { .. })
        ));
        store
            .end_module_referrer(&ResolvedArtifactCleanup {
                referrer: destination,
                carries: Vec::new(),
            })
            .await
            .expect("end destination");
        assert_eq!(edge_count(&store, "module-2").await, 0);
        assert!(
            store
                .get_module_artifact("module-2")
                .await
                .expect("valid test value")
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_carry_rolls_back_fence_and_allows_retry() {
        let (_dir, store) = store().await;
        let source = pin();
        let destination = pin();
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(source.clone()).expect("valid test value"),
                "module-3",
                b"three",
            )
            .await
            .expect("publish");
        let bad = ResolvedArtifactCleanup {
            referrer: source.clone(),
            carries: vec![ArtifactCarry {
                artifact: ArtifactName {
                    store: ArtifactStoreId::LashlangModule,
                    artifact_ref: "missing".into(),
                },
                to: destination,
            }],
        };
        assert!(matches!(
            store.end_module_referrer(&bad).await,
            Err(ArtifactStoreError::CarryArtifactMissing { .. })
        ));
        assert_eq!(edge_count(&store, "module-3").await, 1);
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(source.clone()).expect("valid test value"),
                "module-4",
                b"four",
            )
            .await
            .expect("failed carry did not fence");
        store
            .end_module_referrer(&ResolvedArtifactCleanup {
                referrer: source,
                carries: Vec::new(),
            })
            .await
            .expect("retry end");
    }

    #[test]
    fn referrer_columns_reject_empty_ids_and_old_catalogs_have_no_edge_table() {
        let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
        conn.execute_batch(crate::schema::SCHEMA)
            .expect("create durable core");
        conn.execute(
            "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref) VALUES ('lashlang_module', 'm', 'b')",
            [],
        ).expect("insert artifact pointer");
        assert!(conn.execute(
            "INSERT INTO artifact_referrer_edges (namespace, artifact_ref, referrer_kind, referrer_id) VALUES ('lashlang_module', 'm', 'host_pin', '')",
            [],
        ).is_err());
        assert!(conn.execute(
            "INSERT INTO artifact_referrer_fences (referrer_kind, referrer_id, ended_at_ms) VALUES ('host_pin', '', 1)",
            [],
        ).is_err());

        let old = rusqlite::Connection::open_in_memory().expect("open old SQLite catalog");
        old.execute_batch("CREATE TABLE artifact_owners (owner_kind TEXT, owner_id TEXT)")
            .expect("create old table");
        assert!(
            old.query_row(
                artifact_sql().edges.select_artifact_edges.sql(),
                params![MODULE_ARTIFACT_NAMESPACE, "m"],
                |row| row.get::<_, String>(0)
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn artifact_read_refuses_a_malformed_stored_referrer() {
        let (_dir, store) = store().await;
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(pin()).expect("host pin claim"),
                "malformed-edge",
                b"module",
            )
            .await
            .expect("publish module");
        store
            .conn
            .write(|tx| {
                tx.execute(
                    artifact_sql().edges.insert_edge.sql(),
                    params![
                        MODULE_ARTIFACT_NAMESPACE,
                        "malformed-edge",
                        "host_pin",
                        "not-a-pin"
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("insert malformed edge");
        let error = store
            .get_module_artifact("malformed-edge")
            .await
            .expect_err("malformed referrer pair must fail the artifact read");
        assert!(matches!(
            &error,
            ArtifactStoreError::StoredDataCorrupt {
                record_kind: "artifact referrer edge",
                ..
            }
        ));
        let plugin_error: lash_core_execution::PluginError = error.into();
        assert!(matches!(
            plugin_error,
            lash_core_execution::PluginError::Runtime(runtime)
                if runtime.code == lash_core_execution::RuntimeErrorCode::RuntimeStoreCorrupt
        ));
    }

    #[tokio::test]
    async fn artifact_read_refuses_an_unknown_referrer_kind_and_keeps_the_bytes() {
        let (_dir, store) = store().await;
        store
            .publish_module_artifact(
                &ReferrerClaim::unguarded(pin()).expect("host pin claim"),
                "future-edge",
                b"module",
            )
            .await
            .expect("publish module");
        store
            .conn
            .call(|conn| {
                conn.execute_batch("PRAGMA ignore_check_constraints = ON")?;
                conn.execute(
                    artifact_sql().edges.insert_edge.sql(),
                    params![
                        MODULE_ARTIFACT_NAMESPACE,
                        "future-edge",
                        "synthetic_next",
                        "x"
                    ],
                )?;
                conn.execute_batch("PRAGMA ignore_check_constraints = OFF")?;
                Ok(())
            })
            .await
            .expect("inject an edge written by the next build");
        assert!(matches!(
            store.get_module_artifact("future-edge").await,
            Err(ArtifactStoreError::Incompatible {
                refusal: lash_core_execution::compat::CompatRefusal::UnknownVocabulary {
                    label,
                    ..
                }
            }) if label == "synthetic_next"
        ));
        assert_eq!(edge_count(&store, "future-edge").await, 2);
    }
}
