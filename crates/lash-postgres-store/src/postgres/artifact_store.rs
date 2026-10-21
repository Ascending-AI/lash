use std::collections::BTreeSet;
use std::sync::LazyLock;

use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
use lash_core_store::store::StoreError;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_nodes::stage_module_node;
use lash_store_sql::Dialect;
use lash_store_sql::artifact::referrer_edges::ReferrerEdgeStatements;
use lash_store_sql::artifact::referrer_fences::ReferrerFenceStatements;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

use crate::*;

lash_store_sql::statements! {
    pub(crate) struct ReferrerPostgresStatements @ "artifact_referrer_edge" {
        end_local = "SELECT lash_artifact_end_local(?1, ?2, ?3, ?4, ?5, ?6)";
        /// Delete artifact `?2` of namespace `?1` unless a referrer edge
        /// names it, returning the graph node a bridged artifact rooted.
        delete_unreferenced = "DELETE FROM lash_vm_artifacts AS artifact
             WHERE artifact.namespace = ?1 AND artifact.artifact_ref = ?2
               AND NOT EXISTS (SELECT 1 FROM artifact_referrer_edges AS edge
                   WHERE edge.namespace = artifact.namespace AND edge.artifact_ref = artifact.artifact_ref)
             RETURNING artifact.graph_ref";
    }
}

lash_store_sql::statements! {
    pub(crate) struct LashVmArtifactStatements @ "lash_vm_artifact" {
        /// First absent member, preserving the manifest's refusal order.
        select_missing_manifest = "SELECT wanted.artifact_ref
             FROM unnest(?1::TEXT[], ?2::TEXT[]) WITH ORDINALITY AS wanted(namespace, artifact_ref, ordinal)
             WHERE NOT EXISTS (SELECT 1 FROM lash_vm_artifacts AS stored
                 WHERE stored.namespace = wanted.namespace AND stored.artifact_ref = wanted.artifact_ref)
             ORDER BY wanted.ordinal LIMIT 1";
        /// The whole closure's exact retention edges, in one statement.
        insert_manifest_edges = "INSERT INTO artifact_referrer_edges
             (namespace, artifact_ref, referrer_kind, referrer_id)
             SELECT namespace, artifact_ref, ?3, ?4
             FROM unnest(?1::TEXT[], ?2::TEXT[]) AS wanted(namespace, artifact_ref)
             ON CONFLICT DO NOTHING";
        insert_bytes = "INSERT INTO lash_vm_artifacts (namespace, artifact_ref, storage_kind, artifact_bytes)
             VALUES (?1, ?2, 0, ?3) ON CONFLICT (namespace, artifact_ref) DO NOTHING";
        select_bytes = "SELECT artifact_bytes FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2";
        /// A node-bodied artifact's row: its graph node, which the row roots
        /// through the bridge (FIG-6469).
        insert_node = "INSERT INTO lash_vm_artifacts (namespace, artifact_ref, storage_kind, graph_ref)
             VALUES (?1, ?2, 1, ?3) ON CONFLICT (namespace, artifact_ref) DO NOTHING";
        /// What artifact `?2` of namespace `?1` stores: its bytes, or the
        /// graph node its row roots.
        select_stored = "SELECT artifact_bytes, graph_ref FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2";
        /// Each reference of `?2` in namespace `?1` that referrer `?3`/`?4`
        /// holds no edge to.
        select_unheld = "SELECT wanted.artifact_ref FROM unnest(?2::TEXT[]) AS wanted(artifact_ref)
             WHERE NOT EXISTS (SELECT 1 FROM artifact_referrer_edges AS edge
                 WHERE edge.namespace = ?1 AND edge.artifact_ref = wanted.artifact_ref
                   AND edge.referrer_kind = ?3 AND edge.referrer_id = ?4)";
        exists = "SELECT EXISTS (SELECT 1 FROM lash_vm_artifacts
             WHERE namespace = ?1 AND artifact_ref = ?2)";
        /// One page of namespace `?1` after `?2`: each artifact's bytes, or
        /// the encoded graph node its row roots.
        list_namespace_page = "SELECT artifact.artifact_ref, artifact.artifact_bytes, node.content
             FROM lash_vm_artifacts AS artifact
             LEFT JOIN blobs AS node ON node.hash = artifact.graph_ref
             WHERE artifact.namespace = ?1 AND (?2::text IS NULL OR artifact.artifact_ref > ?2::text)
             ORDER BY artifact.artifact_ref LIMIT ?3";
    }
}

pub(crate) struct ArtifactSql {
    pub(crate) edges: ReferrerEdgeStatements,
    pub(crate) fences: ReferrerFenceStatements,
    pub(crate) postgres: ReferrerPostgresStatements,
    pub(crate) lash_vm_artifacts: LashVmArtifactStatements,
}

