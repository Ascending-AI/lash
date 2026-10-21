//! The lash_vm module-artifact store and the process-execution-env store.
//!
//! The SQLite owner of the artifact family: `artifact_refs` (this backend's
//! pointer from a namespaced reference to its bytes), `artifact_referrer_edges` (the
//! exact edges that keep an artifact alive) and
//! `referrer_fences` (the permanent publication fence). The bytes
//! themselves live in `blobs`, shared with checkpoint storage, which is why
//! reclaiming one is conditional on every other rooting relation.
//!
//! Both traits here are `#[async_trait]` surfaces over the async
//! [`SqliteConnection`]: every DB body is a synchronous rusqlite closure
//! handed to `conn.call` (reads) or `conn.write` (read-then-write), and only
//! the wrapper call is awaited.

use std::sync::LazyLock;

use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::ArtifactStoreId;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_store_sql::artifact::blobs::BlobStatements;
use lash_store_sql::artifact::referrer_edges::ReferrerEdgeStatements;
use lash_store_sql::artifact::referrer_fences::ReferrerFenceStatements;

use super::*;

lash_store_sql::statements! {
    /// `artifact_refs` statements only SQLite issues.
    ///
    /// Every one of them: PostgreSQL keeps artifact bytes inline in
    /// `lash_lash_vm_artifacts` and has no pointer table, so this whole set
    /// exists on one backend only.
    pub(crate) struct RefSqliteStatements @ "artifact_ref" {
        /// First missing manifest member, in the caller's refusal order.
        select_missing_manifest = "SELECT wanted.value ->> 1 FROM json_each(?1) AS wanted
             WHERE NOT EXISTS (SELECT 1 FROM artifact_refs AS stored
                 WHERE stored.namespace = wanted.value ->> 0
                   AND stored.artifact_ref = wanted.value ->> 1)
             ORDER BY CAST(wanted.key AS INTEGER) LIMIT 1";

        /// Acquire a complete checked closure without per-artifact statements.
        insert_manifest_edges = "INSERT INTO artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             SELECT value ->> 0, value ->> 1, ?2, ?3 FROM json_each(?1) WHERE true
             ON CONFLICT DO NOTHING";

        /// Point `?1`/`?2` at blob `?3`. An artifact is immutable, so a
        /// second publication of the same reference keeps the first pointer
        /// and the caller compares what it reads back.
        insert_pointer = "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (namespace, artifact_ref) DO NOTHING";

        /// The blob `?1`/`?2` points at.
        select_blob_ref = "SELECT blob_ref FROM artifact_refs
             WHERE namespace = ?1 AND artifact_ref = ?2";

        /// The blob `?1`/`?2` points at and its encoding: a graph node is a
        /// node-bodied artifact's root (FIG-6469). A dangling pointer has
        /// none.
        select_body_ref = "SELECT refs.blob_ref, blobs.encoding FROM artifact_refs AS refs
             LEFT JOIN blobs ON blobs.hash = refs.blob_ref
             WHERE refs.namespace = ?1 AND refs.artifact_ref = ?2";

        /// Every pointer row, as the collector's root set.
        select_gc_roots = "SELECT namespace, blob_ref FROM artifact_refs
             ORDER BY namespace, artifact_ref";

        /// One preflight page of published artifacts in namespace `?1`, after
        /// reference `?2`, at most `?3` rows.
        select_preflight_page = "SELECT refs.artifact_ref, refs.blob_ref, blobs.content, blobs.encoding
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
    let dialect = crate::schema_layout::MAIN;
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
pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "kernel_document";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";
pub(crate) const PROCESS_DEFINITION_NAMESPACE: &str = "process_definition";
pub(crate) const TOOL_MATERIAL_NAMESPACE: &str = "tool_material";
pub(crate) const TURN_PRELUDE_NAMESPACE: &str = "turn_prelude";

mod held;
pub(crate) mod tool_material;

/// The namespace of a store-set artifact store; an engine's own store has
/// none here.
pub(crate) fn store_namespace(store: &ArtifactStoreId) -> Option<&'static str> {
    match store {
        ArtifactStoreId::KernelDocument => Some(MODULE_ARTIFACT_NAMESPACE),
        ArtifactStoreId::ProcessEnv => Some(PROCESS_ENV_NAMESPACE),
        ArtifactStoreId::ProcessDefinition => Some(PROCESS_DEFINITION_NAMESPACE),
        ArtifactStoreId::ToolMaterial => Some(TOOL_MATERIAL_NAMESPACE),
        ArtifactStoreId::TurnPrelude => Some(TURN_PRELUDE_NAMESPACE),
        ArtifactStoreId::Engine(_) => None,
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
        lash_core_store::artifact_referrer::ArtifactReferrerError::UnknownKind(label) => {
            StoreError::Incompatible {
                refusal: lash_core_store::compat::CompatRefusal::UnknownVocabulary {
                    surface: "artifact referrer edge kind".to_owned(),
                    label,
                },
            }
        }
        other => stored_data_corrupt("artifact referrer edge", other),
    })
}

