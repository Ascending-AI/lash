use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

pub(super) fn resolve_components(
    blobs: &Mutex<HashMap<crate::BlobRef, Vec<u8>>>,
    checkpoint: &crate::HydratedSessionCheckpoint,
) -> Result<crate::HydratedSessionCheckpoint, crate::store::StoreError> {
    let manifest = checkpoint.manifest()?;
    let stored = blobs.lock_recover();
    let mut components = BTreeMap::new();
    for (key, descriptor) in &manifest.components {
        let submitted = checkpoint.components.get(key).ok_or_else(|| {
            crate::store::StoreError::StoredDataCorrupt {
                record_kind: "HydratedSessionCheckpoint",
                message: format!("manifest projection lost component `{key}`"),
            }
        })?;
        let body = match submitted.body() {
            Some(body) => body.to_vec(),
            None => stored.get(&descriptor.blob_ref).cloned().ok_or_else(|| {
                crate::store::StoreError::CheckpointComponentMissing {
                    key: key.clone(),
                    blob_ref: descriptor.blob_ref.clone(),
                }
            })?,
        };
        components.insert(
            key.clone(),
            crate::HydratedCheckpointComponent::hydrated(descriptor.clone(), body),
        );
    }
    Ok(crate::HydratedSessionCheckpoint {
        turn_state: checkpoint.turn_state.clone(),
        components,
        plugin_snapshot_revision: checkpoint.plugin_snapshot_revision,
    })
}
