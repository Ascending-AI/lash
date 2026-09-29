//! `finalize_races_every_writer` (ADR 0115 §6): the writer fence against
//! N+1's finalize, on PostgreSQL and on SQLite (each of its databases).
//!
//! The writers are N: this leg's own process runs N's store library, the
//! default build, whose writable range is `[1,1]`. The lash node binary has
//! no entry that pauses a transaction, and the `AfterFence` seam is a
//! library fault point, so the races run in process rather than through
//! `lash-upgrade-node`. N+1's finalize is the synthetic one every leg uses
//! until `lashctl finalize` (FIG-3800 B): the fleet-format row read `FOR
//! UPDATE` and moved on PostgreSQL, every database's `lash_compat` row
//! rewritten under `BEGIN EXCLUSIVE` on SQLite.
//!
//! On each backend:
//!
//! 1. N pauses a session write right after its fence. N+1's finalize waits
//!    behind it, the paused writer commits under the old epoch, and then
//!    finalize commits.
//! 2. After finalize, a writer of every mutation class the store set's ports
//!    carry (§2.2) fails `WriterFenced`, and every table's row count is
//!    unchanged.
//! 3. On each backend, a commit a build that writes `[1,2]` encoded under
//!    the old epoch meets the finalized one at its fence and is encoded again
//!    under N+1's `F`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use lash_core::compat::VersionRange;
use lash_core::engine::BuildGeneration;
use lash_core::store::LeaseClaim;
use lash_core::{
    FleetFormat, FleetFormatStore as _, SessionCatalogStore as _, SessionCommitStore as _,
    SessionId, SessionMeta, SessionRelation, StoreError, StoreSet,
};
use lash_postgres_store::testing::IsolatedDatabase;
use lash_postgres_store::testing::{AfterFence, HeldFinalize};
use lash_postgres_store::{MigrationPhase, PostgresStorage, PostgresStoreConfig, PostgresStoreSet};
use lash_sqlite_store::testing::{SqliteFaultInjector, SqliteFaultPoint, finalize_fleet_format};
use lash_sqlite_store::{SqliteDatabase, SqliteStoreSet, SqliteStoreSetOptions};
use serde::Serialize;
use sqlx::PgPool;

use crate::support::ARTIFACT_DIR_ENV;

/// N's epoch, and the one N+1's finalize moves the fleet to.
const N_EPOCH: u32 = 1;
const NEXT_EPOCH: u32 = 2;

/// The three SQLite databases of one store.
const SQLITE_DATABASES: [SqliteDatabase; 3] = [
    SqliteDatabase::DurableCore,
    SqliteDatabase::ProcessRegistry,
    SqliteDatabase::Triggers,
];

/// Every row count of one store, by table.
type Rows = BTreeMap<String, i64>;

/// What a mutation class answered after finalize.
#[derive(Debug, Serialize)]
struct Refused {
    class: &'static str,
    error: String,
}

#[derive(Debug, Default, Serialize)]
struct BackendEvidence {
    finalize_waited_for_the_paused_writer: bool,
    refused_after_finalize: Vec<Refused>,
    rows_unchanged_by_refused_writers: bool,
    reencoded_commit_epoch: Option<u32>,
}

#[derive(Debug, Default, Serialize)]
struct Evidence {
    postgres: BackendEvidence,
    sqlite: BackendEvidence,
}

