//! `lashctl migrate` integration proofs (FIG-3816, FIG-3817).
//!
//! The runner is the operational step that owns what worker open deliberately
//! refuses to: provisioning a fresh database and carrying a stamped catalog
//! forward. Each case owns an isolated database and a scratch schema, so its
//! catalog changes and database-wide advisory locks cannot block other cases.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_postgres_store::{MigrateError, MigrationPhase, MigrationRefusal, PostgresStorage};
use sqlx::{Connection, PgConnection, Row};
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod support;

/// A scratch schema named per test, pointed at through `search_path` in the
/// URL exactly the way a deployment pins its catalog.
fn scratch_url(database_url: &str, schema: &str) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-csearch_path%3D{schema}")
}

async fn create_scratch_schema(database_url: &str) -> String {
    let schema = format!("lash_migrate_{}", uuid::Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(database_url)
        .await
        .expect("connect scratch admin");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut admin)
        .await
        .expect("create scratch schema");
    admin.close().await.expect("close scratch admin");
    schema
}

async fn drop_scratch_schema(database_url: &str, schema: &str) {
    let mut admin = PgConnection::connect(database_url)
        .await
        .expect("connect scratch cleanup");
    sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&mut admin)
        .await
        .expect("drop scratch schema");
    admin.close().await.expect("close scratch cleanup");
}

/// The lash relations the scratch schema carries — the dry-run proof counts
/// zero.
async fn scratch_lash_table_count(database_url: &str, schema: &str) -> i64 {
    let mut admin = PgConnection::connect(database_url)
        .await
        .expect("connect scratch inspector");
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM pg_catalog.pg_class AS relation
         JOIN pg_catalog.pg_namespace AS namespace
           ON namespace.oid = relation.relnamespace
         WHERE namespace.nspname = $1
           AND relation.relname LIKE 'lash\\_%'
           AND relation.relkind IN ('r', 'p')",
    )
    .bind(schema)
    .fetch_one(&mut admin)
    .await
    .expect("count lash relations in the scratch schema");
    admin.close().await.expect("close scratch inspector");
    count
}

/// The committed ledger rows under a scratch schema.
async fn ledger_rows(url: &str) -> Vec<(String, String, Option<i32>, i32)> {
    let mut connection = PgConnection::connect(url)
        .await
        .expect("connect to read the migration ledger");
    let rows = sqlx::query(
        "SELECT phase, migration, from_version, to_version
         FROM lash_migrations ORDER BY started_at_ms, migration",
    )
    .map(|row: sqlx::postgres::PgRow| {
        (
            row.get::<String, _>(0),
            row.get::<String, _>(1),
            row.get::<Option<i32>, _>(2),
            row.get::<i32, _>(3),
        )
    })
    .fetch_all(&mut connection)
    .await
    .expect("read the migration ledger");
    connection.close().await.expect("close ledger read");
    rows
}

async fn catalog_definitions(url: &str) -> Vec<String> {
    let mut connection = PgConnection::connect(url).await.expect("inspect catalog");
    let definitions = sqlx::query_scalar::<_, String>(
        "SELECT definition FROM (
            SELECT c.relname || ':' || a.attname || ':' ||
                   pg_catalog.format_type(a.atttypid, a.atttypmod) || ':' || a.attnotnull AS definition
            FROM pg_catalog.pg_class c
            JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
            WHERE c.relnamespace = current_schema()::regnamespace AND a.attnum > 0
                  AND NOT a.attisdropped
            UNION ALL
            SELECT conname || ':' || pg_get_constraintdef(oid)
            FROM pg_catalog.pg_constraint WHERE connamespace = current_schema()::regnamespace
            UNION ALL
            SELECT indexname || ':' || indexdef FROM pg_catalog.pg_indexes
            WHERE schemaname = current_schema()
         ) definitions ORDER BY definition",
    )
    .fetch_all(&mut connection)
    .await
    .expect("read catalog definitions");
    connection.close().await.expect("close catalog inspector");
    definitions
}

/// The stamp `schema.sql` provisions, as `(version, min_reader)` (ADR 0115
/// §1.2, §7): the one component version, which open admits by, the ledger
/// records and the catalog's steps are numbered in.
fn provisioned_stamp() -> (i32, i32) {
    (
        PostgresStorage::schema_version(),
        PostgresStorage::schema_version(),
    )
}

