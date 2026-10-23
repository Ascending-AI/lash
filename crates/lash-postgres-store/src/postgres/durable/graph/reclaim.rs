//! Reclamation: the peel that deletes what retired roots left unreachable,
//! level by level, which the retirement steps (`super::super::pages`) and
//! the online orphan audit run. A peel locks with `SKIP LOCKED` and never
//! waits: a node another transaction holds is left to it, or to a later
//! step or the audit.

use std::collections::BTreeSet;

use sqlx::PgConnection;

use super::{Answer, SQL, db};

/// Peels `frontier`, the roots the caller's transaction retired: deletes
/// each one nothing names or reaches any more, then each child the deleted
/// nodes' edges reached, level by level, stopping wherever something still
/// names a node. Its work is the newly unreachable nodes and their edges,
/// never a closure something still roots. Answers how many it deleted.
pub(crate) async fn peel(tx: &mut PgConnection, frontier: BTreeSet<String>) -> Answer<usize> {
    let mut frontier = frontier;
    let mut deleted = 0;
    while !frontier.is_empty() {
        let (_, gone, children, _) = peel_set(tx, &frontier, 4096).await.map_err(db)?;
        deleted += gone;
        frontier = children;
    }
    Ok(deleted)
}

/// One bounded set, claimed in digest order without waiting. A skipped
/// parent is outside the locked set and protects every descendant it reaches.
pub(crate) async fn peel_set(
    tx: &mut PgConnection,
    frontier: &BTreeSet<String>,
    limit: usize,
) -> Result<(usize, usize, BTreeSet<String>, BTreeSet<String>), sqlx::Error> {
    let candidates: BTreeSet<String> =
        crate::keys::query_scalar(SQL.postgres.retirement_candidates.sql())
            .bind(crate::keys::keys(frontier))
            .bind(i64::try_from(limit.max(1)).unwrap_or(i64::MAX))
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await?
            .into_iter()
            .collect();
    let owner_skipped = super::super::environment::peel_owners(tx, &candidates).await?;
    let lockable: BTreeSet<_> = candidates.difference(&owner_skipped).cloned().collect();
    let locked: BTreeSet<String> =
        crate::keys::query_scalar::<Vec<String>>(SQL.postgres.peel_lock.sql())
            .bind(crate::keys::keys(&lockable))
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await?
            .into_iter()
            .collect();
    let skipped = candidates.difference(&locked).cloned().collect();
    let mut next: BTreeSet<_> = frontier.difference(&candidates).cloned().collect();
    if locked.is_empty() {
        return Ok((candidates.len(), 0, next, skipped));
    }
    let gone: Vec<String> = crate::keys::query_scalar(SQL.postgres.retirement_unprotected.sql())
        .bind(crate::keys::keys(&locked))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    if gone.is_empty() {
        return Ok((candidates.len(), 0, next, skipped));
    }
    super::super::environment::peeled(tx, &gone).await?;
    let children: Vec<String> = crate::keys::query_scalar(SQL.postgres.retirement_edges.sql())
        .bind(crate::keys::keys(&gone))
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await?;
    next.extend(
        children
            .into_iter()
            .filter(|child| !candidates.contains(child)),
    );
    let deleted: Vec<String> = crate::keys::query_scalar(SQL.postgres.retirement_nodes.sql())
        .bind(crate::keys::keys(&gone))
        .fetch_all(crate::observed_sql::executor(tx))
        .await?;
    Ok((candidates.len(), deleted.len(), next, skipped))
}

/// How many roots the catalog names and how many blobs are stored.
pub(crate) async fn graph_counts(tx: &mut PgConnection) -> Answer<(usize, usize)> {
    let mut counts = [0_usize; 2];
    for (count, sql) in counts
        .iter_mut()
        .zip([SQL.graph.catalog_count.sql(), SQL.graph.blob_count.sql()])
    {
        let stored: i64 = crate::observed_sql::query_scalar(sql)
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
        *count = usize::try_from(stored).unwrap_or(0);
    }
    Ok((counts[0], counts[1]))
}

/// One page of the orphan audit: peels up to `limit` blobs after `after`,
/// in digest order, that no root names and no edge reaches. Answers the
/// last orphan the page read (none when it read none) and how many blobs
/// the peel deleted.
pub(crate) async fn audit_page(
    tx: &mut PgConnection,
    after: &str,
    limit: i64,
) -> Answer<(Option<String>, usize)> {
    let orphans: BTreeSet<String> = crate::keys::query_scalar(SQL.graph.orphans_after.sql())
        .bind(if after.is_empty() {
            Vec::new()
        } else {
            super::node_of(after)?.0.to_vec()
        })
        .bind(limit)
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?
        .into_iter()
        .collect();
    let last = orphans.last().cloned();
    Ok((last, peel(tx, orphans).await?))
}
