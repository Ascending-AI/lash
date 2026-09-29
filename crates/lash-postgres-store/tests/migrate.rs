//! `lash migrate` integration proofs (FIG-3816).
//!
//! The runner is the operational step that owns what worker open deliberately
//! refuses to: provisioning a fresh database and carrying a stamped catalog
//! forward. Each case runs against a scratch schema, so nothing here touches
//! the shared test database's catalog.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use lash_postgres_store::{MigrationPhase, PostgresStorage};
use sqlx::{Connection, PgConnection, Row};

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

async fn stamped_version(url: &str) -> i32 {
    let mut connection = PgConnection::connect(url)
        .await
        .expect("connect to read the component stamp");
    let version = sqlx::query_scalar::<_, i32>(
        "SELECT version FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&mut connection)
    .await
    .expect("read the component stamp");
    connection.close().await.expect("close stamp read");
    version
}

/// Provisions a scratch schema from the committed artifact, then rewinds it
/// to stand in for the previous component: the stamp steps back one and the
/// ledger records the predecessor's own bootstrap, the row a catalog that was
/// provisioned at that generation carries. The objects the newest generation
/// adds stay: every expand step is idempotent, so a catalog that already has
/// the step's output proves the same thing — what the predecessor lacks is
/// the stamp, not the bytes.
async fn rewind_to_previous_component(database_url: &str, schema: &str) {
    let predecessor = PostgresStorage::schema_version() - 1;
    let mut admin = PgConnection::connect(database_url)
        .await
        .expect("connect scratch provisioner");
    sqlx::query(&format!("SET search_path TO {schema}"))
        .execute(&mut admin)
        .await
        .expect("point the provisioner at the scratch schema");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut admin)
        .await
        .expect("provision the scratch schema from schema.sql");
    sqlx::query(
        "INSERT INTO lash_migrations (phase, migration, release, state,
                                      from_version, to_version, started_at_ms)
         VALUES ('expand', $1, 'predecessor', 'applied', NULL, $2, 0)",
    )
    .bind(format!("bootstrap-{predecessor}"))
    .bind(predecessor)
    .execute(&mut admin)
    .await
    .expect("record the predecessor's bootstrap in the ledger");
    sqlx::query("UPDATE lash_schema_versions SET version = $1 WHERE component = $2")
        .bind(predecessor)
        .bind("lash-postgres-store")
        .execute(&mut admin)
        .await
        .expect("stamp the scratch schema at the predecessor component");
    admin.close().await.expect("close scratch provisioner");
}

