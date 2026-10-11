use crate::*;

/// Every blob the session manifests in `checkpoint_refs` reach, the roots
/// included: the candidates a session delete may reclaim once its roots are
/// gone.
pub(crate) async fn enumerate_checkpoint_blob_candidates_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    checkpoint_refs: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeSet<String>, StoreError> {
    crate::durable::reachable_from(tx, checkpoint_refs)
        .await
        .map_err(crate::support::graph_store_error)
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
    let locked = crate::observed_sql::query_scalar::<_, String>(
        crate::blobs::blob_sql()
            .postgres
            .lock_reclaim_candidates
            .sql(),
    )
    .bind(&candidate_vec)
    .fetch_all(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(store_sqlx_error)?;
    if locked.len() != candidate_vec.len() {
        return Err(StoreError::Backend(format!(
            "{owner} has a missing checkpoint blob reference"
        )));
    }
    Ok(())
}

/// Reclaims every candidate nothing still roots or references, repeating
/// until a pass deletes nothing: a deleted parent's edges go with it, which
/// frees its children for the next pass.
pub(crate) async fn reclaim_session_checkpoint_blobs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    candidates: std::collections::BTreeSet<String>,
    report: &mut lash_core_execution::SessionBlobReclaimReport,
) -> Result<(), StoreError> {
    // Publication and reclaim already locked this complete set in ascending
    // hash order before owner rows were severed; each pass keeps that order.
    let mut remaining = candidates;
    loop {
        let mut kept = std::collections::BTreeSet::new();
        let mut deleted_now = 0_usize;
        for blob_ref in remaining {
            let deleted = crate::observed_sql::query(
                crate::blobs::blob_sql()
                    .postgres
                    .reclaim_session_candidate
                    .sql(),
            )
            .bind(&blob_ref)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(store_sqlx_error)?
            .rows_affected();
            if deleted == 0 {
                kept.insert(blob_ref);
            } else {
                deleted_now += usize::try_from(deleted).map_err(|_| {
                    StoreError::Backend("deleted blob count does not fit usize".to_string())
                })?;
            }
        }
        report.deleted_blob_count += deleted_now;
        remaining = kept;
        if deleted_now == 0 || remaining.is_empty() {
            report.retained_blob_count += remaining.len();
            return Ok(());
        }
    }
}
