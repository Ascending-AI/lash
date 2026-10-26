//! Diagnostic PG18 server waits. Unknown coverage refuses attribution.
use std::collections::BTreeMap;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row as _};

use super::{Args, pg_statements::Instrument};

#[derive(Debug, Deserialize, Serialize)]
struct Coverage {
    generation: u64,
    samples: u64,
    first_us: i64,
    last_us: i64,
    max_gap_us: i64,
    missed_intervals: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Key {
    pid: i32,
    queryid: i64,
    event_type: String,
    event: String,
}

pub(super) struct Capture {
    coverage: Coverage,
    before: BTreeMap<Key, i64>,
    backends: BTreeMap<i32, String>,
    started_us: i64,
}
impl Capture {
    pub async fn start(instrument: &Instrument) -> Result<Self> {
        let pool = instrument.observer()?;
        sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_wait_sampling")
            .execute(pool)
            .await?;
        sqlx::query("CREATE OR REPLACE FUNCTION lash_wait_sampling_coverage() RETURNS text AS '$libdir/pg_wait_sampling', 'lash_wait_sampling_coverage' LANGUAGE C VOLATILE STRICT").execute(pool).await?;
        for (setting, expected) in [
            ("profile_period", "10"),
            ("history_period", "10"),
            ("profile_pid", "on"),
            ("profile_queries", "top"),
            ("sample_cpu", "off"),
        ] {
            let actual: String = sqlx::query_scalar(&format!("SHOW pg_wait_sampling.{setting}"))
                .fetch_one(pool)
                .await?;
            ensure!(
                actual == expected,
                "pg_wait_sampling.{setting} must be {expected}"
            );
        }
        let backends = backends(instrument).await?;
        ensure!(
            !backends.is_empty(),
            "missing workload backend registration"
        );
        let coverage = coverage(pool).await?;
        ensure!(coverage.samples > 0, "wait sampler has no coverage");
        let before = profile(pool).await?;
        let started_us = clock(pool).await?;
        Ok(Self {
            coverage,
            before,
            backends,
            started_us,
        })
    }
    pub async fn finish(self, instrument: &Instrument, args: &Args) -> Result<()> {
        let pool = instrument.observer()?;
        let ended_us = clock(pool).await?;
        let after = profile(pool).await?;
        let coverage = coverage(pool).await?;
        let backends = backends(instrument).await?;
        let mut refusals = Vec::new();
        if coverage.generation != self.coverage.generation {
            refusals.push("sampler reset or collector restarted");
        }
        if backends != self.backends {
            refusals.push("workload backend population changed or PID reused");
        }
        if coverage.samples <= self.coverage.samples
            || coverage.first_us > self.started_us
            || coverage.last_us < ended_us - 20_000
            || coverage.missed_intervals != self.coverage.missed_intervals
        {
            refusals.push(
                "missing sampling coverage: no progress, uncovered edge, or >20ms sampling gap",
            );
        }
        if self
            .before
            .iter()
            .any(|(k, v)| after.get(k).is_none_or(|next| next < v))
        {
            refusals.push("wait profile reset or lost baseline entries");
        }
        let shape_rows = sqlx::query("SELECT queryid, query FROM pg_stat_statements WHERE dbid=$1::bigint::oid AND userid=$2::bigint::oid AND toplevel")
            .bind(instrument.dbid).bind(instrument.userid).fetch_all(pool).await?;
        let shapes: BTreeMap<i64, String> = shape_rows
            .into_iter()
            .map(|r| (r.get("queryid"), r.get("query")))
            .collect();
        let mut rows = Vec::new();
        for (key, count) in &after {
            if !self.backends.contains_key(&key.pid) {
                continue;
            }
            let baseline = self.before.get(key).copied().unwrap_or(0);
            let samples = count - baseline;
            if samples <= 0 {
                continue;
            }
            // Idle ClientRead samples may carry the prior query ID. Never call them query execution waits.
            if key.queryid != 0 && !shapes.contains_key(&key.queryid) {
                refusals.push("sample query ID is missing from the workload statement cohort");
            }
            rows.push(
                serde_json::json!({"pid":key.pid,"backend_start":self.backends[&key.pid],
                "queryid":key.queryid,"query_shape":shapes.get(&key.queryid),
                "event_type":key.event_type,"event":key.event,"before":baseline,"after":count,
                "samples":samples,"occupancy_estimate_ms":samples*10}),
            );
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row["samples"].as_i64().unwrap_or(0)));
        let all = rows.clone();
        rows.truncate(10);
        let receipt = serde_json::json!({"kind":"lash.pg-waits", "attributed":refusals.is_empty(),
            "refusals":refusals,"sample_period_ms":10,"started_us":self.started_us,"ended_us":ended_us,
            "coverage_before":self.coverage,"coverage_after":coverage,"backend_identities":self.backends,
            "top_10":if refusals.is_empty() {rows} else {Vec::new()},"raw_workload_deltas":all,
            "profile_before":self.before.iter().map(|(k,n)| serde_json::json!({"key":k,"count":n})).collect::<Vec<_>>(),
            "profile_after":after.iter().map(|(k,n)| serde_json::json!({"key":k,"count":n})).collect::<Vec<_>>(),
            "limits":"sample count times 10ms estimates occupancy; not per-invocation duration or proof of CPU; Client waits can carry a prior query ID; diagnostic snapshot/query observer cost included"});
        let path = args.out.with_file_name("pg-waits.json");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&receipt)?),
        )?;
        println!("pg-waits receipt: {}", path.display());
        ensure!(
            refusals.is_empty(),
            "Postgres wait attribution refused: {}",
            refusals.join("; ")
        );
        Ok(())
    }
}
async fn coverage(pool: &PgPool) -> Result<Coverage> {
    let raw: String = sqlx::query_scalar("SELECT lash_wait_sampling_coverage()")
        .fetch_one(pool)
        .await?;
    serde_json::from_str(&raw).context("decode diagnostic wait coverage")
}
async fn clock(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT ((extract(epoch FROM clock_timestamp())-946684800)*1000000)::bigint",
    )
    .fetch_one(pool)
    .await?)
}
async fn backends(instrument: &Instrument) -> Result<BTreeMap<i32, String>> {
    Ok(sqlx::query("SELECT pid, backend_start::text FROM pg_stat_activity WHERE datid=$1::bigint::oid AND usesysid=$2::bigint::oid")
        .bind(instrument.dbid).bind(instrument.userid).fetch_all(instrument.observer()?).await?.into_iter()
        .map(|r|(r.get("pid"),r.get("backend_start"))).collect())
}
async fn profile(pool: &PgPool) -> Result<BTreeMap<Key, i64>> {
    Ok(
        sqlx::query("SELECT pid,queryid,event_type,event,count FROM pg_wait_sampling_profile")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| {
                (
                    Key {
                        pid: r.get("pid"),
                        queryid: r.get("queryid"),
                        event_type: r.get("event_type"),
                        event: r.get("event"),
                    },
                    r.get("count"),
                )
            })
            .collect(),
    )
}
