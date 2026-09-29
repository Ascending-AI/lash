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
            .contains("WHERE session_id = ?1 AND failure_evidence"),
        "failure paging uses the indexed flag: {}",
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
/// Window and page reads must seek within a session's generations. Retention
/// keeps its two filter shapes because an empty exclusion list needs no scan.
#[tokio::test]
async fn every_named_filter_shape_keeps_its_query_plan() {
    let store = crate::test_support::memory_store()
        .await
        .expect("open plan-probe store");
    let sql = session_sql();
    let plans = store
        .conn
        .call(|conn| {
            Ok([
                explain(conn, sql.graph_sqlite.window_rows.sql())?,
                explain(conn, sql.graph_sqlite.page_headers.sql())?,
                explain(conn, sql.turn_commits_sqlite.delete_retained.sql())?,
                explain(
                    conn,
                    sql.turn_commits_sqlite.delete_retained_except_live.sql(),
                )?,
            ])
        })
        .await
        .expect("explain every named filter shape");
    let [window, page, retained, retained_except_live] = plans;
    for (name, plan) in [("window", window), ("page", page)] {
        assert!(
            plan.contains("SEARCH node USING INDEX sqlite_autoindex_graph_nodes_2")
                || plan.contains("SEARCH node USING INDEX sqlite_autoindex_graph_nodes_1"),
            "{name} must seek by session and generation: {plan}"
        );
        assert!(
            !plan.contains("SCAN node"),
            "{name} scanned graph nodes: {plan}"
        );
    }

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
