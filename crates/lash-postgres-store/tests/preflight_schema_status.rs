//! The preflight verdict must agree with the open it precedes.
//!
//! A preflight that passes a deployment the open path refuses is worse than no
//! preflight: the host is told to start, the open refuses, and under a supervisor
//! that is the crash loop the surface exists to replace. The case below is the
//! shape that actually diverged — lash tables present, component version never
//! stamped — asserted against both sides in one test so neither can move alone.

use lash_core::{StorePreflight, StoreSchemaOutcome, StoreSchemaVerdict};
use lash_postgres_store::{PostgresStorePreflight, SchemaCheck};

#[allow(dead_code)]
mod support;

use support::database_url;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::ScratchSchema;

/// A schema carrying every lash table but no version stamp is refused at open
/// (`unstamped_schema`), so the preflight has to refuse it too. Reporting it as
/// `Absent` — "nothing provisioned, the next open would create it" — is the
/// divergence this pins shut.
#[tokio::test]
async fn an_unstamped_schema_is_refused_by_preflight_exactly_as_by_open() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight schema status: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    // The committed DDL stamps the component version at the end. Removing the
    // stamp alone leaves the shape a half-applied host migration leaves behind:
    // every table, no generation.
    scratch
        .apply("DELETE FROM lash_schema_versions WHERE component = 'lash-postgres-store'")
        .await;

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let status = preflight
        .schema_status()
        .await
        .expect("an unstamped schema is readable, so the status call succeeds");

    assert_eq!(
        status.databases.len(),
        1,
        "a PostgreSQL deployment carries one component stamp"
    );
    assert_eq!(
        status.databases[0].verdict,
        StoreSchemaVerdict::Mismatch { found: 0 },
        "an unstamped schema is provisioned-but-ungenerated, not absent"
    );
    assert!(status.databases[0].verdict.refuses_open());
    assert_eq!(status.outcome(), StoreSchemaOutcome::Refused);

    // The other half of the agreement: the open this preflight precedes does
    // refuse, so the verdict above is a prediction and not an opinion.
    let refusal = scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .err()
        .expect("an unstamped schema must not open")
        .to_string();
    assert!(
        refusal.to_lowercase().contains("version"),
        "the open refusal names the version stamp: {refusal}"
    );

    scratch.cleanup().await;
}

/// The complement, so the fix cannot be a blanket refusal: a schema the DDL
/// stamped is reported ready, and a schema with no lash tables at all is
/// reported absent rather than refused.
#[tokio::test]
async fn a_stamped_schema_is_ready_and_an_empty_one_is_absent() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight schema status: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let status = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .schema_status()
        .await
        .expect("read schema status");
    assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Matches);
    assert_eq!(status.outcome(), StoreSchemaOutcome::Ready);

    scratch
        .apply("DROP SCHEMA IF EXISTS lash_preflight_empty CASCADE")
        .await;
    scratch.apply("CREATE SCHEMA lash_preflight_empty").await;
    let empty_pool = harness::pool_with_search_path(&database_url, "lash_preflight_empty").await;
    let empty = PostgresStorePreflight::from_pool(empty_pool.clone())
        .schema_status()
        .await
        .expect("read schema status of an empty schema");
    assert_eq!(
        empty.databases[0].verdict,
        StoreSchemaVerdict::Absent,
        "no lash tables and no stamp is genuinely nothing provisioned"
    );
    assert_eq!(empty.outcome(), StoreSchemaOutcome::Ready);
    empty_pool.close().await;

    scratch
        .apply("DROP SCHEMA IF EXISTS lash_preflight_empty CASCADE")
        .await;
    scratch.cleanup().await;
}
