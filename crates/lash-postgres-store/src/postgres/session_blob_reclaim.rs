use crate::*;

pub(crate) async fn reclaim_session_checkpoint_blobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    candidates: std::collections::BTreeSet<String>,
    checkpoint_ref: Option<&String>,
    report: &mut lash_core::SessionBlobReclaimReport,
) -> Result<(), StoreError> {
    let mut ordered_candidates = Vec::with_capacity(candidates.len());
    if let Some(checkpoint_ref) = checkpoint_ref {
        ordered_candidates.push(checkpoint_ref.clone());
    }
    ordered_candidates.extend(
        candidates
            .into_iter()
            .filter(|candidate| Some(candidate) != checkpoint_ref),
    );
    for blob_ref in ordered_candidates {
        let deleted = sqlx::query(
            "DELETE FROM lash_blobs AS candidate
             WHERE candidate.hash = $1
               AND NOT EXISTS (
                   SELECT 1 FROM lash_sessions AS head
                   WHERE head.checkpoint_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_node_anchors AS anchor
                   WHERE anchor.checkpoint_ref = candidate.hash
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_checkpoint_blob_refs AS edge
                   WHERE edge.blob_ref = candidate.hash
                     AND (
                         EXISTS (
                             SELECT 1 FROM lash_sessions AS head
                             WHERE head.checkpoint_ref = edge.checkpoint_ref
                         )
                         OR EXISTS (
                             SELECT 1 FROM lash_node_anchors AS anchor
                             WHERE anchor.checkpoint_ref = edge.checkpoint_ref
                         )
                     )
               )",
        )
        .bind(&blob_ref)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .rows_affected();
        if deleted == 0 {
            report.retained_blob_count += 1;
        } else {
            report.deleted_blob_count += usize::try_from(deleted).map_err(|_| {
                StoreError::Backend("deleted blob count does not fit usize".to_string())
            })?;
        }
    }
    Ok(())
}
