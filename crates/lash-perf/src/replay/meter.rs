//! The counters a replay reads around every turn, and the metrics they make.
//!
//! Every metric is a difference of two cumulative readings taken before the
//! round's sends and after its last outcome. A metric the store cannot count
//! is absent from a reading, never zero (docs/perf/README.md rule 1).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use lash_core::store::StoreTestSupport as _;
use lash_sansio::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

/// Whether a metric counts work or times it. Per-merge gates bind counted
/// metrics only; wall and CPU time are trend data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Counted,
    Timed,
}

/// One metric's name, unit, kind and meaning.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MetricDef {
    pub name: &'static str,
    pub unit: &'static str,
    pub kind: MetricKind,
    pub meaning: &'static str,
}

const fn counted(name: &'static str, unit: &'static str, meaning: &'static str) -> MetricDef {
    MetricDef {
        name,
        unit,
        kind: MetricKind::Counted,
        meaning,
    }
}

/// Every metric a turn record may carry, in receipt order.
pub const METRICS: [MetricDef; 20] = [
    counted(
        "sql_statements",
        "statements",
        "SQL statements the store executed (SQLite perf witness)",
    ),
    counted("sql_selects", "statements", "SELECT/WITH statements"),
    counted(
        "sql_writes",
        "statements",
        "INSERT, UPDATE and DELETE statements",
    ),
    counted(
        "sql_transactions",
        "statements",
        "BEGIN statements (write and read transactions)",
    ),
    counted("sql_rows_read", "rows", "result rows statements returned"),
    counted(
        "sql_rows_written",
        "rows",
        "rows inserted, updated or deleted (SQLite total_changes)",
    ),
    counted(
        "sql_bytes_written",
        "bytes",
        "bytes of the values bound to writing statements, independent of page layout",
    ),
    counted(
        "store_bytes_written",
        "bytes",
        "database pages written to the WAL times the page size (SQLite DBSTATUS_CACHE_WRITE)",
    ),
    counted(
        "graph_nodes_written",
        "nodes",
        "net growth of the session graph's node rows",
    ),
    counted(
        "graph_nodes_decoded",
        "nodes",
        "session graph node bodies the store decoded",
    ),
    counted(
        "allocations",
        "allocations",
        "host-process heap allocations (stats_alloc)",
    ),
    counted(
        "allocated_bytes",
        "bytes",
        "host-process bytes allocated (stats_alloc)",
    ),
    counted(
        "ipc_frames",
        "frames",
        "frames exchanged with code-mode workers, both directions",
    ),
    counted(
        "ipc_bytes",
        "bytes",
        "frame bytes exchanged with code-mode workers, both directions",
    ),
    counted(
        "worker_executions",
        "executions",
        "cell and process executions the worker pool ran",
    ),
    counted("model_calls", "calls", "scripted model calls the turn made"),
    counted(
        "workers_killed",
        "processes",
        "code-mode workers the crash fixture killed mid-cell",
    ),
    MetricDef {
        name: "host_cpu_ms",
        unit: "ms",
        kind: MetricKind::Timed,
        meaning: "host process CPU, every thread (utime + stime)",
    },
    MetricDef {
        name: "worker_cpu_ms",
        unit: "ms",
        kind: MetricKind::Timed,
        meaning: "code-mode worker CPU the pool charged",
    },
    MetricDef {
        name: "wall_ms",
        unit: "ms",
        kind: MetricKind::Timed,
        meaning: "first send to last outcome of the round",
    },
];

/// The definition of metric `name`.
pub fn metric(name: &str) -> Option<&'static MetricDef> {
    METRICS.iter().find(|def| def.name == name)
}

/// One cumulative reading of every available counter.
pub type Reading = BTreeMap<&'static str, f64>;

/// The difference `after - before` of every metric both readings carry.
pub fn difference(before: &Reading, after: &Reading) -> BTreeMap<String, f64> {
    after
        .iter()
        .filter_map(|(name, after)| {
            before
                .get(name)
                .map(|before| ((*name).to_owned(), after - before))
        })
        .collect()
}

/// Reads the store's own count of its rows, beside the store's connections
/// so the reads land in no measured counter.
pub enum StoreProbe {
    Sqlite {
        connection: Box<Mutex<rusqlite::Connection>>,
        page_size: u64,
        store: Arc<lash_sqlite_store::SqliteStore>,
    },
    Postgres {
        pool: sqlx::PgPool,
    },
}

