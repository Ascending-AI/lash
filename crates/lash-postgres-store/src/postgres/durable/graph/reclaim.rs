//! Reclamation: the peel that deletes what retired roots left unreachable,
//! level by level, which the retirement steps (`super::super::pages`) and
//! the online orphan audit run. A peel locks with `SKIP LOCKED` and never
//! waits: a node another transaction holds is left to it, or to a later
//! step or the audit.

use std::collections::BTreeSet;

use sqlx::PgConnection;

use super::{Answer, SQL, db};

/// Inline bounded reclamation uses the same graph peel as retirement steps.
pub(crate) async fn reclaim(
    tx: &mut PgConnection,
    roots: BTreeSet<String>,
    now: lash_durable::DurableInstant,
) -> Result<(), sqlx::Error> {
    if !roots.is_empty() {
        crate::observed_sql::query(SQL.postgres.retirement_released_holds.sql())
            .bind(crate::keys::keys(&roots))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await?;
        let (_, _, mut next, skipped) = peel_set(tx, &roots, 4096).await?;
        next.extend(skipped);
        super::super::pages::retire_roots(tx, &next, now).await?;
    }
    Ok(())
}

pub(crate) fn retirement_mechanics() -> Vec<&'static str> {
    let pg = &SQL.postgres;
    vec![
        pg.retirement_candidates.sql(),
        pg.peel_lock.sql(),
        pg.retirement_unprotected.sql(),
        pg.retirement_edges.sql(),
        pg.retirement_nodes.sql(),
        super::super::environment::retirement_owners().sql(),
        super::super::environment::retirement_owner_lock().sql(),
        pg.retirement_released_holds.sql(),
        pg.peel_plan.sql(),
        pg.peel_delete.sql(),
    ]
}

/// Render the one-call form from the graph collector's own statements.
/// Environment ownership needs the explicit typed bridge; that case refuses
/// the fast call before deleting anything, as its installer does.
#[cfg(test)]
pub(crate) fn retirement_function_ddl() -> String {
    let pg = &SQL.postgres;
    let inline = super::super::generator::inline_statement;
    let candidates = inline(pg.retirement_candidates.sql(), &["p_roots", "p_limit"]);
    let owners = inline(
        super::super::environment::retirement_owners().sql(),
        &["v_candidates"],
    );
    let owner_lock = inline(
        super::super::environment::retirement_owner_lock().sql(),
        &["v_owner.owner_id"],
    );
    let locks = inline(pg.peel_lock.sql(), &["v_lockable"]);
    let gone = inline(pg.retirement_unprotected.sql(), &["v_locked"]);
    let edges = inline(pg.retirement_edges.sql(), &["p_gone"]);
    let nodes = inline(pg.retirement_nodes.sql(), &["p_gone"]);
    let plan = inline(pg.peel_plan.sql(), &["p_roots", "p_limit"]);
    let released_holds = inline(pg.retirement_released_holds.sql(), &["p_roots"]);
    let delete = inline(pg.peel_delete.sql(), &["v_plan.gone"]);
    let inline_owners = inline(
        super::super::environment::retirement_owners().sql(),
        &["v_plan.gone"],
    );
    let (hold_lock, hold) = super::super::pages::hold_statements();
    let hold_lock = inline(hold_lock.sql(), &["v_next", "'{}'::bytea[]"]);
    let hold = inline(hold.sql(), &["v_next", "'{}'::bytea[]", "p_now", "v_held"]);
    format!("CREATE OR REPLACE FUNCTION lash_graph_peel_plan(p_roots BYTEA[], p_limit BIGINT)
RETURNS TABLE(candidates BYTEA[], locked BYTEA[], gone BYTEA[])
LANGUAGE plpgsql VOLATILE AS $lash_graph_peel_plan$
DECLARE v_candidates BYTEA[]; v_lockable BYTEA[]; v_locked BYTEA[]; v_owner RECORD; v_id TEXT;
BEGIN
    v_candidates := ARRAY({candidates});
    v_lockable := v_candidates;
    FOR v_owner IN {owners} LOOP
        {owner_lock} INTO v_id;
        IF v_id IS NULL THEN v_lockable := array_remove(v_lockable, v_owner.manifest_ref); END IF;
    END LOOP;
    {locks} INTO v_locked;
    RETURN QUERY SELECT v_candidates, v_locked, ARRAY({gone});
END
$lash_graph_peel_plan$;
CREATE OR REPLACE FUNCTION lash_graph_peel_delete(p_gone BYTEA[])
RETURNS BYTEA[] LANGUAGE plpgsql VOLATILE AS $lash_graph_peel_delete$
DECLARE v_children BYTEA[]; v_deleted BIGINT;
BEGIN
    WITH removed AS ({edges}) SELECT COALESCE(array_agg(DISTINCT child_hash), '{{}}') INTO v_children FROM removed;
    WITH removed AS ({nodes}) SELECT count(*) INTO v_deleted FROM removed;
    RETURN v_children;
END
$lash_graph_peel_delete$;
CREATE OR REPLACE FUNCTION lash_graph_retire(p_roots BYTEA[], p_limit BIGINT, p_now BIGINT)
RETURNS VOID LANGUAGE plpgsql VOLATILE AS $lash_graph_retire$
DECLARE v_plan RECORD; v_next BYTEA[]; v_children BYTEA[]; v_held BYTEA[];
BEGIN
    IF cardinality(p_roots) = 0 THEN RETURN; END IF;
    {released_holds};
    {plan} INTO v_plan;
    IF EXISTS ({inline_owners}) THEN RAISE EXCEPTION 'graph retirement needs the explicit environment bridge'; END IF;
    {delete} INTO v_children;
    SELECT COALESCE(array_agg(DISTINCT hash), '{{}}') INTO v_next FROM (
        SELECT unnest(p_roots) AS hash UNION SELECT unnest(v_children) UNION
        SELECT unnest(v_plan.candidates) EXCEPT SELECT unnest(v_plan.locked)
    ) AS pending WHERE NOT (hash = ANY(v_plan.gone));
    {hold_lock} INTO v_held;
    {hold};
END
$lash_graph_retire$;\n")
}

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
    let row = crate::observed_sql::query(SQL.postgres.peel_plan.sql())
        .bind(crate::keys::keys(frontier))
        .bind(i64::try_from(limit.max(1)).unwrap_or(i64::MAX))
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await?;
    let candidates: BTreeSet<String> = crate::keys::get::<Vec<String>>(&row, "candidates")?
        .into_iter()
        .collect();
    let locked: BTreeSet<String> = crate::keys::get::<Vec<String>>(&row, "locked")?
        .into_iter()
        .collect();
    let gone: Vec<String> = crate::keys::get(&row, "gone")?;
    let skipped = candidates.difference(&locked).cloned().collect();
    let mut next: BTreeSet<_> = frontier.difference(&candidates).cloned().collect();
    if gone.is_empty() {
        return Ok((candidates.len(), 0, next, skipped));
    }
    super::super::environment::peeled(tx, &gone).await?;
    let children: Vec<String> = crate::keys::query_scalar(SQL.postgres.peel_delete.sql())
        .bind(crate::keys::keys(&gone))
        .fetch_one(crate::observed_sql::executor(tx))
        .await?;
    next.extend(
        children
            .into_iter()
            .filter(|child| !candidates.contains(child)),
    );
    Ok((candidates.len(), gone.len(), next, skipped))
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
