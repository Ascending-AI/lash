//! The preflight verdict must agree with the open it precedes.
//!
//! A preflight that passes a deployment the open path refuses is worse than no
//! preflight: the host is told to start, the open refuses, and under a supervisor
//! that is the crash loop the surface exists to replace. The case below is the
//! shape that actually diverged — lash tables present, component version never
//! stamped — asserted against both sides in one test so neither can move alone.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_core_execution::{
    FleetFormat, FleetFormatState, StorePreflight, StoreReleaseState, StoreSchemaOutcome,
    StoreSchemaVerdict,
};
use lash_postgres_store::{MigrationPhase, PostgresStorage, PostgresStorePreflight, SchemaCheck};

#[allow(dead_code)]
mod support;

use support::database_url;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::ScratchSchema;

#[test]
fn synthetic_feature_selects_the_owning_compatibility_descriptor() {
    use lash_core_execution::compat::{ComponentId, VersionRange, descriptor};

    let descriptor = descriptor(ComponentId::POSTGRES).expect("PostgreSQL descriptor");
    let next = if cfg!(feature = "synthetic-next") {
        2
    } else {
        1
    };
    assert_eq!(descriptor.reads, VersionRange::between(1, next));
    assert_eq!(descriptor.writes, VersionRange::exactly(next));
}

#[tokio::test]
async fn stamp_one_preflight_open_and_migrate_agree() {
    let Some(database_url) = database_url() else {
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let before = preflight.schema_status().await.expect("stamp-1 status");
    assert_eq!(before.databases[0].verdict, StoreSchemaVerdict::Matches);
    scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("stamp 1 opens before migration");
    let stamp: (i32, i32) = sqlx::query_as("SELECT version, min_reader FROM lash_schema_versions")
        .fetch_one(&scratch.pool)
        .await
        .expect("read stamp after preflight and open");
    assert_eq!(stamp, (1, 1), "preflight and open do not migrate");

    let separator = if database_url.contains('?') { '&' } else { '?' };
    let url = format!(
        "{database_url}{separator}options=-csearch_path%3D{}",
        scratch.name
    );
    let migrated = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("migrate the accepted stamp-1 catalog");
    assert_eq!(
        migrated.executed.len(),
        usize::from(cfg!(feature = "synthetic-next"))
    );
    let stamp: (i32, i32) = sqlx::query_as("SELECT version, min_reader FROM lash_schema_versions")
        .fetch_one(&scratch.pool)
        .await
        .expect("read migrated stamp");
    let expected = if cfg!(feature = "synthetic-next") {
        2
    } else {
        1
    };
    assert_eq!(stamp, (expected, 1));
    let after = preflight.schema_status().await.expect("migrated status");
    assert_eq!(after.databases[0].verdict, StoreSchemaVerdict::Matches);
    scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("the migrated catalog opens");
    assert!(
        PostgresStorage::migrate(&url, MigrationPhase::Expand)
            .await
            .expect("migration rerun")
            .executed
            .is_empty()
    );
    scratch.cleanup().await;
}

#[tokio::test]
async fn stamp_two_preflight_and_open_agree_on_synthetic_shape() {
    let Some(database_url) = database_url() else {
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 2, min_reader = 1;
             ALTER TABLE lash_sessions ADD COLUMN synthetic_next_note TEXT;
             CREATE TABLE lash_synthetic_next (id BIGSERIAL PRIMARY KEY, note TEXT);
             CREATE INDEX idx_lash_synthetic_next_note ON lash_synthetic_next(note)",
        )
        .await;
    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let expected = if cfg!(feature = "synthetic-next") {
        StoreSchemaVerdict::Matches
    } else {
        StoreSchemaVerdict::Expanded { found: 2 }
    };
    assert_eq!(
        preflight
            .schema_status()
            .await
            .expect("stamp-2 status")
            .databases[0]
            .verdict,
        expected
    );
    scratch
        .open_host_provisioned(SchemaCheck::Enforce)
        .await
        .expect("the complete stamp-2 catalog opens");
    for (mutation, missing) in [
        (
            "ALTER TABLE lash_sessions DROP COLUMN synthetic_next_note",
            "missing nullable lash_sessions.synthetic_next_note",
        ),
        (
            "DROP INDEX idx_lash_synthetic_next_note",
            "missing non-unique index idx_lash_synthetic_next_note",
        ),
        (
            "DROP TABLE lash_synthetic_next",
            "missing table lash_synthetic_next",
        ),
    ] {
        scratch.apply(mutation).await;
        let status = preflight
            .schema_status()
            .await
            .expect("incomplete stamp-2 status");
        let open = scratch.open_host_provisioned(SchemaCheck::Enforce).await;
        if cfg!(feature = "synthetic-next") {
            assert!(
                matches!(
                    &status.databases[0].verdict,
                    StoreSchemaVerdict::Refused {
                        refusal: lash_core_execution::compat::CompatRefusal::ShapeRefused { findings, .. }
                    } if findings.iter().any(|finding| finding.contains(missing))
                ),
                "the synthetic generation requires {missing}: {status:?}"
            );
            let error = open
                .err()
                .expect("an incomplete synthetic generation must not open");
            assert!(error.to_string().contains(missing), "{error}");
        } else {
            assert_eq!(status.databases[0].verdict, expected);
            open.expect("the current generation tolerates missing next-generation objects");
        }
    }
    scratch.cleanup().await;
}

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
        StoreSchemaVerdict::Refused {
            refusal: lash_core_execution::compat::CompatRefusal::Unstamped {
                component: "postgres".to_string(),
                writing_release: None,
            },
        },
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
        refusal.to_lowercase().contains("stamp"),
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

async fn wait_for_relation_reader(pool: &sqlx::PgPool, relation: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_locks
                 WHERE relation = $1::regclass AND mode = 'AccessShareLock' AND NOT granted)",
            )
            .bind(relation)
            .fetch_one(pool)
            .await
            .expect("inspect the relation barrier");
            if waiting {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the status reader must reach the relation barrier");
}