fn root_meta(session: &str) -> SessionMeta {
    SessionMeta {
        owning_process_id: None,
        session_id: SessionId::from(session),
        relation: SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

/// A store port's answer: refused `WriterFenced` at N+1's epoch, typed.
fn store_outcome<T>(outcome: Result<T, StoreError>) -> (Result<(), String>, bool) {
    match outcome {
        Ok(_) => (Ok(()), false),
        Err(error) => {
            let fenced = matches!(
                &error,
                StoreError::WriterFenced { recorded, writable }
                    if *recorded == NEXT_EPOCH && *writable == FleetFormat::writable()
            );
            (Err(error.to_string()), fenced)
        }
    }
}

fn plugin_outcome<T>(outcome: Result<T, lash_core::PluginError>) -> (Result<(), String>, bool) {
    match outcome {
        Ok(_) => (Ok(()), false),
        Err(error) => {
            let fenced = matches!(&error, lash_core::PluginError::StoreRefusal(
                lash_core::store::StoreRefusal::WriterFenced { recorded: NEXT_EPOCH, writable }
            ) if *writable == FleetFormat::writable());
            (Err(error.to_string()), fenced)
        }
    }
}

fn maintenance_outcome<T>(outcome: lash_core::MaintenanceResult<T>) -> (Result<(), String>, bool) {
    match outcome {
        Ok(_) => (Ok(()), false),
        Err(error) => match error.stop {
            lash_core::MaintenanceStop::Failed(error) => store_outcome::<()>(Err(error)),
            lash_core::MaintenanceStop::Refused(error) => (Err(error.to_string()), false),
        },
    }
}

/// One writer of each mutation class the store set's ports carry (ADR 0115
/// §2.2), after finalize: each must be refused, typed.
async fn every_writer_is_fenced(stores: &dyn StoreSet, label: &str) -> Result<Vec<Refused>> {
    let session = format!("{label}-after-finalize");
    let factory = stores.session_store_factory();
    let mut refused = Vec::new();
    let mut expect =
        |class: &'static str, (outcome, is_fenced): (Result<(), String>, bool)| -> Result<()> {
            match outcome {
                Ok(()) => bail!("{label}: {class} wrote after finalize"),
                Err(error) => {
                    ensure!(
                        is_fenced,
                        "{label}: {class} was refused, but not fenced: {error}"
                    );
                    refused.push(Refused { class, error });
                    Ok(())
                }
            }
        };

    let admitted = factory
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &SessionId::from(session.as_str()),
        ))
        .await;
    expect("session admission", store_outcome(admitted))?;

    let committed = factory.save_session_meta(root_meta(&session)).await;
    expect("session commit", store_outcome(committed))?;

    let input = factory
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            SessionId::from(session.as_str()),
            lash_core::TurnInputIngress::NextTurn,
            lash_core::TurnInput::text("after finalize"),
        ))
        .await;
    expect("turn input", store_outcome(input))?;

    let generation = BuildGeneration::from_digest([0xf1, 0x2a, 0, 0, 0, 1]);
    let marked = stores
        .generation_drain()
        .mark_draining(&generation, 1)
        .await;
    expect("drain mark", store_outcome(marked))?;

    let lease = stores
        .recovery_leader()
        .acquire(&LeaseClaim {
            name: lash_core::store::LeaseName::new("finalize-races"),
            holder: lash_core::store::HolderId::new("finalize-races-holder"),
            generation_rank: 1,
            ttl_ms: 60_000,
            min_tenure_ms: 0,
        })
        .await;
    expect("leader lease", store_outcome(lease))?;

    let pruned = stores
        .trigger_store()
        .prune_mutation_receipts(u64::MAX)
        .await;
    expect("trigger write", plugin_outcome(pruned))?;

    let process = lash_core::process_id_for_test("finalize-races");
    let released = stores
        .process_registry()
        .release_consumer_hold(&process, "finalize-races-hold")
        .await;
    expect("process registry write", plugin_outcome(released))?;

    let handovers = stores
        .process_continuations()
        .delete_segment_handovers(&process)
        .await;
    expect("process continuation write", plugin_outcome(handovers))?;

    let deleted = factory
        .delete_session(&SessionId::from(format!("{label}-delete").as_str()))
        .await;
    expect("session delete", maintenance_outcome(deleted))?;
    Ok(refused)
}

async fn postgres_rows(pool: &PgPool) -> Result<Rows> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = current_schema() AND tablename LIKE 'lash\\_%'
         ORDER BY tablename",
    )
    .fetch_all(pool)
    .await?;
    let mut rows = Rows::new();
    for table in tables {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await?;
        rows.insert(table, count);
    }
    Ok(rows)
}

fn sqlite_rows(root: &Path) -> Result<Rows> {
    let mut rows = Rows::new();
    for database in SQLITE_DATABASES {
        let connection = rusqlite::Connection::open(root.join(database.file_name()))?;
        let tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for table in tables {
            let count: i64 =
                connection.query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })?;
            rows.insert(format!("{}.{table}", database.name()), count);
        }
    }
    Ok(rows)
}

/// Waits until some backend of this database waits on a lock.
async fn until_postgres_lock_waiter(pool: &PgPool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await?;
            if waiting > 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("finalize never waited on the paused writer")?
}

async fn recorded_postgres_epoch(pool: &PgPool) -> Result<i32> {
    Ok(
        sqlx::query_scalar("SELECT format_version FROM lash_fleet_format WHERE singleton")
            .fetch_one(pool)
            .await?,
    )
}

