//! Test-only fixtures for running Postgres suites in parallel.
//!
//! The workspace shares one configured Postgres database
//! (`LASH_POSTGRES_DATABASE_URL`). Suites that truncate or enumerate every
//! `lash_*` table therefore observe rows written by any concurrently running
//! suite, which under a process-per-test runner shows up as rotating,
//! irreproducible failures. Suites that hold the shared advisory lock take
//! turns; suites that do not need their own database instead of a turn.
//!
//! [`IsolatedDatabase`] gives a suite a uniquely named, freshly created
//! database derived from the configured URL, and retires it on teardown.
//! Creation provisions this build's committed `schema.sql` artifact in a fresh
//! or recycled empty database shell, so a following
//! [`PostgresStorage`](crate::PostgresStorage) open verifies a schema it did
//! not create (FIG-3797).

use sqlx::{Connection, PgConnection};

#[path = "testing/database_url.rs"]
mod database_url;
pub use database_url::required_database_url;

/// The nodes a fixture may serve over one storage: a failover deployment's
/// primary and standby, a drain law's nodes.
pub const FIXTURE_SERVED_NODES: u32 = 4;

/// The attachment sweeps a fixture may hold open at once over one storage:
/// the recovery laws race sweepers and leave a crashed one's session held.
pub const FIXTURE_SWEEP_SESSIONS: u32 = 4;

/// The default [`PostgresHostConfig`](crate::PostgresHostConfig), serving
/// [`FIXTURE_SERVED_NODES`] nodes and [`FIXTURE_SWEEP_SESSIONS`] sweeps: the
/// configuration of laws that do not exercise it.
pub fn fixture_config() -> crate::PostgresHostConfig {
    let mut config = crate::PostgresHostConfig::default();
    config.roles.served_nodes = FIXTURE_SERVED_NODES;
    config.maintenance.max_sweep_sessions = FIXTURE_SWEEP_SESSIONS;
    config
}

/// Open storage over `database_url` under [`fixture_config`]: the fixture
/// open of laws that do not exercise the host configuration. A refusal that
/// is not the store's own is reported as a backend error.
pub async fn connect(database_url: &str) -> Result<crate::PostgresStorage, crate::StoreError> {
    connect_with(database_url, &fixture_config()).await
}

/// [`fixture_config`] with a work pool of `max_connections`, its admission
/// narrowed to fit: the small-pool laws' configuration.
pub fn work_pool_of(max_connections: u32) -> crate::PostgresHostConfig {
    let mut config = fixture_config();
    config.roles.work.max_connections = max_connections;
    config.roles.max_store_operations = max_connections as usize;
    config
}

/// [`connect`] under `config`.
pub async fn connect_with(
    database_url: &str,
    config: &crate::PostgresHostConfig,
) -> Result<crate::PostgresStorage, crate::StoreError> {
    let endpoints = crate::PostgresEndpoints::from_url(database_url)
        .map_err(|error| crate::StoreError::Backend(error.to_string()))?;
    crate::PostgresStorage::connect(&endpoints, config, Default::default())
        .await
        .map_err(store_error)
}

/// Open storage over a host-built `pool` that every role shares, under
/// `config` with its admission narrowed to the pool: the fixture open of
/// laws that hook their pool (a scratch `search_path`, an observed
/// `application_name`).
pub async fn from_pool(
    pool: sqlx::PgPool,
    config: &crate::PostgresHostConfig,
) -> Result<crate::PostgresStorage, crate::StoreError> {
    let config = fitted(&pool, config);
    crate::PostgresStorage::from_pool_set(
        crate::PostgresPoolSet::sharing_for_testing(pool),
        &config,
        Default::default(),
    )
    .await
    .map_err(store_error)
}

/// [`from_pool`] as a build whose writable range is `writable`.
#[cfg(feature = "testing")]
pub async fn from_pool_as(
    pool: sqlx::PgPool,
    config: &crate::PostgresHostConfig,
    writable: lash_core_execution::compat::VersionRange,
) -> Result<crate::PostgresStorage, crate::StoreError> {
    let config = fitted(&pool, config);
    crate::PostgresStorage::from_pool_set_with_fleet_writable_range_for_testing(
        crate::PostgresPoolSet::sharing_for_testing(pool),
        &config,
        writable,
    )
    .await
    .map_err(store_error)
}