/// The component version this build's migrate leaves a store at: the version
/// its compatibility descriptor writes.
fn written_version() -> i32 {
    let writes =
        lash_core_execution::compat::descriptor(lash_core_execution::compat::ComponentId::POSTGRES)
            .expect("the build declares the PostgreSQL store")
            .writes
            .max();
    i32::try_from(writes).expect("the component version fits")
}

/// The catalog steps this build's migrate runs past the version `schema.sql`
/// provisions, as `(ledger id, from, to)`, in the active tier: the synthetic
/// successor carries its one expand (ADR 0115 §6), and N carries none.
fn component_expands() -> Vec<(&'static str, i32, i32)> {
    if cfg!(feature = "synthetic-next") {
        vec![(
            "synthetic-next-expand",
            PostgresStorage::schema_version(),
            written_version(),
        )]
    } else {
        Vec::new()
    }
}

/// The ledger rows [`component_expands`] record: each names the component
/// version it started from and the one it moved the stamp to.
fn component_expand_rows() -> Vec<(String, String, Option<i32>, i32)> {
    component_expands()
        .into_iter()
        .map(|(id, from, to)| ("expand".to_string(), id.to_string(), Some(from), to))
        .collect()
}

/// The component's compatibility stamp, as `(version, min_reader)`.
async fn compat_stamp(url: &str) -> (i32, i32) {
    let mut connection = PgConnection::connect(url)
        .await
        .expect("connect to read the component stamp");
    let stamp = sqlx::query_as::<_, (i32, i32)>(
        "SELECT version, min_reader FROM lash_schema_versions
         WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&mut connection)
    .await
    .expect("read the component stamp");
    connection.close().await.expect("close stamp read");
    stamp
}

