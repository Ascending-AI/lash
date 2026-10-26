//! The counters a replay reads around every turn, and the metrics they make.
//!
//! Every metric is a difference of two cumulative readings taken before the
//! round's sends and after its last outcome. A metric the store cannot count
//! is absent from a reading, never zero (docs/perf/README.md rule 1).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use lash_core::store::StoreTestSupport as _;
use lash_protocol_types::sync::MutexExt as _;
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
pub const METRICS: [MetricDef; 23] = [
    counted(
        "tool_snapshot_callbacks",
        "entries",
        "tool environment_snapshot callback entries; not store writes",
    ),
    counted(
        "tool_manifest_callbacks",
        "entries",
        "tool_manifests callback entries; not store writes",
    ),
    counted(
        "link_payload_callbacks",
        "entries",
        "link_payload callback entries; not store writes",
    ),
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
    traffic: Option<lash_sqlite_store::write_observer::Collector>,
}

impl Meter {
    /// Install the process-global SQL witness. One meter runs at a time.
    pub fn install(
        probe: StoreProbe,
        workers: Option<lash::vm::WorkerPool>,
        sql_shapes: bool,
        traffic: Option<lash_sqlite_store::write_observer::Collector>,
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
            traffic,
        })
    }

    pub fn backend(&self) -> &'static str {
        self.probe.backend()
    }

    pub(super) fn sql_work(
        &self,
        spool: super::sql_work::Spool,
    ) -> Result<(
        lash_core::perf_witness::sql_shapes::work::Receipt,
        serde_json::Value,
    )> {
        let snapshot = self.witness.snapshot();
        let work = spool.finish(
            snapshot
                .sql_shapes
                .context("SQL-shape observer was not installed")?
                .work,
        )?;
        let affected =
            matches!(self.probe, StoreProbe::Sqlite { .. }).then_some(snapshot.sql_rows_written);
        let conserved = work.statements() == snapshot.sql_statements
            && work.rows_returned() == snapshot.sql_rows_read
            && affected.is_none_or(|rows| {
                rows == work
                    .totals
                    .get(&lash_core_ids::perf_witness::sql_shapes::work::Backend::Sqlite)
                    .map_or(0, |c| c.rows_affected)
            });
        let reference = serde_json::json!({"statements": snapshot.sql_statements,
            "rows_returned": snapshot.sql_rows_read, "sqlite_completed_connection_changes": affected,
            "conserved": conserved});
        Ok((work, reference))
    }

    pub fn attach_workers(&mut self, workers: Option<lash::vm::WorkerPool>) {
        self.workers = workers;
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
        Option<super::traffic::Reading>,
    ) {
        let shapes;
        let mut traffic = self
            .traffic
            .as_ref()
            .map(|_| super::traffic::Reading::default());
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
        {
            let work = if let Some(observer) = &self.traffic {
                let (work, physical) =
                    sqlite_cut(|| observer.cut(|| self.witness.snapshot())).await;
                if let Some(traffic) = &mut traffic {
                    traffic.sqlite = Some(physical);
                }
                work
            } else {
                self.witness.snapshot()
            };
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
            if let StoreProbe::Sqlite {
                page_size, store, ..
            } = &self.probe
            {
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
        }
        if let Some(pool) = &self.workers {
            let counters = pool.measurements().counters;
            if let Some(traffic) = &mut traffic {
                traffic.ipc = Some(counters.ipc);
            }
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
        (reading, shapes, traffic)
    }
}

/// A session outcome can race the tail of an observed connection operation.
/// Retake the entire locked counter/physical pair after yielding to its owner.
/// Permanent loss or failure to reach idle within one second keeps the original
/// incomplete receipt, so interval admission still refuses it.
async fn sqlite_cut<T>(
    mut cut: impl FnMut() -> (T, lash_sqlite_store::write_observer::Receipt),
) -> (T, lash_sqlite_store::write_observer::Receipt) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        let (counters, receipt) = cut();
        if receipt.active_operations == 0
            || receipt.overflow_bytes != 0
            || receipt.unknown_bytes != 0
            || receipt.failed_writes != 0
            || tokio::time::Instant::now() >= deadline
        {
            return (counters, receipt);
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(1)).min(deadline),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sqlite_store::write_observer::{Receipt, WriteKey, WriteKind, WriteRow};

    /// TRAFFIC-CONSERVATION (FIG-6540): a physical write can precede its
    /// CACHE_WRITE accounting. Retake both counters at the idle boundary.
    #[tokio::test(start_paused = true)]
    async fn traffic_conservation_waits_for_an_idle_sqlite_cut() {
        let before = Receipt {
            epoch: 1,
            complete: true,
            ..Default::default()
        };
        let partial = Receipt {
            active_operations: 1,
            complete: false,
            write_calls: 1,
            total_bytes: 4096,
            rows: vec![WriteRow {
                key: WriteKey {
                    database: "sample.db".into(),
                    page: Some(2),
                    kind: WriteKind::WalBody,
                    operation: "sqlite.connection".into(),
                    origin: "sample.update".into(),
                },
                bytes: 4096,
            }],
            ..before.clone()
        };
        let mut settled = partial.clone();
        settled.active_operations = 0;
        settled.complete = true;
        settled.write_calls = 2;
        settled.total_bytes = 8192;
        settled.rows[0].bytes = 8192;
        let mut cuts = [(0, partial), (8192, settled)].into_iter();
        let (store_bytes, receipt) = sqlite_cut(|| cuts.next().unwrap()).await;
        assert!(receipt.complete, "the cut split a connection operation");
        let interval = receipt.difference(&before).unwrap();
        assert_eq!(interval.bytes(WriteKind::WalBody), 8192);
        assert!(interval.conserves(store_bytes));
    }

    /// TRAFFIC-CONSERVATION: retries cannot hang or repair an incomplete
    /// receipt by clearing evidence of active work or lost attribution.
    #[tokio::test(start_paused = true)]
    async fn traffic_conservation_bounds_sqlite_cut_wait_and_preserves_refusals() {
        let before = Receipt {
            epoch: 1,
            complete: true,
            ..Default::default()
        };
        let active = Receipt {
            complete: false,
            active_operations: 1,
            ..before.clone()
        };
        let (_, receipt) =
            tokio::time::timeout(Duration::from_secs(2), sqlite_cut(|| (0, active.clone())))
                .await
                .expect("boundary acquisition must stop even if the operation never settles");
        assert_eq!(receipt.active_operations, 1);
        assert!(!receipt.complete);
        assert!(receipt.difference(&before).is_none());
        for (overflow_bytes, unknown_bytes, failed_writes) in [(1, 0, 0), (0, 1, 0), (0, 0, 1)] {
            let lost = Receipt {
                overflow_bytes,
                unknown_bytes,
                failed_writes,
                ..active.clone()
            };
            let mut cuts = [lost].into_iter();
            let (_, receipt) =
                sqlite_cut(|| (0, cuts.next().expect("loss cannot be retried"))).await;
            assert_eq!(receipt.overflow_bytes, overflow_bytes);
            assert_eq!(receipt.unknown_bytes, unknown_bytes);
            assert_eq!(receipt.failed_writes, failed_writes);
            assert!(receipt.difference(&before).is_none());
        }
    }
}