/// Read and retain environment provenance inside the same writer transaction.
fn retain_process_env_tx(
    tx: &rusqlite::Connection,
    env_ref: &str,
    referrer: &ArtifactReferrer,
) -> rusqlite::Result<()> {
    fn read(
        tx: &rusqlite::Connection,
        namespace: &str,
        reference: &str,
    ) -> rusqlite::Result<Vec<u8>> {
        let stored: Option<(String, Option<i64>)> = tx
            .prepare_cached(artifact_sql().refs.select_body_ref.sql())?
            .query_row(params![namespace, reference], |row| {
                Ok((crate::codec::hash_column(row, 0)?, row.get(1)?))
            })
            .optional()?;
        let Some((blob, encoding)) = stored else {
            return Err(artifact_failure(ArtifactStoreError::ArtifactMissing {
                artifact_ref: reference.into(),
            }));
        };
        if encoding == Some(1) {
            SqliteStore::module_node_conn(tx, &blob).map_err(sqlite_conversion_error)
        } else {
            SqliteStore::get_blob_conn(tx, &BlobRef(blob))
                .map_err(sqlite_conversion_error)?
                .ok_or_else(|| {
                    artifact_failure(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: reference.into(),
                    })
                })
        }
    }
    let bytes = read(tx, PROCESS_ENV_NAMESPACE, env_ref)?;
    let mut pending = lash_durable::packages::process_env_roots(&bytes)
        .map_err(|error| artifact_failure(error.into()))?;
    let mut found = std::collections::BTreeSet::new();
    while let Some(reference) = pending.pop() {
        if !found.insert(reference.clone()) {
            continue;
        }
        let bytes = read(tx, MODULE_ARTIFACT_NAMESPACE, &reference)?;
        pending.extend(
            lash_durable::packages::content_dependencies(&reference, &bytes)
                .map_err(|error| artifact_failure(error.into()))?,
        );
    }
    for reference in found {
        crate::conn::cached_execute(
            tx,
            artifact_sql().edges.insert_edge.sql(),
            params![
                MODULE_ARTIFACT_NAMESPACE,
                reference,
                referrer.kind().as_str(),
                referrer.canonical_id()
            ],
        )?;
    }
    Ok(())
}

/// Validate and retain a start's environment in the transaction admitting
/// its process. The writer orders this acquisition against source cleanup.
pub(crate) fn acquire_process_env_tx(
    tx: &rusqlite::Connection,
    env: &lash_core_store::process_identity::ProcessEnvRef,
    process: &lash_protocol_types::ProcessId,
) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
    let referrer = ArtifactReferrer::ProcessRecord(process.clone());
    if artifact_fenced_tx(tx, &referrer).map_err(artifact_sqlite_error)? {
        return Err(ArtifactStoreError::ReferrerEnded { referrer }.into());
    }
    let stored: Option<String> = tx
        .prepare_cached(artifact_sql().refs.select_blob_ref.sql())
        .and_then(|mut statement| {
            statement.query_row(params![PROCESS_ENV_NAMESPACE, env.as_str()], |row| {
                crate::codec::hash_column(row, 0)
            })
        })
        .optional()
        .map_err(artifact_sqlite_error)?;
    if stored.is_none() {
        return Err(ArtifactStoreError::ArtifactMissing {
            artifact_ref: env.as_str().to_owned(),
        }
        .into());
    }
    retain_process_env_tx(tx, env.as_str(), &referrer).map_err(artifact_sqlite_error)?;
    crate::conn::cached_execute(
        tx,
        artifact_sql().edges.insert_edge.sql(),
        params![
            PROCESS_ENV_NAMESPACE,
            env.as_str(),
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )
    .map_err(artifact_sqlite_error)?;
    Ok(())
}