/// Provisions a scratch schema from the committed artifact, then stamps it at
/// component `version`: a catalog that build provisioned, as far as the
/// version gate and the migrate planner can tell.
async fn stamp_component(database_url: &str, schema: &str, version: i32) {
    let mut admin = PgConnection::connect(database_url)
        .await
        .expect("connect scratch provisioner");
    sqlx::query(&format!("SET search_path TO {schema}"))
        .execute(&mut admin)
        .await
        .expect("point the provisioner at the scratch schema");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut admin)
        .await
        .expect("provision the scratch schema from schema.sql");
    sqlx::query("UPDATE lash_schema_versions SET version = $1 WHERE component = $2")
        .bind(version)
        .bind("lash-postgres-store")
        .execute(&mut admin)
        .await
        .expect("stamp the scratch schema at the older component");
    admin.close().await.expect("close scratch provisioner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_on_a_fresh_schema_creates_the_schema_and_the_ledger() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    let report = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("migrate a fresh schema");

    assert_eq!(report.namespace.as_deref(), Some(schema.as_str()));
    assert_eq!(report.found_version, None, "a fresh schema has no stamp");
    assert!(report.applied.is_empty(), "a fresh ledger has no rows");
    assert_eq!(
        report.executed.len(),
        1,
        "a fresh migrate applies exactly the bootstrap: {report:?}"
    );
    assert_eq!(
        report.executed[0].migration,
        format!("bootstrap-{}", PostgresStorage::schema_version())
    );
    assert!(report.executed[0].started_at_ms.is_some());
    assert_eq!(report.executed[0].state, "applied");
    assert!(report.planned.is_empty(), "nothing remains pending");

    let ledger = ledger_rows(&url).await;
    assert_eq!(
        ledger,
        vec![(
            "expand".to_string(),
            format!("bootstrap-{}", PostgresStorage::schema_version()),
            None,
            PostgresStorage::schema_version()
        )],
        "the ledger records the bootstrap"
    );
    assert_eq!(
        stamped_version(&url).await,
        PostgresStorage::schema_version()
    );

    // And the provisioned catalog is openable: the gate a worker takes passes.
    PostgresStorage::connect(&url)
        .await
        .expect("a migrated fresh schema must open")
        .pool()
        .close()
        .await;
    drop_scratch_schema(&database_url, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_migrate_rerun_is_a_no_op() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
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
    assert_eq!(rerun.found_version, Some(PostgresStorage::schema_version()));
    assert_eq!(
        rerun.applied.len(),
        1,
        "the ledger still records the first run's step"
    );
    assert_eq!(
        ledger_rows(&url).await.len(),
        1,
        "a rerun writes no new ledger rows"
    );
    drop_scratch_schema(&database_url, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_reports_the_plan_and_changes_nothing() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    let plan = PostgresStorage::plan_migrations(&url, MigrationPhase::Expand)
        .await
        .expect("plan a fresh migrate");
    assert_eq!(plan.planned.len(), 1, "the plan is the bootstrap");
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_advances_a_stamped_predecessor_component() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    rewind_to_previous_component(&database_url, &schema).await;
    let predecessor = PostgresStorage::schema_version() - 1;

    // The predecessor catalog is below the open gate's supported range: a
    // worker must refuse it, which is why migrate exists.
    let refused = PostgresStorage::connect(&url).await;
    assert!(
        refused.is_err(),
        "a predecessor-stamped catalog must not open directly"
    );

    // The plan's answer is the catalog's: whatever step the build declares
    // carries the predecessor to this build's component.
    let ledger_before = ledger_rows(&url).await;
    let tables_before = scratch_lash_table_count(&database_url, &schema).await;
    let plan = PostgresStorage::plan_migrations(&url, MigrationPhase::Expand)
        .await
        .expect("plan the predecessor upgrade");
    assert_eq!(plan.found_version, Some(predecessor));
    assert_eq!(
        plan.applied.len(),
        1,
        "the predecessor's own ledger row is read back: {plan:?}"
    );
    assert_eq!(
        plan.applied[0].migration,
        format!("bootstrap-{predecessor}")
    );
    assert_eq!(
        plan.planned.len(),
        1,
        "a stamped predecessor is exactly one expand step behind: {plan:?}"
    );
    let step = &plan.planned[0];
    assert_eq!(step.phase, MigrationPhase::Expand.name());
    assert_eq!(step.from_version, Some(predecessor));
    assert_eq!(step.to_version, PostgresStorage::schema_version());
    // Planning changed nothing: the stamp is still the predecessor's, the
    // ledger rows are exactly what the rewind left, and no DDL ran.
    assert_eq!(stamped_version(&url).await, predecessor);
    assert_eq!(
        ledger_rows(&url).await,
        ledger_before,
        "a dry run must not write the ledger"
    );
    assert_eq!(
        scratch_lash_table_count(&database_url, &schema).await,
        tables_before,
        "a dry run must not create tables"
    );

    let report = PostgresStorage::migrate(&url, MigrationPhase::Expand)
        .await
        .expect("migrate the predecessor forward");
    assert_eq!(report.executed.len(), 1);
    assert_eq!(
        report.executed[0].migration, step.migration,
        "migrate executes the step the plan named"
    );
    assert_eq!(
        ledger_rows(&url).await,
        vec![
            ledger_before[0].clone(),
            (
                "expand".to_string(),
                step.migration.clone(),
                Some(predecessor),
                PostgresStorage::schema_version()
            ),
        ],
        "the ledger keeps the predecessor's row and records the step"
    );
    assert_eq!(
        stamped_version(&url).await,
        PostgresStorage::schema_version()
    );
    PostgresStorage::connect(&url)
        .await
        .expect("a migrated predecessor catalog must open")
        .pool()
        .close()
        .await;
    drop_scratch_schema(&database_url, &schema).await;
}

/// Component 139 (FIG-3607) re-keys the process relations, which no expand
/// step can do, so the expand catalog has no step from 138: a catalog stamped
/// 138 is a recreate boundary. Planning and migrating it refuse with the typed
/// range error, and the refused run writes nothing — no ledger row, no stamp
/// move.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_component_without_an_expand_step_is_refused() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);
    let found = 138;
    stamp_component(&database_url, &schema, found).await;

    assert!(
        PostgresStorage::connect(&url).await.is_err(),
        "a catalog stamped 138 must not open directly"
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
        let rendered = refused
            .expect_err("a catalog with no expand step must refuse")
            .to_string();
        assert!(
            rendered.contains(&format!("has version {found}")),
            "the {what} refusal names the found version: {rendered}"
        );
        assert!(
            rendered.contains("has no applicable migration"),
            "the {what} refusal names the missing migration path: {rendered}"
        );
    }
    assert_eq!(stamped_version(&url).await, found);
    assert!(
        ledger_rows(&url).await.is_empty(),
        "a refused migrate records no step"
    );
    drop_scratch_schema(&database_url, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_phases_refuse_before_the_operations_arc() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let schema = create_scratch_schema(&database_url).await;
    let url = scratch_url(&database_url, &schema);

    for phase in [MigrationPhase::Backfill, MigrationPhase::Contract] {
        let migrate_error = PostgresStorage::migrate(&url, phase)
            .await
            .err()
            .unwrap_or_else(|| panic!("migrate --phase {} must refuse", phase.name()));
        let plan_error = PostgresStorage::plan_migrations(&url, phase)
            .await
            .err()
            .unwrap_or_else(|| panic!("plan_migrations --phase {} must refuse", phase.name()));
        for (label, error) in [("migrate", migrate_error), ("plan_migrations", plan_error)] {
            assert!(
                error
                    .to_string()
                    .contains("not supported before the operations arc (FIG-3817)"),
                "{label} --phase {} must name the operations-arc refusal: {error}",
                phase.name()
            );
        }
    }
    // And the refusal ran no DDL.
    assert_eq!(scratch_lash_table_count(&database_url, &schema).await, 0);
    drop_scratch_schema(&database_url, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_migrates_serialize_and_converge() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
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
        executed_total, 1,
        "exactly one racer may execute the bootstrap: {first:?} {second:?}"
    );
    assert_eq!(ledger_rows(&url).await.len(), 1);
    drop_scratch_schema(&database_url, &schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_catalog_below_the_migration_floor_is_refused_not_recreated() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping migrate proof: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
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
}
