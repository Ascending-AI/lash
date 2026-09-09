//! Independent OpenRouter generation records. Never turn missing evidence into a match.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::Path;

fn comparison(row: &Value, data: &Value, local: &str, remote: &str, tolerance: f64) -> Value {
    let a = &row[local];
    let b = &data[remote];
    let status = if tolerance == 0.0 {
        match (a.as_u64(), b.as_u64()) {
            (Some(a), Some(b)) if a == b => "match",
            (Some(_), Some(_)) => "mismatch",
            _ => "unavailable",
        }
    } else {
        match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) if (a - b).abs() <= tolerance => "match",
            (Some(_), Some(_)) => "mismatch",
            _ => "unavailable",
        }
    };
    json!({"field":local,"generation_field":remote,"row":a,"generation":b,"status":status})
}
pub(crate) fn diff(row: &Value, data: &Value) -> Vec<Value> {
    [
        ("prompt_tokens_total", "tokens_prompt", 0.0),
        ("completion_tokens", "tokens_completion", 0.0),
        ("prompt_tokens_total", "native_tokens_prompt", 0.0),
        ("completion_tokens", "native_tokens_completion", 0.0),
        ("cache_read", "native_tokens_cached", 0.0),
        ("reasoning_tokens", "native_tokens_reasoning", 0.0),
        ("cost_usd", "total_cost", 1e-6),
    ]
    .into_iter()
    .map(|(l, r, t)| comparison(row, data, l, r, t))
    .collect()
}
fn sample(rows: Vec<Value>) -> Result<Vec<Value>> {
    let mut groups = BTreeMap::<_, Vec<Value>>::new();
    for row in rows {
        if row["kind"] == "attempt" {
            groups
                .entry((
                    row["model"].to_string(),
                    row["channel"].to_string(),
                    row["dialect"].to_string(),
                ))
                .or_default()
                .push(row);
        }
    }
    let mut random = std::fs::File::open("/dev/urandom")?;
    let mut selected = Vec::new();
    for group in groups.values_mut() {
        // Fisher-Yates, with rejection to avoid modulo bias.
        for i in (1..group.len()).rev() {
            let bound = (i + 1) as u64;
            let limit = u64::MAX - u64::MAX % bound;
            let index = loop {
                let mut bytes = [0; 8];
                random.read_exact(&mut bytes)?;
                let n = u64::from_ne_bytes(bytes);
                if n < limit {
                    break (n % bound) as usize;
                }
            };
            group.swap(i, index);
        }
    }
    let target = 30.max(groups.len());
    while selected.len() < target {
        let mut progress = false;
        for group in groups.values_mut() {
            if let Some(row) = group.pop() {
                selected.push(row);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    Ok(selected)
}
pub(crate) async fn run(path: &Path, key: &str) -> Result<()> {
    let rows = std::fs::read_to_string(path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<Value>, _>>()?;
    let selected = sample(rows)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let mut output = std::fs::File::create(format!("{}.reconcile.jsonl", path.display()))?;
    let mut report = String::from(
        "# OpenRouter reconciliation\n\nStratified random sample of attempt rows, balanced across model/channel/dialect. Exact token comparisons; cost tolerance 0.000001 USD. Both normalized and native generation token counters are shown; counters with different tokenizers are not substituted silently. Cache discount is retained as money, not interpreted as cached tokens.\n\n",
    );
    let mut mismatches = 0;
    let mut unavailable = 0;
    let mut queries = 0;
    for row in &selected {
        let id = row["provider_response_id"].as_str();
        let response = if let Some(id) = id {
            let mut result = None;
            for attempt in 0..3 {
                let fetched = client
                    .get("https://openrouter.ai/api/v1/generation")
                    .query(&[("id", id)])
                    .bearer_auth(key)
                    .send()
                    .await;
                match fetched {
                    Ok(response) if response.status().is_success() => {
                        result = Some(
                            response
                                .json::<Value>()
                                .await
                                .context("decode generation record")?,
                        );
                        break;
                    }
                    Ok(response) => {
                        result = Some(
                            json!({"error":{"status":response.status().as_u16(),"body":response.text().await.unwrap_or_default()}}),
                        );
                    }
                    Err(error) => {
                        result = Some(json!({"error":{"message":error.to_string()}}));
                    }
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
            queries += 1;
            result.unwrap_or(Value::Null)
        } else {
            json!({"error":{"message":"attempt has no provider_response_id"}})
        };
        let comparisons = diff(row, &response["data"]);
        let bad = comparisons
            .iter()
            .filter(|c| c["status"] == "mismatch")
            .count();
        let missing = comparisons
            .iter()
            .filter(|c| c["status"] == "unavailable")
            .count();
        mismatches += bad;
        unavailable += missing;
        let evidence = crate::provider_log::redact(
            json!({"kind":"reconcile","model":row["model"],"channel":row["channel"],"dialect":row["dialect"],"task":row["task"],"repetition":row["repetition"],"round":row["round"],"provider_response_id":id,"comparisons":comparisons,"generation":response,"mismatches":bad,"unavailable":missing}),
            key,
        );
        writeln!(output, "{evidence}")?;
        use std::fmt::Write as _;
        writeln!(
            report,
            "## {} / {} / {} / {} / round {} ({})\n",
            row["model"],
            row["channel"],
            row["dialect"],
            row["task"],
            row["round"],
            id.unwrap_or("missing id")
        )
        .unwrap();
        report.push_str("| Row field | Generation field | Row | Generation | Result |\n|---|---|---:|---:|---|\n");
        for c in &comparisons {
            writeln!(
                report,
                "| {} | {} | {} | {} | {} |",
                c["field"].as_str().unwrap(),
                c["generation_field"].as_str().unwrap(),
                c["row"],
                c["generation"],
                c["status"].as_str().unwrap()
            )
            .unwrap();
        }
        writeln!(
            report,
            "\ncache_discount: {}; fetch error: {}\n",
            response["data"]["cache_discount"], evidence["generation"]["error"]
        )
        .unwrap();
    }
    let counts = format!(
        "Sampled {} rows; queried {queries}; mismatching fields {mismatches}; unavailable fields {unavailable}.\n",
        selected.len()
    );
    report.insert_str(0, &format!("{counts}\n"));
    std::fs::write(format!("{}.reconcile.md", path.display()), report)?;
    print!("{counts}");
    if selected.len() < 30 || mismatches > 0 || unavailable > 0 {
        anyhow::bail!(
            "reconciliation has insufficient, mismatching or unavailable evidence; see report"
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_tokens_cost_tolerance_and_absence_are_independent() {
        assert_eq!(
            comparison(
                &json!({"n":9007199254740992_u64}),
                &json!({"n":9007199254740993_u64}),
                "n",
                "n",
                0.0
            )["status"],
            "mismatch"
        );
        let row = json!({"prompt_tokens_total":100,"completion_tokens":20,"cache_read":50,"reasoning_tokens":10,"cost_usd":0.001});
        let data = json!({"tokens_prompt":99,"tokens_completion":20,"native_tokens_prompt":100,"native_tokens_completion":20,"native_tokens_cached":50,"native_tokens_reasoning":10,"total_cost":0.0010009});
        let c = diff(&row, &data);
        assert_eq!(c.iter().filter(|v| v["status"] == "mismatch").count(), 1);
        assert_eq!(c[6]["status"], "match");
        assert_eq!(
            comparison(
                &row,
                &json!({"total_cost":0.001002}),
                "cost_usd",
                "total_cost",
                1e-6
            )["status"],
            "mismatch"
        );
        assert!(
            diff(&row, &Value::Null)
                .iter()
                .all(|v| v["status"] == "unavailable")
        );
    }
    #[test]
    fn sample_covers_every_cohort_without_duplicate_rows() {
        let rows=(0..5).flat_map(|cohort|(0..10).map(move |i|json!({"kind":"attempt","model":"m","channel":cohort,"dialect":"d","task":i}))).collect();
        let selected = sample(rows).unwrap();
        assert_eq!(selected.len(), 30);
        for c in 0..5 {
            assert_eq!(selected.iter().filter(|r| r["channel"] == c).count(), 6);
        }
        assert_eq!(
            selected
                .iter()
                .map(Value::to_string)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            30
        );
    }
}
