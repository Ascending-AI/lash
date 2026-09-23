use super::*;

use sql::list_processes_query;

#[test]
fn recently_retired_query_uses_bounded_live_and_retired_indexes() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let mut stmt = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            process_sql().process_sqlite.list_recent_retired.sql()
        ))
        .expect("prepare recently retired query plan");
    let plan = stmt
        .query_map(
            params![
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                Option::<i64>::None,
                Option::<i64>::None,
                100_i64,
            ],
            |row| row.get::<_, String>(3),
        )
        .expect("explain recently retired query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect recently retired query plan");

    assert!(
        plan.iter().any(|step| {
            step.contains("idx_processes_status") || step.contains("idx_processes_live_worklist")
        }),
        "live branch must use a live/status index, plan: {plan:?}"
    );
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_recent_retired")),
        "retired branch must seek the recency index, plan: {plan:?}"
    );
    assert!(
        plan.iter()
            .all(|step| !step.contains("sqlite_autoindex_processes_1")),
        "bounded poll must not scan the all-history primary-key index, plan: {plan:?}"
    );
}

#[test]
fn observed_recently_retired_query_seeks_recency_before_observer_history() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let mut stmt = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            process_sql()
                .registry_sqlite
                .list_observed_recent_retired
                .sql()
        ))
        .expect("prepare observed query plan");
    let plan = stmt
        .query_map(params!["session", Option::<String>::None, 100_i64], |row| {
            row.get::<_, String>(3)
        })
        .expect("explain observed query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_recent_retired")),
        "retired branch must seek recency: {plan:?}"
    );
    assert!(
        plan.iter().any(|step| step.contains("idx_processes_status")
            || step.contains("idx_processes_live_worklist")),
        "live branch must use a live index: {plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.contains("SCAN o")),
        "observer history must use keyed probes: {plan:?}"
    );
}

#[test]
fn pending_cancel_query_seeks_the_partial_cancel_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = list_processes_query(
        &lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            cancel_pending_before_ms: Some(1_700_000_000_000),
            ..lash_core_execution::ProcessListFilter::default()
        },
        None,
        None,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare pending-cancel query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain pending-cancel query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect pending-cancel query plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_pending_cancel")),
        "a populated pending-cancel bound must seek the partial index, plan: {plan:?}"
    );
}

#[test]
fn parent_scope_query_seeks_the_parent_scope_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = list_processes_query(
        &lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            parent_scope: Some(lash_core_execution::ParentScope::turn(
                lash_sansio::SessionId::from("plan-session"),
                lash_core_execution::TurnId::from("plan-turn"),
            )),
            ..lash_core_execution::ProcessListFilter::default()
        },
        None,
        None,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare parent-scope query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain parent-scope query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect parent-scope query plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_parent_scope")),
        "a populated parent scope must seek the scope index, plan: {plan:?}"
    );
    assert!(
        plan.iter().all(|step| !step.contains("SCAN processes")),
        "the scope lookup must not degrade to a table scan, plan: {plan:?}"
    );
}

#[test]
fn an_absent_scope_filter_emits_no_scope_predicate() {
    let (sql, values) = list_processes_query(
        &lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            ..lash_core_execution::ProcessListFilter::default()
        },
        None,
        None,
    );
    assert_eq!(sql, process_sql().process_sqlite.list.sql());
    assert!(
        !sql.contains("parent_scope_kind") && !sql.contains("cancel_requested_at_ms"),
        "an unpopulated filter must not widen the statement: {sql}"
    );
    assert_eq!(
        values.len(),
        9,
        "only the fixed bindings are present when neither new filter is populated"
    );
}

/// The partial index is spelled as a literal in `schema.rs` (the lifecycle
/// vocabulary gate exempts that file), so nothing but this assertion keeps it
/// equal to the fragment the query generates.
#[test]
fn the_pending_cancel_index_predicate_is_the_generated_fragment() {
    let predicate =
        lash_core_execution::store_backend_support::nonterminal_process_status_predicate_sql(
            "status",
        );
    assert_eq!(
        predicate,
        "status NOT IN ('completed', 'failed', 'cancelled', 'abandoned')"
    );
    assert!(
        crate::schema::PROCESS_SCHEMA.contains(&format!(
            "WHERE cancel_requested_at_ms IS NOT NULL\n      AND {predicate}"
        )),
        "the index predicate must be byte-identical to the query predicate"
    );
}
