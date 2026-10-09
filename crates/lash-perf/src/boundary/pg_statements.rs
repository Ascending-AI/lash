//! FIG-5724: a statement belongs to this run only if both its database and
//! executing role match. Setup is a baseline, never a server-wide reset.
use std::collections::BTreeMap;
use std::str::FromStr;

use anyhow::{Context as _, Result, ensure};
use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions as _, PgPool, Row as _};

use super::Args;

const SHAPES: &str = "SELECT queryid, query, calls, rows, total_exec_time, \
    shared_blks_hit, shared_blks_read, plans, stats_since::text \
    FROM pg_stat_statements \
    WHERE dbid = $1::bigint::oid AND userid = $2::bigint::oid \
    AND toplevel AND queryid IS NOT NULL";

#[derive(Clone, Debug, Default, Serialize)]
struct Shape {
    queryid: i64,
    query: String,
    calls: i64,
    rows: i64,
    total_exec_time_ms: f64,
    mean_exec_time_ms: f64,
    shared_blks_hit: i64,
    shared_blks_read: i64,
    plans: Option<i64>,
    baseline_calls: i64,
    #[serde(skip)]
    stats_since: String,
}

pub(super) struct Snapshot {
    info: (i64, String),
    shapes: BTreeMap<i64, Shape>,
    started_at_unix_ms: u128,
    finished_at_unix_ms: u128,
}

fn unix_ms() -> Result<u128> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis())
}

fn delta(before: &Snapshot, after: Snapshot) -> Result<Vec<Shape>> {
    ensure!(
        before.info == after.info,
        "pg_stat_statements was reset or evicted entries during the window"
    );
    ensure!(
        before.shapes.keys().all(|id| after.shapes.contains_key(id)),
        "a baseline queryid disappeared during the window"
    );
    let mut shapes = Vec::new();
    for (id, mut shape) in after.shapes {
        if let Some(prior) = before.shapes.get(&id) {
            ensure!(
                shape.stats_since == prior.stats_since,
                "queryid {id} was reset during the window"
            );
            shape.baseline_calls = prior.calls;
            shape.calls -= prior.calls;
            shape.rows -= prior.rows;
            shape.total_exec_time_ms -= prior.total_exec_time_ms;
            shape.shared_blks_hit -= prior.shared_blks_hit;
            shape.shared_blks_read -= prior.shared_blks_read;
            shape.plans = shape.plans.zip(prior.plans).map(|(a, b)| a - b);
        }
        ensure!(
            shape.calls >= 0
                && shape.rows >= 0
                && shape.total_exec_time_ms >= 0.0
                && shape.shared_blks_hit >= 0
                && shape.shared_blks_read >= 0
                && shape.plans.is_none_or(|plans| plans >= 0),
            "queryid {id} counters decreased during the window"
        );
        if shape.calls > 0 || shape.plans.is_some_and(|plans| plans > 0) {
            shape.mean_exec_time_ms = if shape.calls > 0 {
                shape.total_exec_time_ms / shape.calls as f64
            } else {
                0.0
            };
            shapes.push(shape);
        }
    }
    Ok(shapes)
}

/// Pools for administration and observation never use the workload's role.
pub(super) struct Instrument {
    admin: PgPool,
    observer: Option<PgPool>,
    pub(super) workload_url: String,
    name: String,
    dbid: i64,
    userid: i64,
    pg_version: String,
    planning: bool,
}

