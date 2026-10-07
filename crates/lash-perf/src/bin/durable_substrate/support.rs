//! Counters read around a measurement window, distributions and the
//! report sink.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result, ensure};
use lash_sansio::sync::MutexExt as _;
use serde::Serialize;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::deploy::Database;
use crate::recorder::Transaction;

/// What PostgreSQL counted, cumulatively. The statement filter is L12a's:
/// statements of this database whose text holds an `INSERT`, `UPDATE` or
/// `DELETE` token, counter queries excluded.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PgCounters {
    /// Write-bearing statement calls.
    pub write_statements: i64,
    /// Rows those statements reported.
    pub write_rows: i64,
    /// Every statement call, reads included.
    pub all_statements: i64,
    /// WAL insert position, bytes.
    pub wal_bytes: i64,
    /// Bytes of every relation of the database, indexes and TOAST included.
    pub relation_bytes: i64,
    /// Connections open to the database.
    pub connections: i64,
}

impl PgCounters {
    /// `self - before`, except the connection count, which is a level.
    pub fn since(&self, before: &Self) -> Self {
        Self {
            write_statements: self.write_statements - before.write_statements,
            write_rows: self.write_rows - before.write_rows,
            all_statements: self.all_statements - before.all_statements,
            wal_bytes: self.wal_bytes - before.wal_bytes,
            relation_bytes: self.relation_bytes - before.relation_bytes,
            connections: self.connections,
        }
    }
}

/// Where the counters come from.
pub enum Counters {
    /// PostgreSQL's statistics views, through a pool of its own.
    Postgres(PgPool),
    /// The SQLite file and its WAL.
    Sqlite(PathBuf),
}

/// Counter snapshot of either dialect.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Snapshot {
    /// PostgreSQL's counters.
    pub postgres: Option<PgCounters>,
    /// SQLite file plus WAL bytes.
    pub sqlite_bytes: Option<u64>,
}

impl Snapshot {
    /// `self - before`.
    pub fn since(&self, before: &Self) -> Self {
        Self {
            postgres: match (self.postgres, before.postgres) {
                (Some(after), Some(before)) => Some(after.since(&before)),
                _ => None,
            },
            sqlite_bytes: match (self.sqlite_bytes, before.sqlite_bytes) {
                (Some(after), Some(before)) => Some(after.saturating_sub(before)),
                _ => None,
            },
        }
    }
}

const STATEMENTS: &str = "SELECT \
    COALESCE(SUM(calls) FILTER (WHERE query ~* '\\m(INSERT|UPDATE|DELETE)\\M'), 0)::bigint, \
    COALESCE(SUM(rows) FILTER (WHERE query ~* '\\m(INSERT|UPDATE|DELETE)\\M'), 0)::bigint, \
    COALESCE(SUM(calls), 0)::bigint \
    FROM pg_stat_statements \
    WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
    AND query NOT ILIKE '%pg_stat%' AND query NOT ILIKE '%pg_current_wal%' \
    AND query NOT ILIKE '%pg_total_relation_size%'";

const LEVELS: &str = "SELECT \
    pg_wal_lsn_diff(pg_current_wal_insert_lsn(), '0/0')::bigint, \
    (SELECT COALESCE(SUM(pg_total_relation_size(c.oid)), 0)::bigint FROM pg_class c \
       JOIN pg_namespace n ON n.oid = c.relnamespace \
       WHERE c.relkind IN ('r', 'm') AND n.nspname NOT IN ('pg_catalog', 'information_schema')), \
    (SELECT count(*)::bigint FROM pg_stat_activity WHERE datname = current_database())";

impl Counters {
    /// Counters for `database`.
    pub async fn open(database: &Database) -> Result<Self> {
        Ok(match database {
            Database::Postgres(url) => {
                let pool = PgPoolOptions::new()
                    .max_connections(1)
                    .connect(url)
                    .await
                    .context("connect the counter pool")?;
                // The server preloads the library; the view lives in the
                // extension, which the store's schema does not create.
                sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
                    .execute(&pool)
                    .await
                    .context("create pg_stat_statements")?;
                Self::Postgres(pool)
            }
            Database::Sqlite(path) => Self::Sqlite(path.clone()),
        })
    }

    /// The counters now.
    pub async fn read(&self) -> Result<Snapshot> {
        match self {
            Self::Postgres(pool) => {
                let (write_statements, write_rows, all_statements): (i64, i64, i64) =
                    sqlx::query_as(STATEMENTS).fetch_one(pool).await?;
                let (wal_bytes, relation_bytes, connections): (i64, i64, i64) =
                    sqlx::query_as(LEVELS).fetch_one(pool).await?;
                Ok(Snapshot {
                    postgres: Some(PgCounters {
                        write_statements,
                        write_rows,
                        all_statements,
                        wal_bytes,
                        relation_bytes,
                        connections,
                    }),
                    sqlite_bytes: None,
                })
            }
            Self::Sqlite(path) => Ok(Snapshot {
                postgres: None,
                sqlite_bytes: Some(file_bytes(path) + file_bytes(&wal_path(path))),
            }),
        }
    }

    /// Close the counter pool.
    pub async fn close(self) {
        if let Self::Postgres(pool) = self {
            pool.close().await;
        }
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Durable-port transactions by label.
pub fn by_label(transactions: &[Transaction]) -> BTreeMap<&'static str, usize> {
    let mut labels = BTreeMap::new();
    for transaction in transactions.iter().filter(|transaction| transaction.ok) {
        *labels.entry(transaction.label).or_default() += 1;
    }
    labels
}

/// A distribution, nearest-rank, milliseconds.
#[derive(Clone, Debug, Serialize)]
pub struct Distribution {
    /// Samples.
    pub count: usize,
    /// Median.
    pub p50_ms: f64,
    /// 99th percentile (the maximum below 100 samples).
    pub p99_ms: f64,
    /// Maximum.
    pub max_ms: f64,
}

impl Distribution {
    /// The distribution of `micros`.
    pub fn of(micros: &[u64]) -> Result<Self> {
        ensure!(!micros.is_empty(), "a distribution needs samples");
        let mut sorted = micros.to_vec();
        sorted.sort_unstable();
        let rank = |percent: usize| sorted[(sorted.len() * percent).div_ceil(100) - 1];
        let ms = |value: u64| value as f64 / 1_000.0;
        Ok(Self {
            count: sorted.len(),
            p50_ms: ms(rank(50)),
            p99_ms: ms(rank(99)),
            max_ms: ms(sorted[sorted.len() - 1]),
        })
    }
}

/// The JSON-lines report: every record is one line, written at once.
pub struct Report {
    file: Mutex<std::fs::File>,
}

impl Report {
    /// Append to `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            file: Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open {}", path.display()))?,
            ),
        })
    }

    /// Write `record` as one line, tagged with its `kind`.
    pub fn write(&self, kind: &str, record: &impl Serialize) -> Result<()> {
        let mut value = serde_json::to_value(record)?;
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "kind".to_owned(),
                serde_json::Value::String(kind.to_owned()),
            );
        }
        let line = serde_json::to_string(&value)?;
        let mut file = self.file.lock_recover();
        writeln!(file, "{line}")?;
        file.flush()?;
        Ok(())
    }
}
