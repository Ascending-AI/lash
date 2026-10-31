//! Reclamation: the peel that deletes what retired roots left unreachable,
//! in bounded candidate sets, which retirement steps and the online
//! orphan audit run. A peel locks with `SKIP LOCKED` and never
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
) -> Result<usize, sqlx::Error> {
    if !roots.is_empty() {
        crate::observed_sql::query(SQL.postgres.retirement_released_holds.sql())
            .bind(crate::keys::keys(&roots))
            .execute(crate::observed_sql::executor(&mut *tx))
            .await?;
        let (_, gone, mut next, skipped) = peel_set(tx, &roots, 4096, true).await?;
        next.extend(skipped);
        super::super::pages::retire_roots(tx, &next, now).await?;
        return Ok(gone);
    }
    Ok(0)
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
    let candidates = inline(
        pg.retirement_candidates.sql(),
        &["p_roots", "p_limit", "p_closure"],
    );
    let owners = inline(
        super::super::environment::retirement_owners().sql(),
        &["v_lockable"],
    );
    let owner_lock = inline(
        super::super::environment::retirement_owner_lock().sql(),
        &["v_owner.owner_id"],
    );
    let locks = inline(pg.peel_lock.sql(), &["v_lockable"]);
    let eligible = inline(pg.retirement_unprotected.sql(), &["v_candidates"]);
    let gone = inline(pg.retirement_unprotected.sql(), &["v_locked"]);
    let edges = inline(pg.retirement_edges.sql(), &["p_gone"]);
    let nodes = inline(pg.retirement_nodes.sql(), &["p_gone"]);
    let plan = inline(pg.peel_plan.sql(), &["p_roots", "p_limit", "TRUE"]);
    let released_holds = inline(pg.retirement_released_holds.sql(), &["p_roots"]);
    let delete = inline(pg.peel_delete.sql(), &["v_plan.gone"]);
    let inline_owners = inline(
        super::super::environment::retirement_owners().sql(),
        &["v_plan.gone"],
    );
    let (hold_lock, hold) = super::super::pages::hold_statements();
    let hold_lock = inline(hold_lock.sql(), &["v_next", "'{}'::bytea[]"]);
    let hold = inline(hold.sql(), &["v_next", "'{}'::bytea[]", "p_now", "v_held"]);
    format!("CREATE OR REPLACE FUNCTION lash_graph_peel_plan(p_roots BYTEA[], p_limit BIGINT, p_closure BOOLEAN)
RETURNS TABLE(candidates BYTEA[], locked BYTEA[], gone BYTEA[], deferred BYTEA[])
LANGUAGE plpgsql VOLATILE AS $lash_graph_peel_plan$
DECLARE v_candidates BYTEA[]; v_eligible BYTEA[]; v_lockable BYTEA[]; v_locked BYTEA[]; v_owner RECORD; v_id TEXT;
BEGIN
    v_candidates := ARRAY({candidates});
    v_eligible := ARRAY({eligible});
    v_lockable := v_eligible;
    FOR v_owner IN {owners} LOOP
        {owner_lock} INTO v_id;
        IF v_id IS NULL THEN v_lockable := array_remove(v_lockable, v_owner.manifest_ref); END IF;
    END LOOP;
    {locks} INTO v_locked;
    RETURN QUERY SELECT v_candidates, v_locked, ARRAY({gone}),
        ARRAY(SELECT hash FROM lash_blobs WHERE hash = ANY(v_eligible) AND NOT (hash = ANY(v_locked)));
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
    IF EXISTS ({inline_owners}) THEN RAISE EXCEPTION 'graph retirement needs the explicit environment bridge' USING ERRCODE = 'LX004'; END IF;
    {delete} INTO v_children;
    SELECT COALESCE(array_agg(DISTINCT hash), '{{}}') INTO v_next FROM (
        (SELECT unnest(p_roots) AS hash EXCEPT SELECT unnest(v_plan.candidates)) UNION
        (SELECT unnest(v_children) AS hash EXCEPT SELECT unnest(v_plan.candidates)) UNION
        SELECT unnest(v_plan.deferred) AS hash
    ) AS pending WHERE NOT (hash = ANY(v_plan.gone));
    {hold_lock} INTO v_held;
    {hold};
END
$lash_graph_retire$;\n")
}

/// Retire one bounded set from an inline caller. Unfinished work goes
/// to ordinary retirement holds, without retaining delete locks for a loop.
pub(crate) async fn peel(tx: &mut PgConnection, frontier: BTreeSet<String>) -> Answer<usize> {
    reclaim(tx, frontier, lash_durable::DurableInstant(0))
        .await
        .map_err(db)
}

/// One bounded set, claimed in digest order without waiting. A skipped
/// parent is outside the locked set and protects every descendant it reaches.
pub(crate) async fn peel_set(
    tx: &mut PgConnection,
    frontier: &BTreeSet<String>,
    limit: usize,
    closure: bool,
) -> Result<(usize, usize, BTreeSet<String>, BTreeSet<String>), sqlx::Error> {
    let row = crate::observed_sql::query(SQL.postgres.peel_plan.sql())
        .bind(crate::keys::keys(frontier))
        .bind(i64::try_from(limit.max(1)).unwrap_or(i64::MAX))
        .bind(closure)
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await?;
    let candidates: BTreeSet<String> = crate::keys::get::<Vec<String>>(&row, "candidates")?
        .into_iter()
        .collect();
    let gone: Vec<String> = crate::keys::get(&row, "gone")?;
    let skipped = crate::keys::get::<Vec<String>>(&row, "deferred")?
        .into_iter()
        .collect();
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
    let (_, gone, next, _) = peel_set(tx, &orphans, 4096, true).await.map_err(db)?;
    // New orphan children may precede the old cursor. Continue in a fresh
    // transaction instead of retaining delete locks while walking levels.
    let last = if next.is_empty() {
        last
    } else {
        Some(String::new())
    };
    Ok((last, gone))
}
