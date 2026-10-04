//! The SQLite retained tool-material store (FIG-4889): bundles in
//! `artifact_refs`/`blobs` under the `tool_material` namespace, leased by
//! Run-segment and source edges in `artifact_referrer_edges` and fenced in
//! `referrer_fences` like every other referrer.

use lash_core_execution::store::ToolMaterialStore;
use lash_core_execution::store::plugin_writers::PluginRevision;
use lash_core_execution::tool_run::{
    MaterialBundle, MaterialHolder, MaterialLocation, MaterialOwner, MaterialPayload, MaterialRef,
    MaterialRefusal, MaterialRetentionError, RetainedBundle,
};

use super::*;

/// What a lease operation found instead of applying.
enum LeaseRefusal {
    HolderEnded,
    BundleMissing,
}

impl LeaseRefusal {
    fn into_error(
        self,
        holder: &MaterialHolder,
        reference: &MaterialRef,
    ) -> MaterialRetentionError {
        match self {
            Self::HolderEnded => MaterialRetentionError::HolderEnded {
                holder: Box::new(holder.clone()),
            },
            Self::BundleMissing => MaterialRefusal::Missing {
                reference: Box::new(reference.clone()),
            }
            .into(),
        }
    }

    /// A read under an ended holder is of retired material; a read without
    /// a lease on the bundle finds nothing to serve.
    fn read_refusal(self, reference: &MaterialRef) -> MaterialRetentionError {
        let reference = Box::new(reference.clone());
        match self {
            Self::HolderEnded => MaterialRefusal::Retired { reference },
            Self::BundleMissing => MaterialRefusal::Missing { reference },
        }
        .into()
    }
}