/// Sets the component stamp of `schema` to `version`, floor included.
async fn stamp_component(database_url: &str, schema: &str, version: i32) {
    let mut admin = PgConnection::connect(database_url)
        .await
        .expect("connect scratch provisioner");
    sqlx::query(&format!(
        "UPDATE {schema}.lash_schema_versions SET version = $1, min_reader = $1
         WHERE component = 'lash-postgres-store'"
    ))
    .bind(version)
    .execute(&mut admin)
    .await
    .expect("stamp the older component");
    admin.close().await.expect("close scratch provisioner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_on_a_fresh_schema_creates_the_schema_and_the_ledger() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    let report = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("migrate a fresh schema");

    assert_eq!(report.namespace.as_deref(), Some(schema.as_str()));
    assert_eq!(report.found_version, None, "a fresh schema has no stamp");
    assert!(report.applied.is_empty(), "a fresh ledger has no rows");
    assert_eq!(
        report
            .executed
            .iter()
            .map(|step| step.migration.clone())
            .collect::<Vec<_>>(),
        std::iter::once(format!("bootstrap-{}", PostgresStorage::schema_version()))
            .chain(
                component_expands()
                    .into_iter()
                    .map(|(id, ..)| id.to_string())
            )
            .collect::<Vec<_>>(),
        "a fresh migrate applies the bootstrap and this build's component expands: {report:?}"
    );
    assert!(report.executed[0].started_at_ms.is_some());
    assert_eq!(report.executed[0].state, "applied");
    assert!(report.planned.is_empty(), "nothing remains pending");

    let ledger = ledger_rows(&url).await;
    assert_eq!(
        ledger,
        std::iter::once((
            "expand".to_string(),
            format!("bootstrap-{}", PostgresStorage::schema_version()),
            None,
            PostgresStorage::schema_version()
        ))
        .chain(component_expand_rows())
        .collect::<Vec<_>>(),
        "the ledger records the bootstrap and the component expands"
    );
    assert_eq!(
        compat_stamp(&url).await,
        (written_version(), provisioned_stamp().1),
        "the migrate writes this build's compatibility stamp"
    );

    // And the provisioned catalog is openable: the gate a worker takes passes.
    PostgresStorage::connect(&url)
        .await
        .expect("a migrated fresh schema must open")
        .pool()
        .close()
        .await;
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migrate_rerun_is_a_no_op() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("first migrate");
    let rerun = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("rerun migrate");

    assert!(
        rerun.executed.is_empty(),
        "a rerun applies nothing: {rerun:?}"
    );
    assert_eq!(rerun.found_version, Some(written_version()));
    assert_eq!(
        rerun.applied.len(),
        1 + component_expands().len(),
        "the ledger still records the first run's steps"
    );
    assert_eq!(
        ledger_rows(&url).await.len(),
        1 + component_expands().len(),
        "a rerun writes no new ledger rows"
    );
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

/// The fleet epoch the scratch schema records, or `None` without a row.
async fn recorded_fleet_epoch(url: &str) -> Option<i32> {
    let mut connection = PgConnection::connect(url)
        .await
        .expect("connect fleet-epoch inspector");
    let epoch = sqlx::query_scalar("SELECT format_version FROM lash_fleet_format WHERE singleton")
        .fetch_optional(&mut connection)
        .await
        .expect("read the fleet epoch");
    connection
        .close()
        .await
        .expect("close fleet-epoch inspector");
    epoch
}

/// `lash migrate` seeds `F` at the migrating build's writable floor, and an
/// open never records it (FIG-4075, ADR 0115 §2.1): a store with no epoch
/// refuses typed and stays unrecorded, and a migrate rerun seeds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_seeds_the_fleet_epoch_and_an_open_never_records_one() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    let seed = lash_core_execution::FleetFormat::seed(lash_core_execution::FleetFormat::writable());
    let seeded = i32::try_from(seed.version()).expect("epoch fits");

    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("migrate a fresh schema");
    assert_eq!(
        recorded_fleet_epoch(&url).await,
        Some(seeded),
        "a fresh migrate seeds F before any open"
    );

    // A catalog a build predating the seed migrated: the open refuses and
    // decides nothing.
    let mut admin = PgConnection::connect(&url).await.expect("connect scratch");
    sqlx::query("DELETE FROM lash_fleet_format")
        .execute(&mut admin)
        .await
        .expect("drop the fleet epoch");
    admin.close().await.expect("close scratch");
    let refused = PostgresStorage::connect(&url)
        .await
        .err()
        .expect("an open with no recorded F must refuse");
    assert!(
        matches!(
            &refused,
            lash_core_execution::StoreError::Incompatible {
                refusal: lash_core_execution::compat::CompatRefusal::FleetUnrecorded { component, writing_release }
            } if component == "postgres"
                && writing_release.as_deref() == Some(env!("CARGO_PKG_VERSION"))
        ),
        "the refusal is typed: {refused}"
    );
    assert_eq!(
        recorded_fleet_epoch(&url).await,
        None,
        "a refused open records no F"
    );

    let rerun = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("rerun migrate");
    assert!(
        rerun.executed.is_empty(),
        "the rerun applies no step: {rerun:?}"
    );
    assert_eq!(
        recorded_fleet_epoch(&url).await,
        Some(seeded),
        "a migrate rerun seeds a catalog that records no F"
    );
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("the seeded catalog opens");
    assert_eq!(storage.fleet_format(), seed);
    storage.pool().close().await;
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_reports_the_plan_and_changes_nothing() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    let plan = PostgresStorage::plan_migrations(&url, MigrationPhase::Expand)
        .await
        .expect("plan a fresh migrate");
    assert_eq!(
        plan.planned.len(),
        1 + component_expands().len(),
        "the plan is the bootstrap and this build's component expands"
    );
    assert!(plan.executed.is_empty());
    assert_eq!(
        scratch_lash_table_count(&database_url, &schema).await,
        0,
        "a dry run must not provision"
    );

    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("migrate after the dry run");
    let replan = PostgresStorage::plan_migrations(&url, MigrationPhase::Expand)
        .await
        .expect("replan after migrate");
    assert!(
        replan.planned.is_empty(),
        "a current catalog has nothing pending: {replan:?}"
    );
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

#[cfg(not(feature = "synthetic-next"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_release_ledger_has_only_baseline_bootstrap_evidence() {
    let database = migrator_database()
        .await
        .expect("the ledger law requires PostgreSQL");
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("provision release baseline");
    let rows = ledger_rows(&url).await;
    assert_eq!(rows.len(), 1, "only baseline bootstrap evidence: {rows:?}");
    assert_eq!(rows[0].0, "expand");
    assert_eq!(
        rows[0].1,
        format!("bootstrap-{}", PostgresStorage::schema_version())
    );
    assert_eq!(rows[0].2, None, "bootstrap has no predecessor");
    assert_eq!(rows[0].3, PostgresStorage::schema_version());
    drop_scratch_schema(&database_url, &schema).await;
}

/// FIG-4493: a fresh 1.0 store's ledger holds no pre-1.0 transition: its
/// bootstrap provisions the release baseline 1, and only the synthetic-next
/// build's step carries it to 2. The store then opens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "release cut: FIG-4493"]
async fn a_fresh_1_0_store_records_only_its_version_1_bootstrap_and_opens() {
    let database = migrator_database()
        .await
        .expect("the ledger law requires PostgreSQL");
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("provision the release baseline");
    let mut expected = vec![("expand".to_string(), "bootstrap-1".to_string(), None, 1)];
    expected.extend(component_expand_rows());
    let rows = ledger_rows(&url).await;
    assert_eq!(rows, expected);
    assert!(
        rows.iter()
            .all(|(_, _, from, to)| from.is_none_or(|from| from == 1) && (1..=2).contains(to)),
        "a ledger row outside the release baseline 1 and its successor: {rows:?}"
    );
    let newest = written_version();
    assert_eq!(compat_stamp(&url).await.0, newest);
    PostgresStorage::connect(&url)
        .await
        .expect("a fresh 1.0 store opens");
    drop_scratch_schema(&database_url, &schema).await;
}

/// FIG-4493: the production catalog carries no predecessor step, and the
/// cut restarts every counter at 1, so no stamp lies below the release
/// baseline: the unsupported populated predecessor is a store a pre-1.0
/// build populated. It carries a stamp above every version this build reads
/// and a release stamp older than this build's. Open, plan and migrate refuse
/// it as pre-release state, typed, and change no schema, stamp or ledger row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_a_pre_release_build_populated_is_refused_unchanged() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("provision the store a pre-release build populated");
    // The last pre-1.0 PostgreSQL schema was 141.
    stamp_component(&database_url, &schema, written_version() + 140).await;
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect scratch provisioner");
    sqlx::query(&format!(
        "UPDATE {schema}.lash_release_stamp SET release_version = '0.0.0-alpha'
         WHERE singleton = TRUE"
    ))
    .execute(&mut admin)
    .await
    .expect("stamp the store as a pre-release build's");
    admin.close().await.expect("close scratch provisioner");
    let ledger_before = ledger_rows(&url).await;
    let stamp_before = compat_stamp(&url).await;
    let catalog_before = catalog_definitions(&url).await;

    let pre_release = |error: &lash_core_execution::StoreError| {
        matches!(
            error,
            lash_core_execution::StoreError::Incompatible {
                refusal: lash_core_execution::compat::CompatRefusal::PreRelease { component, .. }
            } if component == lash_core_execution::compat::ComponentId::POSTGRES.as_str()
        )
    };
    let opened = PostgresStorage::connect(&url)
        .await
        .err()
        .expect("a pre-release store must not open");
    assert!(
        pre_release(&opened),
        "the open refusal stays typed: {opened:?}"
    );
    for (what, refused) in [
        (
            "plan",
            PostgresStorage::plan_migrations(&url, MigrationPhase::Expand)
                .await
                .map(|_| ()),
        ),
        (
            "migrate",
            PostgresStorage::migrate(&url, MigrationPhase::Expand)
                .await
                .map(|_| ()),
        ),
    ] {
        let error = refused.expect_err("a pre-release store must not migrate");
        assert!(
            matches!(&error, MigrateError::Store(store) if pre_release(store)),
            "the {what} refusal names pre-release state: {error:?}"
        );
    }
    assert_eq!(compat_stamp(&url).await, stamp_before);
    assert_eq!(catalog_definitions(&url).await, catalog_before);
    assert_eq!(
        ledger_rows(&url).await,
        ledger_before,
        "a refused migrate records no step"
    );
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_phases_refuse_an_uninstalled_catalog_and_wait_for_finalize() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    for phase in [MigrationPhase::Backfill, MigrationPhase::Contract] {
        for (label, outcome) in [
            ("migrate", PostgresStorage::migrate(&url, phase).await),
            (
                "plan_migrations",
                PostgresStorage::plan_migrations(&url, phase).await,
            ),
        ] {
            match outcome {
                Err(MigrateError::Refused(MigrationRefusal::Unprovisioned { phase: named })) => {
                    assert_eq!(named, phase.name(), "{label}");
                }
                other => panic!(
                    "{label} --phase {} on an uninstalled catalog must refuse: {other:?}",
                    phase.name()
                ),
            }
        }
    }
    // And the refusal ran no DDL.
    assert_eq!(scratch_lash_table_count(&database_url, &schema).await, 0);

    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("expand provisions the catalog");
    let ledger_before = ledger_rows(&url).await;
    for phase in [MigrationPhase::Backfill, MigrationPhase::Contract] {
        let outcome = PostgresStorage::migrate(&url, phase).await;
        if cfg!(feature = "synthetic-next") {
            // The synthetic catalog carries a backfill and a contract step,
            // and `F` still records the release before it: both wait.
            assert!(
                matches!(
                    outcome,
                    Err(MigrateError::Refused(
                        MigrationRefusal::BackfillBeforeFinalize { .. }
                            | MigrationRefusal::ContractBeforeFinalize { .. }
                    ))
                ),
                "--phase {} before finalize must refuse: {outcome:?}",
                phase.name()
            );
        } else {
            // The 1.0 release carries neither: both run and do nothing.
            let report = outcome.unwrap_or_else(|error| {
                panic!("--phase {} on the 1.0 catalog: {error}", phase.name())
            });
            assert!(report.executed.is_empty(), "{report:?}");
        }
    }
    assert_eq!(
        ledger_rows(&url).await,
        ledger_before,
        "a later phase that waits records no step"
    );
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_migrates_serialize_and_converge() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    // Two runners racing one database serialize on the same advisory lock a
    // verifying open holds; whichever runs second sees a committed catalog and
    // does nothing.
    let (first, second) = tokio::join!(
        PostgresStorage::migrate(&url, MigrationPhase::Expand),
        PostgresStorage::migrate(&url, MigrationPhase::Expand),
    );
    let first = first.expect("first concurrent migrate");
    let second = second.expect("second concurrent migrate");
    let executed_total = first.executed.len() + second.executed.len();
    assert_eq!(
        executed_total,
        1 + component_expands().len(),
        "exactly one racer may execute each step: {first:?} {second:?}"
    );
    assert_eq!(ledger_rows(&url).await.len(), 1 + component_expands().len());
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_below_the_migration_floor_is_refused_not_recreated() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    // A catalog stamped far below what the expand catalog reaches has no
    // migration path: the runner must refuse with the typed range error rather
    // than guessing.
    let mut admin = PgConnection::connect(&database_url)
        .await
        .expect("connect scratch provisioner");
    sqlx::query(&format!("SET search_path TO {schema}"))
        .execute(&mut admin)
        .await
        .expect("point the provisioner at the scratch schema");
    sqlx::query(
        "CREATE TABLE lash_schema_versions (component TEXT PRIMARY KEY, version INTEGER NOT NULL)",
    )
    .execute(&mut admin)
    .await
    .expect("create a stamp table only");
    sqlx::query("INSERT INTO lash_schema_versions (component, version) VALUES ($1, $2)")
        .bind("lash-postgres-store")
        .bind(7)
        .execute(&mut admin)
        .await
        .expect("stamp an unreachable predecessor");
    admin.close().await.expect("close scratch provisioner");

    let error = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect_err("a catalog with no migration path must refuse");
    let rendered = error.to_string();
    assert!(
        rendered.contains("has version 7"),
        "the refusal names the found version: {rendered}"
    );
    assert!(
        rendered.contains("has no applicable migration"),
        "the refusal names the missing migration path: {rendered}"
    );
    assert_eq!(
        scratch_lash_table_count(&database_url, &schema).await,
        1,
        "a refused migrate runs no DDL"
    );
    drop_scratch_schema(&database_url, &schema).await;
    drop(database);
}

