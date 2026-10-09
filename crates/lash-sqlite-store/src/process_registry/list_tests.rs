use super::*;

use sql::roster_query;

#[test]
fn retired_roster_page_uses_live_and_recency_indexes() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = roster_query(
        &lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            retired_since_ms: Some(100),
            ..Default::default()
        },
        None,
        None,
        Some("p_"),
        "~",
        257,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare recently retired page query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain recently retired query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect recently retired query plan");

    assert!(
        plan.iter().any(|step| {
            step.contains("idx_processes_status") || step.contains("idx_processes_non_terminal")
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
            || step.contains("idx_processes_non_terminal")),
        "live branch must use a live index: {plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.contains("SCAN o")),
        "observer history must use keyed probes: {plan:?}"
    );
}

#[test]
fn pending_cancel_roster_page_seeks_the_partial_cancel_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = roster_query(
        &lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            cancel_pending_before_ms: Some(1_700_000_000_000),
            ..lash_core_execution::ProcessListFilter::default()
        },
        None,
        None,
        Some("p_"),
        "~",
        257,
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
fn until_scope_roster_page_seeks_the_lifetime_scope_index() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let (sql, values) = roster_query(
        &lash_core_execution::ProcessListFilter {
            status: lash_core_execution::ProcessStatusFilter::Any,
            until: Some(lash_core_execution::ScopeId::turn(
                lash_sansio::SessionId::from("plan-session"),
                lash_core_execution::TurnId::from("plan-turn"),
            )),
            ..lash_core_execution::ProcessListFilter::default()
        },
        None,
        None,
        Some("p_"),
        "~",
        257,
    );
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare until-scope query plan");
    let plan = stmt
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain until-scope query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect until-scope query plan");
    assert!(
        plan.iter()
            .any(|step| step.contains("idx_processes_lifetime_scope")),
        "a populated until scope must seek the scope index, plan: {plan:?}"
    );
    assert!(
        plan.iter().all(|step| !step.contains("SCAN processes")),
        "the scope lookup must not degrade to a table scan, plan: {plan:?}"
    );
}

#[test]
fn retention_keeps_terminal_process_until_cascade_drains() {
    let conn = rusqlite::Connection::open_in_memory().expect("open retention fixture");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("create schema");
    conn.execute_batch("INSERT INTO processes (process_id, originator_id, identity_kind, created_at_ms, updated_at_ms, change_seq, lifetime, record_json, cascade_cursor) VALUES ('parent', 'origin', 'standard', 0, 0, 1, 'detached', '{\"last_event_sequence\":0,\"lifecycle\":{\"state\":\"terminal\",\"outcome\":{\"type\":\"settled\",\"output\":{\"outcome\":{\"status\":\"success\"}}}}}', 'child')").expect("terminal with unfinished cascade");
    let sql = process_sql().process_sqlite.list_prunable_terminal.sql();
    let count = |conn: &rusqlite::Connection| {
        conn.prepare(sql)
            .expect("prepare candidates")
            .query_map(params![1, Option::<i64>::None], |_| Ok(()))
            .expect("candidates")
            .count()
    };
    assert_eq!(
        count(&conn),
        0,
        "an unfinished cancellation cascade must retain its parent"
    );
    conn.execute_batch("UPDATE processes SET cascade_cursor = NULL")
        .expect("drain cascade");
    assert_eq!(
        count(&conn),
        1,
        "a drained terminal is eligible for retention"
    );
}
