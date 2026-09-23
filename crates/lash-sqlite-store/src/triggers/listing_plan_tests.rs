//! Planner witnesses for the trigger listings, and the dispatch that picks
//! them.
//!
//! The subscription and occurrence listings used to build their `WHERE` clause
//! out of whichever filter fields were set. Writing that as one statement with
//! optional predicates — `column = COALESCE(?N, column)`, or
//! `(?N IS NULL OR column = ?N)` — is result-equivalent and plan-fatal: the
//! right-hand side mentions the column, so SQLite cannot seek an index through
//! it and every listing degrades to a full scan with a filter, whatever the
//! caller bound. These tests are what notices, because nothing else would: the
//! conformance suites only compare answers, and the answers were always right.
//!
//! Each named shape therefore asserts `SEARCH … USING INDEX` with the exact
//! index columns the seek uses. `list_all` asserts the opposite — it is the
//! general listing and scans by design, and pinning that keeps the fallback
//! honest about what it costs.

use super::*;
use lash_core_execution::TriggerOccurrenceFilter;
use lash_core_execution::TriggerSubscriptionFilter;

/// Enough rows, and an `ANALYZE`, for the planner to be making a real choice
/// rather than defaulting on an empty table.
const FIXTURE: &str = "WITH RECURSIVE n(i) AS (
         SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000
     )
     INSERT INTO trigger_subscriptions (
         subscription_id, owner_scope, subscription_key, incarnation, revision,
         definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
         created_at_ms, updated_at_ms, record_json
     )
     SELECT printf('plan-%05d', i), printf('session:owner-%03d', i % 50),
            printf('key-%05d', i), 'incarnation', i, 'fingerprint',
            printf('source-type-%02d', i % 20), printf('source-key-%02d', i % 7),
            'enabled', NULL, i, i, '{}'
     FROM n;
     WITH RECURSIVE n(i) AS (
         SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000
     )
     INSERT INTO trigger_occurrences (
         occurrence_id, idempotency_key, source_type, source_key,
         occurred_at_ms, record_json
     )
     SELECT printf('occurrence-%05d', i), printf('idempotency-%05d', i),
            printf('source-type-%02d', i % 20), printf('source-key-%02d', i % 7),
            i, '{}'
     FROM n;
     ANALYZE;";

/// The plan SQLite chooses for `sql` with `values` bound, as one line.
async fn plan_for(sql: &'static str, values: Vec<rusqlite::types::Value>) -> String {
    let store = SqliteTriggerStore::memory()
        .await
        .expect("open an in-memory trigger store");
    store
        .conn
        .call(move |conn| {
            conn.execute_batch(FIXTURE)?;
            let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                    row.get::<_, String>(3)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows.join(" | "))
        })
        .await
        .expect("explain the trigger listing")
}

fn text(value: &str) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(value.to_string())
}

fn epoch(value: i64) -> rusqlite::types::Value {
    rusqlite::types::Value::Integer(value)
}

/// A filter setting exactly these fields.
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
/// fail here, which is what makes this a witness for the listing rather than
/// for the SQL.
#[tokio::test]
async fn every_named_subscription_listing_seeks_its_index() {
    let sql = trigger_sql();
    for (name, statement, values, expected) in [
        (
            "by owner scope",
            subscription_list_sql(&subscription_filter(
                Some("session:owner-001"),
                None,
                None,
                None,
            )),
            vec![text("session:owner-001")],
            "idx_trigger_subscriptions_registrant (owner_scope=?)",
        ),
        (
            "by owner scope and subscription key",
            subscription_list_sql(&subscription_filter(
                Some("session:owner-001"),
                Some("key-00001"),
                None,
                None,
            )),
            vec![text("session:owner-001"), text("key-00001")],
            "(owner_scope=? AND subscription_key=?)",
        ),
        (
            "by source type",
            subscription_list_sql(&subscription_filter(
                None,
                None,
                Some("source-type-01"),
                None,
            )),
            vec![text("source-type-01")],
            "idx_trigger_subscriptions_source (source_type=?)",
        ),
        (
            "by source",
            subscription_list_sql(&subscription_filter(
                None,
                None,
                Some("source-type-01"),
                Some("source-key-01"),
            )),
            vec![text("source-type-01"), text("source-key-01")],
            "idx_trigger_subscriptions_source (source_type=? AND source_key=?)",
        ),
        (
            "select_enabled_for_source",
            sql.subscription_sqlite.select_enabled_for_source.sql(),
            vec![text("source-type-01"), text("source-key-01")],
            "idx_trigger_subscriptions_source (source_type=? AND source_key=? AND lifecycle=?)",
        ),
        (
            "select_enabled_for_source_and_owner",
            sql.subscription_sqlite
                .select_enabled_for_source_and_owner
                .sql(),
            vec![
                text("source-type-01"),
                text("source-key-01"),
                text("session:owner-001"),
            ],
            "idx_trigger_subscriptions_source (source_type=? AND source_key=? AND lifecycle=?)",
        ),
    ] {
        let plan = plan_for(statement, values).await;
        assert!(
            plan.contains("SEARCH trigger_subscriptions"),
            "`{name}` must seek rather than scan the table: {plan}"
        );
        assert!(
            plan.contains(expected),
            "`{name}` must seek through {expected}: {plan}"
        );
    }
}