impl Instrument {
    pub(super) async fn open(args: &Args) -> Result<Self> {
        let url = args
            .postgres_url
            .as_deref()
            .context("--pg-statements requires --postgres-url for a private PG18 server")?;
        let options = PgConnectOptions::from_str(url)?;
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        let pg_version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&admin)
            .await?;
        let version: String = sqlx::query_scalar("SHOW server_version_num")
            .fetch_one(&admin)
            .await?;
        ensure!(
            version.parse::<u32>()? / 10000 == 18,
            "--pg-statements requires PostgreSQL 18"
        );
        let name = format!("lash_perf_{}", uuid::Uuid::new_v4().simple());
        let password = uuid::Uuid::new_v4().simple().to_string();
        // Identifiers and password contain only a fixed prefix and UUID hex.
        sqlx::query(&format!("CREATE ROLE {name} LOGIN PASSWORD '{password}'"))
            .execute(&admin)
            .await
            .context("create the private workload role")?;
        if let Err(error) = sqlx::query(&format!("CREATE DATABASE {name} OWNER {name}"))
            .execute(&admin)
            .await
        {
            sqlx::query(&format!("DROP ROLE {name}"))
                .execute(&admin)
                .await?;
            return Err(error.into());
        }
        let workload = options
            .clone()
            .database(&name)
            .username(&name)
            .password(&password);
        let mut instrument = Self {
            admin,
            observer: None,
            workload_url: workload.to_url_lossy().to_string(),
            name,
            dbid: 0,
            userid: 0,
            pg_version,
            planning: false,
        };
        let setup = instrument.prepare(options, workload).await;
        if let Err(error) = setup {
            instrument.close().await?;
            return Err(error);
        }
        Ok(instrument)
    }

    async fn prepare(
        &mut self,
        options: PgConnectOptions,
        workload: PgConnectOptions,
    ) -> Result<()> {
        let observer = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.database(&self.name))
            .await?;
        self.observer = Some(observer);
        let observer = self.observer()?;
        sqlx::query("CREATE EXTENSION pg_stat_statements")
            .execute(observer)
            .await
            .context(
                "create the private pg_stat_statements extension (the server must preload it)",
            )?;
        let track: String = sqlx::query_scalar("SHOW pg_stat_statements.track")
            .fetch_one(observer)
            .await?;
        ensure!(
            track != "none",
            "pg_stat_statements.track must be top or all"
        );
        let planning: String = sqlx::query_scalar("SHOW pg_stat_statements.track_planning")
            .fetch_one(observer)
            .await?;
        let (dbid, userid): (i64, i64) = sqlx::query_as(
            "SELECT d.oid::bigint, r.oid::bigint FROM pg_database d, pg_roles r WHERE d.datname = $1 AND r.rolname = $1"
        ).bind(&self.name).fetch_one(observer).await?;
        self.dbid = dbid;
        self.userid = userid;
        self.planning = planning == "on";
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(workload)
            .await?;
        let result = sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
            .execute(&pool)
            .await;
        pool.close().await;
        result.context("provision the baseline product schema")?;
        Ok(())
    }

    fn observer(&self) -> Result<&PgPool> {
        self.observer
            .as_ref()
            .context("statement observer is not connected")
    }

    async fn info(&self) -> Result<(i64, String)> {
        Ok(
            sqlx::query_as("SELECT dealloc, stats_reset::text FROM pg_stat_statements_info")
                .fetch_one(self.observer()?)
                .await?,
        )
    }

    pub(super) async fn snapshot(&self) -> Result<Snapshot> {
        let started_at_unix_ms = unix_ms()?;
        let info = self.info().await?;
        let rows = sqlx::query(SHAPES)
            .bind(self.dbid)
            .bind(self.userid)
            .fetch_all(self.observer()?)
            .await?;
        let mut shapes = BTreeMap::new();
        for row in rows {
            let shape = Shape {
                queryid: row.try_get("queryid")?,
                query: row
                    .try_get("query")
                    .context("pg_stat_statements query text is unavailable")?,
                calls: row.try_get("calls")?,
                rows: row.try_get("rows")?,
                total_exec_time_ms: row.try_get("total_exec_time")?,
                shared_blks_hit: row.try_get("shared_blks_hit")?,
                shared_blks_read: row.try_get("shared_blks_read")?,
                plans: if self.planning {
                    Some(row.try_get("plans")?)
                } else {
                    None
                },
                stats_since: row.try_get("stats_since")?,
                ..Default::default()
            };
            shapes.insert(shape.queryid, shape);
        }
        ensure!(
            info == self.info().await?,
            "pg_stat_statements changed generation during a snapshot"
        );
        Ok(Snapshot {
            info,
            shapes,
            started_at_unix_ms,
            finished_at_unix_ms: unix_ms()?,
        })
    }

    pub(super) async fn finish(&self, args: &Args, before: Snapshot, top: usize) -> Result<()> {
        let after = self.snapshot().await?;
        let window_finished_at_unix_ms = after.started_at_unix_ms;
        let shapes = delta(&before, after)?;
        ensure!(
            !shapes.is_empty(),
            "pg_stat_statements captured no workload statements"
        );
        let baseline_only_queryids: Vec<_> = before
            .shapes
            .keys()
            .filter(|id| !shapes.iter().any(|shape| shape.queryid == **id))
            .collect();
        let receipt = serde_json::json!({
            "kind": "lash.pg-statements", "pg_version": self.pg_version,
            "workload": {"case": "pg-facade", "identity": args.workload,
                "operations": args.operations, "callers": args.callers},
            "run": {"database": self.name, "role": self.name, "dbid": self.dbid,
                "userid": self.userid, "gate_id": std::env::var("KILN_GATE_ID").ok()},
            "window": "after_schema_node_and_session_setup_before_first_send_through_last_send_settlement_before_shutdown",
            "window_started_at_unix_ms": before.finished_at_unix_ms,
            "window_finished_at_unix_ms": window_finished_at_unix_ms,
            "filter": "dbid AND userid AND toplevel AND queryid IS NOT NULL",
            "planning_enabled": self.planning,
            "baseline_calls": before.shapes.values().map(|shape| shape.calls).sum::<i64>(),
            "baseline_only_queryids": baseline_only_queryids,
            "explain": "skipped: pg_stat_statements has no representative bound parameters",
            "statements": shapes,
        });
        let path = args.out.with_extension("pg-statements.json");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&receipt)?),
        )?;
        let text = rankings(&shapes, top);
        std::fs::write(args.out.with_extension("pg-statements.txt"), &text)?;
        print!("{text}");
        println!("pg-statements receipt: {}", path.display());
        Ok(())
    }

    pub(super) async fn close(mut self) -> Result<()> {
        if let Some(observer) = self.observer.take() {
            observer.close().await;
        }
        let database = sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.admin)
            .await;
        let role = sqlx::query(&format!("DROP ROLE {}", self.name))
            .execute(&self.admin)
            .await;
        self.admin.close().await;
        database.context("remove the private workload database")?;
        role.context("remove the private workload role")?;
        Ok(())
    }
}

