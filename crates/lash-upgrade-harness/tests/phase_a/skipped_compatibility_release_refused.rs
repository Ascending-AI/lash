use anyhow::{Result, bail, ensure};
use lash_core_store::compat::{
    CompatDescriptor, CompatRefusal, CompatStamp, ComponentId, StampRead, VersionRange, admit,
};
use lash_core_store::store::StoreError;
use lash_postgres_store::{PostgresStorage, PostgresStoreConfig};
use lash_sqlite_store::{SqliteDatabase, Store};
use lash_upgrade_harness::harness::{Case, LASHCTL_N_ENV, NodeBuilds, Operator, Services};
use sqlx::PgPool;

fn skipped_component(component: ComponentId, stamp: CompatStamp) -> Result<()> {
    let descriptor = CompatDescriptor {
        component,
        reads: VersionRange::between(stamp.version + 1, stamp.version + 2),
        writes: VersionRange::exactly(stamp.version + 2),
    };
    match admit(&descriptor, StampRead::Present(stamp)) {
        Err(CompatRefusal::TooOld {
            component: refused,
            found,
            reads,
        }) => {
            ensure!(refused == component.as_str());
            ensure!(found == stamp.version);
            ensure!(reads.min() > found);
            Ok(())
        }
        other => bail!(
            "{} did not refuse a skipped component: {other:?}",
            component
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just phase-a` runs it"]
async fn skipped_compatibility_release_refused() -> Result<()> {
    // A database of the leg's own: its stamps and rows never meet another leg's.
    let services = Services::from_env()?.isolated("skipped").await?;
    let builds = NodeBuilds::from_env()?;
    let operator = Operator::from_env(&services, LASHCTL_N_ENV)?;
    let scratch = tempfile::tempdir()?;
    let postgres = Case::postgres("skipped-postgres", &services, scratch.path())?;
    operator.run("migrate", None)?;
    builds.n.probe(&postgres, None)?;
    // A compatibility build may open F=1; a build starting at F=2 must
    // refuse before it registers a deployment or handles a turn.
    builds.next.probe(&postgres, None)?;
    let pool = PgPool::connect(&services.postgres_url).await?;
    let skipped_f = VersionRange::between(2, 3);
    let error = PostgresStorage::from_pool_with_fleet_writable_range_for_testing(
        pool.clone(),
        PostgresStoreConfig::default(),
        skipped_f,
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("PostgreSQL opened below the writable F range"))?;
    ensure!(
        matches!(&error, StoreError::Incompatible { refusal: CompatRefusal::FleetOutsideWritable { recorded: 1, writable } } if *writable == skipped_f),
        "PostgreSQL returned a different refusal: {error}"
    );
    let (version, min_reader): (i32, i32) = sqlx::query_as(
        "SELECT version, min_reader FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .fetch_one(&pool)
    .await?;
    skipped_component(
        ComponentId::POSTGRES,
        CompatStamp {
            version: u32::try_from(version)?,
            min_reader: u32::try_from(min_reader)?,
        },
    )?;
    let fleet: i32 =
        sqlx::query_scalar("SELECT format_version FROM lash_fleet_format WHERE singleton = TRUE")
            .fetch_one(&pool)
            .await?;
    ensure!(fleet == 1, "refused PostgreSQL open changed F to {fleet}");
    pool.close().await;

    let sqlite = Case::sqlite("skipped-sqlite", &services, scratch.path())?;
    let root = scratch.path().join("skipped-sqlite/stores");
    builds.n.probe(&sqlite, None)?;
    let databases = [
        (SqliteDatabase::DurableCore, ComponentId::SQLITE_CORE),
        (
            SqliteDatabase::ProcessRegistry,
            ComponentId::SQLITE_REGISTRY,
        ),
        (SqliteDatabase::Triggers, ComponentId::SQLITE_TRIGGERS),
    ];
    for (database, component) in databases {
        let path = root.join(database.file_name());
        let connection = rusqlite::Connection::open(&path)?;
        let (version, min_reader, fleet): (i64, i64, i64) = connection.query_row(
            "SELECT version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(fleet == 1, "{} started at F={fleet}", database.name());
        skipped_component(
            component,
            CompatStamp {
                version: u32::try_from(version)?,
                min_reader: u32::try_from(min_reader)?,
            },
        )?;
    }
    let core = root.join(SqliteDatabase::DurableCore.file_name());
    let error = Store::open_with_fleet_writable_range_for_testing(&core, skipped_f)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("SQLite opened below the writable F range"))?;
    ensure!(
        matches!(&error, StoreError::Incompatible { refusal: CompatRefusal::FleetOutsideWritable { recorded: 1, writable } } if *writable == skipped_f),
        "SQLite returned a different refusal: {error}"
    );
    let fleet: i64 = rusqlite::Connection::open(&core)?.query_row(
        "SELECT fleet_format FROM lash_compat WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    ensure!(fleet == 1, "refused SQLite open changed F to {fleet}");
    Ok(())
}
