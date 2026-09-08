use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

use crate::{ReasoningEffort, TaskResult};

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    pub(crate) cost: Option<f64>,
}
impl Usage {
    pub(crate) fn from_attempts(attempts: &[Value]) -> Self {
        let mut usage = Self::default();
        let mut costs = Vec::new();
        for row in attempts {
            usage.input += row["tokens"]["input"].as_u64().unwrap_or(0);
            usage.output += row["tokens"]["output"].as_u64().unwrap_or(0);
            usage.cache_read += row["tokens"]["cache_read"].as_u64().unwrap_or(0);
            usage.cache_write += row["tokens"]["cache_write"].as_u64().unwrap_or(0);
            costs.push(row["cost"].as_f64());
        }
        usage.cost = complete_cost(costs.into_iter());
        usage
    }
    fn tokens(&self) -> u64 {
        // Lash normalizes provider usage into disjoint input/cache buckets.
        self.input + self.output + self.cache_read + self.cache_write
    }
}
fn complete_cost(costs: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let values = costs.collect::<Option<Vec<_>>>()?;
    (!values.is_empty()).then(|| values.iter().sum())
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    model: String,
    channel: String,
    dialect: String,
    reasoning_effort: ReasoningEffort,
    passed: usize,
    rows: usize,
    rounds: usize,
    executions: usize,
    failed_exec_iterations: usize,
    tool_call_count: usize,
    expected_tool_call_count: usize,
    matched_tool_call_percent: f64,
    cost_unknown: usize,
    #[serde(flatten)]
    usage: Usage,
    wall_total_s: f64,
    wall_median_s: f64,
    tokens_per_task: f64,
    cost_per_task: Option<f64>,
}

pub(crate) fn aggregate(results: &[TaskResult]) -> Vec<Summary> {
    let mut cohorts = BTreeMap::<_, Vec<&TaskResult>>::new();
    for row in results {
        cohorts
            .entry((&row.model, &row.channel, &row.dialect))
            .or_default()
            .push(row);
    }
    cohorts
        .into_iter()
        .map(|((model, channel, dialect), rows)| {
            let mut walls = rows
                .iter()
                .map(|row| row.wall_ms as f64 / 1000.0)
                .collect::<Vec<_>>();
            walls.sort_by(f64::total_cmp);
            let middle = walls.len() / 2;
            let median = if walls.len().is_multiple_of(2) {
                (walls[middle - 1] + walls[middle]) / 2.0
            } else {
                walls[middle]
            };
            let usage = Usage {
                input: rows.iter().map(|row| row.usage.input).sum(),
                output: rows.iter().map(|row| row.usage.output).sum(),
                cache_read: rows.iter().map(|row| row.usage.cache_read).sum(),
                cache_write: rows.iter().map(|row| row.usage.cache_write).sum(),
                cost: complete_cost(rows.iter().map(|row| row.usage.cost)),
            };
            Summary {
                model: model.clone(),
                channel: channel.clone(),
                dialect: dialect.clone(),
                reasoning_effort: rows[0].reasoning_effort,
                passed: rows.iter().filter(|row| row.passed).count(),
                rows: rows.len(),
                rounds: rows.iter().map(|row| row.rounds).sum(),
                executions: rows.iter().map(|row| row.executions).sum(),
                failed_exec_iterations: rows.iter().map(|row| row.failed_exec_iterations).sum(),
                tool_call_count: rows.iter().map(|row| row.tool_call_count).sum(),
                expected_tool_call_count: rows.iter().map(|row| row.expected_tool_call_count).sum(),
                matched_tool_call_percent: 100.0
                    * rows
                        .iter()
                        .filter(|row| row.tool_call_count == row.expected_tool_call_count)
                        .count() as f64
                    / rows.len() as f64,
                cost_unknown: rows.iter().filter(|row| row.cost_unknown).count(),
                wall_total_s: walls.iter().sum(),
                wall_median_s: median,
                tokens_per_task: usage.tokens() as f64 / rows.len() as f64,
                cost_per_task: usage.cost.map(|cost| cost / rows.len() as f64),
                usage,
            }
        })
        .collect()
}

fn money(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.6}"))
        .unwrap_or_else(|| "n/a".into())
}
fn delta(value: Option<f64>, base: Option<f64>) -> String {
    match (value, base) {
        (Some(value), Some(base)) if base != 0.0 => {
            format!("{:+.1}%", (value / base - 1.0) * 100.0)
        }
        _ => "n/a".into(),
    }
}