async fn postgres_leg(url: &str, scratch: &Path) -> Result<BackendEvidence> {
    let mut evidence = BackendEvidence::default();
    PostgresStorage::migrate(url, MigrationPhase::Expand).await?;
    let seam = AfterFence::new();
    let storage = PostgresStorage::connect(url)
        .await?
        .with_after_fence_for_testing(seam.clone());
    let pool = storage.pool().clone();
    ensure!(
        recorded_postgres_epoch(&pool).await? == 1,
        "N opened at another epoch"
    );

    // 1. The paused writer is ordered before finalize.
    let mut pause = seam.pause_next();
    let store = storage.store();
    let writer =
        tokio::spawn(async move { store.save_session_meta(root_meta("pg-straddles")).await });
    ensure!(
        pause.reached().await == N_EPOCH,
        "the paused writer read another epoch"
    );
    let finalize = tokio::spawn({
        let pool = pool.clone();
        async move { HeldFinalize::begin(&pool, NEXT_EPOCH).await?.commit().await }
    });
    until_postgres_lock_waiter(&pool).await?;
    ensure!(
        !finalize.is_finished(),
        "PostgreSQL finalize did not wait for the paused writer"
    );
    ensure!(recorded_postgres_epoch(&pool).await? == 1);
    evidence.finalize_waited_for_the_paused_writer = true;
    pause.release();
    writer
        .await?
        .context("the paused PostgreSQL writer commits under N's epoch")?;
    finalize
        .await?
        .context("PostgreSQL finalize commits after the writer")?;
    ensure!(recorded_postgres_epoch(&pool).await? == 2);
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM lash_session_meta WHERE session_id = 'pg-straddles'",
    )
    .fetch_one(&pool)
    .await?;
    ensure!(kept == 1, "the straddling PostgreSQL writer's row is gone");

    // 2. Every writer after finalize is fenced and writes nothing.
    let attachments = scratch.join("postgres-attachments");
    std::fs::create_dir_all(&attachments)?;
    let stores = PostgresStoreSet::new(
        &storage,
        Arc::new(lash::persistence::FileAttachmentStore::new(attachments)),
    );
    let before = postgres_rows(&pool).await?;
    evidence.refused_after_finalize = every_writer_is_fenced(&stores, "pg").await?;
    let after = postgres_rows(&pool).await?;
    ensure!(
        before == after,
        "a fenced PostgreSQL writer changed rows: {before:?} -> {after:?}"
    );
    evidence.rows_unchanged_by_refused_writers = true;
    drop(stores);
    drop(storage);

    // 3. A pre-encoded commit straddling finalize is encoded again. A build
    // writing `[1,2]` opened before the move still encodes under epoch 1;
    // epoch 1 pins the receipt at a version no build writes, so the stored
    // receipt tells which epoch encoded it.
    sqlx::query("UPDATE lash_fleet_format SET format_version = 1 WHERE singleton")
        .execute(&pool)
        .await?;
    let next = PostgresStorage::from_pool_with_fleet_writable_range_for_testing(
        PgPool::connect(url).await?,
        PostgresStoreConfig::default(),
        VersionRange::between(N_EPOCH, NEXT_EPOCH),
    )
    .await?;
    const PINNED: u32 = 7;
    let store = next.store().with_fleet_format_for_testing(
        FleetFormat::from_version(N_EPOCH).with_writer_pins(&[lash_core::WriterPin {
            constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
            generation: N_EPOCH,
            version: PINNED,
        }]),
    );
    let session = SessionId::from("pg-reencoded");
    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &session,
        ))
        .await?;
    HeldFinalize::begin(&pool, NEXT_EPOCH)
        .await?
        .commit()
        .await?;
    let receipt = store
        .commit_runtime_state(persisted_commit(&session))
        .await
        .context("the straddling PostgreSQL commit re-encodes and lands")?;
    ensure!(
        receipt.schema_version != PINNED,
        "the receipt kept epoch 1's pin"
    );
    let stored: String = sqlx::query_scalar(
        "SELECT result_json FROM lash_runtime_turn_commits WHERE session_id = $1",
    )
    .bind(session.as_str())
    .fetch_one(&pool)
    .await?;
    let stored: serde_json::Value = serde_json::from_str(&stored)?;
    ensure!(
        stored["schema_version"] != serde_json::json!(PINNED),
        "the stored PostgreSQL receipt was encoded under epoch 1: {stored}"
    );
    let reencoded = store.fleet_format().version();
    ensure!(
        reencoded == NEXT_EPOCH,
        "the PostgreSQL commit landed under epoch {reencoded}"
    );
    evidence.reencoded_commit_epoch = Some(reencoded);
    next.pool().close().await;
    pool.close().await;
    Ok(evidence)
}

