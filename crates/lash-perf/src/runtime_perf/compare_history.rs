//! Compare slopes use duration-trend's existing per-operation duration metric.
use super::duration_trend::{
    self, BuildMode, DurationHistoryRecord, DurationMetricHistoryValue, ExperimentIdentity,
    HISTORY_RECORD_VERSION,
};
use crate::compare::{Args, Receipt};
use anyhow::Result;
use std::collections::BTreeMap;

pub(crate) fn record(args: &Args, receipt: &Receipt) -> Result<Vec<String>> {
    let prior = if args.history.exists() {
        duration_trend::load_history(&args.history)?
    } else {
        Vec::new()
    };
    let mut records = Vec::new();
    let mut failures = Vec::new();
    for row in receipt.rows.iter().filter(|r| r.median.slope_ns > 0.0) {
        let scenario = if row.engine == "lash" {
            format!("compare/{}/{}", row.dialect, row.workload)
        } else {
            format!("compare/{}/{}/{}", row.engine, row.dialect, row.workload)
        };
        let mut identity = ExperimentIdentity::capture(
            serde_json::json!({"geometry":receipt.geometry,"source_hash":row.source_hash,"cpu":receipt.cpu,"operations":row.operations,"trips":row.trips}),
        );
        identity.backend = "kernel-page-fixture-memory/production-worker-embedding".into();
        identity.configured_durability = "none (compute only)".into();
        identity.workload = scenario.clone();
        if row.engine != "lash" {
            identity.backend = format!("external/{}", row.engine);
            identity.compiler = format!("executable-sha256={}", row.engine_identity);
            identity.allocator = "external interpreter allocator".into();
        }
        let slope_ms = row.median.slope_ns / 1e6;
        let mut slopes = row
            .samples
            .iter()
            .map(|p| crate::compare::math::fit(row.operations, [p[0].ns, p[1].ns]).slope_ns / 1e6)
            .collect::<Vec<_>>();
        slopes.sort_by(f64::total_cmp);
        let p95 = slopes[(slopes.len() * 95).div_ceil(100).saturating_sub(1)];
        let mut high = row
            .samples
            .iter()
            .map(|p| p[1].ns / 1e6)
            .collect::<Vec<_>>();
        high.sort_by(f64::total_cmp);
        let high_p95 = high[(high.len() * 95).div_ceil(100).saturating_sub(1)];
        if args.check && row.engine == "lash" {
            let baseline = prior.iter().rev().find(|r| {
                r.scenario == scenario && r.profile == "compare" && r.identity == identity
            });
            match baseline.and_then(|r| r.duration_metrics_ms.get("slope_per_operation")) {
                Some(baseline) if slope_ms > baseline.median_ms * (1.0 + args.tolerance) => {
                    failures.push(format!(
                        "{scenario}: {:.2} ns/op vs {:.2} baseline (+{:.0}% allowed)",
                        row.median.slope_ns,
                        baseline.median_ms * 1e6,
                        args.tolerance * 100.0
                    ))
                }
                Some(_) => {}
                None => failures.push(format!("{scenario}: no comparable recorded baseline")),
            }
        }
        records.push(DurationHistoryRecord {
            version: HISTORY_RECORD_VERSION,
            identity,
            scenario,
            profile: "compare".into(),
            runs: args.runs,
            warmups: args.warmups,
            turns: row.operations[1] as usize,
            build_mode: BuildMode::current(),
            commit: receipt.commit.clone(),
            run_id: receipt.run_id.clone(),
            recorded_at: receipt.recorded_at.clone(),
            total_ms: row.median_ns[1] / 1e6,
            total_p95_ms: Some(high_p95),
            duration_metrics_ms: BTreeMap::from([(
                "slope_per_operation".into(),
                DurationMetricHistoryValue {
                    median_ms: slope_ms,
                    p95_ms: p95,
                },
            )]),
            kernel: None,
            replay: None,
        });
    }
    // Failed checks must not ratchet the regression into the next baseline.
    if failures.is_empty() {
        if let Some(parent) = args.history.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        duration_trend::append_records(&args.history, &records)?;
    }
    Ok(failures)
}
