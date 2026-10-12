//! The six-operation matrix. Each sample starts with a fresh session and
//! exactly the declared number of prior committed turns; samples never grow
//! the next sample's history. SQL belongs to the owning task/connection only.
use crate::Run;
use crate::deploy::Database;
use crate::support::Distribution;
use crate::turn::{Script, session_id};
use crate::turns::{close, one_turn, turn_id};
use anyhow::{Context as _, Result, ensure};
use lash_core_execution::facade_support::sql::{self, Receipt};
use lash_core_store::store::{HistoryAnchor, HistoryBudget};
use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};

fn host() -> serde_json::Value {
    let load1 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok());
    let cpu_psi = std::fs::read_to_string("/proc/pressure/cpu").ok();
    serde_json::json!({"load1": load1, "cpu_psi": cpu_psi})
}
fn record(
    run: &Run<'_>,
    sample: usize,
    operation: &str,
    receipt: &Receipt,
    samples: &mut BTreeMap<String, Vec<u64>>,
) -> Result<()> {
    ensure!(receipt.statements > 0, "{operation} has no physical SQL");
    ensure!(
        receipt.unmeasured_bound_statements == 0,
        "{operation} has unknown bind bytes"
    );
    samples
        .entry(operation.to_owned())
        .or_default()
        .push(receipt.elapsed_nanos / 1_000);
    run.report.write(
        "operation",
        &serde_json::json!({
            "operation": operation, "sample": sample, "dialect": run.database.dialect(),
            "prior_turns": run.prior_turns, "parks": run.cell_parks, "value_bytes": run.value_bytes,
            "scope": "owning store call, caller and delegated connection work; background excluded",
            "sql": receipt, "host_at_recording": host(),
        }),
    )
}