/// `config` with its admission narrowed to fit `pool`.
fn fitted(pool: &sqlx::PgPool, config: &crate::PostgresHostConfig) -> crate::PostgresHostConfig {
    let mut config = config.clone();
    config.roles.max_store_operations = config
        .roles
        .max_store_operations
        .min(pool.options().get_max_connections() as usize);
    config
}

/// [`PostgresStorage::migrate`](crate::PostgresStorage::migrate) of
/// `database_url` under the default configuration.
pub async fn migrate(
    database_url: &str,
    phase: crate::MigrationPhase,
) -> Result<crate::MigrationReport, crate::MigrateError> {
    let endpoints = crate::PostgresEndpoints::from_url(database_url).map_err(|error| {
        crate::MigrateError::Store(crate::StoreError::Backend(error.to_string()))
    })?;
    crate::PostgresStorage::migrate(&endpoints, &crate::PostgresHostConfig::default(), phase).await
}

/// [`PostgresStorage::plan_migrations`](crate::PostgresStorage::plan_migrations)
/// of `database_url` under the default configuration.
pub async fn plan_migrations(
    database_url: &str,
    phase: crate::MigrationPhase,
) -> Result<crate::MigrationReport, crate::MigrateError> {
    let endpoints = crate::PostgresEndpoints::from_url(database_url).map_err(|error| {
        crate::MigrateError::Store(crate::StoreError::Backend(error.to_string()))
    })?;
    crate::PostgresStorage::plan_migrations(
        &endpoints,
        &crate::PostgresHostConfig::default(),
        phase,
    )
    .await
}

/// A host open's refusal as the store error a fixture propagates: the
/// store's own as it is, any other as a backend error naming it.
pub fn store_error(error: crate::PostgresHostError) -> crate::StoreError {
    match error {
        crate::PostgresHostError::Store(error) => error,
        other => crate::StoreError::Backend(other.to_string()),
    }
}

/// Arm (`true`) or disarm a cut of every session-mail producer transaction
/// at the session actor's wake, over `pool`: the producer's transaction
/// rolls back before it commits.
///
/// # Errors
///
/// The trigger's DDL failed.
pub async fn cut_session_wakes(pool: &sqlx::PgPool, armed: bool) -> sqlx::Result<()> {
    crate::durable::cut_session_wakes(pool, armed).await
}

/// Returns the production trigger-subscription listing SQL for conformance assertions.
///
/// The filter no longer builds the statement; it selects one (FIG-3385). The
/// text is the named statement its shape is served by, which is what the
/// listing actually issues.
pub fn trigger_subscription_list_sql(
    filter: &lash_core_execution::TriggerSubscriptionFilter,
) -> String {
    crate::trigger_store::subscription_list_sql(filter).to_string()
}

/// One stored value: where it is, its bytes, and every JSON document those
/// bytes decode to as this store writes them. The twin of
/// `lash_sqlite_store::testing::StoredCell`.
#[derive(Clone, Debug)]
pub struct StoredCell {
    /// `<table>.<column>#<row>`, or `schema/<table>.<column>` for a column's
    /// own declaration.
    pub location: String,
    pub bytes: Vec<u8>,
    /// The value as JSON text (a `json`/`jsonb` column's included) or as a
    /// msgpack record; empty for a scalar.
    pub documents: Vec<serde_json::Value>,
}

