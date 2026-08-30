use super::*;

#[test]
fn recently_retired_query_uses_bounded_live_and_retired_indexes() {
    let conn = rusqlite::Connection::open_in_memory().expect("open query-plan database");
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("install process schema");
    let mut stmt = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {LIST_PROCESSES_RECENT_RETIRED_SQL}"
        ))
        .expect("prepare recently retired query plan");
    let plan = stmt
        .query_map(
            params![
                Option::<String>::None,
                Option::<i64>::None,
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