fn rankings(shapes: &[Shape], top: usize) -> String {
    use std::fmt::Write as _;
    let mut text = String::from("PG statement deltas (server executions, not wire round trips)\n");
    for by_calls in [false, true] {
        let mut sorted: Vec<_> = shapes.iter().collect();
        sorted.sort_by(|a, b| {
            let order = if by_calls {
                b.calls.cmp(&a.calls)
            } else {
                b.total_exec_time_ms.total_cmp(&a.total_exec_time_ms)
            };
            order.then(a.queryid.cmp(&b.queryid))
        });
        let _ = writeln!(
            text,
            "\nTop {top} by {}\nqueryid calls rows total_ms mean_ms shared_hit shared_read plans query",
            if by_calls {
                "calls"
            } else {
                "total execution time"
            }
        );
        for shape in sorted.into_iter().take(top) {
            let query = shape.query.split_whitespace().collect::<Vec<_>>().join(" ");
            let plans = shape
                .plans
                .map_or_else(|| "disabled".into(), |plans| plans.to_string());
            let _ = writeln!(
                text,
                "{} {} {} {:.3} {:.3} {} {} {} {}",
                shape.queryid,
                shape.calls,
                shape.rows,
                shape.total_exec_time_ms,
                shape.mean_exec_time_ms,
                shape.shared_blks_hit,
                shape.shared_blks_read,
                plans,
                query
            );
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(shapes: Vec<Shape>) -> Snapshot {
        Snapshot {
            info: (0, "generation".into()),
            shapes: shapes
                .into_iter()
                .map(|shape| (shape.queryid, shape))
                .collect(),
            started_at_unix_ms: 0,
            finished_at_unix_ms: 0,
        }
    }

    /// FIG-5724: subtract setup even when it used the same queryid; derive
    /// mean time from the window, never subtract cumulative means.
    #[test]
    fn measured_deltas_exclude_setup_and_recompute_the_mean() -> Result<()> {
        let prior = Shape {
            queryid: 1,
            query: "SELECT $1".into(),
            calls: 10,
            rows: 10,
            total_exec_time_ms: 100.0,
            shared_blks_hit: 20,
            shared_blks_read: 5,
            plans: Some(4),
            ..Default::default()
        };
        let setup_only = Shape {
            queryid: 2,
            calls: 1,
            ..prior.clone()
        };
        let after = Shape {
            calls: 12,
            rows: 16,
            total_exec_time_ms: 106.0,
            shared_blks_hit: 29,
            shared_blks_read: 6,
            plans: Some(5),
            ..prior.clone()
        };
        let fresh = Shape {
            queryid: 3,
            calls: 2,
            total_exec_time_ms: 8.0,
            plans: None,
            ..Default::default()
        };
        let shapes = delta(
            &snapshot(vec![prior, setup_only.clone()]),
            snapshot(vec![after, setup_only, fresh]),
        )?;
        assert_eq!(shapes.len(), 2);
        let delta = &shapes[0];
        assert_eq!(
            (
                delta.calls,
                delta.rows,
                delta.shared_blks_hit,
                delta.shared_blks_read,
                delta.plans
            ),
            (2, 6, 9, 1, Some(1))
        );
        assert_eq!(delta.total_exec_time_ms, 6.0);
        assert_eq!(delta.mean_exec_time_ms, 3.0);
        assert_eq!(shapes[1].mean_exec_time_ms, 4.0);
        assert_eq!(shapes[1].plans, None);
        Ok(())
    }

    /// FIG-5724: an invalidated statistics window must fail, never publish
    /// plausible partial counts after a reset or eviction.
    #[test]
    fn invalidated_statistics_windows_are_refused() {
        let shape = Shape {
            queryid: 1,
            calls: 3,
            stats_since: "first".into(),
            ..Default::default()
        };
        let before = snapshot(vec![shape.clone()]);
        let mut reset = snapshot(vec![shape.clone()]);
        reset.info.1 = "reset".into();
        assert!(delta(&before, reset).is_err());
        let mut evicted = snapshot(vec![shape.clone()]);
        evicted.info.0 = 1;
        assert!(delta(&before, evicted).is_err());
        assert!(delta(&before, snapshot(vec![])).is_err());
        let changed = Shape {
            stats_since: "second".into(),
            ..shape.clone()
        };
        assert!(delta(&before, snapshot(vec![changed])).is_err());
        let decreased = Shape { calls: 2, ..shape };
        assert!(delta(&before, snapshot(vec![decreased])).is_err());
    }
}