/// The statement the occurrence listing picks for a filter of this shape.
fn occurrence_list_sql(source_type: bool, source_key: bool) -> &'static str {
    trigger_sql()
        .occurrence
        .list_for(OccurrenceListShape::of(source_type, source_key))
        .sql()
}

/// The same witness as the subscription listing's, through the same dispatch.
#[tokio::test]
async fn every_named_occurrence_listing_seeks_its_index() {
    for (name, statement, values, expected) in [
        (
            "by source type",
            occurrence_list_sql(true, false),
            vec![text("source-type-01"), epoch(i64::MIN), epoch(i64::MAX)],
            "idx_trigger_occurrences_source (source_type=?)",
        ),
        (
            "by source",
            occurrence_list_sql(true, true),
            vec![
                text("source-type-01"),
                text("source-key-01"),
                epoch(10),
                epoch(500),
            ],
            "idx_trigger_occurrences_source (source_type=? AND source_key=? \
             AND occurred_at_ms>? AND occurred_at_ms<?)",
        ),
    ] {
        let plan = plan_for(statement, values).await;
        assert!(
            plan.contains("SEARCH trigger_occurrences"),
            "`{name}` must seek rather than scan the table: {plan}"
        );
        assert!(
            plan.contains(expected),
            "`{name}` must seek through {expected}: {plan}"
        );
    }
    // The window survives as an index range even when it is the default one:
    // both bounds are always bound, so the comparison stays sargable.
    let plan = plan_for(
        occurrence_list_sql(true, true),
        vec![
            text("source-type-01"),
            text("source-key-01"),
            epoch(i64::MIN),
            epoch(i64::MAX),
        ],
    )
    .await;
    assert!(
        plan.contains("occurred_at_ms>? AND occurred_at_ms<?"),
        "the default window must still be an index range: {plan}"
    );
}

#[tokio::test]
async fn the_general_listings_scan_by_design() {
    let plan = plan_for(
        subscription_list_sql(&TriggerSubscriptionFilter::default()),
        Vec::new(),
    )
    .await;
    assert!(
        plan.contains("SCAN trigger_subscriptions"),
        "the general subscription listing has nothing to seek on: {plan}"
    );
}

#[test]
fn every_production_filter_shape_picks_its_named_statement() {
    let sql = trigger_sql();
    let owner = || Some("session:owner-001".to_string());
    let source_type = || Some("source-type-01".to_string());
    let source_key = || Some("source-key-01".to_string());

    // Each row is a filter shape a production caller passes today, named by
    // where it is built.
    for (caller, filter, expected) in [
        (
            "facade Admin::subscriptions with a default filter",
            TriggerSubscriptionFilter::default(),
            sql.subscription.list_all.sql(),
        ),
        (
            "SessionOps::list_trigger_registrations / the List command",
            TriggerSubscriptionFilter {
                registrant_scope_id: owner(),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_by_owner.sql(),
        ),
        (
            "SessionOps::trigger_registrations_by_source_type",
            TriggerSubscriptionFilter {
                registrant_scope_id: owner(),
                source_type: source_type(),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_by_owner.sql(),
        ),
        (
            "the workbench cron registration probe",
            TriggerSubscriptionFilter {
                registrant_scope_id: owner(),
                source_type: source_type(),
                source_key: source_key(),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_by_owner.sql(),
        ),
        (
            "the workbench trigger-record route",
            TriggerSubscriptionFilter {
                registrant_scope_id: owner(),
                subscription_key: Some("key-00001".to_string()),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_by_owner_and_key.sql(),
        ),
        (
            "TriggerSubscriptionFilter::for_source_type",
            TriggerSubscriptionFilter {
                source_type: source_type(),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_by_source_type.sql(),
        ),
        (
            "a remote filter naming a whole source",
            TriggerSubscriptionFilter {
                source_type: source_type(),
                source_key: source_key(),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_by_source.sql(),
        ),
        (
            "a lashlang List filtered on enablement alone",
            TriggerSubscriptionFilter {
                enabled: Some(true),
                ..TriggerSubscriptionFilter::default()
            },
            sql.subscription.list_all.sql(),
        ),
    ] {
        assert_eq!(
            subscription_list_sql(&filter),
            expected,
            "the filter built by {caller} must be served by its named statement"
        );
    }

    for (caller, filter, expected) in [
        (
            "an unfiltered occurrence listing",
            TriggerOccurrenceFilter::default(),
            sql.occurrence.list_all.sql(),
        ),
        (
            "an occurrence listing by source type",
            TriggerOccurrenceFilter {
                source_type: source_type(),
                ..TriggerOccurrenceFilter::default()
            },
            sql.occurrence.list_by_source_type.sql(),
        ),
        (
            "an occurrence listing by source and window",
            TriggerOccurrenceFilter {
                source_type: source_type(),
                source_key: source_key(),
                occurred_at_start_ms: Some(10),
                occurred_at_end_ms: Some(500),
            },
            sql.occurrence.list_by_source.sql(),
        ),
        (
            "an occurrence listing by window alone",
            TriggerOccurrenceFilter {
                occurred_at_start_ms: Some(10),
                ..TriggerOccurrenceFilter::default()
            },
            sql.occurrence.list_all.sql(),
        ),
    ] {
        let shape =
            OccurrenceListShape::of(filter.source_type.is_some(), filter.source_key.is_some());
        assert_eq!(
            sql.occurrence.list_for(shape).sql(),
            expected,
            "the filter built by {caller} must be served by its named statement"
        );
    }
}