/// Every column declaration and every non-null cell of every table in the
/// storage's schema, with the documents each decodes to.
///
/// An inspection hook for simulation checkers that audit what a finished
/// run persisted (lash-sim's crash-matrix catalog audit, FIG-4179). It never
/// writes, and no lash component reads through it.
pub async fn read_stored_cells_for_testing(
    storage: &crate::PostgresStorage,
) -> Result<Vec<StoredCell>, String> {
    use sqlx::Row as _;
    let columns = sqlx::query(
        "SELECT table_name::text AS table_name, column_name::text AS column_name, \
                data_type::text AS data_type \
         FROM information_schema.columns \
         WHERE table_schema = current_schema() \
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(storage.pool())
    .await
    .map_err(|error| format!("list the columns: {error}"))?;
    let mut cells = Vec::new();
    for column in columns {
        let table: String = column.get("table_name");
        let name: String = column.get("column_name");
        let data_type: String = column.get("data_type");
        cells.push(StoredCell {
            location: format!("schema/{table}.{name}"),
            bytes: format!("{table}.{name}").into_bytes(),
            documents: Vec::new(),
        });
        let values: Vec<Vec<u8>> = if data_type == "bytea" {
            sqlx::query_scalar(&format!(
                "SELECT \"{name}\" FROM \"{table}\" WHERE \"{name}\" IS NOT NULL"
            ))
            .fetch_all(storage.pool())
            .await
        } else {
            sqlx::query_scalar::<_, String>(&format!(
                "SELECT \"{name}\"::text FROM \"{table}\" WHERE \"{name}\" IS NOT NULL"
            ))
            .fetch_all(storage.pool())
            .await
            .map(|values| values.into_iter().map(String::into_bytes).collect())
        }
        .map_err(|error| format!("read `{table}.{name}`: {error}"))?;
        for (index, bytes) in values.into_iter().enumerate() {
            let structured =
                |value: serde_json::Value| (value.is_object() || value.is_array()).then_some(value);
            let documents = serde_json::from_slice(&bytes)
                .ok()
                .and_then(structured)
                .or_else(|| {
                    rmp_serde::from_slice::<serde_json::Value>(&bytes)
                        .ok()
                        .and_then(structured)
                })
                .into_iter()
                .collect();
            cells.push(StoredCell {
                location: format!("{table}.{name}#{index}"),
                bytes,
                documents,
            });
        }
    }
    Ok(cells)
}

/// The `AfterFence` seam (ADR 0115 §6): pauses a guarded transaction right
/// after its writer fence, while it holds the fence lock shared.
///
/// Install it on a storage with
/// [`PostgresStorage::with_after_fence_for_testing`](crate::PostgresStorage::with_after_fence_for_testing);
/// every handle that storage hands out passes it. Each [`Self::pause_next`]
/// arms one pause, taken by the next transaction whose fence admits `F`, in
/// arming order. It also counts fences that met lock contention, so a test can
/// tell a retried fence from a first one.
#[derive(Clone, Debug, Default)]
pub struct AfterFence {
    state: std::sync::Arc<std::sync::Mutex<AfterFenceState>>,
}

#[derive(Debug, Default)]
struct AfterFenceState {
    armed: std::collections::VecDeque<ArmedPause>,
    passed: Vec<u32>,
    contended: u64,
}

#[derive(Debug)]
struct ArmedPause {
    reached: tokio::sync::oneshot::Sender<u32>,
    gate: std::sync::Arc<lash_core_execution::testing::Gate>,
}

/// One armed pause: the transaction that takes it waits after its fence
/// until [`Self::release`] (or until this handle is dropped).
#[derive(Debug)]
pub struct FencePause {
    reached: Option<tokio::sync::oneshot::Receiver<u32>>,
    gate: std::sync::Arc<lash_core_execution::testing::Gate>,
}

impl AfterFence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pause the next guarded transaction that passes its fence.
    pub fn pause_next(&self) -> FencePause {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let gate = std::sync::Arc::new(lash_core_execution::testing::Gate::new(
            "postgres writer fence",
        ));
        self.lock_state().armed.push_back(ArmedPause {
            reached: reached_tx,
            gate: std::sync::Arc::clone(&gate),
        });
        FencePause {
            reached: Some(reached_rx),
            gate,
        }
    }

    /// The epoch each fence that admitted its transaction read, in order.
    pub fn passed(&self) -> Vec<u32> {
        self.lock_state().passed.clone()
    }

    /// How many fences failed on lock contention.
    pub fn contended(&self) -> u64 {
        self.lock_state().contended
    }

    pub(crate) fn record_contended(&self) {
        self.lock_state().contended += 1;
    }

    /// Called by a fence that admitted `recorded`: takes the next armed pause,
    /// if any, and waits for its release.
    pub(crate) async fn pass(&self, recorded: u32) {
        let armed = {
            let mut state = self.lock_state();
            state.passed.push(recorded);
            state.armed.pop_front()
        };
        if let Some(armed) = armed {
            let _ = armed.reached.send(recorded);
            armed.gate.pass().await;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AfterFenceState> {
        use lash_sansio::sync::MutexExt;
        self.state.lock_recover()
    }
}

impl FencePause {
    /// Wait until a transaction has taken this pause; answers the `F` its
    /// fence read.
    ///
    /// # Panics
    ///
    /// Panics when called twice, or when the seam was dropped unreached.
    #[expect(
        clippy::expect_used,
        reason = "test-harness helper: a pause awaited twice or never reachable is a test-authoring fault"
    )]
    pub async fn reached(&mut self) -> u32 {
        self.gate.reached(1).await;
        self.reached
            .take()
            .expect("a pause is awaited once")
            .await
            .expect("the seam holding this pause was dropped before a fence reached it")
    }

    /// Let the paused transaction continue.
    pub fn release(self) {
        self.gate.open_all();
    }
}

