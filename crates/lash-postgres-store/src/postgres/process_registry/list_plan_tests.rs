//! Planner witnesses for the two index-served list filters.
//!
//! A partial index is only usable when the planner can prove the query
//! predicate implies the index predicate, and PostgreSQL compares those
//! expressions structurally rather than semantically. That makes the pending
//! cancel index a byte-for-byte contract between `schema.sql` and the
//! statement this build sends: if either side's wording drifts — a reordered
//! `NOT IN` list, a rewritten nonterminal fragment — the index silently stops
//! being used and only a plan assertion notices.
//!
//! A 10,000-row fleet with three matches makes these witnesses about the
//! filtered roster workload. An empty table lets the keyset's primary-key
//! range win on cost even when the filter's index is eligible. Statistics
//! must describe the sparse selection before asserting the chosen index.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

async fn plan_for(filter: &lash_core_execution::ProcessListFilter) -> Option<String> {
    let url = crate::testing::required_database_url();
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("connect the planner-witness database");
    use lash_core_execution::{Lifetime, ProcessProvenance, ProcessRegistrar as _};
    lash_core_execution::testing::process_execution_env_fixture(&storage.process_env_store()).await;
    let template = storage
        .process_registry()
        .register_process(lash_core_execution::testing::held_engine_registration(
            serde_json::Value::Null,
            ProcessProvenance::host(),
            Lifetime::Detached,
        ))
        .await
        .expect("register planner template");
    let mut rows = Vec::new();
    for index in 0..10_000 {
        let mut record = template.clone();
        record.id = ProcessId::fixture(&format!("planner-{index:05}"));
        if matches!(index, 1000 | 5000 | 9000) {
            record.cancel_request = Some(Box::new(lash_core_execution::CancelRequest::new(
                lash_core_execution::CancelOrigin::OperatorRequested,
                "planner",
                1,
            )));
            if let Some(scope) = &filter.until {
                record.lifetime = lash_core_execution::LifetimeDecision::Until {
                    scope: scope.clone(),
                    grant: lash_core_execution::ScopeGrant::Ancestor,
                };
                record.ancestry = lash_core_execution::Ancestry::from_scopes([scope.clone()]);
            }
        }
        let scope = record.lifetime.scope();
        rows.push(serde_json::json!({
            "record": record,
            "scope_kind": scope.map(|scope| scope.storage_kind()),
            "scope_id": scope.map(|scope| scope.storage_id()),
        }));
    }
    sqlx::query("DELETE FROM lash_processes")
        .execute(storage.pool())
        .await
        .expect("remove template");
    let inserted = sqlx::query("INSERT INTO lash_processes (process_id, originator_id, identity_kind, created_at_ms, updated_at_ms, lifetime, lifetime_scope_kind, lifetime_scope_id, record_json) SELECT r#>>'{record,id}', 'host', r#>>'{record,identity,kind}', (r#>>'{record,created_at_ms}')::bigint, (r#>>'{record,updated_at_ms}')::bigint, r#>>'{record,lifetime,lifetime}', r->>'scope_kind', r->>'scope_id', (r->'record')::text FROM jsonb_array_elements($1) r")
        .bind(serde_json::Value::Array(rows)).execute(storage.pool()).await.expect("seed planner fleet");
    assert_eq!(inserted.rows_affected(), 10_000);
    sqlx::query("ANALYZE lash_processes")
        .execute(storage.pool())
        .await
        .expect("measure sparse filter statistics");
    let sql = crate::process_sql::roster_sql(filter);
    let explain = format!("EXPLAIN (FORMAT TEXT) {sql}");
    let mut query = sqlx::query_scalar::<_, String>(&explain)
        .bind(filter.status.labels())
        .bind(filter.originator.as_ref().map(|o| o.originator_id()))
        .bind(filter.identity_kind.as_deref())
        .bind(filter.identity_label.as_deref())
        .bind(Option::<serde_json::Value>::None)
        .bind(filter.created_at_start_ms.map(crate::clamp_epoch_ms))
        .bind(filter.created_at_end_ms.map(crate::clamp_epoch_ms))
        .bind(filter.retired_since_ms.map(crate::clamp_epoch_ms));
    if let Some(scope) = &filter.until {
        query = query.bind(scope.storage_kind()).bind(scope.storage_id());
    }
    if let Some(before_ms) = filter.cancel_pending_before_ms {
        query = query.bind(crate::clamp_epoch_ms(before_ms));
    }
    let query = query
        .bind(Option::<String>::None)
        .bind(Option::<String>::None)
        .bind(Some("p_"))
        .bind("~")
        .bind(257_i64);
    let mut connection = storage
        .pool()
        .acquire()
        .await
        .expect("acquire a planner-witness connection");
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *connection)
        .await
        .expect("disable sequential scans for the witness");
    let rows = query
        .fetch_all(&mut *connection)
        .await
        .expect("explain the list statement");
    Some(rows.join("\n"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_cancel_roster_page_uses_the_partial_cancel_index() {
    let Some(plan) = plan_for(&lash_core_execution::ProcessListFilter {
        status: lash_core_execution::ProcessStatusFilter::Any,
        cancel_pending_before_ms: Some(1_700_000_000_000),
        ..lash_core_execution::ProcessListFilter::default()
    })
    .await
    else {
        return;
    };
    assert!(
        plan.contains("idx_lash_processes_pending_cancel"),
        "the pending-cancel predicate must match the partial index byte for byte:\n{plan}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn until_scope_roster_page_uses_the_lifetime_scope_index() {
    let Some(plan) = plan_for(&lash_core_execution::ProcessListFilter {
        status: lash_core_execution::ProcessStatusFilter::Any,
        until: Some(lash_core_execution::ScopeId::turn(
            SessionId::from("plan-session"),
            lash_core_execution::TurnId::from("plan-turn"),
        )),
        ..lash_core_execution::ProcessListFilter::default()
    })
    .await
    else {
        return;
    };
    assert!(
        plan.contains("idx_lash_processes_lifetime_scope"),
        "a populated until scope must seek the scope index:\n{plan}"
    );
}
