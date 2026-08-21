use crate::*;

pub(crate) async fn enumerate_checkpoint_blob_candidates_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint_refs: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeSet<String>, StoreError> {
    let mut candidates = checkpoint_refs.clone();
    if !checkpoint_refs.is_empty() {
        let checkpoint_ref_vec = checkpoint_refs.iter().cloned().collect::<Vec<_>>();
        candidates.extend(
            sqlx::query_scalar::<_, String>(
                "SELECT DISTINCT blob_ref
                 FROM lash_checkpoint_blob_refs
                 WHERE checkpoint_ref = ANY($1::TEXT[])
                 ORDER BY blob_ref",
            )
            .bind(checkpoint_ref_vec)
            .fetch_all(&mut **tx)
            .await
            .map_err(store_sqlx_error)?,
        );
    }
    Ok(candidates)
}

pub(crate) async fn lock_session_blob_candidates_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    candidates: &std::collections::BTreeSet<String>,
    owner: &str,
) -> Result<(), StoreError> {
    if candidates.is_empty() {
        return Ok(());
    }
    let candidate_vec = candidates.iter().cloned().collect::<Vec<_>>();
    // Same global lock order as checkpoint publication in support.rs: every
    // blob row in the complete union is locked by ascending content hash before
    // any owner edge is severed.
    let locked = sqlx::query_scalar::<_, String>(
        "SELECT hash FROM lash_blobs
         WHERE hash = ANY($1::TEXT[])
         ORDER BY hash
         FOR UPDATE",
    )
    .bind(&candidate_vec)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if locked.len() != candidate_vec.len() {
        return Err(StoreError::Backend(format!(
            "{owner} has a missing checkpoint blob reference"
        )));
    }
    Ok(())
}

pub(crate) async fn reclaim_session_checkpoint_blobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    candidates: std::collections::BTreeSet<String>,
    checkpoint_refs: &std::collections::BTreeSet<String>,
    report: &mut lash_core::SessionBlobReclaimReport,
) -> Result<(), StoreError> {
    if !checkpoint_refs.is_empty() {
        let checkpoint_ref_vec = checkpoint_refs.iter().cloned().collect::<Vec<_>>();
        // Sever every outgoing edge of every root that stopped being a live
        // root in this owner transaction before any blob delete. A root may
        // remain as another root's opaque component, so deleting roots one at
        // a time cannot provide this ordering. Shared live roots keep both
        // their row and projection edges.
        sqlx::query(
            "DELETE FROM lash_checkpoint_blob_refs AS edge
             WHERE edge.checkpoint_ref = ANY($1::TEXT[])
               AND NOT EXISTS (
                   SELECT 1 FROM lash_sessions AS head
                   WHERE head.checkpoint_ref = edge.checkpoint_ref
               )
               AND NOT EXISTS (
                   SELECT 1 FROM lash_node_anchors AS anchor
                   WHERE anchor.checkpoint_ref = edge.checkpoint_ref
               )",
        )
        .bind(checkpoint_ref_vec)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    // Publication and reclaim already locked this complete set in ascending
    // hash order before owner rows were severed. Keep candidate row work in the
    // same deterministic order after every dead root's edges are gone.
    for blob_ref in candidates {
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