impl Drop for FencePause {
    fn drop(&mut self) {
        self.gate.open_all();
    }
}

/// The `BeforeTurnCommit` seam: pauses a transaction that wrote a change of
/// the turn feed (a receipt, a session's fault or deletion) right before its
/// `COMMIT`, every statement run: a deliberately late committer.
///
/// Install it on a storage with
/// [`PostgresStorage::with_before_turn_commit_for_testing`](crate::PostgresStorage::with_before_turn_commit_for_testing).
/// Each [`Self::pause_next`] arms one pause, taken by the next such
/// transaction, in arming order; one that finds none armed passes.
#[derive(Clone, Debug, Default)]
pub struct BeforeTurnCommit {
    armed: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<TurnCommitPause>>>,
}

/// One armed [`BeforeTurnCommit`] pause: the commit that takes it waits until
/// [`Self::release`], or until this handle is dropped.
#[derive(Clone, Debug)]
pub struct TurnCommitPause {
    gate: std::sync::Arc<lash_core_execution::testing::Gate>,
}

impl BeforeTurnCommit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pause the next transaction that commits a turn change.
    pub fn pause_next(&self) -> TurnCommitPause {
        use lash_sansio::sync::MutexExt;
        let pause = TurnCommitPause {
            gate: std::sync::Arc::new(lash_core_execution::testing::Gate::new(
                "postgres turn commit",
            )),
        };
        self.armed.lock_recover().push_back(pause.clone());
        pause
    }

    /// Called by a transaction that wrote a turn change: takes the next armed
    /// pause, if any, and waits for its release.
    pub(crate) async fn pass(&self) {
        use lash_sansio::sync::MutexExt;
        let armed = self.armed.lock_recover().pop_front();
        if let Some(armed) = armed {
            armed.gate.pass().await;
        }
    }
}

impl TurnCommitPause {
    /// Wait until a commit has taken this pause.
    pub async fn reached(&self) {
        self.gate.reached(1).await;
    }

    /// Let the paused commit continue.
    pub fn release(&self) {
        self.gate.open_all();
    }
}

/// A move of `F`, for tests that stand in for a newer fleet or race the
/// writer fence (ADR 0115 §2.2): the fleet-format row read `FOR UPDATE` and
/// moved to `epoch`, held open until [`HeldFinalize::commit`].
pub struct HeldFinalize {
    row: crate::guarded_tx::FleetRowTx,
    fence: crate::guarded_tx::WriterFence,
    epoch: u32,
}

impl HeldFinalize {
    /// Begin the move as a build whose writable range is `[1, epoch]`: waits
    /// behind every writer holding the row `FOR SHARE`.
    pub async fn begin(
        pool: &sqlx::PgPool,
        epoch: u32,
    ) -> Result<Self, lash_core_execution::StoreError> {
        use lash_core_execution::StoreError;
        let writable = lash_core_execution::compat::VersionRange::new(1, epoch)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let fence = crate::guarded_tx::WriterFence::new(
            writable,
            lash_core_execution::FleetFormat::from_version(epoch),
        );
        let mut row = crate::guarded_tx::begin_fleet_row(pool, &fence).await?;
        if row.recorded < epoch {
            let version = i32::try_from(epoch).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "lash_fleet_format.format_version",
                message: format!("not a fleet-format version: {epoch}"),
            })?;
            sqlx::query(
                crate::session_sql::session_sql()
                    .fleet_format
                    .update_format_version
                    .sql(),
            )
            .bind(version)
            .execute(row.connection())
            .await
            .map_err(crate::store_sqlx_error)?;
        }
        Ok(Self { row, fence, epoch })
    }

    /// Commit the move.
    pub async fn commit(self) -> Result<(), lash_core_execution::StoreError> {
        self.row.commit().await?;
        self.fence.observe(self.epoch);
        Ok(())
    }
}

/// Where a [`CommitFault`] loses a durable commit's connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LostCommit {
    /// The server session ends before `COMMIT` reaches it: the transaction
    /// rolls back, and the client's `COMMIT` fails on a dead connection.
    BeforeCommit,
    /// `COMMIT` lands, and its acknowledgement is lost on the way back.
    AfterCommit,
}

