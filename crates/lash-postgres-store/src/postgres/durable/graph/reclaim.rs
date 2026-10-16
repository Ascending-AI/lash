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
        let (gone, children) = peel_level(tx, &frontier).await?;
        deleted += gone;
        frontier = children;
    }
    Ok(deleted)
}

/// One level of [`peel`]: locks what of `frontier` no other transaction
/// holds, then deletes in a statement of its own every locked node nothing
/// names or reaches; answers how many with the children their edges
/// reached.
async fn peel_level(
    tx: &mut PgConnection,
    frontier: &BTreeSet<String>,
) -> Answer<(usize, BTreeSet<String>)> {
    let level: Vec<&str> = frontier.iter().map(String::as_str).collect();
    let level: Vec<String> = crate::observed_sql::query_scalar(SQL.postgres.peel_lock.sql())
        .bind(&level)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(db)?;
    if level.is_empty() {
        return Ok((0, BTreeSet::new()));
    }
    let (gone, children): (i64, Vec<String>) =
        crate::observed_sql::query_as(SQL.postgres.peel_level.sql())
            .bind(&level)
            .fetch_one(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?;
    Ok((
        usize::try_from(gone).unwrap_or_default(),
        children.into_iter().collect(),
    ))
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
    let orphans: BTreeSet<String> =
        crate::observed_sql::query_scalar(SQL.graph.orphans_after.sql())
            .bind(after)
            .bind(limit)
            .fetch_all(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(db)?
            .into_iter()
            .collect();
    let last = orphans.last().cloned();
    Ok((last, peel(tx, orphans).await?))
}
