//! Planner witnesses for the trigger listings, and the dispatch that picks
//! them.
//!
//! The twin of `lash-sqlite-store`'s `triggers/listing_plan_tests.rs`, and for
//! the same reason: a listing whose predicates are optional at the SQL level —
//! `column = COALESCE($N, column)`, or `($N IS NULL OR column = $N)` — is
//! result-equivalent and plan-fatal. The right-hand side mentions the column,
//! so the comparison is not sargable and the listing reads the whole relation
//! and filters, whatever the caller bound. Only a plan assertion notices,
//! because the answers were always right.
//!
//! Two settings make the witness mean what it says, and both are load-bearing:
//!
//! * `enable_seqscan = off`, as the process-registry list witness uses it: an
//!   empty fixture table would otherwise be scanned whatever the indexes say.
//! * `plan_cache_mode = force_generic_plan`, over a real `PREPARE`. A custom
//!   plan is built with the parameter values in hand, and PostgreSQL will then
//!   fold `COALESCE('a-value', column)` to the constant and seek anyway — so a
//!   custom-plan witness passes the very regression this asserts against. The
//!   generic plan is the one a long-lived pool executes from the sixth
//!   execution onwards, and it is the one that cannot fold. Measured on
//!   PostgreSQL 16 with the optional-predicate spelling restored: the custom
//!   plan still reports `Index Cond: (owner_scope = 'session:owner-001')`,
//!   the generic plan reports no index condition at all.

use super::*;

/// One parameter of a prepared witness: its SQL type, and the literal its
/// `EXECUTE` passes.
struct Param {
    sql_type: &'static str,
    literal: String,
}

fn text(value: &str) -> Param {
    Param {
        sql_type: "text",
        literal: format!("'{}'", value.replace('\'', "''")),
    }
}

fn epoch(value: i64) -> Param {
    Param {
        sql_type: "bigint",
        // Quoted, because `-9223372036854775808::bigint` parses as a negated
        // literal that does not fit before the cast applies.
        literal: format!("'{value}'::bigint"),
    }
}

/// The generic plan PostgreSQL executes for `sql` with `params`, as one
/// string, or `None` when no database is configured.
async fn plan_for(sql: &str, params: &[Param]) -> Option<String> {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").ok()?;
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let storage = crate::PostgresStorage::connect(database.url())
        .await
        .expect("connect the planner-witness database");
    let mut connection = storage
        .pool()
        .acquire()
        .await
        .expect("acquire a planner-witness connection");
    for setting in [
        "SET enable_seqscan = off",
        "SET plan_cache_mode = force_generic_plan",
    ] {
        sqlx::query(setting)
            .execute(&mut *connection)
            .await
            .expect("configure the planner witness");
    }
    let types = params
        .iter()
        .map(|param| param.sql_type)
        .collect::<Vec<_>>()
        .join(", ");
    let arguments = params
        .iter()
        .map(|param| param.literal.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let prepare = if types.is_empty() {
        format!("PREPARE witness AS {sql}")
    } else {
        format!("PREPARE witness ({types}) AS {sql}")
    };
    sqlx::query(&prepare)
        .execute(&mut *connection)
        .await
        .expect("prepare the witness statement");
    let execute = if arguments.is_empty() {
        "EXPLAIN (FORMAT TEXT) EXECUTE witness".to_string()
    } else {
        format!("EXPLAIN (FORMAT TEXT) EXECUTE witness({arguments})")
    };
    let rows = sqlx::query_scalar::<_, String>(&execute)
        .fetch_all(&mut *connection)
        .await
        .expect("explain the trigger listing");
    Some(rows.join("\n"))
}

/// One planner witness: what it is called, the statement, what it binds, the
/// index it must read through, and the columns that index must position on.
struct Witness {
    name: &'static str,
    statement: &'static str,
    params: Vec<Param>,
    index: &'static str,
    seek_columns: &'static [&'static str],
}

