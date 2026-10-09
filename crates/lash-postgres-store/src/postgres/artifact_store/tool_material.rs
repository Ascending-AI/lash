//! The PostgreSQL retained tool-material store (FIG-4889): bundles in
//! `lash_lash_vm_artifacts` under the `tool_material` namespace, leased by
//! Run-segment and source edges and fenced like every other referrer, on the
//! same referrer-then-artifact lock order as the other artifact writers.

use lash_core_execution::store::ToolMaterialStore;
use lash_core_execution::store::plugin_writers::PluginRevision;
use lash_core_execution::tool_run::{
    MaterialBundle, MaterialHolder, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef,
    MaterialRefusal, MaterialRetentionError, RetainedBundle,
};

use super::*;
use crate::support::store_sqlx_error;

async fn lock_bundle_tx(
    tx: &mut Transaction<'_, Postgres>,
    artifact_ref: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_by_text
            .sql(),
    )
    .bind(format!(
        "lash-artifact:{TOOL_MATERIAL_NAMESPACE}:{artifact_ref}"
    ))
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

async fn holder_ended_tx(
    tx: &mut Transaction<'_, Postgres>,
    referrer: &ArtifactReferrer,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(artifact_sql().fences.select_is_fenced.sql())
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

async fn insert_lease_tx(
    tx: &mut Transaction<'_, Postgres>,
    artifact_ref: &str,
    referrer: &ArtifactReferrer,
) -> Result<(), StoreError> {
    sqlx::query(artifact_sql().edges.insert_edge.sql())
        .bind(TOOL_MATERIAL_NAMESPACE)
        .bind(artifact_ref)
        .bind(referrer.kind().as_str())
        .bind(referrer.canonical_id())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(())
}

/// A release names no carry and no guard, so only the end's storage
/// failures reach it; each keeps its store class.
fn release_store_error(error: ArtifactStoreError) -> StoreError {
    match error {
        ArtifactStoreError::StoredDataCorrupt { source } => StoreError::StoredDataCorrupt {
            record_kind: "tool material bundle",
            message: source.to_string(),
        },
        ArtifactStoreError::Incompatible { refusal } => StoreError::Incompatible { refusal },
        ArtifactStoreError::StoreRefusal(refusal) => refusal.into_store_error(),
        other => StoreError::Backend(other.to_string()),
    }
}

fn missing(reference: &MaterialRef) -> MaterialRetentionError {
    MaterialRefusal::Missing {
        reference: Box::new(reference.clone()),
    }
    .into()
}

#[async_trait::async_trait]
impl ToolMaterialStore for PostgresLashVmArtifactStore {
    async fn retain_material(
        &self,
        holder: &MaterialHolder,
        bundle: &MaterialBundle,
    ) -> Result<RetainedBundle, MaterialRetentionError> {
        let referrer = holder.referrer();
        let artifact_ref = &bundle.artifact().artifact_ref;
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_referrer_tx(&mut tx, &referrer)
            .await
            .map_err(store_sqlx_error)?;
        if holder_ended_tx(&mut tx, &referrer).await? {
            return Err(MaterialRetentionError::HolderEnded {
                holder: Box::new(holder.clone()),
            });
        }
        lock_bundle_tx(&mut tx, artifact_ref).await?;
        sqlx::query(artifact_sql().lash_vm_artifacts.insert_bytes.sql())
            .bind(TOOL_MATERIAL_NAMESPACE)
            .bind(artifact_ref)
            .bind(bundle.bytes())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        let stored: Vec<u8> =
            sqlx::query_scalar(artifact_sql().lash_vm_artifacts.select_bytes.sql())
                .bind(TOOL_MATERIAL_NAMESPACE)
                .bind(artifact_ref)
                .fetch_one(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        if stored != bundle.bytes() {
            return Err(MaterialRetentionError::Store(
                StoreError::StoredDataCorrupt {
                    record_kind: "tool material bundle",
                    message: format!(
                        "bundle `{artifact_ref}` is stored with bytes of another name"
                    ),
                },
            ));
        }
        insert_lease_tx(&mut tx, artifact_ref, &referrer).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(bundle.retained_by(holder.clone()))
    }

    async fn acquire_material(
        &self,
        holder: &MaterialHolder,
        bundle: &RetainedBundle,
    ) -> Result<RetainedBundle, MaterialRetentionError> {
        let Some(first) = bundle.references.first() else {
            return Err(MaterialRetentionError::Store(
                StoreError::StoredDataCorrupt {
                    record_kind: "tool material bundle",
                    message: "a retained bundle names no material".to_owned(),
                },
            ));
        };
        if bundle.artifact.store != lash_core_execution::ArtifactStoreId::ToolMaterial {
            return Err(missing(first));
        }
        let referrer = holder.referrer();
        let artifact_ref = &bundle.artifact.artifact_ref;
        let mut tx = begin_guarded(&self.pool, &self.fence).await?;
        lock_referrer_tx(&mut tx, &referrer)
            .await
            .map_err(store_sqlx_error)?;
        if holder_ended_tx(&mut tx, &referrer).await? {
            return Err(MaterialRetentionError::HolderEnded {
                holder: Box::new(holder.clone()),
            });
        }
        lock_bundle_tx(&mut tx, artifact_ref).await?;
        let exists: bool = sqlx::query_scalar(artifact_sql().lash_vm_artifacts.exists.sql())
            .bind(TOOL_MATERIAL_NAMESPACE)
            .bind(artifact_ref)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if !exists {
            return Err(missing(first));
        }
        insert_lease_tx(&mut tx, artifact_ref, &referrer).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(bundle.held_by(holder.clone()))
    }

    async fn release_material(
        &self,
        holder: &MaterialHolder,
    ) -> Result<(), MaterialRetentionError> {
        let cleanup = ResolvedArtifactCleanup {
            referrer: holder.referrer(),
            carries: Vec::new(),
        };
        self.end_namespaced(TOOL_MATERIAL_NAMESPACE, &cleanup)
            .await
            .map_err(|error| MaterialRetentionError::Store(release_store_error(error)))
    }

    async fn read_material(
        &self,
        holder: &MaterialHolder,
        reference: &MaterialRef,
        owner: &MaterialOwner,
        available: &[PluginRevision],
    ) -> Result<MaterialPayload, MaterialRetentionError> {
        let MaterialLocation::RetainedArtifact { artifact } = &reference.location else {
            return Err(missing(reference));
        };
        if artifact.store != lash_core_execution::ArtifactStoreId::ToolMaterial {
            return Err(missing(reference));
        }
        let referrer = holder.referrer();
        let mut tx = self.pool.begin().await.map_err(store_sqlx_error)?;
        if holder_ended_tx(&mut tx, &referrer).await? {
            return Err(MaterialRefusal::Retired {
                reference: Box::new(reference.clone()),
            }
            .into());
        }
        let leased: bool = sqlx::query_scalar(artifact_sql().edges.select_edge_exists.sql())
            .bind(TOOL_MATERIAL_NAMESPACE)
            .bind(&artifact.artifact_ref)
            .bind(referrer.kind().as_str())
            .bind(referrer.canonical_id())
            .fetch_one(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        if !leased {
            return Err(missing(reference));
        }
        let bytes: Vec<u8> =
            sqlx::query_scalar(artifact_sql().lash_vm_artifacts.select_bytes.sql())
                .bind(TOOL_MATERIAL_NAMESPACE)
                .bind(&artifact.artifact_ref)
                .fetch_one(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(MaterialBundle::read(&bytes, reference, owner, available)?)
    }
}