/// The catalog is the only path that moves the stamp (FIG-4704): a database
/// this build bootstraps and one provisioned from `schema.sql` are carried to
/// the version this build writes by the same catalog steps. The plan names
/// each step with the component versions it moves between, the run executes
/// exactly the plan, each step's ledger row records those versions, and both
/// stores end at one stamp.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_store_and_an_expanded_store_reach_one_stamp_through_the_catalog() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url().to_string();
    let steps = |report: &lash_postgres_store::MigrationReport, planned: bool| {
        let rows = if planned {
            &report.planned
        } else {
            &report.executed
        };
        rows.iter()
            .map(|step| (step.migration.clone(), step.from_version, step.to_version))
            .collect::<Vec<_>>()
    };
    let bootstrap = (
        format!("bootstrap-{}", PostgresStorage::schema_version()),
        None,
        PostgresStorage::schema_version(),
    );
    let catalog: Vec<_> = component_expands()
        .into_iter()
        .map(|(id, from, to)| (id.to_string(), Some(from), to))
        .collect();

    // Fresh: this build's migrate bootstraps the database, then walks the
    // catalog from the version the bootstrap stamped.
    let fresh = create_scratch_schema(&database_url).await;
    let fresh_url = scratch_url(&database_url, &fresh);
    let plan = PostgresStorage::plan_migrations(&fresh_url, MigrationPhase::Expand)
        .await
        .expect("plan the fresh store");
    let run = PostgresStorage::migrate(&fresh_url, MigrationPhase::Expand)
        .await
        .expect("migrate the fresh store");
    let expected: Vec<_> = std::iter::once(bootstrap).chain(catalog.clone()).collect();
    assert_eq!(steps(&plan, true), expected, "the fresh plan");
    assert_eq!(
        steps(&run, false),
        expected,
        "the fresh run executes its plan"
    );

    // Expanded: a store the previous build provisioned, which is `schema.sql`
    // at its own stamp, owes exactly the catalog's steps.
    let expanded = create_scratch_schema(&database_url).await;
    let expanded_url = scratch_url(&database_url, &expanded);
    let mut admin = PgConnection::connect(&expanded_url)
        .await
        .expect("connect scratch provisioner");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut admin)
        .await
        .expect("provision the store from schema.sql");
    admin.close().await.expect("close scratch provisioner");
    assert_eq!(compat_stamp(&expanded_url).await, provisioned_stamp());
    let plan = PostgresStorage::plan_migrations(&expanded_url, MigrationPhase::Expand)
        .await
        .expect("plan the provisioned store");
    assert_eq!(plan.found_version, Some(provisioned_stamp().0));
    let run = PostgresStorage::migrate(&expanded_url, MigrationPhase::Expand)
        .await
        .expect("migrate the provisioned store");
    assert_eq!(steps(&plan, true), catalog, "the provisioned store's plan");
    assert_eq!(
        steps(&run, false),
        catalog,
        "the provisioned store's run executes its plan"
    );

    // One stamp, reached by the same ledger rows past the bootstrap.
    let stamp = (written_version(), provisioned_stamp().1);
    assert_eq!(compat_stamp(&fresh_url).await, stamp, "the fresh stamp");
    assert_eq!(
        compat_stamp(&expanded_url).await,
        stamp,
        "the expanded stamp"
    );
    assert_eq!(
        ledger_rows(&fresh_url).await[1..],
        ledger_rows(&expanded_url).await[..],
        "both ledgers record the same catalog steps"
    );
    assert_eq!(ledger_rows(&expanded_url).await, component_expand_rows());
    for url in [&fresh_url, &expanded_url] {
        let replan = PostgresStorage::plan_migrations(url, MigrationPhase::Expand)
            .await
            .expect("replan a migrated store");
        assert_eq!(replan.found_version, Some(written_version()));
        assert!(replan.planned.is_empty(), "nothing remains: {replan:?}");
        PostgresStorage::connect(url)
            .await
            .expect("a migrated store opens")
            .pool()
            .close()
            .await;
    }
    drop_scratch_schema(&database_url, &fresh).await;
    drop_scratch_schema(&database_url, &expanded).await;
    drop(database);
}