pub async fn run(run: &Run<'_>) -> Result<()> {
    ensure!(
        run.samples > 0 && run.cell_parks > 0 && run.nodes == 1,
        "operation matrix needs samples, at least one park, and one serving node"
    );
    let mut deployment = run.deployment();
    deployment.boot_many(1).await?;
    let dialect = match run.database {
        Database::Sqlite(_) => "sqlite",
        Database::Postgres(_) => "postgres",
    };
    let mut samples = BTreeMap::new();
    for sample in 0..run.samples {
        let session = session_id(&format!("operations-{sample}"))?;
        run.report.write("operation_host_start", &host())?;
        let (answer, receipt) = sql::collect(
            format!("{session}/open"),
            dialect,
            crate::cells::create_session(deployment.cells(0), &session),
        )
        .await;
        answer?;
        record(run, sample, "session.open", &receipt, &mut samples)?;
        deployment.scripts.set(&session, Script::default());
        for prior in 0..run.prior_turns {
            one_turn(
                &deployment,
                0,
                &session,
                &turn_id(&format!("{session}/prior-{prior}"))?,
            )
            .await?;
        }
        deployment.scripts.set(
            &session,
            Script {
                cell_calls: run.cell_parks,
                cell_payload: run.value_bytes,
                ..Script::default()
            },
        );
        let from = deployment.recorder.now_us();
        one_turn(
            &deployment,
            0,
            &session,
            &turn_id(&format!("{session}/measured"))?,
        )
        .await?;
        let transactions = deployment.recorder.transactions_since(from);
        let actor = crate::turn::session_actor(&session)?.to_string();
        let owned: Vec<_> = transactions
            .iter()
            .filter(|tx| tx.ok && tx.actor == actor)
            .collect();
        let settled = owned
            .iter()
            .filter(|tx| tx.label == "round.outcome")
            .count();
        ensure!(
            settled == run.cell_parks,
            "cell settled {settled} of {} awaits",
            run.cell_parks
        );
        let mut parks = 0;
        let mut ends = 0;
        let mut commits = 0;
        for tx in &owned {
            // This script's cell answers after its last park. The next
            // model.start consumes that answer and retires its root in the
            // same transaction; there is no standalone CellEnd node.
            let operation = if tx.label == "model.start" && parks == run.cell_parks {
                ends += 1;
                "cell.end"
            } else if tx.label == "cell.snapshot+admit" {
                parks += 1;
                "cell.park"
            } else if tx.label == "turn.commit" {
                commits += 1;
                "turn.commit"
            } else if tx.exec_roots > 0 {
                "cell.quiet_snapshot"
            } else {
                continue;
            };
            let receipt = tx
                .sql
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("{operation} has no SQL receipt"))?;
            record(run, sample, operation, receipt, &mut samples)?;
        }
        ensure!(
            parks == run.cell_parks && ends == 1 && commits == 1,
            "incomplete owning windows: parks={parks}, ends={ends}, commits={commits}"
        );
        let (answer, receipt) = sql::collect(
            format!("{session}/history"),
            dialect,
            deployment
                .producer(0)
                .session_store_factory()
                .load_ancestors(
                    &session,
                    HistoryAnchor::Head,
                    HistoryBudget {
                        max_nodes: NonZeroU32::new(2).context("nonzero page")?,
                        max_bytes: NonZeroU64::new(1 << 20).context("nonzero bytes")?,
                    },
                ),
        )
        .await;
        let page = answer?;
        ensure!(
            page.nodes.len() == 2,
            "history sample needs a complete two-node page"
        );
        record(run, sample, "history.read", &receipt, &mut samples)?;
        // Opening the measurement handle is setup, outside the GC window.
        let (answer, receipt) = match &run.database {
            Database::Sqlite(path) => {
                let stores = lash_sqlite_store::SqliteStoreSet::open(
                    path,
                    lash_sqlite_store::SqliteSynchronous::Full,
                )
                .await?;
                sql::collect(
                    format!("{session}/gc"),
                    dialect,
                    stores.durable_store().audit_orphans(),
                )
                .await
            }
            Database::Postgres(url) => {
                let storage = lash_postgres_store::testing::connect(url).await?;
                lash_perf::perf_support::require_durable_postgres(storage.pool()).await?;
                let collected = sql::collect(
                    format!("{session}/gc"),
                    dialect,
                    storage.durable_store().audit_orphans(),
                )
                .await;
                storage.pool().close().await;
                collected
            }
        };
        let gc = answer?;
        record(run, sample, "graph.gc", &receipt, &mut samples)?;
        run.report.write(
            "gc_work",
            &serde_json::json!({"sample": sample, "audited_blobs": gc}),
        )?;
        close(&deployment, std::slice::from_ref(&session)).await?;
    }
    for operation in [
        "turn.commit",
        "cell.park",
        "cell.end",
        "session.open",
        "history.read",
        "graph.gc",
    ] {
        let raw = samples
            .get(operation)
            .ok_or_else(|| anyhow::anyhow!("missing {operation}"))?;
        run.report.write("operation_distribution", &serde_json::json!({
            "operation": operation, "samples_us": raw, "latency": Distribution::of(raw)?,
            "counts": "each operation record is an owning-window sum; parents and children overlap",
        }))?;
    }
    deployment.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    /// SQL-OPERATION-MATRIX: a fresh fixed-history sample must observe every
    /// owning boundary, with exactly its declared parks and one consuming
    /// model.start as the fused cell end.
    #[tokio::test]
    async fn operation_matrix_covers_each_owning_boundary() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("operations.jsonl");
        let report = crate::support::Report::open(&path).unwrap();
        let worker =
            std::path::PathBuf::from(std::env::var_os("LASH_VM_WORKER").expect("worker runfile"));
        let run = Run {
            worker: &worker,
            report: &report,
            database: Database::Sqlite(fixture.path().join("lash.db")),
            nodes: 1,
            samples: 1,
            pool: crate::deploy::PoolSize { min: 1, max: 4 },
            prior_turns: 2,
            cell_parks: 2,
            value_bytes: 16,
        };
        let answer =
            tokio::time::timeout(std::time::Duration::from_secs(90), super::run(&run)).await;
        if !matches!(&answer, Ok(Ok(()))) {
            eprintln!(
                "matrix partial records: {}",
                std::fs::read_to_string(&path).unwrap()
            );
        }
        answer
            .expect("matrix completed within its fixture budget")
            .unwrap();
        let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let ends: Vec<_> = records
            .iter()
            .filter(|record| record["kind"] == "operation" && record["operation"] == "cell.end")
            .collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["sql"]["operation"], "s/operations-0/model.start");
        for operation in [
            "turn.commit",
            "cell.park",
            "cell.end",
            "session.open",
            "history.read",
            "graph.gc",
        ] {
            assert!(
                records
                    .iter()
                    .any(|record| record["kind"] == "operation_distribution"
                        && record["operation"] == operation)
            );
        }
        for record in records
            .iter()
            .filter(|record| record["kind"] == "operation")
        {
            assert_eq!(record["prior_turns"], 2);
            assert!(record["sql"]["bound_bytes"].is_u64());
            assert!(record["sql"]["result_bytes"].is_u64());
            assert!(record["sql"]["rows_affected"].is_u64());
            eprintln!("SQL_OPERATION {}", record);
        }
    }
}