impl StoreProbe {
    /// A probe of the SQLite store set `stores`.
    pub fn sqlite(stores: &lash::sqlite::SqliteStoreSet) -> Result<Self> {
        let connection = rusqlite::Connection::open_with_flags(
            stores.database_uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .context("open the replay's SQLite probe")?;
        let page_size: u64 = connection
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .context("read the SQLite page size")?;
        Ok(Self::Sqlite {
            connection: Box::new(Mutex::new(connection)),
            page_size,
            store: stores.session_store_factory(),
        })
    }

    async fn graph_nodes(&self) -> Option<f64> {
        match self {
            Self::Sqlite { connection, .. } => connection
                .lock_recover()
                .query_row("SELECT COUNT(*) FROM graph_nodes", [], |row| {
                    row.get::<_, i64>(0)
                })
                .ok()
                .map(|count| count as f64),
            Self::Postgres { pool } => {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM lash_graph_nodes")
                    .fetch_one(pool)
                    .await
                    .ok()
                    .map(|count| count as f64)
            }
        }
    }

    pub fn backend(&self) -> &'static str {
        match self {
            Self::Sqlite { .. } => "sqlite-file",
            Self::Postgres { .. } => "postgres",
        }
    }
}

/// Every counter source of one replay run.
pub struct Meter {
    witness: lash_core::perf_witness::Collector,
    probe: StoreProbe,
    workers: Option<lash::vm::WorkerPool>,
}

impl Meter {
    /// Install the process-global SQL witness. One meter runs at a time.
    pub fn install(
        probe: StoreProbe,
        workers: Option<lash::vm::WorkerPool>,
        sql_shapes: bool,
    ) -> Result<Self> {
        Ok(Self {
            witness: (if sql_shapes {
                lash_core::perf_witness::Collector::install_with_sql_shapes()
            } else {
                lash_core::perf_witness::Collector::install()
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?,
            probe,
            workers,
        })
    }

    pub fn backend(&self) -> &'static str {
        self.probe.backend()
    }

    /// Read every available counter now.
    pub async fn read(&self, model_calls: u64) -> Reading {
        self.read_with_shapes(model_calls).await.0
    }

    /// Counters and attribution use the very same locked SQL snapshot.
    pub async fn read_with_shapes(
        &self,
        model_calls: u64,
    ) -> (
        Reading,
        Option<lash_core::perf_witness::sql_shapes::Receipt>,
    ) {
        let mut shapes = None;
        let mut reading = Reading::new();
        let alloc = crate::GLOBAL_ALLOCATOR.stats();
        reading.insert(
            "allocations",
            (alloc.allocations + alloc.reallocations) as f64,
        );
        reading.insert("allocated_bytes", alloc.bytes_allocated as f64);
        if let Some(cpu) = crate::perf_support::scheduler::process_cpu_ms() {
            reading.insert("host_cpu_ms", cpu);
        }
        reading.insert("model_calls", model_calls as f64);
        if let Some(nodes) = self.probe.graph_nodes().await {
            reading.insert("graph_nodes_written", nodes);
        }
        if let StoreProbe::Sqlite {
            page_size, store, ..
        } = &self.probe
        {
            let work = self.witness.snapshot();
            shapes = work.sql_shapes.clone();
            let verb = |name: &str| {
                work.sql_statements_by_verb
                    .get(name)
                    .copied()
                    .unwrap_or_default() as f64
            };
            reading.insert("sql_statements", work.sql_statements as f64);
            reading.insert("sql_selects", verb("select"));
            reading.insert(
                "sql_writes",
                verb("insert") + verb("update") + verb("delete"),
            );
            reading.insert("sql_transactions", verb("begin"));
            reading.insert("sql_rows_read", work.sql_rows_read as f64);
            reading.insert("sql_rows_written", work.sql_rows_written as f64);
            reading.insert("sql_bytes_written", work.sql_bytes_written as f64);
            reading.insert(
                "store_bytes_written",
                (work.store_pages_written * page_size) as f64,
            );
            reading.insert(
                "graph_nodes_decoded",
                store.decoded_row_counts_for_testing().graph_node_bodies as f64,
            );
        }
        if let Some(pool) = &self.workers {
            let counters = pool.measurements().counters;
            reading.insert(
                "ipc_frames",
                (counters.ipc_sent_messages + counters.ipc_received_messages) as f64,
            );
            reading.insert(
                "ipc_bytes",
                (counters.ipc_sent_bytes + counters.ipc_received_bytes) as f64,
            );
            reading.insert(
                "worker_executions",
                (counters.cell_executions + counters.process_executions) as f64,
            );
            reading.insert("workers_killed", super::workers::killed() as f64);
            reading.insert("worker_cpu_ms", pool.measured_cpu().as_secs_f64() * 1_000.0);
        }
        (reading, shapes)
    }
}