static ARTIFACT_SQL: LazyLock<ArtifactSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    ArtifactSql {
        edges: ReferrerEdgeStatements::render(dialect),
        fences: ReferrerFenceStatements::render(dialect),
        postgres: ReferrerPostgresStatements::render(dialect),
        lash_vm_artifacts: LashVmArtifactStatements::render(dialect),
    }
});

pub(crate) fn artifact_sql() -> &'static ArtifactSql {
    &ARTIFACT_SQL
}

pub(crate) const MODULE_ARTIFACT_NAMESPACE: &str = "kernel_document";
pub(crate) const PROCESS_ENV_NAMESPACE: &str = "process_execution_env";
pub(crate) const PROCESS_DEFINITION_NAMESPACE: &str = "process_definition";
pub(crate) const TOOL_MATERIAL_NAMESPACE: &str = "tool_material";
pub(crate) const TURN_PRELUDE_NAMESPACE: &str = "turn_prelude";

#[path = "artifact_store/local_end.rs"]
mod local_end;
pub(crate) use local_end::{
    EndArguments as LocalEndArguments, failure as end_local_failure, validate as validate_local_end,
};

#[cfg(test)]
pub(crate) use local_end::function_ddl as local_end_function_ddl;

#[path = "artifact_store/tool_material.rs"]
mod tool_material;

/// The namespace of a store-set artifact store; an engine's own store has
/// none here.
pub(crate) fn store_namespace(
    store: &lash_core_store::artifact_referrer::ArtifactStoreId,
) -> Option<&'static str> {
    use lash_core_store::artifact_referrer::ArtifactStoreId;
    match store {
        ArtifactStoreId::KernelDocument => Some(MODULE_ARTIFACT_NAMESPACE),
        ArtifactStoreId::ProcessEnv => Some(PROCESS_ENV_NAMESPACE),
        ArtifactStoreId::ProcessDefinition => Some(PROCESS_DEFINITION_NAMESPACE),
        ArtifactStoreId::ToolMaterial => Some(TOOL_MATERIAL_NAMESPACE),
        ArtifactStoreId::TurnPrelude => Some(TURN_PRELUDE_NAMESPACE),
        ArtifactStoreId::Engine(_) => None,
    }
}