pub(crate) fn markdown(summaries: &[Summary]) -> String {
    let mut out = String::new();
    let models = summaries
        .iter()
        .map(|row| &row.model)
        .collect::<std::collections::BTreeSet<_>>();
    for model in models {
        let rows = summaries
            .iter()
            .filter(|row| &row.model == model)
            .collect::<Vec<_>>();
        writeln!(
            out,
            "## {model} (reasoning: {})\n",
            rows[0].reasoning_effort.name()
        )
        .unwrap();
        writeln!(out, "| Cohort | Pass/rows | Input | Output | Cache read | Cache write | Cost USD | Wall total s | Wall median s | Tokens/task | Cost/task USD | Executions | Failed exec | Host calls | Expected N | Matched N | Rounds | Cost unknown |").unwrap();
        writeln!(
            out,
            "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        )
        .unwrap();
        for row in &rows {
            writeln!(
                out,
                "| {}/{} | {}/{} | {} | {} | {} | {} | {} | {:.3} | {:.3} | {:.1} | {} | {} | {} | {} | {} | {:.1}% | {} | {} |",
                row.channel,
                row.dialect,
                row.passed,
                row.rows,
                row.usage.input,
                row.usage.output,
                row.usage.cache_read,
                row.usage.cache_write,
                money(row.usage.cost),
                row.wall_total_s,
                row.wall_median_s,
                row.tokens_per_task,
                money(row.cost_per_task),
                row.executions, row.failed_exec_iterations, row.tool_call_count, row.expected_tool_call_count, row.matched_tool_call_percent, row.rounds, row.cost_unknown
            )
            .unwrap();
        }
        out.push('\n');
        for native in rows.iter().filter(|row| row.channel == "native") {
            if let Some(cell) = rows
                .iter()
                .find(|row| row.channel == "cell" && row.dialect == native.dialect)
            {
                comparison(
                    &mut out,
                    &format!("native vs cell ({})", native.dialect),
                    native,
                    cell,
                );
            }
            if let Some(standard) = rows.iter().find(|row| row.channel == "standard") {
                comparison(
                    &mut out,
                    &format!("standard vs native ({})", native.dialect),
                    standard,
                    native,
                );
            }
        }
        out.push('\n');
    }
    out.push_str("Tokens = input + output + cache read + cache write (Lash uses disjoint usage buckets). Costs are n/a if any attempt lacks provider cost. Deltas compare per-task means; wall totals sum task durations and are not elapsed run time. Failed rows are included.\n");
    out
}
fn comparison(out: &mut String, label: &str, value: &Summary, base: &Summary) {
    writeln!(
        out,
        "- {label}: Δ tokens {}, Δ cost {}, Δ wall {}.",
        delta(Some(value.tokens_per_task), Some(base.tokens_per_task)),
        delta(value.cost_per_task, base.cost_per_task),
        delta(
            Some(value.wall_total_s / value.rows as f64),
            Some(base.wall_total_s / base.rows as f64)
        )
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aggregation_counts_retries_cache_cost_and_even_median() {
        let usage = Usage::from_attempts(&[
            json!({"tokens":{"input":100,"output":10,"cache_read":40},"cost":0.02}),
            json!({"tokens":{"input":50,"output":5,"cache_read":10},"cost":0.01}),
        ]);
        let make = |wall_ms, passed| TaskResult {
            model: "model".into(),
            reasoning_effort: ReasoningEffort::Medium,
            run: 1,
            id: "task".into(),
            dialect: "none".into(),
            channel: "standard".into(),
            wall_ms,
            passed,
            failure_reason: None,
            rounds: 2,
            iterations: 2,
            executions: 0,
            expected_tool_call_count: 1,
            cost_unknown: false,
            max_task_cost_usd: 0.10,
            turn_wall_limit_secs: 120,
            tool_call_count: 1,
            submit_count: 1,
            failed_exec_iterations: 0,
            finish_value: Some(json!(1)),
            seed: crate::world::World::seeded(),
            checker: String::new(),
            usage: usage.clone(),
        };
        let mut rows = vec![make(1000, true), make(3000, false)];
        let summary = aggregate(&rows).remove(0);
        assert_eq!((summary.passed, summary.rows, summary.rounds), (1, 2, 4));
        assert_eq!(
            (
                summary.usage.input,
                summary.usage.output,
                summary.usage.cache_read
            ),
            (300, 30, 100)
        );
        assert_eq!(summary.usage.cost, Some(0.06));
        assert_eq!(
            (
                summary.wall_total_s,
                summary.wall_median_s,
                summary.tokens_per_task
            ),
            (4.0, 2.0, 215.0)
        );
        assert_eq!(summary.matched_tool_call_percent, 100.0);
        rows[1].usage.cost = None;
        rows[1].cost_unknown = true;
        rows[1].tool_call_count = 2;
        assert_eq!(aggregate(&rows)[0].cost_unknown, 1);
        assert_eq!(aggregate(&rows)[0].matched_tool_call_percent, 50.0);
        assert_eq!(aggregate(&rows)[0].usage.cost, None);
        assert!(markdown(&aggregate(&rows)).contains("n/a"));
        assert_eq!(Usage::from_attempts(&[]).cost, None);
        assert_eq!(
            Usage::from_attempts(&[json!({"cost":1}), json!({})]).cost,
            None
        );
    }
}