async fn seed_release(scratch: &ScratchSchema) {
    scratch
        .apply(
            "INSERT INTO lash_release_stamp
                (singleton, release_version, schema_versions, written_at_epoch_ms)
             VALUES (TRUE, 'before', 'lash-postgres-store=1', 1)",
        )
        .await;
}

fn assert_release(status: &lash_core_execution::StoreSchemaStatus, expected: &str) {
    let StoreReleaseState::Stamped(stamp) = &status.release else {
        panic!("the release must remain readable: {:?}", status.release);
    };
    assert_eq!(stamp.release, expected);
}

#[tokio::test]
async fn status_holds_the_schema_lock_through_optional_probes_and_migration() {
    let Some(database_url) = database_url() else {
        return;
    };
    let _guard = support::SharedDatabaseLock::acquire(&database_url).await;
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_release(&scratch).await;
    let before = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .schema_status()
        .await
        .expect("read the catalog before the migration barrier");
    let barrier_pool = harness::pool_with_search_path(&database_url, &scratch.name).await;
    let mut barrier = barrier_pool.begin().await.expect("begin release barrier");
    sqlx::query("LOCK TABLE lash_release_stamp IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *barrier)
        .await
        .expect("block the release probe after shape and stamp reads");
    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let status = tokio::spawn(async move { preflight.schema_status().await });
    wait_for_relation_reader(&barrier_pool, "lash_release_stamp").await;

    let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    let mut migrator = scratch.pool.acquire().await.expect("acquire migrator");
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
        .bind(namespace)
        .bind(key)
        .fetch_one(&mut *migrator)
        .await
        .expect("test the migration lock while status is collecting optional facts");
    if acquired {
        sqlx::query("SELECT pg_advisory_unlock($1, $2)")
            .bind(namespace)
            .bind(key)
            .execute(&mut *migrator)
            .await
            .expect("release the unexpected exclusive lock");
    }
    barrier.commit().await.expect("release the probe barrier");
    let status = status.await.expect("status task").expect("schema status");
    barrier_pool.close().await;
    assert!(
        !acquired,
        "migration must not acquire the schema key between shape and release observations"
    );
    assert_eq!(status.databases[0].verdict, before.databases[0].verdict);
    assert_eq!(status.databases[0].min_reader, Some(1));
    assert_release(&status, "before");

    sqlx::query("SELECT pg_advisory_lock($1, $2)")
        .bind(namespace)
        .bind(key)
        .execute(&mut *migrator)
        .await
        .expect("migration proceeds after the observation closes");
    let mut migration = sqlx::Connection::begin(&mut *migrator)
        .await
        .expect("begin migration");
    sqlx::raw_sql(
        "UPDATE lash_schema_versions SET version = 3, min_reader = 1;
         ALTER TABLE lash_sessions ADD CONSTRAINT observation_restriction CHECK (TRUE);
         UPDATE lash_release_stamp SET release_version = 'after', written_at_epoch_ms = 2",
    )
    .execute(&mut *migration)
    .await
    .expect("commit stamp, expanded restriction and release together");
    migration.commit().await.expect("commit migration");
    sqlx::query("SELECT pg_advisory_unlock($1, $2)")
        .bind(namespace)
        .bind(key)
        .execute(&mut *migrator)
        .await
        .expect("release migration key");
    drop(migrator);
    let after = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .schema_status()
        .await
        .expect("observe the completed migration");
    assert!(matches!(
        &after.databases[0].verdict,
        StoreSchemaVerdict::Refused {
            refusal: lash_core_execution::compat::CompatRefusal::ShapeRefused { findings, .. }
        } if findings.iter().any(|finding| finding.contains("observation_restriction"))
    ));
    assert_eq!(after.databases[0].min_reader, Some(1));
    assert_release(&after, "after");
    scratch.cleanup().await;
}

#[tokio::test]
async fn status_keeps_release_and_fleet_in_one_snapshot_during_finalize() {
    let Some(database_url) = database_url() else {
        return;
    };
    let _guard = support::SharedDatabaseLock::acquire(&database_url).await;
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_release(&scratch).await;
    let before: i32 = sqlx::query_scalar("SELECT format_version FROM lash_fleet_format")
        .fetch_one(&scratch.pool)
        .await
        .expect("read initial fleet");
    let barrier_pool = harness::pool_with_search_path(&database_url, &scratch.name).await;
    let mut finalize = barrier_pool.begin().await.expect("begin finalize barrier");
    sqlx::query("LOCK TABLE lash_fleet_format IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *finalize)
        .await
        .expect("block the fleet probe after the release read");
    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let status = tokio::spawn(async move { preflight.schema_status().await });
    wait_for_relation_reader(&barrier_pool, "lash_fleet_format").await;
    sqlx::raw_sql(
        "UPDATE lash_release_stamp SET release_version = 'after', written_at_epoch_ms = 2;
         UPDATE lash_fleet_format SET format_version = format_version + 1",
    )
    .execute(&mut *finalize)
    .await
    .expect("advance release and fleet without the schema advisory key");
    finalize.commit().await.expect("commit finalize");
    let status = status.await.expect("status task").expect("schema status");
    barrier_pool.close().await;
    assert_release(&status, "before");
    assert_eq!(
        status.fleet_format,
        FleetFormatState::Recorded(FleetFormat::from_version(
            u32::try_from(before).expect("nonnegative fleet")
        )),
        "the fleet must come from the snapshot that supplied the release"
    );
    let after = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .schema_status()
        .await
        .expect("observe finalized fleet");
    assert_release(&after, "after");
    assert_eq!(
        after.fleet_format,
        FleetFormatState::Recorded(FleetFormat::from_version(
            u32::try_from(before + 1).expect("nonnegative fleet")
        ))
    );
    scratch.cleanup().await;
}

#[tokio::test]
async fn cancelling_status_releases_the_shared_schema_key() {
    let Some(database_url) = database_url() else {
        return;
    };
    let _guard = support::SharedDatabaseLock::acquire(&database_url).await;
    let scratch = ScratchSchema::provision(&database_url).await;
    let barrier_pool = harness::pool_with_search_path(&database_url, &scratch.name).await;
    let mut barrier = barrier_pool.begin().await.expect("begin release barrier");
    sqlx::query("LOCK TABLE lash_release_stamp IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *barrier)
        .await
        .expect("block the release probe");
    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let status = tokio::spawn(async move { preflight.schema_status().await });
    wait_for_relation_reader(&barrier_pool, "lash_release_stamp").await;
    status.abort();
    assert!(
        status
            .await
            .expect_err("cancel the status task")
            .is_cancelled()
    );
    barrier.commit().await.expect("release relation barrier");
    let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    let mut exclusive = barrier_pool
        .acquire()
        .await
        .expect("acquire exclusive probe");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        sqlx::query("SELECT pg_advisory_lock($1, $2)")
            .bind(namespace)
            .bind(key)
            .execute(&mut *exclusive)
            .await
    })
    .await
    .expect("cancellation must release the session key")
    .expect("acquire exclusive schema key");
    sqlx::query("SELECT pg_advisory_unlock($1, $2)")
        .bind(namespace)
        .bind(key)
        .execute(&mut *exclusive)
        .await
        .expect("release exclusive schema key");
    drop(exclusive);
    barrier_pool.close().await;
    scratch.cleanup().await;
}

#[tokio::test]
async fn optional_relation_errors_do_not_abort_status_or_later_probes() {
    let Some(database_url) = database_url() else {
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    for mutation in [
        "ALTER TABLE lash_release_stamp DROP COLUMN release_version",
        "DROP TABLE lash_release_stamp",
        "ALTER TABLE lash_fleet_format DROP COLUMN format_version",
        "DROP TABLE lash_fleet_format",
        "ALTER TABLE lash_schema_versions DROP COLUMN min_reader",
    ] {
        scratch.apply(mutation).await;
        let status = PostgresStorePreflight::from_pool(scratch.pool.clone())
            .schema_status()
            .await
            .expect("optional relation failures must remain typed status facts");
        assert!(matches!(
            status.databases[0].verdict,
            StoreSchemaVerdict::Unreadable { .. } | StoreSchemaVerdict::Refused { .. }
        ));
        if mutation.starts_with("ALTER TABLE lash_release_stamp") {
            assert!(matches!(
                status.release,
                StoreReleaseState::Unreadable { .. }
            ));
            assert!(matches!(status.fleet_format, FleetFormatState::Recorded(_)));
        } else {
            assert_eq!(status.release, StoreReleaseState::Unstamped);
            if mutation.starts_with("ALTER TABLE lash_fleet_format") {
                assert!(matches!(
                    status.fleet_format,
                    FleetFormatState::Unreadable { .. }
                ));
            } else if mutation == "DROP TABLE lash_release_stamp" {
                assert!(matches!(status.fleet_format, FleetFormatState::Recorded(_)));
            } else {
                assert_eq!(status.fleet_format, FleetFormatState::Unrecorded);
            }
        }
    }
    scratch.cleanup().await;
}

#[tokio::test]
async fn status_takes_its_first_snapshot_after_a_waiting_migration_commits() {
    let Some(database_url) = database_url() else {
        return;
    };
    let _guard = support::SharedDatabaseLock::acquire(&database_url).await;
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_release(&scratch).await;
    let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    let mut migrator = scratch.pool.acquire().await.expect("acquire migrator");
    sqlx::query("SELECT pg_advisory_lock($1, $2)")
        .bind(namespace)
        .bind(key)
        .execute(&mut *migrator)
        .await
        .expect("hold the schema key before status starts");
    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let status = tokio::spawn(async move { preflight.schema_status().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_locks
                 WHERE locktype = 'advisory' AND classid = $1::oid AND objid = $2::oid
                   AND mode = 'ShareLock' AND NOT granted)",
            )
            .bind(namespace)
            .bind(key)
            .fetch_one(&scratch.pool)
            .await
            .expect("inspect the schema-lock barrier");
            if waiting {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("status must wait behind the migration");
    sqlx::raw_sql(
        "BEGIN;
         UPDATE lash_schema_versions SET version = 3, min_reader = 1;
         ALTER TABLE lash_sessions ADD COLUMN observation_note TEXT;
         UPDATE lash_release_stamp SET release_version = 'after', written_at_epoch_ms = 2;
         COMMIT",
    )
    .execute(&mut *migrator)
    .await
    .expect("commit the expanded catalog while status waits");
    sqlx::query("SELECT pg_advisory_unlock($1, $2)")
        .bind(namespace)
        .bind(key)
        .execute(&mut *migrator)
        .await
        .expect("release the schema key");
    drop(migrator);
    let status = status.await.expect("status task").expect("schema status");
    assert_eq!(
        status.databases[0].verdict,
        StoreSchemaVerdict::Expanded { found: 3 }
    );
    assert_eq!(status.databases[0].min_reader, Some(1));
    assert_release(&status, "after");
    scratch.cleanup().await;
}

#[tokio::test]
async fn status_preserves_expanded_and_synthetic_policy_without_writes() {
    let Some(database_url) = database_url() else {
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_release(&scratch).await;
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 3, min_reader = 1;
             ALTER TABLE lash_sessions ADD COLUMN observation_note TEXT",
        )
        .await;
    let readonly = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect({
            let schema = scratch.name.clone();
            move |connection, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {schema}"))
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET default_transaction_read_only = on")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(&database_url)
        .await
        .expect("create a pool that rejects preflight writes");
    let preflight = PostgresStorePreflight::from_pool(readonly.clone());
    let expanded = preflight
        .schema_status()
        .await
        .expect("read expanded status");
    assert_eq!(
        expanded.databases[0].verdict,
        StoreSchemaVerdict::Expanded { found: 3 }
    );
    assert_release(&expanded, "before");
    scratch
        .apply(
            "UPDATE lash_schema_versions SET version = 2;
             ALTER TABLE lash_sessions DROP COLUMN observation_note;
             ALTER TABLE lash_sessions ADD COLUMN synthetic_next_note TEXT;
             CREATE TABLE lash_synthetic_next (id BIGSERIAL PRIMARY KEY, note TEXT);
             CREATE INDEX idx_lash_synthetic_next_note ON lash_synthetic_next(note)",
        )
        .await;
    let synthetic = preflight
        .schema_status()
        .await
        .expect("read next-build status");
    let native_next = cfg!(feature = "synthetic-next");
    if native_next {
        assert_eq!(synthetic.databases[0].verdict, StoreSchemaVerdict::Matches);
    } else {
        assert_eq!(
            synthetic.databases[0].verdict,
            StoreSchemaVerdict::Expanded { found: 2 }
        );
    }
    scratch
        .apply("ALTER TABLE lash_sessions DROP COLUMN synthetic_next_note")
        .await;
    let missing = preflight
        .schema_status()
        .await
        .expect("read missing next-build shape");
    if native_next {
        assert!(matches!(
            &missing.databases[0].verdict,
            StoreSchemaVerdict::Refused {
                refusal: lash_core_execution::compat::CompatRefusal::ShapeRefused { findings, .. }
            } if findings.iter().any(|finding| finding.contains("missing nullable lash_sessions.synthetic_next_note"))
        ));
    } else {
        assert_eq!(
            missing.databases[0].verdict,
            StoreSchemaVerdict::Expanded { found: 2 }
        );
    }
    let stamp: (i32, i32) = sqlx::query_as("SELECT version, min_reader FROM lash_schema_versions")
        .fetch_one(&scratch.pool)
        .await
        .expect("read unchanged compatibility stamp");
    assert_eq!(stamp, (2, 1));
    assert_release(&missing, "before");
    readonly.close().await;
    scratch.cleanup().await;
}