/// A connection lost at the next durable commit's `COMMIT`, once: the
/// client cannot tell from the error whether the transaction committed.
///
/// Install it on a durable store with
/// [`PostgresDurableStore::with_commit_fault_for_testing`](crate::PostgresDurableStore::with_commit_fault_for_testing).
#[derive(Debug)]
pub struct CommitFault {
    lost: LostCommit,
    armed: std::sync::atomic::AtomicBool,
}

impl CommitFault {
    /// Lose the next commit's connection where `lost` says.
    pub fn new(lost: LostCommit) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            lost,
            armed: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// Whether the fault has been taken.
    pub fn taken(&self) -> bool {
        !self.armed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// `COMMIT` `tx`, losing its connection the first time.
    pub(crate) async fn commit(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), sqlx::Error> {
        if !self.armed.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return tx.commit().await;
        }
        match self.lost {
            LostCommit::BeforeCommit => {
                // The session ends itself; the statement's own error is the
                // fatal notice, and the `COMMIT` after it meets a dead
                // connection.
                let _ = sqlx::query("SELECT pg_terminate_backend(pg_backend_pid())")
                    .execute(&mut *tx)
                    .await;
                tx.commit().await
            }
            LostCommit::AfterCommit => {
                tx.commit().await?;
                Err(sqlx::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected: the acknowledgement of COMMIT was lost",
                )))
            }
        }
    }
}

/// Move `F` to `epoch` in one transaction (see [`HeldFinalize`]).
pub async fn finalize_fleet_epoch(
    pool: &sqlx::PgPool,
    epoch: u32,
) -> Result<(), lash_core_execution::StoreError> {
    HeldFinalize::begin(pool, epoch).await?.commit().await
}

// Only database names survive a cell's runtime. Vacant shells are sealed and
// contain no law's schema or sessions. Reusing them bounds allocation by peak live
// fixtures per maintenance URL instead of the number of crash cuts. The test
// server's teardown removes the shells; no cell requests a cluster checkpoint.
static VACANT_DATABASES: tokio::sync::Mutex<std::collections::BTreeMap<String, Vec<String>>> =
    tokio::sync::Mutex::const_new(std::collections::BTreeMap::new());

/// A throwaway Postgres database with a fresh name, schema and catalog identity.
///
/// Construction reuses a sealed, empty database shell, or allocates one with
/// `CREATE DATABASE ... STRATEGY = WAL_LOG` when concurrent demand grows, then
/// provisions this build's baseline schema. Every store open still performs
/// full catalog verification. `Drop` seals the cell, terminates its sessions
/// and removes its schema before returning its shell to the process's pool.
///
/// Neither setup nor teardown requests a cluster-wide checkpoint: dropping a
/// database would stall unrelated cells on the same server. The number of shells
/// is bounded by peak simultaneous fixtures per process and maintenance URL.
/// Callers supplying a server own
/// its teardown, which removes the empty shells as well.
#[derive(Debug)]
pub struct IsolatedDatabase {
    maintenance_url: String,
    database_name: String,
    url: String,
}

impl IsolatedDatabase {
    /// # Panics
    ///
    /// Panics when the base URL cannot be parsed or the database cannot be
    /// created; both are test-configuration faults with no useful recovery.
    #[expect(
        clippy::expect_used,
        reason = "test-harness helper: a base URL that will not parse or a database that will not create is a test-configuration fault with no useful recovery, as the doc comment above states"
    )]
    pub async fn create(base_url: &str) -> Self {
        let database_name = format!("lash_test_{}", uuid::Uuid::new_v4().simple());
        let url = replace_database_name(base_url, &database_name);
        let mut connection = PgConnection::connect(base_url)
            .await
            .expect("connect Postgres maintenance database for test isolation");
        let vacant = VACANT_DATABASES
            .lock()
            .await
            .get_mut(base_url)
            .and_then(Vec::pop);
        if let Some(vacant) = vacant {
            // A retired URL must never reopen a later cell. Names are generated
            // here, never supplied by callers, and rename happens while sealed.
            sqlx::query(&format!(
                "ALTER DATABASE \"{vacant}\" RENAME TO \"{database_name}\""
            ))
            .execute(&mut connection)
            .await
            .expect("rename the vacant database for its next cell");
            sqlx::query(&format!(
                "ALTER DATABASE \"{database_name}\" ALLOW_CONNECTIONS true"
            ))
            .execute(&mut connection)
            .await
            .expect("open the renamed database for its next cell");
        } else {
            sqlx::query(&format!(
                "CREATE DATABASE \"{database_name}\" WITH STRATEGY = WAL_LOG"
            ))
            .execute(&mut connection)
            .await
            .unwrap_or_else(|error| {
                panic!("allocate isolated test database {database_name}: {error}")
            });
        }
        connection
            .close()
            .await
            .expect("close Postgres maintenance connection");
        let isolated = Self {
            maintenance_url: base_url.to_string(),
            database_name,
            url,
        };
        let mut connection = PgConnection::connect(&isolated.url)
            .await
            .expect("connect isolated database for its baseline schema");
        sqlx::raw_sql(
            "CREATE SCHEMA IF NOT EXISTS public AUTHORIZATION pg_database_owner;
             GRANT USAGE ON SCHEMA public TO PUBLIC",
        )
        .execute(&mut connection)
        .await
        .expect("create the isolated database's public schema");
        sqlx::raw_sql(crate::schema::SCHEMA_DDL)
            .execute(&mut connection)
            .await
            .expect("provision the isolated database's baseline schema");
        sqlx::query("UPDATE lash_catalog_identity SET catalog_id = gen_random_uuid()::text")
            .execute(&mut connection)
            .await
            .expect("give the isolated database its own catalog identity");
        connection
            .close()
            .await
            .expect("close isolated catalog identity connection");
        isolated
    }

    /// The connection URL for the isolated database.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The generated database name, for diagnostics.
    pub fn database_name(&self) -> &str {
        &self.database_name
    }
}

