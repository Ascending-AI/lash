//! The witness ledger's writer, as a node uses it: the outside world.
//!
//! The ledger is a database of its own, written by insert only: on a
//! PostgreSQL leg a database on the lash server (`witness.sql`), written under
//! the insert-only `lash_witness_writer` role and stamped by the database's
//! clock; on a SQLite leg a file of its own beside the lash database
//! ([`SQLITE_SCHEMA`]), whose triggers refuse every update and delete. No
//! lash store, transaction or schema touches it, so what it holds is what the
//! bodies did, whatever the nodes' stores say. A body writes its entry before
//! anything else and retries the write until it lands: a body that cannot
//! leave its evidence does not run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::node::Database;

/// How long a failed witness write waits before it tries again.
const RETRY: Duration = Duration::from_millis(100);

/// How often a held body looks for its release.
const POLL: Duration = Duration::from_millis(50);

/// The SQLite leg's ledger: `witness.sql`'s tables, append-only by trigger.
pub const SQLITE_SCHEMA: &str = "
CREATE TABLE witness_effects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    call_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    node TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('entered', 'returned')),
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TABLE witness_model_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    call_index INTEGER NOT NULL CHECK (call_index IN (1, 2)),
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    node TEXT NOT NULL,
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TABLE witness_nemesis (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN (
        'kill', 'stop', 'partition', 'heal', 'restart-begin', 'restart-complete', 'release'
    )),
    node TEXT,
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TRIGGER witness_effects_append_only BEFORE UPDATE ON witness_effects
    BEGIN SELECT RAISE(ABORT, 'the witness ledger is append-only'); END;
CREATE TRIGGER witness_effects_kept BEFORE DELETE ON witness_effects
    BEGIN SELECT RAISE(ABORT, 'the witness ledger is append-only'); END;
CREATE TRIGGER witness_model_attempts_append_only BEFORE UPDATE ON witness_model_attempts
    BEGIN SELECT RAISE(ABORT, 'the witness ledger is append-only'); END;
CREATE TRIGGER witness_model_attempts_kept BEFORE DELETE ON witness_model_attempts
    BEGIN SELECT RAISE(ABORT, 'the witness ledger is append-only'); END;
CREATE TRIGGER witness_nemesis_append_only BEFORE UPDATE ON witness_nemesis
    BEGIN SELECT RAISE(ABORT, 'the witness ledger is append-only'); END;
CREATE TRIGGER witness_nemesis_kept BEFORE DELETE ON witness_nemesis
    BEGIN SELECT RAISE(ABORT, 'the witness ledger is append-only'); END;
";

/// Open a connection to the SQLite ledger at `path` that waits out the
/// other processes' writes.
///
/// # Errors
///
/// The file cannot be opened.
pub fn open_sqlite(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(15))?;
    Ok(connection)
}

/// Where a ledger lives.
#[derive(Clone, Debug)]
enum Ledger {
    Postgres(PgPool),
    Sqlite(Arc<PathBuf>),
}

/// One node's handle on the witness ledger.
#[derive(Clone, Debug)]
pub struct Witness {
    ledger: Ledger,
    node: String,
}

/// A ledger write or read that failed and is retried.
type Failure = Box<dyn std::error::Error + Send + Sync>;

impl Witness {
    /// Connect `node`'s writer to the ledger `witness`. A PostgreSQL pool
    /// connects lazily, so a node boots while the server restarts.
    ///
    /// # Errors
    ///
    /// The URL does not parse.
    pub fn connect(witness: &Database, node: &str) -> Result<Self, sqlx::Error> {
        let ledger = match witness {
            Database::Postgres(url) => Ledger::Postgres(
                PgPoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(Duration::from_secs(2))
                    .connect_lazy(url)?,
            ),
            Database::Sqlite(path) => Ledger::Sqlite(Arc::new(path.clone())),
        };
        Ok(Self {
            ledger,
            node: node.to_owned(),
        })
    }