/// A manifest's store-set share as `(namespace, reference)` pairs. Only the
/// module and environment stores share a definition's transaction.
fn definition_manifest(
    manifest: &[lash_core_store::artifact_referrer::ArtifactName],
) -> Result<Vec<(&'static str, String)>, ArtifactStoreError> {
    use lash_core_store::artifact_referrer::ArtifactStoreId;
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

fn backend(error: impl ToString) -> ArtifactStoreError {
    ArtifactStoreError::Backend(error.to_string())
}

/// The advisory lock key of `referrer`.
pub(crate) fn referrer_lock_key(referrer: &ArtifactReferrer) -> String {
    format!(
        "lash-artifact-referrer:{}:{}",
        referrer.kind().as_str(),
        referrer.canonical_id()
    )
}

pub(crate) async fn lock_referrer_tx(
    conn: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<(), sqlx::Error> {
    let key = referrer_lock_key(referrer);
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(key)
    .execute(crate::observed_sql::executor(conn))
    .await
    .map(|_| ())
}

/// Validate and retain a start's environment in its admission transaction.
/// The artifact lock orders this acquisition against source cleanup.
pub(crate) async fn acquire_process_env_tx(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    env: &lash_core_store::process_identity::ProcessEnvRef,
    process: &lash_protocol_types::ProcessId,
) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
    let referrer = ArtifactReferrer::ProcessRecord(process.clone());
    lock_referrer_tx(tx, &referrer).await.map_err(backend)?;
    if is_fenced_tx(tx, &referrer).await? {
        return Err(ArtifactStoreError::ReferrerEnded { referrer }.into());
    }
    lock_artifact_tx(tx, PROCESS_ENV_NAMESPACE, env.as_str()).await?;
    let exists: bool =
        crate::observed_sql::query_scalar(artifact_sql().lash_vm_artifacts.exists.sql())
            .bind(PROCESS_ENV_NAMESPACE)
            .bind(env.as_str())
            .fetch_one(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(backend)?;
    if !exists {
        return Err(ArtifactStoreError::ArtifactMissing {
            artifact_ref: env.as_str().to_owned(),
        }
        .into());
    }
    crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
        .bind(PROCESS_ENV_NAMESPACE)
        .bind(env.as_str())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .execute(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(backend)?;
    Ok(())
}

async fn lock_artifact_tx(
    tx: &mut sqlx::PgConnection,
    namespace: &str,
    artifact_ref: &str,
) -> Result<(), ArtifactStoreError> {
    let key = format!("lash-artifact:{namespace}:{artifact_ref}");
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(key)
    .execute(crate::observed_sql::executor(&mut *tx))
    .await
    .map_err(backend)?;
    Ok(())
}

pub(crate) async fn is_fenced_tx(
    tx: &mut sqlx::PgConnection,
    referrer: &ArtifactReferrer,
) -> Result<bool, ArtifactStoreError> {
    crate::observed_sql::query_scalar(artifact_sql().fences.select_is_fenced.sql())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)
}

fn decode_edge_referrer(row: &PgRow) -> Result<ArtifactReferrer, ArtifactStoreError> {
    let kind: String = row.try_get("referrer_kind").map_err(backend)?;
    let id: String = row.try_get("referrer_id").map_err(backend)?;
    ArtifactReferrer::decode(&kind, &id).map_err(|error| match error {
        lash_core_store::artifact_referrer::ArtifactReferrerError::UnknownKind(label) => {
            StoreError::Incompatible {
                refusal: lash_core_store::compat::CompatRefusal::UnknownVocabulary {
                    surface: "artifact referrer edge kind".to_owned(),
                    label,
                },
            }
            .into()
        }
        other => StoreError::StoredDataCorrupt {
            record_kind: "artifact_referrer_edge",
            message: other.to_string(),
        }
        .into(),
    })
}

impl PostgresLashVmArtifactStore {
    async fn write_namespaced(
        &self,
        namespace: &str,
        artifact_ref: &str,
        bytes: Option<&[u8]>,
        claim: &ReferrerClaim,
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        // A published process execution environment carries plugin config
        // namespaces: they are admitted against the fleet record's writer
        // ranges before the artifact is written (FIG-4746).
        let plugin_publication = match bytes {
            Some(bytes) if namespace == PROCESS_ENV_NAMESPACE => {
                lash_core_store::store::plugin_writers::PluginPublication::of_process_env(bytes)
                    .map_err(|error| ArtifactStoreError::Encode(error.to_string()))?
            }
            _ => Default::default(),
        };
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        tx.admit_plugin_writers(&plugin_publication)
            .await
            .map_err(ArtifactStoreError::from)?;
        lock_referrer_tx(&mut tx, &claim.referrer())
            .await
            .map_err(backend)?;
        if is_fenced_tx(&mut tx, &claim.referrer()).await? {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer(),
            });
        }
        lock_artifact_tx(&mut tx, namespace, artifact_ref).await?;
        if let Some(cleanup) = claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, self.clock.timestamp_ms())
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        if let Some(bytes) = bytes {
            crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .bind(bytes)
                .execute(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
            let stored: Vec<u8> = crate::observed_sql::query_scalar(
                artifact_sql().lash_vm_artifacts.select_bytes.sql(),
            )
            .bind(namespace)
            .bind(artifact_ref)
            .fetch_one(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
            if stored != bytes {
                return Err(ArtifactStoreError::Immutable {
                    artifact_ref: artifact_ref.to_owned(),
                });
            }
        } else {
            let exists: bool =
                crate::observed_sql::query_scalar(artifact_sql().lash_vm_artifacts.exists.sql())
                    .bind(namespace)
                    .bind(artifact_ref)
                    .fetch_one(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(backend)?;
            if !exists {
                return Err(ArtifactStoreError::ArtifactMissing {
                    artifact_ref: artifact_ref.to_owned(),
                });
            }
        }
        crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    /// Hold every module artifact under the claim in one transaction: the
    /// referrer's lock, fence and guard once, every artifact's lock in key
    /// order, each artifact stored as its hold says, then every edge. An
    /// acquisition the referrer already holds writes nothing: the referrer's
    /// lock keeps its edge, and the edge keeps the stored bytes.
    async fn hold_module_artifact_batch(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        lock_referrer_tx(&mut tx, &claim.referrer())
            .await
            .map_err(backend)?;
        if is_fenced_tx(&mut tx, &claim.referrer()).await? {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer(),
            });
        }
        let acquired: Vec<&str> = artifacts
            .iter()
            .filter(|artifact| {
                matches!(
                    artifact,
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Acquire { .. }
                )
            })
            .map(lash_execution_contracts::module_artifacts::ModuleArtifactHold::module_ref)
            .collect();
        let unheld: BTreeSet<String> = if acquired.is_empty() {
            BTreeSet::new()
        } else {
            crate::observed_sql::query_scalar::<_, String>(
                artifact_sql().lash_vm_artifacts.select_unheld.sql(),
            )
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(&acquired)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .fetch_all(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?
            .into_iter()
            .collect()
        };
        let artifacts: Vec<&lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>> =
            artifacts
                .iter()
                .filter(|artifact| match artifact {
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Publish {
                        ..
                    } => true,
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Retain {
                        ..
                    } => true,
                    lash_execution_contracts::module_artifacts::ModuleArtifactHold::Acquire {
                        module_ref,
                        ..
                    } => unheld.contains(*module_ref),
                })
                .collect();
        let mut locked: Vec<&str> = artifacts
            .iter()
            .map(|artifact| artifact.module_ref())
            .collect();
        locked.sort_unstable();
        locked.dedup();
        for artifact_ref in &locked {
            lock_artifact_tx(&mut tx, MODULE_ARTIFACT_NAMESPACE, artifact_ref).await?;
        }
        if let Some(cleanup) = claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, self.clock.timestamp_ms())
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        for artifact in artifacts {
            let artifact_ref = artifact.module_ref();
            let verified = matches!(
                artifact,
                lash_execution_contracts::module_artifacts::ModuleArtifactHold::Publish { .. }
            );
            if let Some(body) = artifact.body() {
                match body.node_kind() {
                    None => {
                        Self::hold_module_bytes_tx(&mut tx, artifact_ref, body.bytes(), verified)
                            .await?;
                    }
                    Some(kind) => {
                        Self::hold_module_node_tx(
                            &mut tx,
                            artifact_ref,
                            kind,
                            body.bytes(),
                            verified,
                        )
                        .await?;
                    }
                }
            } else {
                let exists: bool = crate::observed_sql::query_scalar(
                    artifact_sql().lash_vm_artifacts.exists.sql(),
                )
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(artifact_ref)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
                if !exists {
                    return Err(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: artifact_ref.to_owned(),
                    });
                }
            }
            crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(artifact_ref)
                .bind(claim.referrer().kind().as_str())
                .bind(claim.referrer().canonical_id())
                .execute(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)
    }

    /// Store module bytes under `artifact_ref` if absent, and verify they
    /// equal the stored bytes when `verified`.
    async fn hold_module_bytes_tx(
        tx: &mut sqlx::PgConnection,
        artifact_ref: &str,
        bytes: &[u8],
        verified: bool,
    ) -> Result<(), ArtifactStoreError> {
        crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(artifact_ref)
            .bind(bytes)
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
        if verified {
            let stored: Option<Vec<u8>> = crate::observed_sql::query_scalar(
                artifact_sql().lash_vm_artifacts.select_bytes.sql(),
            )
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(artifact_ref)
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
            if stored.as_deref() != Some(bytes) {
                return Err(ArtifactStoreError::Immutable {
                    artifact_ref: artifact_ref.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Store a node body under `artifact_ref` if absent, once, as its graph
    /// node, which the row roots; verify a stored row roots that node when
    /// `verified`. The caller holds the artifact's lock.
    async fn hold_module_node_tx(
        tx: &mut sqlx::PgConnection,
        artifact_ref: &str,
        kind: lash_core_store::checkpoint_graph::NodeKind,
        logical: &[u8],
        verified: bool,
    ) -> Result<(), ArtifactStoreError> {
        let stored: Option<(Option<Vec<u8>>, Option<crate::keys::Key>)> =
            crate::observed_sql::query_as(artifact_sql().lash_vm_artifacts.select_stored.sql())
                .bind(MODULE_ARTIFACT_NAMESPACE)
                .bind(artifact_ref)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        match stored {
            Some(_) if !verified => Ok(()),
            Some((_, root)) => {
                let staged = stage_module_node(kind, logical)?;
                if root.as_deref() == Some(staged.root.to_hex().as_str()) {
                    Ok(())
                } else {
                    Err(ArtifactStoreError::Immutable {
                        artifact_ref: artifact_ref.to_owned(),
                    })
                }
            }
            None => {
                let staged = stage_module_node(kind, logical)?;
                crate::durable::install_staged(&mut *tx, &staged.nodes)
                    .await
                    .map_err(module_node_refused)?;
                crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_node.sql())
                    .bind(MODULE_ARTIFACT_NAMESPACE)
                    .bind(artifact_ref)
                    .bind(crate::keys::key(staged.root.to_hex()))
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
                Ok(())
            }
        }
    }

    /// Hold one definition closure under the claim in one transaction: the
    /// referrer's lock and fence, then every artifact's lock in key order,
    /// every manifest artifact stored, the descriptor published (verified
    /// byte for byte against a stored one) or stored, the claim's guard, then
    /// every edge (ADR 0113 §3.6).
    async fn hold_definition_closure(
        &self,
        claim: &ReferrerClaim,
        id: &str,
        descriptor: Option<&[u8]>,
        manifest: &[(&'static str, String)],
    ) -> Result<(), ArtifactStoreError> {
        if !claim.referrer().kind().holds_artifacts() {
            return Err(ArtifactStoreError::ReferrerKindRefused {
                kind: claim.referrer().kind(),
            });
        }
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        lock_referrer_tx(&mut tx, &claim.referrer())
            .await
            .map_err(backend)?;
        if is_fenced_tx(&mut tx, &claim.referrer()).await? {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer(),
            });
        }
        let mut locked: Vec<(&str, &str)> = manifest
            .iter()
            .map(|(namespace, artifact_ref)| (*namespace, artifact_ref.as_str()))
            .chain(std::iter::once((PROCESS_DEFINITION_NAMESPACE, id)))
            .collect();
        locked.sort_by_key(|(namespace, artifact_ref)| {
            format!("lash-artifact:{namespace}:{artifact_ref}")
        });
        locked.dedup();
        let lock_keys: Vec<_> = locked
            .iter()
            .map(|(namespace, artifact_ref)| format!("lash-artifact:{namespace}:{artifact_ref}"))
            .collect();
        crate::observed_sql::query(
            crate::connection_sql::connection_sql()
                .lock_xact_by_ordered_texts
                .sql(),
        )
        .bind(&lock_keys)
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(backend)?;
        let (namespaces, references): (Vec<_>, Vec<_>) = manifest
            .iter()
            .map(|(namespace, reference)| (*namespace, reference.as_str()))
            .unzip();
        let missing: Option<String> = crate::observed_sql::query_scalar(
            artifact_sql()
                .lash_vm_artifacts
                .select_missing_manifest
                .sql(),
        )
        .bind(&namespaces)
        .bind(&references)
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(backend)?;
        if let Some(artifact_ref) = missing {
            return Err(ArtifactStoreError::ArtifactMissing { artifact_ref });
        }
        match descriptor {
            Some(bytes) => {
                crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
                    .bind(PROCESS_DEFINITION_NAMESPACE)
                    .bind(id)
                    .bind(bytes)
                    .execute(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(backend)?;
                let stored: Vec<u8> = crate::observed_sql::query_scalar(
                    artifact_sql().lash_vm_artifacts.select_bytes.sql(),
                )
                .bind(PROCESS_DEFINITION_NAMESPACE)
                .bind(id)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
                if stored != bytes {
                    return Err(ArtifactStoreError::Immutable {
                        artifact_ref: id.to_owned(),
                    });
                }
            }
            None => {
                let exists: bool = crate::observed_sql::query_scalar(
                    artifact_sql().lash_vm_artifacts.exists.sql(),
                )
                .bind(PROCESS_DEFINITION_NAMESPACE)
                .bind(id)
                .fetch_one(crate::observed_sql::executor(&mut **tx))
                .await
                .map_err(backend)?;
                if !exists {
                    return Err(ArtifactStoreError::ArtifactMissing {
                        artifact_ref: id.to_owned(),
                    });
                }
            }
        }
        if let Some(cleanup) = claim.guard_cleanup() {
            crate::obligation_ledger::arm_cleanup_tx(&mut tx, &cleanup, self.clock.timestamp_ms())
                .await
                .map_err(ArtifactStoreError::from)?;
        }
        let (namespaces, references): (Vec<_>, Vec<_>) = locked.into_iter().unzip();
        crate::observed_sql::query(artifact_sql().lash_vm_artifacts.insert_manifest_edges.sql())
            .bind(&namespaces)
            .bind(&references)
            .bind(claim.referrer().kind().as_str())
            .bind(claim.referrer().canonical_id())
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn end_namespaced(
        &self,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        if cleanup.carries.is_empty() {
            local_end::end(
                &mut tx,
                &cleanup.referrer,
                None,
                self.clock.timestamp_ms(),
                &[namespace],
            )
            .await
            .map_err(ArtifactStoreError::from)?;
        } else {
            Self::end_namespaced_tx(&mut tx, namespace, cleanup, self.clock.timestamp_ms()).await?;
        }
        tx.commit().await.map_err(backend)
    }

    pub(crate) async fn end_namespaced_tx(
        tx: &mut sqlx::PgConnection,
        namespace: &str,
        cleanup: &ResolvedArtifactCleanup,
        now_ms: u64,
    ) -> Result<(), ArtifactStoreError> {
        for carry in &cleanup.carries {
            if !carry.to.kind().holds_artifacts() {
                return Err(ArtifactStoreError::ReferrerKindRefused {
                    kind: carry.to.kind(),
                });
            }
        }
        let mut referrers: Vec<ArtifactReferrer> = cleanup
            .carries
            .iter()
            .map(|carry| carry.to.clone())
            .collect();
        referrers.push(cleanup.referrer.clone());
        referrers.sort_by_key(|referrer| {
            format!(
                "lash-artifact-referrer:{}:{}",
                referrer.kind().as_str(),
                referrer.canonical_id()
            )
        });
        referrers.dedup();
        for referrer in &referrers {
            lock_referrer_tx(tx, referrer).await.map_err(backend)?;
        }
        let edges = crate::observed_sql::query(
            artifact_sql()
                .edges
                .select_referrer_edges_in_namespace
                .sql(),
        )
        .bind(namespace)
        .bind(cleanup.referrer.kind().as_str())
        .bind(cleanup.referrer.canonical_id())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)?;
        let source_refs: BTreeSet<String> = edges
            .iter()
            .map(|row| {
                decode_edge_referrer(row)?;
                row.try_get::<String, _>(1).map_err(backend)
            })
            .collect::<Result<_, _>>()?;
        let all_refs: BTreeSet<String> = source_refs
            .iter()
            .cloned()
            .chain(
                cleanup
                    .carries
                    .iter()
                    .map(|carry| carry.artifact.artifact_ref.clone()),
            )
            .collect();
        for artifact_ref in &all_refs {
            lock_artifact_tx(tx, namespace, artifact_ref).await?;
        }
        crate::observed_sql::query(artifact_sql().fences.insert_fence.sql())
            .bind(cleanup.referrer.kind().as_str())
            .bind(cleanup.referrer.canonical_id())
            .bind(crate::support::clamp_epoch_ms(now_ms))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(backend)?;
        for carry in &cleanup.carries {
            if is_fenced_tx(tx, &carry.to).await? {
                continue;
            }
            let artifact_ref = &carry.artifact.artifact_ref;
            let exists: bool =
                crate::observed_sql::query_scalar(artifact_sql().lash_vm_artifacts.exists.sql())
                    .bind(namespace)
                    .bind(artifact_ref)
                    .fetch_one(crate::observed_sql::executor(&mut *tx))
                    .await
                    .map_err(backend)?;
            if !exists {
                return Err(ArtifactStoreError::CarryArtifactMissing {
                    artifact_ref: artifact_ref.clone(),
                    to: carry.to.clone(),
                });
            }
            crate::observed_sql::query(artifact_sql().edges.insert_edge.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .bind(carry.to.kind().as_str())
                .bind(carry.to.canonical_id())
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        }
        crate::observed_sql::query(
            artifact_sql()
                .edges
                .delete_referrer_edges_in_namespace
                .sql(),
        )
        .bind(namespace)
        .bind(cleanup.referrer.kind().as_str())
        .bind(cleanup.referrer.canonical_id())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)?;
        for artifact_ref in &source_refs {
            use lash_core_store::store::EnumerationProgress;
            use lash_core_store::store::ReclamationEnumeration;
            let rows = crate::observed_sql::query(artifact_sql().edges.select_artifact_edges.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_all(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
            let referrers = rows
                .iter()
                .map(|row| {
                    decode_edge_referrer(row)
                        .map(|referrer| (referrer.kind(), referrer.canonical_id()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut enumeration = ReclamationEnumeration::<
                lash_core_store::artifact_referrer::ArtifactReferrerKind,
                (
                    lash_core_store::artifact_referrer::ArtifactReferrerKind,
                    String,
                ),
            >::new();
            for kind in lash_core_store::artifact_referrer::ArtifactReferrerKind::ALL {
                enumeration
                    .page(
                        kind,
                        0,
                        referrers
                            .iter()
                            .filter(|(source, _)| *source == kind)
                            .cloned(),
                        EnumerationProgress::Exhausted,
                    )
                    .map_err(ArtifactStoreError::from)?;
            }
            let witness = enumeration.finish().map_err(ArtifactStoreError::from)?;
            Self::delete_unreferenced_artifact_tx(tx, namespace, artifact_ref, &witness).await?;
        }
        Ok(())
    }

    /// Retire the database-owned share in the owner's transaction. The
    /// ledger still owes acknowledgements from independent stores.
    pub(crate) async fn end_local_referrer_tx(
        tx: &mut sqlx::PgConnection,
        referrer: &ArtifactReferrer,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        local_end::end(tx, referrer, None, now_ms, &local_end::NAMESPACES).await
    }

    pub(crate) async fn end_local_execution_tx(
        tx: &mut sqlx::PgConnection,
        journal: &lash_protocol_types::EffectJournalIdentity,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        local_end::end(
            tx,
            &ArtifactReferrer::Execution(journal.clone()),
            Some(journal),
            now_ms,
            &local_end::NAMESPACES,
        )
        .await
    }

    async fn delete_unreferenced_artifact_tx(
        tx: &mut sqlx::PgConnection,
        namespace: &str,
        artifact_ref: &str,
        referrers: &lash_core_store::store::CompleteArtifactReferrers,
    ) -> Result<(), ArtifactStoreError> {
        if !referrers.is_empty() {
            return Ok(());
        }
        let bridged: Option<Option<String>> =
            crate::keys::query_scalar(artifact_sql().postgres.delete_unreferenced.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_optional(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(backend)?;
        // A bridged artifact's row was its graph node's root: peel it.
        crate::durable::peel(tx, bridged.flatten().into_iter().collect())
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn get_namespaced(
        &self,
        namespace: &str,
        artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        let edges = crate::observed_sql::query(artifact_sql().edges.select_artifact_edges.sql())
            .bind(namespace)
            .bind(artifact_ref)
            .fetch_all(crate::observed_sql::executor(&self.pool))
            .await
            .map_err(backend)?;
        for row in &edges {
            decode_edge_referrer(row)?;
        }
        let stored: Option<(Option<Vec<u8>>, Option<crate::keys::Key>)> =
            crate::observed_sql::query_as(artifact_sql().lash_vm_artifacts.select_stored.sql())
                .bind(namespace)
                .bind(artifact_ref)
                .fetch_optional(crate::observed_sql::executor(&self.pool))
                .await
                .map_err(backend)?;
        match stored {
            None => Ok(None),
            Some((Some(bytes), _)) => Ok(Some(bytes)),
            Some((None, root)) => self.module_node_body(root.as_deref()).await,
        }
    }

    /// The logical body of the node a bridged artifact's row roots, its
    /// closure read in one statement: none when a reclaim took the row's
    /// node after the row was read.
    async fn module_node_body(
        &self,
        root: Option<&str>,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        use lash_core_store::checkpoint_graph::{GraphRefusal, NodeId};
        let corrupt = |message: String| ArtifactStoreError::StoredDataCorrupt {
            source:
                lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage {
                    record_kind: "module artifact node".into(),
                    message,
                },
        };
        let root = root
            .and_then(NodeId::from_hex)
            .ok_or_else(|| corrupt("a bridged row names no node".into()))?;
        let mut connection = self.pool.acquire().await.map_err(backend)?;
        let closure = match crate::durable::graph_closure(
            &mut connection,
            root,
            &lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .await
        {
            Ok(closure) => closure,
            Err(GraphRefusal::MissingNode(missing)) if missing == root => return Ok(None),
            Err(refusal) => return Err(corrupt(refusal.to_string())),
        };
        lash_execution_contracts::module_nodes::module_node_body(root, &closure).map(Some)
    }
}

/// A graph refusal of a module node's install: what the store holds under
/// the node's digest is not that node.
fn module_node_refused(
    refusal: lash_core_store::checkpoint_graph::GraphRefusal,
) -> ArtifactStoreError {
    ArtifactStoreError::StoredDataCorrupt {
        source: lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption::Storage {
            record_kind: "module artifact node".into(),
            message: refusal.to_string(),
        },
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::module_artifacts::ModuleArtifactStore
    for PostgresLashVmArtifactStore
{
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
        self.write_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref, Some(bytes), claim)
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
        self.write_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref, None, claim)
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
        if artifacts
            .iter()
            .any(|artifact| !crate::namespace::is_valid_opaque_key(artifact.module_ref()))
        {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.hold_module_artifact_batch(claim, artifacts).await
    }

    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let cleanup = lash_durable::packages::expand_package_cleanup(self, cleanup)
            .await
            .map_err(ArtifactStoreError::from)?;
        self.end_namespaced(MODULE_ARTIFACT_NAMESPACE, &cleanup)
            .await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(module_ref) {
            return Err(ArtifactStoreError::Encode(
                "invalid module reference".into(),
            ));
        }
        self.get_namespaced(MODULE_ARTIFACT_NAMESPACE, module_ref)
            .await
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEnvStore for PostgresLashVmArtifactStore {
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
        self.write_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str(), Some(bytes), claim)
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
        self.write_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str(), None, claim)
            .await
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(PROCESS_ENV_NAMESPACE, cleanup).await
    }

    async fn get_process_env(
        &self,
        env_ref: &lash_core_store::process_identity::ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(env_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid process execution environment reference".into(),
            ));
        }
        self.get_namespaced(PROCESS_ENV_NAMESPACE, env_ref.as_str())
            .await
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::turn_prelude::TurnPreludeStore for PostgresLashVmArtifactStore {
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
        self.write_namespaced(
            TURN_PRELUDE_NAMESPACE,
            prelude_ref.as_str(),
            Some(bytes),
            claim,
        )
        .await
    }

    async fn end_turn_prelude_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(TURN_PRELUDE_NAMESPACE, cleanup).await
    }

    async fn get_turn_prelude(
        &self,
        prelude_ref: &lash_execution_contracts::turn_prelude::TurnPreludeRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        if !crate::namespace::is_valid_opaque_key(prelude_ref.as_str()) {
            return Err(ArtifactStoreError::Encode(
                "invalid turn prelude reference".into(),
            ));
        }
        self.get_namespaced(TURN_PRELUDE_NAMESPACE, prelude_ref.as_str())
            .await
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessDefinitionStore for PostgresLashVmArtifactStore {
    async fn publish_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_protocol_types::ProcessDefinitionId,
        descriptor: &[u8],
        manifest: &[lash_core_store::artifact_referrer::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.hold_definition_closure(
            claim,
            id.as_str(),
            Some(descriptor),
            &definition_manifest(manifest)?,
        )
        .await
    }

    async fn acquire_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_protocol_types::ProcessDefinitionId,
        manifest: &[lash_core_store::artifact_referrer::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.hold_definition_closure(claim, id.as_str(), None, &definition_manifest(manifest)?)
            .await
    }

    async fn end_process_definition_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.end_namespaced(PROCESS_DEFINITION_NAMESPACE, cleanup)
            .await
    }

    async fn get_process_definition(
        &self,
        id: &lash_protocol_types::ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        self.get_namespaced(PROCESS_DEFINITION_NAMESPACE, id.as_str())
            .await
    }
}

#[async_trait::async_trait]
impl lash_durable::packages::PackageStorage for PostgresLashVmArtifactStore {
    async fn package_referrer_ended(
        &self,
        referrer: &ArtifactReferrer,
    ) -> Result<bool, lash_durable::DurableError> {
        let mut tx = self.pool.acquire().await.map_err(backend)?;
        let held = crate::observed_sql::query(
            artifact_sql()
                .edges
                .select_referrer_edges_in_namespace
                .sql(),
        )
        .bind(MODULE_ARTIFACT_NAMESPACE)
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(backend)?
        .is_some();
        Ok(!held && is_fenced_tx(&mut tx, referrer).await?)
    }
    async fn hold_package_record(
        &self,
        claim: &ReferrerClaim,
        reference: &str,
        bytes: Option<&[u8]>,
        dependencies: &[String],
    ) -> Result<(), lash_durable::DurableError> {
        use lash_execution_contracts::module_artifacts::{
            ModuleArtifactBody, ModuleArtifactHold, ModuleArtifactStore,
        };
        let mut holds: Vec<_> = dependencies
            .iter()
            .map(|reference| ModuleArtifactHold::Retain {
                module_ref: reference,
            })
            .collect();
        holds.push(match bytes {
            Some(bytes) => ModuleArtifactHold::Publish {
                module_ref: reference,
                body: ModuleArtifactBody::Bytes(bytes),
            },
            None => ModuleArtifactHold::Retain {
                module_ref: reference,
            },
        });
        self.hold_module_artifacts(claim, &holds)
            .await
            .map_err(Into::into)
    }
    async fn put_package_labels(
        &self,
        claim: &ReferrerClaim,
        reference: &str,
        digest: &[u8; 32],
        json: &str,
    ) -> Result<(), lash_durable::DurableError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(ArtifactStoreError::from)?;
        let referrer = claim.referrer();
        lock_referrer_tx(&mut tx, &referrer)
            .await
            .map_err(backend)?;
        if is_fenced_tx(&mut tx, &referrer).await? {
            return Err(ArtifactStoreError::ReferrerEnded { referrer }.into());
        }
        lock_artifact_tx(&mut tx, MODULE_ARTIFACT_NAMESPACE, reference).await?;
        let sql = lash_store_sql::durable::packages::PackageStatements::render(Dialect::postgres());
        let held = crate::observed_sql::query(sql.labels_held.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(reference)
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .fetch_optional(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?
            .is_some();
        if !held {
            return Err(ArtifactStoreError::ArtifactMissing {
                artifact_ref: reference.to_owned(),
            }
            .into());
        }
        crate::observed_sql::query(sql.insert_labels.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(reference)
            .bind(digest.as_slice())
            .bind(json)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend).map_err(Into::into)
    }
    async fn package_labels(
        &self,
        reference: &str,
    ) -> Result<Vec<String>, lash_durable::DurableError> {
        let sql = lash_store_sql::durable::packages::PackageStatements::render(Dialect::postgres());
        let mut connection = self.pool.acquire().await.map_err(backend)?;
        crate::observed_sql::query_scalar::<_, String>(sql.labels.sql())
            .bind(MODULE_ARTIFACT_NAMESPACE)
            .bind(reference)
            .fetch_all(crate::observed_sql::executor(&mut *connection))
            .await
            .map_err(backend)
            .map_err(Into::into)
    }
    async fn load_package_record(
        &self,
        reference: &str,
    ) -> Result<Option<Vec<u8>>, lash_durable::DurableError> {
        use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
        self.get_module_artifact(reference)
            .await
            .map_err(Into::into)
    }
    async fn end_package_records(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), lash_durable::DurableError> {
        self.end_namespaced(MODULE_ARTIFACT_NAMESPACE, cleanup)
            .await
            .map_err(Into::into)
    }
}