fn bundle_exists_tx(tx: &rusqlite::Connection, artifact_ref: &str) -> rusqlite::Result<bool> {
    Ok(tx
        .query_row(
            artifact_sql().refs.select_blob_ref.sql(),
            params![TOOL_MATERIAL_NAMESPACE, artifact_ref],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn insert_lease_tx(
    tx: &rusqlite::Connection,
    artifact_ref: &str,
    referrer: &ArtifactReferrer,
) -> rusqlite::Result<()> {
    crate::conn::cached_execute(
        tx,
        artifact_sql().edges.insert_edge.sql(),
        params![
            TOOL_MATERIAL_NAMESPACE,
            artifact_ref,
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )?;
    Ok(())
}

#[async_trait::async_trait]
impl ToolMaterialStore for SqliteStore {
    async fn retain_material(
        &self,
        holder: &MaterialHolder,
        bundle: &MaterialBundle,
    ) -> Result<RetainedBundle, MaterialRetentionError> {
        let referrer = holder.referrer();
        let artifact_ref = bundle.artifact().artifact_ref.clone();
        let bytes = bundle.bytes().to_vec();
        let blob_profile = self.options.blob_profile;
        let refused = self
            .conn
            .write(move |tx| {
                if artifact_fenced_tx(tx, &referrer)? {
                    return Ok(Some(LeaseRefusal::HolderEnded));
                }
                let blob_ref = Self::insert_artifact_blob_conn(
                    tx,
                    BlobArtifactDescriptor::tool_material(),
                    &bytes,
                    blob_profile,
                    tx.fleet(),
                )?;
                crate::conn::cached_execute(
                    tx,
                    artifact_sql().refs.insert_pointer.sql(),
                    params![TOOL_MATERIAL_NAMESPACE, artifact_ref, blob_ref.as_str()],
                )?;
                let stored_blob_ref: String = tx.query_row(
                    artifact_sql().refs.select_blob_ref.sql(),
                    params![TOOL_MATERIAL_NAMESPACE, artifact_ref],
                    |row| row.get(0),
                )?;
                if stored_blob_ref != blob_ref.as_str() {
                    return Err(sqlite_conversion_error(stored_data_corrupt(
                        "tool material bundle",
                        format!("bundle `{artifact_ref}` is stored with bytes of another name"),
                    )));
                }
                insert_lease_tx(tx, &artifact_ref, &referrer)?;
                Ok(None)
            })
            .await
            .map_err(sqlite_error)?;
        match refused {
            None => Ok(bundle.retained_by(holder.clone())),
            Some(refusal) => Err(refusal.into_error(holder, &bundle.references()[0])),
        }
    }

    async fn acquire_material(
        &self,
        holder: &MaterialHolder,
        bundle: &RetainedBundle,
    ) -> Result<RetainedBundle, MaterialRetentionError> {
        let Some(first) = bundle.references.first() else {
            return Err(MaterialRetentionError::Store(stored_data_corrupt(
                "tool material bundle",
                "a retained bundle names no material",
            )));
        };
        if bundle.artifact.store != ArtifactStoreId::ToolMaterial {
            return Err(MaterialRefusal::Missing {
                reference: Box::new(first.clone()),
            }
            .into());
        }
        let referrer = holder.referrer();
        let artifact_ref = bundle.artifact.artifact_ref.clone();
        let refused = self
            .conn
            .write(move |tx| {
                if artifact_fenced_tx(tx, &referrer)? {
                    return Ok(Some(LeaseRefusal::HolderEnded));
                }
                if !bundle_exists_tx(tx, &artifact_ref)? {
                    return Ok(Some(LeaseRefusal::BundleMissing));
                }
                insert_lease_tx(tx, &artifact_ref, &referrer)?;
                Ok(None)
            })
            .await
            .map_err(sqlite_error)?;
        match refused {
            None => Ok(bundle.held_by(holder.clone())),
            Some(refusal) => Err(refusal.into_error(holder, first)),
        }
    }

    async fn release_material(
        &self,
        holder: &MaterialHolder,
    ) -> Result<(), MaterialRetentionError> {
        let referrer = holder.referrer();
        let now_ms = self.clock.timestamp_ms();
        self.conn
            .write(move |tx| release_material_tx(tx, &referrer, now_ms))
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    async fn read_material(
        &self,
        holder: &MaterialHolder,
        reference: &MaterialRef,
        owner: &MaterialOwner,
        available: &[PluginRevision],
    ) -> Result<MaterialPayload, MaterialRetentionError> {
        let MaterialLocation::RetainedArtifact { artifact } = &reference.location else {
            return Err(MaterialRefusal::Missing {
                reference: Box::new(reference.clone()),
            }
            .into());
        };
        if artifact.store != ArtifactStoreId::ToolMaterial {
            return Err(MaterialRefusal::Missing {
                reference: Box::new(reference.clone()),
            }
            .into());
        }
        let referrer = holder.referrer();
        let artifact_ref = artifact.artifact_ref.clone();
        let read = self
            .conn
            .call(move |conn| {
                if artifact_fenced_tx(conn, &referrer)? {
                    return Ok(Err(LeaseRefusal::HolderEnded));
                }
                let leased: bool = conn.query_row(
                    artifact_sql().edges.select_edge_exists.sql(),
                    params![
                        TOOL_MATERIAL_NAMESPACE,
                        artifact_ref,
                        referrer.kind().as_str(),
                        referrer.canonical_id()
                    ],
                    |row| row.get(0),
                )?;
                if !leased {
                    return Ok(Err(LeaseRefusal::BundleMissing));
                }
                let blob_ref: String = conn.query_row(
                    artifact_sql().refs.select_blob_ref.sql(),
                    params![TOOL_MATERIAL_NAMESPACE, artifact_ref],
                    |row| row.get(0),
                )?;
                let bytes = Self::get_blob_conn(conn, &BlobRef(blob_ref))
                    .map_err(sqlite_conversion_error)?
                    .ok_or_else(|| {
                        sqlite_conversion_error(stored_data_corrupt(
                            "tool material bundle",
                            format!("bundle `{artifact_ref}` points at a missing blob"),
                        ))
                    })?;
                Ok(Ok(bytes))
            })
            .await
            .map_err(sqlite_error)?;
        let bytes = read.map_err(|refusal| refusal.read_refusal(reference))?;
        Ok(MaterialBundle::read(&bytes, reference, owner, available)?)
    }
}

fn release_material_tx(
    tx: &rusqlite::Connection,
    referrer: &ArtifactReferrer,
    now_ms: u64,
) -> rusqlite::Result<()> {
    let held: Vec<String> = {
        let mut stmt = tx.prepare_cached(
            artifact_sql()
                .edges
                .select_referrer_edges_in_namespace
                .sql(),
        )?;
        stmt.query_map(
            params![
                TOOL_MATERIAL_NAMESPACE,
                referrer.kind().as_str(),
                referrer.canonical_id()
            ],
            |row| row.get(1),
        )?
        .collect::<rusqlite::Result<_>>()?
    };
    fence_artifact_referrer_tx(tx, referrer, now_ms)?;
    crate::conn::cached_execute(
        tx,
        artifact_sql()
            .edges
            .delete_referrer_edges_in_namespace
            .sql(),
        params![
            TOOL_MATERIAL_NAMESPACE,
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )?;
    for artifact_ref in held {
        SqliteStore::reclaim_unreferenced_artifact_tx(tx, TOOL_MATERIAL_NAMESPACE, &artifact_ref)?;
    }
    Ok(())
}

/// Carry and end material holders under the session head's write transaction.
pub(crate) fn commit_run_material_tx(
    tx: &rusqlite::Connection,
    cleanups: &[ResolvedArtifactCleanup],
    now_ms: u64,
) -> Result<(), StoreError> {
    for cleanup in cleanups {
        for carry in &cleanup.carries {
            if artifact_fenced_tx(tx, &carry.to).map_err(sqlite_error)? {
                return Err(StoreError::ArtifactReferrerEnded {
                    referrer: carry.to.clone(),
                });
            }
            let leased: bool = tx
                .query_row(
                    artifact_sql().edges.select_edge_exists.sql(),
                    params![
                        TOOL_MATERIAL_NAMESPACE,
                        carry.artifact.artifact_ref,
                        cleanup.referrer.kind().as_str(),
                        cleanup.referrer.canonical_id()
                    ],
                    |row| row.get(0),
                )
                .map_err(sqlite_error)?;
            if !leased
                || !bundle_exists_tx(tx, &carry.artifact.artifact_ref).map_err(sqlite_error)?
            {
                return Err(StoreError::ArtifactCarryMissing {
                    artifact_ref: carry.artifact.artifact_ref.clone(),
                    to: carry.to.clone(),
                });
            }
            insert_lease_tx(tx, &carry.artifact.artifact_ref, &carry.to).map_err(sqlite_error)?;
        }
        release_material_tx(tx, &cleanup.referrer, now_ms).map_err(sqlite_error)?;
    }
    Ok(())
}
