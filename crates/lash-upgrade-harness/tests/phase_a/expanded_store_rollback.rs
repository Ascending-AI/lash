use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use lash_core_store::compat::{CompatRefusal, ComponentId};
use lash_core_store::store::StoreSchemaVerdict;
use lash_sqlite_store::{SqliteDatabase, verify_schema_at};
use lash_upgrade_harness::harness::{
    Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, NodeBuilds, Operator, Services,
};
use lash_upgrade_harness::node::served_by;
use sqlx::{Connection, PgConnection};

async fn sqlite_refusal(path: &Path, database: SqliteDatabase) -> Result<CompatRefusal> {
    let report = verify_schema_at(path, database).await;
    match report.verdict {
        StoreSchemaVerdict::Refused { refusal } => Ok(refusal),
        other => bail!("{} did not refuse: {other:?}", database.name()),
    }
}

fn sqlite_path(root: &Path, database: SqliteDatabase) -> std::path::PathBuf {
    root.join(database.file_name())
}

fn sqlite_execute(path: &Path, sql: &str) -> Result<()> {
    rusqlite::Connection::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .execute_batch(sql)
        .with_context(|| format!("run SQLite fixture on {}", path.display()))
}

async fn postgres_execute(connection: &mut PgConnection, sql: &str) -> Result<()> {
    sqlx::raw_sql(sql).execute(connection).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn expanded_store_rollback() -> Result<()> {
    let services = Services::from_env()?;
    let builds = NodeBuilds::from_env()?;
    let operator_n = Operator::from_env(&services, LASHCTL_N_ENV)?;
    let operator_next = Operator::from_env(&services, LASHCTL_NEXT_ENV)?;
    let scratch = tempfile::tempdir()?;

    let postgres = Case::postgres("expanded-postgres", &services, scratch.path())?;
    operator_n.run("migrate", None)?;
    builds.n.probe(&postgres, None)?;
    let migration = operator_next.run("migrate", None)?;
    ensure!(
        migration["executed"].as_array().is_some_and(|steps| steps
            .iter()
            .any(|step| step["migration"] == "synthetic-next-expand")),
        "N+1 did not execute its PostgreSQL expand: {migration}"
    );
    let mut pg = PgConnection::connect(&services.postgres_url).await?;
    let expanded: (bool, bool, bool) = sqlx::query_as(
        "SELECT EXISTS (
             SELECT 1 FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name = 'lash_sessions'
               AND column_name = 'synthetic_next_note'
               AND is_nullable = 'YES'
         ), to_regclass('lash_synthetic_next') IS NOT NULL,
            to_regclass('idx_lash_synthetic_next_note') IS NOT NULL",
    )
    .fetch_one(&mut pg)
    .await?;
    ensure!(
        expanded == (true, true, true),
        "incomplete expand: {expanded:?}"
    );
    postgres_execute(&mut pg, "DROP INDEX idx_lash_synthetic_next_note").await?;
    ensure!(
        matches!(
            builds.next.probe_refusal(&postgres)?,
            CompatRefusal::ShapeRefused { .. }
        ),
        "N+1 admitted a component-2 stamp without its declared index"
    );
    postgres_execute(
        &mut pg,
        "CREATE INDEX idx_lash_synthetic_next_note ON lash_synthetic_next(note)",
    )
    .await?;
    let preflight = operator_n.run("preflight", None)?;
    ensure!(
        preflight["databases"][0]["verdict"] == "expanded",
        "N did not admit PostgreSQL as Expanded: {preflight}"
    );
    let session = postgres.session_id("rollback");
    let n = builds.n.serve(&postgres)?;
    let n_generation = n.ready().context("N ready")?.generation.clone();
    let written = builds
        .n
        .turn(&postgres, &session, "N writes after expand")?;
    ensure!(
        written.reply.as_deref() == Some(served_by(builds.n.label(), &n_generation).as_str()),
        "N did not write the turn: {written:?}"
    );
    n.stop()?;
    let read = builds.next.probe(&postgres, Some(&session))?;
    ensure!(
        read.session_present == Some(true),
        "N+1 did not read N's PostgreSQL row: {read:?}"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lash_sessions WHERE session_id = $1")
        .bind(&session)
        .fetch_one(&mut pg)
        .await?;
    ensure!(rows == 1, "N's PostgreSQL session row was not retained");

    let sqlite = Case::sqlite("expanded-sqlite", &services, scratch.path())?;
    let sqlite_root = scratch.path().join("expanded-sqlite/stores");
    builds.n.probe(&sqlite, None)?;
    builds.next.probe(&sqlite, None)?;
    let databases = [
        SqliteDatabase::DurableCore,
        SqliteDatabase::ProcessRegistry,
        SqliteDatabase::Triggers,
    ];
    for database in databases {
        let path = sqlite_path(&sqlite_root, database);
        let connection = rusqlite::Connection::open(&path)?;
        let stamp: (i64, i64, i64) = connection.query_row(
            "SELECT version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        ensure!(
            stamp == (2, 1, 1),
            "{} has stamp {stamp:?}",
            database.name()
        );
    }
    let session = sqlite.session_id("rollback");
    let n = builds.n.serve(&sqlite)?;
    let n_generation = n.ready().context("N SQLite ready")?.generation.clone();
    let written = builds.n.turn(&sqlite, &session, "N writes after expand")?;
    ensure!(
        written.reply.as_deref() == Some(served_by(builds.n.label(), &n_generation).as_str()),
        "N did not write the SQLite turn: {written:?}"
    );
    n.stop()?;
    let read = builds.next.probe(&sqlite, Some(&session))?;
    ensure!(
        read.session_present == Some(true),
        "N+1 did not read N's SQLite row: {read:?}"
    );
    let core = sqlite_path(&sqlite_root, SqliteDatabase::DurableCore);
    let rows: i64 = rusqlite::Connection::open(&core)?.query_row(
        "SELECT COUNT(*) FROM session_head WHERE session_id = ?1",
        [&session],
        |row| row.get(0),
    )?;
    ensure!(rows == 1, "N's SQLite session row was not retained");

    let postgres_unsafe = [
        (
            "not null without default",
            "ALTER TABLE lash_sessions ADD COLUMN synthetic_required TEXT NOT NULL DEFAULT 'seed'; ALTER TABLE lash_sessions ALTER COLUMN synthetic_required DROP DEFAULT",
            "ALTER TABLE lash_sessions DROP COLUMN synthetic_required",
        ),
        (
            "CHECK NOT VALID",
            "ALTER TABLE lash_sessions ADD CONSTRAINT synthetic_check CHECK (head_revision >= 0) NOT VALID",
            "ALTER TABLE lash_sessions DROP CONSTRAINT synthetic_check",
        ),
        (
            "UNIQUE",
            "ALTER TABLE lash_sessions ADD CONSTRAINT synthetic_unique UNIQUE (head_revision)",
            "ALTER TABLE lash_sessions DROP CONSTRAINT synthetic_unique",
        ),
        (
            "FOREIGN KEY NOT VALID",
            "ALTER TABLE lash_sessions ADD CONSTRAINT synthetic_fk FOREIGN KEY (leaf_node_id) REFERENCES lash_blobs(hash) NOT VALID",
            "ALTER TABLE lash_sessions DROP CONSTRAINT synthetic_fk",
        ),
        (
            "EXCLUDE",
            "ALTER TABLE lash_sessions ADD CONSTRAINT synthetic_exclude EXCLUDE USING gist (int8range(head_revision, head_revision + 1) WITH &&)",
            "ALTER TABLE lash_sessions DROP CONSTRAINT synthetic_exclude",
        ),
        (
            "trigger",
            "CREATE FUNCTION synthetic_reject() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; CREATE TRIGGER synthetic_trigger BEFORE INSERT ON lash_sessions FOR EACH ROW EXECUTE FUNCTION synthetic_reject()",
            "DROP TRIGGER synthetic_trigger ON lash_sessions; DROP FUNCTION synthetic_reject()",
        ),
    ];
    for (name, addition, cleanup) in postgres_unsafe {
        postgres_execute(&mut pg, addition)
            .await
            .with_context(|| name.to_string())?;
        ensure!(
            matches!(
                builds.n.probe_refusal(&postgres)?,
                CompatRefusal::ShapeRefused { .. }
            ),
            "PostgreSQL admitted unsafe {name}"
        );
        postgres_execute(&mut pg, cleanup).await?;
    }

    let trigger_db = sqlite_path(&sqlite_root, SqliteDatabase::Triggers);
    let sqlite_unsafe = [
        (
            "not null without default",
            "ALTER TABLE trigger_subscriptions ADD COLUMN synthetic_required TEXT NOT NULL",
            "ALTER TABLE trigger_subscriptions DROP COLUMN synthetic_required",
        ),
        (
            "CHECK",
            "ALTER TABLE trigger_subscriptions ADD COLUMN synthetic_checked TEXT CHECK (synthetic_checked <> 'blocked')",
            "ALTER TABLE trigger_subscriptions DROP COLUMN synthetic_checked",
        ),
        (
            "UNIQUE",
            "CREATE UNIQUE INDEX synthetic_unique ON trigger_subscriptions(owner_scope)",
            "DROP INDEX synthetic_unique",
        ),
        (
            "FOREIGN KEY",
            "ALTER TABLE trigger_subscriptions ADD COLUMN synthetic_ref TEXT REFERENCES trigger_subscriptions(subscription_id)",
            "ALTER TABLE trigger_subscriptions DROP COLUMN synthetic_ref",
        ),
        (
            "trigger",
            "CREATE TRIGGER synthetic_trigger BEFORE INSERT ON trigger_subscriptions BEGIN SELECT RAISE(ABORT, 'unsafe'); END",
            "DROP TRIGGER synthetic_trigger",
        ),
    ];
    for (name, addition, cleanup) in sqlite_unsafe {
        sqlite_execute(&trigger_db, addition).with_context(|| name.to_string())?;
        ensure!(
            matches!(
                sqlite_refusal(&trigger_db, SqliteDatabase::Triggers).await?,
                CompatRefusal::ShapeRefused { .. }
            ),
            "SQLite admitted unsafe {name}"
        );
        ensure!(
            matches!(
                builds.n.probe_refusal(&sqlite)?,
                CompatRefusal::ShapeRefused { .. }
            ),
            "N SQLite process admitted unsafe {name}"
        );
        sqlite_execute(&trigger_db, cleanup)?;
    }

    postgres_execute(
        &mut pg,
        "UPDATE lash_schema_versions SET min_reader = 2 WHERE component = 'lash-postgres-store'",
    )
    .await?;
    ensure!(
        matches!(
            builds.n.probe_refusal(&postgres)?,
            CompatRefusal::ReaderFloorAbove { min_reader: 2, .. }
        ),
        "N admitted PostgreSQL above its reader floor"
    );
    for database in databases {
        let path = sqlite_path(&sqlite_root, database);
        sqlite_execute(&path, "UPDATE lash_compat SET min_reader = 2")?;
        ensure!(
            matches!(
                sqlite_refusal(&path, database).await?,
                CompatRefusal::ReaderFloorAbove { min_reader: 2, .. }
            ),
            "N admitted {} above its reader floor",
            database.name()
        );
        sqlite_execute(&path, "UPDATE lash_compat SET min_reader = 1")?;
    }
    for database in databases {
        sqlite_execute(
            &sqlite_path(&sqlite_root, database),
            "UPDATE lash_compat SET min_reader = 2",
        )?;
    }
    ensure!(
        matches!(
            builds.n.probe_refusal(&sqlite)?,
            CompatRefusal::ReaderFloorAbove { min_reader: 2, .. }
        ),
        "N SQLite process admitted the raised reader floor"
    );
    for database in databases {
        sqlite_execute(
            &sqlite_path(&sqlite_root, database),
            "UPDATE lash_compat SET min_reader = 1",
        )?;
    }

    postgres_execute(
        &mut pg,
        "DELETE FROM lash_schema_versions WHERE component = 'lash-postgres-store'",
    )
    .await?;
    ensure!(
        matches!(
            builds.n.probe_refusal(&postgres)?,
            CompatRefusal::Unstamped { .. }
        ),
        "N admitted populated PostgreSQL without a stamp"
    );
    for database in databases {
        let path = sqlite_path(&sqlite_root, database);
        sqlite_execute(&path, "DELETE FROM lash_compat")?;
        ensure!(
            matches!(
                sqlite_refusal(&path, database).await?,
                CompatRefusal::Unstamped { .. }
            ),
            "N admitted populated {} without a stamp",
            database.name()
        );
        ensure!(
            matches!(
                builds.n.probe_refusal(&sqlite)?,
                CompatRefusal::Unstamped { .. }
            ),
            "N SQLite process admitted populated {} without a stamp",
            database.name()
        );
        let component = match database {
            SqliteDatabase::DurableCore => ComponentId::SQLITE_CORE,
            SqliteDatabase::ProcessRegistry => ComponentId::SQLITE_REGISTRY,
            SqliteDatabase::Triggers => ComponentId::SQLITE_TRIGGERS,
        };
        rusqlite::Connection::open(&path)?.execute(
            "INSERT INTO lash_compat (singleton, component, version, min_reader, fleet_format) VALUES (1, ?1, 2, 1, 1)",
            [component.as_str()],
        )?;
    }
    pg.close().await?;
    Ok(())
}
