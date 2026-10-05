//! Leg evidence from a Restate server's own Prometheus metrics.
use crate::e2e::case::Leg;
use anyhow::{Result, ensure};
use std::collections::BTreeMap;
use std::path::Path;

/// Scrape every metrics endpoint, retain the bodies under `directory`, and
/// sum `restate_invoker_invocation_tasks_total` per `status` label. A `live`
/// leg only records; a `replay` leg must show the always-suspending server
/// parked at least one invocation task, or the leg never exercised replay.
pub async fn observe_leg(
    leg: Leg,
    metrics_urls: &[String],
    directory: &Path,
) -> Result<serde_json::Value> {
    let (bodies, tasks) = scrape(metrics_urls).await?;
    std::fs::write(directory.join("restate-metrics.txt"), &bodies)?;
    let receipt = serde_json::json!({
        "leg": leg.manifest(),
        "invocation_tasks": tasks,
        "metrics": "restate-metrics.txt",
    });
    if leg == Leg::Replay {
        ensure!(
            tasks.get("suspended").copied().unwrap_or_default() > 0,
            "replay leg observed no suspended invocation task; the server did not run always-suspending"
        );
    }
    Ok(receipt)
}

/// The invocation tasks the servers behind `metrics_urls` have parked so far.
/// Two reads around one Run bound the suspensions that Run's window caused.
pub async fn suspended_tasks(metrics_urls: &[String]) -> Result<u64> {
    let (_, tasks) = scrape(metrics_urls).await?;
    Ok(tasks.get("suspended").copied().unwrap_or_default())
}

/// Every metrics body, and `restate_invoker_invocation_tasks_total` summed
/// per `status` label across them.
async fn scrape(metrics_urls: &[String]) -> Result<(String, BTreeMap<String, u64>)> {
    let http = reqwest::Client::builder().no_proxy().build()?;
    let mut bodies = String::new();
    for url in metrics_urls {
        let body = http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        bodies.push_str(&format!("# {url}\n{body}\n"));
    }
    let mut tasks: BTreeMap<String, u64> = BTreeMap::new();
    for line in bodies.lines() {
        let Some(rest) = line.strip_prefix("restate_invoker_invocation_tasks_total{") else {
            continue;
        };
        let Some((labels, sample)) = rest.split_once('}') else {
            continue;
        };
        let status = labels.split(',').find_map(|label| {
            let (key, value) = label.split_once('=')?;
            (key.trim() == "status").then(|| value.trim_matches('"').to_owned())
        });
        let Some(value) = sample
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<f64>().ok())
        else {
            continue;
        };
        if let Some(status) = status {
            *tasks.entry(status).or_insert(0) += value as u64;
        }
    }
    Ok((bodies, tasks))
}