/// Just the `Index Cond:` lines of `plan`: what the index positions on, as
/// opposed to what it filters after positioning. An unsargable comparison
/// contributes nothing here and everything to `Filter:`.
fn index_conditions(plan: &str) -> String {
    plan.lines()
        .filter(|line| line.trim_start().starts_with("Index Cond:"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn subscription_filter(
    owner_scope: Option<&str>,
    subscription_key: Option<&str>,
    source_type: Option<&str>,
    source_key: Option<&str>,
) -> TriggerSubscriptionFilter {
    TriggerSubscriptionFilter {
        registrant_scope_id: owner_scope.map(str::to_string),
        subscription_key: subscription_key.map(str::to_string),
        source_type: source_type.map(str::to_string),
        source_key: source_key.map(str::to_string),
        ..TriggerSubscriptionFilter::default()
    }
}

/// Every seeking shape is explained through the dispatch production uses, not
/// through a field named by hand: a shape routed to the wrong statement has to
/// fail here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_named_subscription_listing_seeks_its_index() {
    let sql = trigger_store::trigger_sql();
    let cases = vec![
        Witness {
            name: "by owner scope",
            statement: trigger_store::subscription_list_sql(&subscription_filter(
                Some("session:owner-001"),
                None,
                None,
                None,
            )),
            params: vec![text("session:owner-001")],
            index: "idx_lash_trigger_subscriptions_registrant",
            seek_columns: &["owner_scope"],
        },
        Witness {
            name: "by owner scope and subscription key",
            statement: trigger_store::subscription_list_sql(&subscription_filter(
                Some("session:owner-001"),
                Some("key-00001"),
                None,
                None,
            )),
            params: vec![text("session:owner-001"), text("key-00001")],
            index: "idx_lash_trigger_subscriptions_registrant",
            seek_columns: &["owner_scope", "subscription_key"],
        },
        Witness {
            name: "by source type",
            statement: trigger_store::subscription_list_sql(&subscription_filter(
                None,
                None,
                Some("source-type-01"),
                None,
            )),
            params: vec![text("source-type-01")],
            index: "idx_lash_trigger_subscriptions_source",
            seek_columns: &["source_type"],
        },
        Witness {
            name: "by source",
            statement: trigger_store::subscription_list_sql(&subscription_filter(
                None,
                None,
                Some("source-type-01"),
                Some("source-key-01"),
            )),
            params: vec![text("source-type-01"), text("source-key-01")],
            index: "idx_lash_trigger_subscriptions_source",
            seek_columns: &["source_type", "source_key"],
        },
        Witness {
            name: "the ingress reservation lookup",
            statement: sql.subscription_postgres.select_enabled_for_source.sql(),
            params: vec![text("source-type-01"), text("source-key-01")],
            index: "idx_lash_trigger_subscriptions_source",
            seek_columns: &["source_type", "source_key", "lifecycle"],
        },
        // PostgreSQL seeks the owner scope here and filters the source, where
        // SQLite seeks the source and filters the owner. Both are index seeks
        // over the same three equalities; which one a planner prefers is its
        // own affair, and what this pins is that it has one to seek at all.
        Witness {
            name: "the ingress reservation lookup, session-scoped",
            statement: sql
                .subscription_postgres
                .select_enabled_for_source_and_owner
                .sql(),
            params: vec![
                text("source-type-01"),
                text("source-key-01"),
                text("session:owner-001"),
            ],
            index: "idx_lash_trigger_subscriptions_registrant",
            seek_columns: &["owner_scope"],
        },
    ];
    assert_witnesses(cases).await;
}

/// The same witness as the subscription listing's, through the same dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_named_occurrence_listing_seeks_its_index() {
    let sql = trigger_store::trigger_sql();
    let cases = vec![
        Witness {
            name: "by source type",
            statement: sql.occurrence.list_by_source_type.sql(),
            // The default window, which is what an unfiltered caller binds: it
            // has to stay an index condition rather than become a heap filter.
            params: vec![text("source-type-01"), epoch(i64::MIN), epoch(i64::MAX)],
            index: "idx_lash_trigger_occurrences_source",
            seek_columns: &["source_type"],
        },
        Witness {
            name: "by source",
            statement: sql.occurrence.list_by_source.sql(),
            params: vec![
                text("source-type-01"),
                text("source-key-01"),
                epoch(10),
                epoch(500),
            ],
            index: "idx_lash_trigger_occurrences_source",
            seek_columns: &["source_type", "source_key", "occurred_at_ms"],
        },
    ];
    assert_witnesses(cases).await;
}

/// Explain each witness and hold it to its index and its seek columns.
async fn assert_witnesses(cases: Vec<Witness>) {
    for case in cases {
        let Some(plan) = plan_for(case.statement, &case.params).await else {
            eprintln!("skipping the trigger listing plan witness: no database configured");
            return;
        };
        let name = case.name;
        assert!(
            !plan.contains("Seq Scan"),
            "`{name}` must not fall back to a sequential scan: {plan}"
        );
        assert!(
            plan.contains(case.index),
            "`{name}` must read through {}: {plan}",
            case.index
        );
        let conditions = index_conditions(&plan);
        for column in case.seek_columns {
            assert!(
                conditions.contains(column),
                "`{name}` must position the index on `{column}` rather than \
                 filter through it: {plan}"
            );
        }
    }
}
