use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::Mutex;

pub(super) fn resolve_component<T>(
    blobs: &Mutex<HashMap<crate::BlobRef, T>>,
    component: &'static str,
    body: Option<&T>,
    existing_ref: Option<&crate::BlobRef>,
) -> Result<(Option<crate::BlobRef>, Option<T>), crate::store::StoreError>
where
    T: Clone + serde::Serialize,
{
    if let Some(body) = body {
        let bytes = rmp_serde::to_vec_named(body).map_err(|err| {
            crate::store::StoreError::Backend(format!(
                "failed to encode checkpoint {component}: {err}"
            ))
        })?;
        return Ok((
            Some(crate::BlobRef(crate::stable_hash::sha256_hex(&bytes))),
            Some(body.clone()),
        ));
    }
    let Some(blob_ref) = existing_ref else {
        return Ok((None, None));
    };
    let body = blobs.lock_recover().get(blob_ref).cloned().ok_or_else(|| {
        crate::store::StoreError::CheckpointComponentMissing {
            component,
            blob_ref: blob_ref.clone(),
        }
    })?;
    Ok((Some(blob_ref.clone()), Some(body)))
}