    /// The node this handle writes as.
    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }

    /// Record that a body for `call` of `tool` was entered on this node.
    pub async fn entered(&self, call: &str, tool: &str) {
        self.effect(call, tool, "entered").await;
    }

    /// Record that the body for `call` of `tool` returned on this node.
    pub async fn returned(&self, call: &str, tool: &str) {
        self.effect(call, tool, "returned").await;
    }

    async fn effect(&self, call: &str, tool: &str, phase: &str) {
        self.retry(|ledger| async move {
            match ledger {
                Ledger::Postgres(pool) => sqlx::query(
                    "INSERT INTO witness_effects (call_id, tool, node, phase) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(call)
                .bind(tool)
                .bind(&self.node)
                .bind(phase)
                .execute(&pool)
                .await
                .map(drop)
                .map_err(Failure::from),
                Ledger::Sqlite(path) => {
                    let row = [call, tool, &self.node, phase].map(str::to_owned);
                    on_sqlite(path, move |connection| {
                        connection
                            .execute(
                                "INSERT INTO witness_effects (call_id, tool, node, phase) \
                                 VALUES (?1, ?2, ?3, ?4)",
                                row,
                            )
                            .map(drop)
                    })
                    .await
                }
            }
        })
        .await;
    }

    /// Record that an attempt of the turn's model call `call` started on
    /// this node, and answer its number: one more than the attempts the
    /// ledger already holds for the call. An attempt writes its entry before
    /// it does anything else, so the ledger counts every attempt the outside
    /// world saw.
    pub async fn model_attempt(&self, call: u32) -> u32 {
        let call = i32::try_from(call).unwrap_or(i32::MAX);
        let attempt: i32 = self
            .retry(|ledger| async move {
                match ledger {
                    Ledger::Postgres(pool) => sqlx::query_scalar(
                        "INSERT INTO witness_model_attempts (call_index, attempt, node) \
                         SELECT $1, COALESCE(MAX(attempt), 0) + 1, $2 \
                         FROM witness_model_attempts WHERE call_index = $1 \
                         RETURNING attempt",
                    )
                    .bind(call)
                    .bind(&self.node)
                    .fetch_one(&pool)
                    .await
                    .map_err(Failure::from),
                    Ledger::Sqlite(path) => {
                        let node = self.node.clone();
                        on_sqlite(path, move |connection| {
                            connection.query_row(
                                "INSERT INTO witness_model_attempts (call_index, attempt, node) \
                                 SELECT ?1, COALESCE(MAX(attempt), 0) + 1, ?2 \
                                 FROM witness_model_attempts WHERE call_index = ?1 \
                                 RETURNING attempt",
                                rusqlite::params![call, node],
                                |row| row.get(0),
                            )
                        })
                        .await
                    }
                }
            })
            .await;
        u32::try_from(attempt).unwrap_or(u32::MAX)
    }

    /// Wait until the test records the nemesis marker `release`: a held
    /// body's cue to go on. Read errors (the server restarting) are retried.
    pub async fn released(&self) {
        loop {
            let found: Result<i64, Failure> = match self.ledger.clone() {
                Ledger::Postgres(pool) => sqlx::query_scalar(
                    "SELECT count(*) FROM witness_nemesis WHERE kind = 'release'",
                )
                .fetch_one(&pool)
                .await
                .map_err(Failure::from),
                Ledger::Sqlite(path) => {
                    on_sqlite(path, |connection| {
                        connection.query_row(
                            "SELECT count(*) FROM witness_nemesis WHERE kind = 'release'",
                            [],
                            |row| row.get(0),
                        )
                    })
                    .await
                }
            };
            if matches!(found, Ok(count) if count > 0) {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn retry<T, F, Fut>(&self, write: F) -> T
    where
        F: Fn(Ledger) -> Fut,
        Fut: std::future::Future<Output = Result<T, Failure>>,
    {
        loop {
            match write(self.ledger.clone()).await {
                Ok(answer) => return answer,
                Err(error) => {
                    eprintln!("{}: witness write failed, retrying: {error}", self.node);
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    }
}

/// Run `work` on a connection of its own to the SQLite ledger at `path`, off
/// the async runtime.
async fn on_sqlite<T, F>(path: Arc<PathBuf>, work: F) -> Result<T, Failure>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || open_sqlite(&path).and_then(|connection| work(&connection)))
        .await
        .map_err(Failure::from)?
        .map_err(Failure::from)
}

/// Where a held workload stops and waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// Nothing holds.
    Nothing,
    /// The turn's first model call holds on its first attempt until the
    /// node dies or stops.
    Model,
    /// The turn's first model call holds on its first attempt until the
    /// `release` marker.
    ModelUntilRelease,
    /// `ext.write`'s body holds after its witness entry until the node dies.
    Step,
    /// `ext.write`'s body holds after its witness entry until the `release`
    /// marker, then returns.
    StepUntilRelease,
}

impl Hold {
    /// The hold a node's `LASH_WORKERS_HOLD` names.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "" | "nothing" => Some(Self::Nothing),
            "model" => Some(Self::Model),
            "model-until-release" => Some(Self::ModelUntilRelease),
            "step" => Some(Self::Step),
            "step-until-release" => Some(Self::StepUntilRelease),
            _ => None,
        }
    }

    /// The value that names this hold.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Nothing => "nothing",
            Self::Model => "model",
            Self::ModelUntilRelease => "model-until-release",
            Self::Step => "step",
            Self::StepUntilRelease => "step-until-release",
        }
    }
}