fn persisted_commit(session: &SessionId) -> lash_core::RuntimeCommit {
    let state = lash_core::RuntimeSessionState {
        session_id: session.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    lash_core::RuntimeCommit::persisted_state_for_test(&state, &[])
}

async fn sqlite_leg(scratch: &Path) -> Result<BackendEvidence> {
    let mut evidence = BackendEvidence::default();
    let root = scratch.join("sqlite-stores");
    std::fs::create_dir_all(&root)?;
    let injector = SqliteFaultInjector::default();
    let stores = SqliteStoreSet::open_with_options_and_clock(
        &root,
        SqliteStoreSetOptions {
            fault_injector: Some(injector.clone()),
            ..SqliteStoreSetOptions::default()
        },
        Arc::new(lash_core::facade_support::SystemClock),
    )
    .await?;
    let location = stores.location().clone();

    // 1. The paused writer is ordered before finalize, in the durable core
    // where the session writer runs; finalize holds every database.
    let pause = injector.pause(SqliteFaultPoint::AfterFence);
    let factory = stores.session_store_factory();
    let writer = tokio::spawn({
        let factory = Arc::clone(&factory);
        async move {
            factory
                .save_session_meta(root_meta("sqlite-straddles"))
                .await
        }
    });
    pause.wait_until_reached().await;
    let finalize = tokio::task::spawn_blocking({
        let location = location.clone();
        move || finalize_fleet_format(&location, NEXT_EPOCH)
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    ensure!(
        !finalize.is_finished(),
        "SQLite finalize did not wait for the paused writer"
    );
    evidence.finalize_waited_for_the_paused_writer = true;
    pause.release();
    writer
        .await?
        .context("the paused SQLite writer commits under N's epoch")?;
    finalize
        .await?
        .context("SQLite finalize commits after the writer")?;
    for database in SQLITE_DATABASES {
        let fleet: i64 = rusqlite::Connection::open(root.join(database.file_name()))?.query_row(
            "SELECT fleet_format FROM lash_compat WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            fleet == 2,
            "{} was not finalized: F={fleet}",
            database.name()
        );
    }

    // 2. Every writer after finalize, in each database, is fenced and writes
    // nothing.
    let before = sqlite_rows(&root)?;
    evidence.refused_after_finalize = every_writer_is_fenced(&stores, "sqlite").await?;
    let after = sqlite_rows(&root)?;
    ensure!(
        before == after,
        "a fenced SQLite writer changed rows: {before:?} -> {after:?}"
    );
    let kept: i64 = rusqlite::Connection::open(root.join(SqliteDatabase::DurableCore.file_name()))?
        .query_row(
            "SELECT count(*) FROM session_meta WHERE session_id = 'sqlite-straddles'",
            [],
            |row| row.get(0),
        )?;
    ensure!(kept == 1, "the straddling SQLite writer's row is gone");
    evidence.rows_unchanged_by_refused_writers = true;
    drop(factory);
    drop(stores);

    for database in SQLITE_DATABASES {
        rusqlite::Connection::open(root.join(database.file_name()))?.execute(
            "UPDATE lash_compat SET fleet_format = 1 WHERE singleton = 1",
            [],
        )?;
    }
    const PINNED: u32 = 7;
    let store = lash_sqlite_store::SqliteStore::open_with_fleet_writable_range_for_testing(
        &root.join(SqliteDatabase::DurableCore.file_name()),
        VersionRange::between(N_EPOCH, NEXT_EPOCH),
    )
    .await?
    .with_fleet_format_for_testing(FleetFormat::from_version(N_EPOCH).with_writer_pins(&[
        lash_core::WriterPin {
            constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
            generation: N_EPOCH,
            version: PINNED,
        },
    ]));
    let session = SessionId::from("sqlite-reencoded");
    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            &session,
        ))
        .await?;
    finalize_fleet_format(&location, NEXT_EPOCH)?;
    let receipt = store
        .commit_runtime_state(persisted_commit(&session))
        .await
        .context("the straddling SQLite commit re-encodes and lands")?;
    ensure!(
        receipt.schema_version != PINNED,
        "the receipt kept epoch 1's pin"
    );
    let stored: String =
        rusqlite::Connection::open(root.join(SqliteDatabase::DurableCore.file_name()))?.query_row(
            "SELECT result_json FROM runtime_turn_commits WHERE session_id = ?1",
            [session.as_str()],
            |row| row.get(0),
        )?;
    let stored: serde_json::Value = serde_json::from_str(&stored)?;
    ensure!(
        stored["schema_version"] != serde_json::json!(PINNED),
        "the stored SQLite receipt kept epoch 1's pin"
    );
    let reencoded = store.fleet_format().version();
    ensure!(
        reencoded == NEXT_EPOCH,
        "the SQLite commit landed under epoch {reencoded}"
    );
    evidence.reencoded_commit_epoch = Some(reencoded);

    Ok(evidence)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs PostgreSQL: `just phase-a` runs it"]
async fn finalize_races_every_writer() -> Result<()> {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .context("LASH_POSTGRES_DATABASE_URL is required")?;
    let database = IsolatedDatabase::create(&url).await;
    let temporary = tempfile::tempdir()?;
    let scratch = std::env::var_os(ARTIFACT_DIR_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf())
        .join("finalize_races_every_writer");
    std::fs::create_dir_all(&scratch)?;
    let evidence = Evidence {
        postgres: postgres_leg(database.url(), &scratch).await?,
        sqlite: sqlite_leg(&scratch).await?,
    };
    let path = scratch.join("finalize-races.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&evidence)?)
        .map_err(|error| anyhow!("write {}: {error}", path.display()))
}