impl Drop for IsolatedDatabase {
    fn drop(&mut self) {
        let maintenance_url = self.maintenance_url.clone();
        let database_name = self.database_name.clone();
        let url = self.url.clone();
        // Finish retirement before cell admission is released. The separate
        // runtime also works when the cell's current-thread runtime has stopped.
        let dropped = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async move {
                let mut connection = PgConnection::connect(&maintenance_url)
                    .await
                    .map_err(|error| error.to_string())?;
                // Keep one connection to the cell before sealing it. No new
                // session can race termination, and even a held read transaction
                // or a panicking law cannot keep the local schema alive.
                let mut cell = PgConnection::connect(&url)
                    .await
                    .map_err(|error| error.to_string())?;
                sqlx::query(&format!(
                    "ALTER DATABASE \"{database_name}\" ALLOW_CONNECTIONS false"
                ))
                .execute(&mut connection)
                .await
                .map_err(|error| error.to_string())?;
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut cell)
                    .await
                    .map_err(|error| error.to_string())?;
                sqlx::query(
                    "SELECT pg_terminate_backend(pid, 5000) FROM pg_stat_activity
                     WHERE datname = $1 AND pid <> $2",
                )
                .bind(&database_name)
                .bind(pid)
                .execute(&mut connection)
                .await
                .map_err(|error| error.to_string())?;
                let schemas: Vec<String> = sqlx::query_scalar(
                    "SELECT format('DROP SCHEMA %I CASCADE', nspname) FROM pg_namespace
                     WHERE nspname <> 'information_schema' AND nspname !~ '^pg_'",
                )
                .fetch_all(&mut cell)
                .await
                .map_err(|error| error.to_string())?;
                for ddl in schemas {
                    sqlx::raw_sql(&ddl)
                        .execute(&mut cell)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                cell.close().await.map_err(|error| error.to_string())?;
                connection
                    .close()
                    .await
                    .map_err(|error| error.to_string())?;
                VACANT_DATABASES
                    .lock()
                    .await
                    .entry(maintenance_url)
                    .or_default()
                    .push(database_name);
                Ok::<(), String>(())
            })
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!(
                    "warning: could not retire isolated test database {}: {error}",
                    self.database_name
                );
            }
            Err(_) => {
                eprintln!(
                    "warning: teardown thread panicked retiring isolated test database {}",
                    self.database_name
                );
            }
        }
    }
}

/// Rewrites the database component of a Postgres connection URL.
///
/// Kept string-level rather than URL-parsing so query parameters, credentials,
/// and non-standard hosts survive verbatim.
fn replace_database_name(base_url: &str, database_name: &str) -> String {
    let (before_query, query) = match base_url.find('?') {
        Some(index) => (&base_url[..index], &base_url[index..]),
        None => (base_url, ""),
    };
    let scheme_end = before_query
        .find("://")
        .map(|index| index + 3)
        .unwrap_or_else(|| panic!("Postgres URL {base_url} has no scheme"));
    let authority_end = before_query[scheme_end..]
        .find('/')
        .map(|index| scheme_end + index)
        .unwrap_or(before_query.len());
    format!("{}/{database_name}{query}", &before_query[..authority_end])
}
