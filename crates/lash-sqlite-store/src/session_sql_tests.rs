//! Proofs about the session-core statement set itself.
//!
//! Two things are checked here that no behavioural test can see: that every
//! statement in the set renders at all (rendering is lazy, so a malformed
//! neutral statement would otherwise surface as a panic in whichever caller
//! happened to touch it first), and that each named filter shape keeps the
//! query plan it was named for.

use super::*;
use crate::session_sql::session_sql;

/// Every statement in the set renders, and the rendered text addresses the
/// durable-core tables the way this store has always addressed them.
///
/// `render` panics on a malformed neutral statement, naming it, and the set is
/// a `LazyLock`, so touching one field renders all of them.
#[test]
fn every_session_core_statement_renders_unqualified() {
    let sql = session_sql();
    assert!(
        sql.head
            .select_meta
            .sql()
            .contains("FROM session_head WHERE session_id = ?1"),
        "the head read addresses `session_head` unqualified: {}",
        sql.head.select_meta.sql()
    );
    assert!(
        sql.graph.insert.sql().contains("INSERT INTO graph_nodes"),
        "the shared node insert addresses `graph_nodes` unqualified: {}",
        sql.graph.insert.sql()
    );
    assert!(
        sql.turn_commits
            .select_failure_settlements
            .sql()
            .contains(r#"result_json LIKE '%"failure_evidence"%'"#),
        "a `{{` inside a SQL string literal survives rendering: {}",
        sql.turn_commits.select_failure_settlements.sql()
    );
    assert!(
        sql.head
            .corrupt_head_json
            .sql()
            .contains("'{not-current-json'"),
        "a brace inside a SQL string literal is the literal's own text: {}",
        sql.head.corrupt_head_json.sql()
    );
}

/// The query plan of every named filter shape, pinned.
///
/// Each of these pairs exists because a single statement carrying an optional
/// predicate — `?2 IS NULL OR generation <= ?2`, `turn_id NOT IN (…)` with an
/// empty list — cannot be planned well for either shape. Pinning the plans is
/// what makes that claim checkable: a change that turns one of these seeks
/// into a scan, or that adds an index one of them should now be using, moves
/// the text below instead of passing silently.
#[tokio::test]
async fn every_named_filter_shape_keeps_its_query_plan() {
    let store = Store::memory().await.expect("open plan-probe store");
    let sql = session_sql();
    let plans = store
        .conn
        .call(|conn| {
            Ok([
                explain(conn, sql.graph_sqlite.select_readable.sql())?,
                explain(conn, sql.graph_sqlite.select_readable_to_generation.sql())?,
                explain(conn, sql.turn_commits_sqlite.delete_retained.sql())?,
                explain(
                    conn,
                    sql.turn_commits_sqlite.delete_retained_except_live.sql(),
                )?,
            ])
        })
        .await
        .expect("explain every named filter shape");
    let [readable, readable_bounded, retained, retained_except_live] = plans;

    // Both readable-graph shapes scan `graph_nodes` today, and that is not
    // what the split buys: the disjunction with the fork-lineage `EXISTS` is
    // what keeps the scan, and no index this schema carries can serve it. What
    // the split buys is that the bounded shape's `generation <= ?2` is a plain
    // comparison a future index could serve, where `?2 IS NULL OR …` could
    // never be.
    assert_eq!(
        readable,
        "SCAN node\n\
         CORRELATED SCALAR SUBQUERY 1\n\
         SEARCH lineage USING INDEX sqlite_autoindex_fork_lineage_1 \
         (session_id=? AND ancestor_session_id=?)\n\
         USE TEMP B-TREE FOR ORDER BY"
    );
    assert_eq!(readable_bounded, readable);

    // The retention sweep's two shapes differ exactly where they are meant to:
    // the exclusion list is a `json_each` scan the plain shape does not pay
    // for, and both reach the deleted set through its primary key.
    assert!(
        retained.contains("sqlite_autoindex_deleted_sessions_1"),
        "the plain retention shape probes the deleted set by key; plan was:\n{retained}"
    );
    assert!(
        retained_except_live.contains("sqlite_autoindex_deleted_sessions_1"),
        "the exclusion retention shape probes the deleted set by key; plan was:\n\
         {retained_except_live}"
    );
    assert!(
        !retained.contains("json_each"),
        "the plain shape carries no exclusion list; plan was:\n{retained}"
    );
    assert!(
        retained_except_live.contains("json_each"),
        "the exclusion shape binds its live keys as one JSON array; plan was:\n\
         {retained_except_live}"
    );
}

/// `EXPLAIN QUERY PLAN` for `statement`, as one newline-joined string.
fn explain(conn: &Connection, statement: &str) -> rusqlite::Result<String> {
    let mut prepared = conn.prepare(&format!("EXPLAIN QUERY PLAN {statement}"))?;
    // The plan is a property of the statement, not of the values, so every
    // parameter is bound NULL purely to satisfy the binding count.
    let bound = vec![rusqlite::types::Value::Null; prepared.parameter_count()];
    let rows = prepared
        .query_map(rusqlite::params_from_iter(bound), |row| {
            row.get::<_, String>(3)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.join("\n"))
}