/// Catalog laws use a database of their own because the advisory key is
/// database-wide, even when the catalog under test is a scratch schema.
async fn migrator_database() -> Option<lash_postgres_store::testing::IsolatedDatabase> {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return None;
    };
    Some(lash_postgres_store::testing::IsolatedDatabase::create(&database_url).await)
}

async fn hold_migrator_lock(database_url: &str) -> PgConnection {
    let mut holder = PgConnection::connect(database_url)
        .await
        .expect("connect migrator lock holder");
    let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    sqlx::query("SELECT pg_advisory_lock($1, $2)")
        .bind(namespace)
        .bind(key)
        .execute(&mut holder)
        .await
        .expect("hold migrator lock from a second connection");
    holder
}

async fn observe_migrator_wait(holder: &mut PgConnection) {
    let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM pg_locks
                 WHERE locktype = 'advisory' AND NOT granted
                   AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                   AND classid = $1::int::oid AND objid = $2::int::oid AND objsubid = 2)",
            )
            .bind(namespace)
            .bind(key)
            .fetch_one(&mut *holder)
            .await
            .expect("observe the migrator queued on the held lock");
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the migrator must queue on the advisory lock");
}

async fn migrate_waits_past_inherited_timeouts(dry_run: bool) {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url();
    let schema = create_scratch_schema(database_url).await;
    let url = scratch_url(database_url, &schema);
    PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("provision before testing advisory acquisition");
    let ledger_before = ledger_rows(&url).await;
    // Restrictive deployment settings must not turn a short advisory wait
    // into Contended or a statement cancellation. Keep the holder alive for
    // longer than both inherited bounds, after observing the actual waiter.
    let url = format!("{url}%20-clock_timeout%3D100ms%20-cstatement_timeout%3D2s");
    let mut holder = hold_migrator_lock(database_url).await;
    let runner = async {
        if dry_run {
            PostgresStorage::plan_migrations(&url, MigrationPhase::Expand).await
        } else {
            PostgresStorage::migrate(&url, MigrationPhase::Expand).await
        }
    };
    let (namespace, key) = PostgresStorage::schema_advisory_lock_key();
    let (result, ()) = tokio::join!(runner, async {
        observe_migrator_wait(&mut holder).await;
        // PostgreSQL releases the lock in the same command that delays it;
        // scheduling the test's cleanup cannot extend the intended hold.
        sqlx::raw_sql(&format!(
            "SELECT pg_sleep(2.1); SELECT pg_advisory_unlock({namespace}, {key})"
        ))
        .execute(&mut holder)
        .await
        .expect("release the holder after waiting past the inherited timeouts");
    });
    holder
        .close()
        .await
        .expect("close the migrator lock holder");
    let ledger_after = ledger_rows(&url).await;
    drop_scratch_schema(database_url, &schema).await;
    drop(database);
    assert_eq!(
        ledger_after, ledger_before,
        "a waiting rerun or plan changes no ledger rows"
    );
    let report = result.expect("migrate succeeds after the holder releases within 30 seconds");
    assert!(
        report.executed.is_empty(),
        "a rerun or plan applies no step: {report:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_waits_for_a_holder_despite_inherited_timeouts() {
    migrate_waits_past_inherited_timeouts(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_dry_run_waits_for_a_holder_despite_inherited_timeouts() {
    migrate_waits_past_inherited_timeouts(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_refuses_a_holder_only_after_the_documented_bound() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url();
    let schema = create_scratch_schema(database_url).await;
    let url = scratch_url(database_url, &schema);
    let mut holder = hold_migrator_lock(database_url).await;
    let started = Instant::now();
    let mut runner = Box::pin(PostgresStorage::migrate(&url, MigrationPhase::Expand));
    tokio::select! {
        result = &mut runner => panic!("migrator must queue before refusing: {result:?}"),
        () = observe_migrator_wait(&mut holder) => {},
    }
    // Connection setup and host scheduling have their own startup allowance;
    // the acquisition deadline starts after PostgreSQL reports the waiter.
    let result = tokio::time::timeout(Duration::from_secs(35), runner).await;
    let elapsed = started.elapsed();
    let tables = scratch_lash_table_count(database_url, &schema).await;
    holder.close().await.expect("release held migrator lock");
    drop_scratch_schema(database_url, &schema).await;
    drop(database);
    let result = result.expect("the server bounds advisory acquisition at 30 seconds");
    assert!(
        matches!(
            result,
            Err(MigrateError::Store(
                lash_core_execution::StoreError::Contended
            ))
        ),
        "a holder past the bound must return typed Contended: {result:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(30),
        "refused too early: {elapsed:?}"
    );
    assert_eq!(tables, 0, "a timed-out migrator changes no catalog objects");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_queued_migrator_leaves_no_lock_behind() {
    let Some(database) = migrator_database().await else {
        return;
    };
    let database_url = database.url();
    let schema = create_scratch_schema(database_url).await;
    let url = scratch_url(database_url, &schema);
    let mut holder = hold_migrator_lock(database_url).await;
    let mut runner = Box::pin(PostgresStorage::migrate(&url, MigrationPhase::Expand));
    tokio::select! {
        result = &mut runner => panic!("migrator must queue before cancellation: {result:?}"),
        () = observe_migrator_wait(&mut holder) => {},
    }
    drop(runner);
    holder.close().await.expect("release the migrator lock");
    let report = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("the next migrator acquires the lock and provisions the catalog");
    assert_eq!(report.executed.len(), 1 + component_expands().len());
    drop_scratch_schema(database_url, &schema).await;
    drop(database);
}