pub(crate) fn artifact_fenced_tx(
    tx: &rusqlite::Connection,
    referrer: &ArtifactReferrer,
) -> rusqlite::Result<bool> {
    tx.prepare_cached(artifact_sql().fences.select_is_fenced.sql())
        .and_then(|mut statement| {
            statement.query_row(
                params![referrer.kind().as_str(), referrer.canonical_id()],
                |row| row.get(0),
            )
        })
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
        plugin_publication: lash_core_store::store::plugin_writers::PluginPublication,
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let blob_profile = self.options.blob_profile;
        let now_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| {
                // The plugin namespaces the artifact carries are admitted
                // against the fleet record's writer ranges before it is
                // written (FIG-4746).
                tx.admit_plugin_writers(&plugin_publication)?;
                let referrer = &claim.referrer();
                if artifact_fenced_tx(tx, referrer)? {
                    return Err(artifact_failure(ArtifactStoreError::ReferrerEnded {
                        referrer: referrer.clone(),
                    }));
                }
                if let Some(cleanup) = claim.guard_cleanup() {
                    crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms)
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
                    params![
                        namespace,
                        artifact_ref,
                        crate::codec::SqlHash(blob_ref.as_str())
                    ],
                )?;
                let stored_blob_ref: String = tx
                    .prepare_cached(artifact_sql().refs.select_blob_ref.sql())
                    .and_then(|mut statement| {
                        statement.query_row(params![namespace, artifact_ref], |row| {
                            crate::codec::hash_column(row, 0)
                        })
                    })?;
                if stored_blob_ref != blob_ref.as_str() {
                    return Err(artifact_failure(ArtifactStoreError::Immutable {
                        artifact_ref,
                    }));
                }
                if namespace == PROCESS_ENV_NAMESPACE {
                    retain_process_env_tx(tx, &artifact_ref, referrer)?;
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
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let now_ms = self.clock.timestamp_ms();
        self.conn.write(move |tx| {
            let referrer = &claim.referrer();
            if artifact_fenced_tx(tx, referrer)? {
                return Err(artifact_failure(ArtifactStoreError::ReferrerEnded { referrer: referrer.clone() }));
            }
            let exists: bool = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2)").and_then(|mut statement| statement.query_row(
                params![namespace, artifact_ref], |row| row.get(0)))?;
            if !exists {
                return Err(artifact_failure(ArtifactStoreError::ArtifactMissing { artifact_ref }));
            }
            if let Some(cleanup) = claim.guard_cleanup() {
                crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms)
                    .map_err(sqlite_conversion_error)?;
            }
            if namespace == PROCESS_ENV_NAMESPACE {
                retain_process_env_tx(tx, &artifact_ref, referrer)?;
            }
            crate::conn::cached_execute(tx, artifact_sql().edges.insert_edge.sql(),
                params![namespace, artifact_ref, referrer.kind().as_str(), referrer.canonical_id()])?;
            Ok(())
        }).await.map_err(artifact_sqlite_error)
    }

    async fn end_artifact_referrer(
        &self,
        namespace: &'static str,
        cleanup: ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        for carry in &cleanup.carries {
            if !carry.to.kind().holds_artifacts() {
                return Err(ArtifactStoreError::ReferrerKindRefused {
                    kind: carry.to.kind(),
                });
            }
        }
        let now_ms = self.clock.timestamp_ms();
        let expected_store = match namespace {
            MODULE_ARTIFACT_NAMESPACE => ArtifactStoreId::KernelDocument,
            PROCESS_ENV_NAMESPACE => ArtifactStoreId::ProcessEnv,
            PROCESS_DEFINITION_NAMESPACE => ArtifactStoreId::ProcessDefinition,
            TURN_PRELUDE_NAMESPACE => ArtifactStoreId::TurnPrelude,
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
        self.conn
            .write(move |tx| Self::end_namespaced_tx(tx, namespace, &cleanup, now_ms))
            .await
            .map_err(artifact_sqlite_error)
    }

    /// Fence, carry, sever and reclaim in the owner's existing transaction.
    pub(crate) fn end_namespaced_tx(
        tx: &rusqlite::Connection,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let refs: Vec<String> = {
            let mut stmt = tx.prepare_cached(
                artifact_sql()
                    .edges
                    .select_referrer_edges_in_namespace
                    .sql(),
            )?;
            stmt.query_map(
                params![
                    namespace,
                    cleanup.referrer.kind().as_str(),
                    cleanup.referrer.canonical_id()
                ],
                |row| {
                    let artifact_ref = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let id: String = row.get(3)?;
                    decode_stored_edge(&kind, &id).map_err(sqlite_conversion_error)?;
                    Ok(artifact_ref)
                },
            )?
            .collect::<rusqlite::Result<_>>()?
        };
        if refs.is_empty() && artifact_fenced_tx(tx, &cleanup.referrer)? {
            return Ok(());
        }
        fence_artifact_referrer_tx(tx, &cleanup.referrer, now_ms)?;
        let mut carries = cleanup.carries.clone();
        carries.sort_by(|left, right| left.artifact.artifact_ref.cmp(&right.artifact.artifact_ref));
        for carry in &carries {
            if artifact_fenced_tx(tx, &carry.to)? {
                continue;
            }
            let artifact_ref = &carry.artifact.artifact_ref;
            let exists: bool = tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2)").and_then(|mut statement| statement.query_row(
                    params![namespace, artifact_ref], |row| row.get(0)))?;
            if !exists {
                return Err(artifact_failure(ArtifactStoreError::CarryArtifactMissing {
                    artifact_ref: artifact_ref.clone(),
                    to: carry.to.clone(),
                }));
            }
            if namespace == PROCESS_ENV_NAMESPACE {
                retain_process_env_tx(tx, artifact_ref, &carry.to)?;
            }
            crate::conn::cached_execute(
                tx,
                artifact_sql().edges.insert_edge.sql(),
                params![
                    namespace,
                    artifact_ref,
                    carry.to.kind().as_str(),
                    carry.to.canonical_id()
                ],
            )?;
        }
        crate::conn::cached_execute(
            tx,
            artifact_sql()
                .edges
                .delete_referrer_edges_in_namespace
                .sql(),
            params![
                namespace,
                cleanup.referrer.kind().as_str(),
                cleanup.referrer.canonical_id()
            ],
        )?;
        Self::reclaim_unreferenced_artifacts_tx(tx, namespace, &refs)
    }

    /// The database-owned share of an end. The obligation remains until
    /// independent engine stores and attachment bytes acknowledge it.
    pub(crate) fn end_local_referrer_tx(
        tx: &rusqlite::Connection,
        referrer: &ArtifactReferrer,
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        let cleanup = ResolvedArtifactCleanup {
            referrer: referrer.clone(),
            carries: Vec::new(),
        };
        for namespace in [
            MODULE_ARTIFACT_NAMESPACE,
            PROCESS_ENV_NAMESPACE,
            PROCESS_DEFINITION_NAMESPACE,
            TURN_PRELUDE_NAMESPACE,
        ] {
            Self::end_namespaced_tx(tx, namespace, &cleanup, now_ms)?;
        }
        Ok(())
    }

    /// Hold one definition closure under the claim in one transaction: the
    /// referrer's fence, every manifest artifact stored, the descriptor
    /// published (verified byte for byte against a stored one) or stored,
    /// the claim's guard, then every edge (ADR 0113 §3.6).
    async fn hold_definition_closure(
        &self,
        claim: ReferrerClaim,
        id: String,
        descriptor: Option<Vec<u8>>,
        manifest: Vec<(&'static str, String)>,
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let blob_profile = self.options.blob_profile;
        let now_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| {
                let referrer = &claim.referrer();
                if artifact_fenced_tx(tx, referrer)? {
                    return Err(artifact_failure(ArtifactStoreError::ReferrerEnded {
                        referrer: referrer.clone(),
                    }));
                }
                let stored = |namespace: &str, artifact_ref: &str| -> rusqlite::Result<bool> {
                    tx.prepare_cached("SELECT EXISTS (SELECT 1 FROM artifact_refs WHERE namespace = ?1 AND artifact_ref = ?2)").and_then(|mut statement| statement.query_row(
                        params![namespace, artifact_ref],
                        |row| row.get(0),
                    ))
                };
                let manifest_json = serde_json::to_string(&manifest)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                let missing: Option<String> = tx.prepare_cached(artifact_sql().refs.select_missing_manifest.sql()).and_then(|mut statement| statement.query_row(
                    params![manifest_json], |row| row.get(0),
                )).optional()?;
                if let Some(artifact_ref) = missing {
                    return Err(artifact_failure(ArtifactStoreError::ArtifactMissing {
                        artifact_ref,
                    }));
                }
                match &descriptor {
                    Some(bytes) => {
                        let blob_ref = Self::insert_artifact_blob_conn(
                            tx,
                            BlobArtifactDescriptor::process_definition(),
                            bytes,
                            blob_profile,
                            tx.fleet(),
                        )?;
                        crate::conn::cached_execute(
                            tx,
                            artifact_sql().refs.insert_pointer.sql(),
                            params![PROCESS_DEFINITION_NAMESPACE, id, crate::codec::SqlHash(blob_ref.as_str())],
                        )?;
                        let stored_blob_ref: String = tx.prepare_cached(artifact_sql().refs.select_blob_ref.sql()).and_then(|mut statement| statement.query_row(
                            params![PROCESS_DEFINITION_NAMESPACE, id],
                            |row| crate::codec::hash_column(row, 0),
                        ))?;
                        if stored_blob_ref != blob_ref.as_str() {
                            return Err(artifact_failure(ArtifactStoreError::Immutable {
                                artifact_ref: id,
                            }));
                        }
                    }
                    None => {
                        if !stored(PROCESS_DEFINITION_NAMESPACE, &id)? {
                            return Err(artifact_failure(ArtifactStoreError::ArtifactMissing {
                                artifact_ref: id,
                            }));
                        }
                    }
                }
                if let Some(cleanup) = claim.guard_cleanup() {
                    crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now_ms)
                        .map_err(sqlite_conversion_error)?;
                }
                let closure: Vec<_> = manifest
                    .iter()
                    .map(|(namespace, artifact_ref)| (*namespace, artifact_ref.as_str()))
                    .chain(std::iter::once((PROCESS_DEFINITION_NAMESPACE, id.as_str())))
                    .collect();
                crate::conn::cached_execute(
                    tx,
                    artifact_sql().refs.insert_manifest_edges.sql(),
                    params![serde_json::to_string(&closure).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
                        referrer.kind().as_str(), referrer.canonical_id()],
                )?;
                Ok(())
            })
            .await
            .map_err(artifact_sqlite_error)
    }

    async fn get_artifact_ref_blob(
        &self,
        namespace: &'static str,
        artifact_ref: String,
        missing_diagnostic: String,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        #[cfg(feature = "testing")]
        let pauses = self.conn.pauses();
        // The pointer, its edges and the body it names are one snapshot: a
        // release that reclaims the pointer and peels its body cannot land
        // between them.
        // The read runs on a reader, never queued behind the writer's
        // batches (FIG-6939).
        let resolved = self
            .read_connection()
            .read(move |conn| {
                let stored: Option<(String, Option<i64>)> = conn
                    .prepare_cached(artifact_sql().refs.select_body_ref.sql())
                    .and_then(|mut statement| {
                        statement.query_row(params![namespace, artifact_ref], |row| {
                            Ok((crate::codec::hash_column(row, 0)?, row.get(1)?))
                        })
                    })
                    .optional()?;
                let Some((blob_ref, encoding)) = stored else {
                    return Ok(None);
                };
                #[cfg(feature = "testing")]
                if let Some(pauses) = pauses.as_ref() {
                    pauses.reach_compound_read(crate::testing::SqliteCompoundRead::Artifact);
                }
                let mut edges =
                    conn.prepare_cached(artifact_sql().edges.select_artifact_edges.sql())?;
                let mut rows = edges.query(params![namespace, artifact_ref])?;
                while let Some(row) = rows.next()? {
                    let kind: String = row.get(0)?;
                    let id: String = row.get(1)?;
                    decode_stored_edge(&kind, &id).map_err(sqlite_conversion_error)?;
                }
                if encoding == Some(1) {
                    return Self::module_node_conn(conn, &blob_ref)
                        .map(|body| Some(Some(body)))
                        .map_err(sqlite_conversion_error);
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

impl SqliteStore {
    /// The logical body of the node-bodied artifact whose row roots
    /// `root`: its closure read in the caller's transaction, every node
    /// checked.
    fn module_node_conn(conn: &rusqlite::Connection, root: &str) -> Result<Vec<u8>, StoreError> {
        let corrupt = |message: String| stored_data_corrupt("module artifact node", message);
        let node = lash_core_store::checkpoint_graph::NodeId::from_hex(root)
            .ok_or_else(|| corrupt(format!("`{root}` is not a node reference")))?;
        let closure = crate::durable::graph_closure(
            conn,
            node,
            &lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .map_err(sqlite_error)?
        .map_err(|refusal| corrupt(refusal.to_string()))?;
        lash_execution_contracts::module_nodes::module_node_body(node, &closure)
            .map_err(|error| corrupt(error.to_string()))
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::module_artifacts::ModuleArtifactStore for SqliteStore {
    fn durability_tier(&self) -> lash_execution_contracts::module_artifacts::DurabilityTier {
        lash_execution_contracts::module_artifacts::DurabilityTier::Durable
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
        self.publish_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.to_owned(),
            BlobArtifactDescriptor::kernel_document(),
            bytes.to_vec(),
            claim.clone(),
            Default::default(),
        )
        .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        if lash_durable::packages::is_package_reference(module_ref) {
            return lash_durable::packages::acquire_content(self, claim, module_ref)
                .await
                .map_err(Into::into);
        }
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

    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        if artifacts.is_empty() {
            return Ok(());
        }
        let artifacts = artifacts
            .iter()
            .map(|artifact| {
                if !crate::namespace::is_valid_opaque_key(artifact.module_ref()) {
                    return Err(ArtifactStoreError::Encode(
                        "invalid module reference".into(),
                    ));
                }
                Ok(held::HeldArtifact {
                    artifact_ref: artifact.module_ref().to_owned(),
                    bytes: artifact.body().bytes().to_vec(),
                    node: artifact.body().node_kind(),
                    verified: matches!(
                        artifact,
                        lash_execution_contracts::module_artifacts::ModuleArtifactHold::Publish { .. }
                    ),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.hold_module_artifact_batch(claim.clone(), artifacts)
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
            return Err(ArtifactStoreError::StoredDataCorrupt {
                source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::InvalidReference {
                    record_kind: "module".into(),
                    reference: module_ref.into(),
                },
            });
        }
        self.get_artifact_ref_blob(
            MODULE_ARTIFACT_NAMESPACE,
            module_ref.to_owned(),
            format!("lash_vm module artifact `{module_ref}`"),
        )
        .await
        .map_err(Into::into)
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEnvStore for SqliteStore {
    async fn publish_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
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
        let plugin_publication =
            lash_core_store::store::plugin_writers::PluginPublication::of_process_env(bytes)
                .map_err(|error| ArtifactStoreError::Encode(error.to_string()))?;
        self.publish_artifact_ref_blob(
            PROCESS_ENV_NAMESPACE,
            env_ref.as_str().to_owned(),
            BlobArtifactDescriptor::process_execution_env(),
            bytes.to_vec(),
            claim.clone(),
            plugin_publication,
        )
        .await
    }

    async fn acquire_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
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

    async fn get_process_env(
        &self,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::StoredDataCorrupt {
                source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::InvalidReference {
                    record_kind: "process execution environment".into(),
                    reference: env_ref.as_str().into(),
                },
            });
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

#[async_trait::async_trait]
impl lash_execution_contracts::turn_prelude::TurnPreludeStore for SqliteStore {
    async fn publish_turn_prelude(
        &self,
        claim: &ReferrerClaim,
        prelude_ref: &lash_execution_contracts::turn_prelude::TurnPreludeRef,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        if !prelude_ref.matches_store_bytes(bytes) {
            return Err(ArtifactStoreError::Immutable {
                artifact_ref: prelude_ref.as_str().to_owned(),
            });
        }
        // A prelude is journal-scoped turn state, not a plugin publication:
        // it carries no namespace the fleet's writer ranges admit.
        self.publish_artifact_ref_blob(
            TURN_PRELUDE_NAMESPACE,
            prelude_ref.as_str().to_owned(),
            BlobArtifactDescriptor::turn_prelude(),
            bytes.to_vec(),
            claim.clone(),
            lash_core_store::store::plugin_writers::PluginPublication::default(),
        )
        .await
    }

    async fn end_turn_prelude_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_artifact_referrer(TURN_PRELUDE_NAMESPACE, cleanup.clone())
            .await
    }

    async fn get_turn_prelude(
        &self,
        prelude_ref: &lash_execution_contracts::turn_prelude::TurnPreludeRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(prelude_ref.as_str()) {
            return Err(ArtifactStoreError::StoredDataCorrupt {
                source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::InvalidReference {
                    record_kind: "turn prelude".into(),
                    reference: prelude_ref.as_str().into(),
                },
            });
        }
        self.get_artifact_ref_blob(
            TURN_PRELUDE_NAMESPACE,
            prelude_ref.as_str().to_owned(),
            format!("turn prelude `{prelude_ref}`"),
        )
        .await
        .map_err(Into::into)
    }
}

/// A manifest's store-set share as `(namespace, reference)` pairs. Only the
/// module and environment stores share a definition's transaction.
fn definition_manifest(
    manifest: &[lash_core_store::artifact_referrer::ArtifactName],
) -> Result<Vec<(&'static str, String)>, ArtifactStoreError> {
    manifest
        .iter()
        .map(|artifact| {
            let namespace = match &artifact.store {
                ArtifactStoreId::KernelDocument => MODULE_ARTIFACT_NAMESPACE,
                ArtifactStoreId::ProcessEnv => PROCESS_ENV_NAMESPACE,
                other => {
                    return Err(ArtifactStoreError::Backend(format!(
                        "a definition manifest held in the descriptor's transaction names \
                         store {other:?}"
                    )));
                }
            };
            if !crate::namespace::is_valid_opaque_key(&artifact.artifact_ref) {
                return Err(ArtifactStoreError::Encode(
                    "invalid definition manifest reference".into(),
                ));
            }
            Ok((namespace, artifact.artifact_ref.clone()))
        })
        .collect()
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessDefinitionStore for SqliteStore {
    async fn publish_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_protocol_types::ProcessDefinitionId,
        descriptor: &[u8],
        manifest: &[lash_core_store::artifact_referrer::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.hold_definition_closure(
            claim.clone(),
            id.as_str().to_owned(),
            Some(descriptor.to_vec()),
            definition_manifest(manifest)?,
        )
        .await
    }

    async fn acquire_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_protocol_types::ProcessDefinitionId,
        manifest: &[lash_core_store::artifact_referrer::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.hold_definition_closure(
            claim.clone(),
            id.as_str().to_owned(),
            None,
            definition_manifest(manifest)?,
        )
        .await
    }

    async fn end_process_definition_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_artifact_referrer(PROCESS_DEFINITION_NAMESPACE, cleanup.clone())
            .await
    }

    async fn get_process_definition(
        &self,
        id: &lash_protocol_types::ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        self.get_artifact_ref_blob(
            PROCESS_DEFINITION_NAMESPACE,
            id.as_str().to_owned(),
            format!("process definition `{id}`"),
        )
        .await
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core_store::artifact_referrer::ArtifactCarry;
    use lash_core_store::artifact_referrer::ArtifactName;
    use lash_core_store::artifact_referrer::ArtifactStoreId;
    use lash_core_store::artifact_referrer::HostArtifactPin;
    use lash_execution_contracts::module_artifacts::ModuleArtifactStore;

    fn pin() -> ArtifactReferrer {
        ArtifactReferrer::HostPin(HostArtifactPin::mint())
    }

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::open(
            &dir.path().join("artifact.db"),
            crate::SqliteSynchronous::Normal,
        )
        .await
        .expect("open store");
        (dir, store)
    }

    async fn edge_count(store: &SqliteStore, artifact_ref: &str) -> i64 {
        let artifact_ref = artifact_ref.to_owned();
        store.conn.call(move |conn| {
            conn.query_row("SELECT COUNT(*) FROM artifact_referrer_edges WHERE namespace = 'kernel_document' AND artifact_ref = ?1",
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
        let journal = lash_protocol_types::ExecutionScope::runtime_operation("guarded-publication")
            .journal_identity()
            .expect("journal identity");
        let claim = ReferrerClaim::guarded(
            lash_core_store::artifact_referrer::ReferrerGuard::Journal(journal),
        );
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
        use lash_core_store::store::ArtifactCleanupLedger as _;
        let ledger = crate::obligation_ledger::SqliteArtifactCleanupLedger::new(&store);
        ledger
            .arm_cleanup(
                &lash_core_store::artifact_referrer::ArtifactCleanup::ended(
                    claim.referrer(),
                    Vec::new(),
                    None,
                ),
                1,
            )
            .await
            .expect("end the guarded reader");
        assert_eq!(
            edge_count(&store, "guarded-module").await,
            0,
            "the end transaction retires its local edge without a due pass"
        );
        assert_eq!(
            store
                .get_module_artifact("guarded-module")
                .await
                .expect("read"),
            None
        );
        assert!(
            matches!(
                store
                    .publish_module_artifact(&claim, "guarded-module", b"guarded")
                    .await,
                Err(ArtifactStoreError::ReferrerEnded { .. })
            ),
            "the end retains its publication fence"
        );
    }

    #[tokio::test]
    async fn process_environment_publish_acquire_and_end_preserve_other_namespace() {
        use lash_execution_contracts::process::ProcessEnvStore;
        let (_dir, store) = store().await;
        let spec = lash_execution_contracts::testing::process_env_with_limit(1024);
        let bytes = spec.to_store_bytes().expect("encode environment");
        let env_ref = spec.stable_ref().expect("environment reference");
        let source = pin();
        let other = pin();
        store
            .publish_process_env(
                &ReferrerClaim::unguarded(source.clone()).expect("source claim"),
                &env_ref,
                &bytes,
            )
            .await
            .expect("publish environment");
        store
            .acquire_process_env(
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
                .get_process_env(&env_ref)
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
                    store: ArtifactStoreId::KernelDocument,
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
                    store: ArtifactStoreId::KernelDocument,
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
            "INSERT INTO blobs (hash, content, encoding) VALUES (zeroblob(32), X'00', 0)",
            [],
        )
        .expect("insert artifact blob");
        conn.execute(
            "INSERT INTO artifact_refs (namespace, artifact_ref, blob_ref) VALUES ('kernel_document', 'm', zeroblob(32))",
            [],
        ).expect("insert artifact pointer");
        assert!(conn.execute(
            "INSERT INTO artifact_referrer_edges (namespace, artifact_ref, referrer_kind, referrer_id) VALUES ('kernel_document', 'm', 'host_pin', '')",
            [],
        ).is_err());
        assert!(conn.execute(
            "INSERT INTO referrer_fences (referrer_kind, referrer_id, ended_at_ms) VALUES ('host_pin', '', 1)",
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
                source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage { record_kind, .. },
            } if record_kind == "artifact referrer edge"
        ));
        let plugin_error: lash_execution_contracts::plugin_error::PluginError = error.into();
        assert!(matches!(
            plugin_error,
            lash_execution_contracts::plugin_error::PluginError::Runtime(runtime)
                if runtime.code == lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt
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
                refusal: lash_core_store::compat::CompatRefusal::UnknownVocabulary {
                    label,
                    ..
                }
            }) if label == "synthetic_next"
        ));
        assert_eq!(edge_count(&store, "future-edge").await, 2);
    }
}
