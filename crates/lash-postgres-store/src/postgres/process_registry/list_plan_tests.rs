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
//! `enable_seqscan = off` makes the witness independent of table statistics:
//! an empty fixture table would otherwise be scanned whatever the indexes say,
//! while a predicate the planner cannot match still falls back to a sequential
//! scan under the setting, which is exactly the failure this asserts against.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

async fn plan_for(filter: &lash_core::ProcessListFilter) -> Option<String> {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").ok()?;
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let storage = crate::PostgresStorage::connect(database.url())
        .await
        .expect("connect the planner-witness database");
    let sql = list_processes_sql(filter);
    let explain = format!("EXPLAIN (FORMAT TEXT) {sql}");
    let mut query = sqlx::query_scalar::<_, String>(&explain)
        .bind(filter.status.labels())
        .bind(filter.originator.as_ref().map(|o| o.originator_id()))
        .bind(filter.identity_kind.as_deref())
        .bind(filter.identity_label.as_deref())
        .bind(Option::<serde_json::Value>::None)
        .bind(filter.caused_by_occurrence_id.as_deref())
        .bind(filter.caused_by_subscription_id.as_deref())
        .bind(filter.created_at_start_ms.map(crate::clamp_epoch_ms))
        .bind(filter.created_at_end_ms.map(crate::clamp_epoch_ms))
        .bind(filter.retired_since_ms.map(crate::clamp_epoch_ms));
    if let Some(parent) = &filter.parent_scope {
        query = query.bind(parent.storage_kind()).bind(parent.storage_id());
    }
    if let Some(before_ms) = filter.cancel_pending_before_ms {
        query = query.bind(crate::clamp_epoch_ms(before_ms));
    }
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
async fn pending_cancel_list_uses_the_partial_cancel_index() {
    let Some(plan) = plan_for(&lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::Any,
        cancel_pending_before_ms: Some(1_700_000_000_000),
        ..lash_core::ProcessListFilter::default()
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
async fn parent_scope_list_uses_the_parent_scope_index() {
    let Some(plan) = plan_for(&lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::Any,
        parent_scope: Some(lash_core::ParentScope::Turn {
            session_id: SessionId::from("plan-session"),
            turn_id: lash_core::TurnId::from("plan-turn"),
        }),
        ..lash_core::ProcessListFilter::default()
    })
    .await
    else {
        return;
    };
    assert!(
        plan.contains("idx_lash_processes_parent_scope"),
        "a populated parent scope must seek the scope index:\n{plan}"
    );
}

#[test]
fn an_unpopulated_filter_leaves_no_predicate_behind() {
    let sql = list_processes_sql(&lash_core::ProcessListFilter {
        status: lash_core::ProcessStatusFilter::Any,
        ..lash_core::ProcessListFilter::default()
    });
    assert_eq!(sql, process_sql().process_postgres.list.sql());
    assert!(
        !sql.contains("parent_scope_kind") && !sql.contains("cancel_requested_at_ms"),
        "an absent filter must not widen the statement:\n{sql}"
    );
}

/// `schema.sql` spells the partial index out as SQL text, so this is the only
/// check that keeps it equal to the fragment the query generates.
#[test]
fn the_pending_cancel_index_predicate_is_the_generated_fragment() {
    let predicate =
        lash_core::store_backend_support::nonterminal_process_status_predicate_sql("status");
    assert_eq!(
        predicate,
        "status NOT IN ('completed', 'failed', 'cancelled', 'abandoned')"
    );
    assert!(
        crate::PostgresStorage::schema_ddl().contains(&format!(
            "WHERE cancel_requested_at_ms IS NOT NULL\n      AND {predicate}"
        )),
        "the index predicate must be byte-identical to the query predicate"
    );
}
