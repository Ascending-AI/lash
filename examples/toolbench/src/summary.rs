use crate::TaskResult;
pub(crate) use crate::accounting::Usage;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    model: String,
    channel: String,
    dialect: String,
    reasoning_effort: crate::ReasoningEffort,
    passed: usize,
    rows: usize,
    rounds: usize,
    provider_calls: usize,
    retries: usize,
    cost_unknown: usize,
    #[serde(flatten)]
    usage: Usage,
    wall_total_s: f64,
    wall_median_s: f64,
    prompt_per_task: Option<f64>,
    completion_per_task: Option<f64>,
    reasoning_per_task: Option<f64>,
    cost_per_task: Option<f64>,
    rounds_per_task: f64,
    system_prompt_tokens_first_call_mean: Option<f64>,
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
                .map(|r| r.wall_ms as f64 / 1000.0)
                .collect::<Vec<_>>();
            walls.sort_by(f64::total_cmp);
            let n = rows.len();
            let median = if n.is_multiple_of(2) {
                (walls[n / 2 - 1] + walls[n / 2]) / 2.0
            } else {
                walls[n / 2]
            };
            let values = rows
                .iter()
                .map(|r| serde_json::to_value(&r.usage).unwrap())
                .collect::<Vec<_>>();
            let usage = Usage::from_attempts(&values);
            let mean = |v: Option<u64>| v.map(|v| v as f64 / n as f64);
            Summary {
                model: model.clone(),
                channel: channel.clone(),
                dialect: dialect.clone(),
                reasoning_effort: rows[0].reasoning_effort,
                passed: rows.iter().filter(|r| r.passed).count(),
                rows: n,
                rounds: rows.iter().map(|r| r.rounds).sum(),
                provider_calls: rows.iter().map(|r| r.provider_calls).sum(),
                retries: rows.iter().map(|r| r.retries).sum(),
                cost_unknown: rows.iter().filter(|r| r.cost_unknown).count(),
                wall_total_s: walls.iter().sum(),
                wall_median_s: median,
                prompt_per_task: mean(usage.prompt_tokens_total),
                completion_per_task: mean(usage.completion_tokens),
                reasoning_per_task: mean(usage.reasoning_tokens),
                cost_per_task: usage.cost.map(|v| v / n as f64),
                rounds_per_task: rows.iter().map(|r| r.rounds).sum::<usize>() as f64 / n as f64,
                system_prompt_tokens_first_call_mean: mean(
                    rows.iter().map(|r| r.system_prompt_tokens_first_call).sum(),
                ),
                usage,
            }
        })
        .collect()
}
fn number<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into())
}
fn decimal(v: Option<f64>, places: usize) -> String {
    v.map(|v| format!("{v:.places$}"))
        .unwrap_or_else(|| "n/a".into())
}
fn delta(v: Option<f64>, b: Option<f64>) -> String {
    match (v, b) {
        (Some(v), Some(b)) if b != 0.0 => format!("{:+.1}%", (v / b - 1.0) * 100.0),
        _ => "n/a".into(),
    }
}
pub(crate) fn markdown(summaries: &[Summary]) -> String {
    let mut out = String::new();
    for model in summaries
        .iter()
        .map(|r| &r.model)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let rows = summaries
            .iter()
            .filter(|r| &r.model == model)
            .collect::<Vec<_>>();
        writeln!(
            out,
            "## {model} (reasoning: {})\n",
            rows[0].reasoning_effort.name()
        )
        .unwrap();
        out.push_str("| Cohort | Pass | Attempts | Prompt total | of which cached (read) | Cache write | Completion | Reasoning | Cost USD | Wall total/median s | Prompt/task | Completion/task | Reasoning/task | Cost/task USD | Attempts/task | First prompt mean | Retries |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        for r in &rows {
            writeln!(out,"| {}/{} | {}/{} | {} | {} | {} | {} | {} | {} | {} | {:.3}/{:.3} | {} | {} | {} | {} | {:.2} | {} | {} |",r.channel,r.dialect,r.passed,r.rows,r.rounds,number(r.usage.prompt_tokens_total),number(r.usage.cache_read),number(r.usage.cache_write),number(r.usage.completion_tokens),number(r.usage.reasoning_tokens),decimal(r.usage.cost,6),r.wall_total_s,r.wall_median_s,decimal(r.prompt_per_task,1),decimal(r.completion_per_task,1),decimal(r.reasoning_per_task,1),decimal(r.cost_per_task,6),r.rounds_per_task,decimal(r.system_prompt_tokens_first_call_mean,1),r.retries).unwrap();
        }
        out.push('\n');
        for r in &rows {
            if let Some(base) = rows.iter().find(|b| b.channel == "standard")
                && r.channel != "standard"
            {
                writeln!(out,"- {}/{} vs standard: Δ prompt/task {}, Δ completion/task {}, Δ reasoning/task {}, Δ cost/task {}, Δ attempts/task {}.",r.channel,r.dialect,delta(r.prompt_per_task,base.prompt_per_task),delta(r.completion_per_task,base.completion_per_task),delta(r.reasoning_per_task,base.reasoning_per_task),delta(r.cost_per_task,base.cost_per_task),delta(Some(r.rounds_per_task),Some(base.rounds_per_task))).unwrap();
            }
        }
        out.push('\n');
    }
    out.push_str("Prompt total includes uncached, cache read and cache write. Reasoning is INCLUDED in completion. Attempts count actual provider attempts (including retries); protocol rounds are separately recorded on attempts. First prompt includes protocol, task and tool definitions, not just the system prompt. Missing metering makes sums n/a. Failed tasks are included; preflight is reported separately. Wall totals sum task durations, not elapsed run time. All cohorts route through the local recorder hop; its latency and capture I/O are inside measured wall time.\n");
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn arithmetic_keeps_missing_usage_unknown_and_reasoning_in_completion() {
        let u = Usage::from_attempts(&[
            json!({"prompt_tokens_total":150,"prompt_uncached":100,"cache_read":40,"cache_write":10,"completion_tokens":20,"reasoning_tokens":15,"cost_usd":0.02}),
            json!({"prompt_tokens_total":200,"prompt_uncached":50,"cache_read":150,"cache_write":0,"completion_tokens":30,"reasoning_tokens":25,"cost_usd":0.01}),
        ]);
        assert_eq!(u.prompt_tokens_total, Some(350));
        assert_eq!(u.completion_tokens, Some(50));
        assert_eq!(u.reasoning_tokens, Some(40));
        assert_eq!(u.cost, Some(0.03));
        assert_eq!(Usage::from_attempts(&[json!({})]).cost, None);
        assert_eq!(Usage::from_attempts(&[]).cost, Some(0.0));
        assert_eq!(delta(Some(300.0), Some(100.0)), "+200.0%");
    }

    #[test]
    fn cohort_sums_unknowns_medians_and_per_task_means() {
        let usage = Usage::from_raw(
            &json!({"prompt_tokens":350,"completion_tokens":50,"prompt_tokens_details":{"cached_tokens":190,"cache_write_tokens":10},"completion_tokens_details":{"reasoning_tokens":40},"cost":0.03}),
        );
        let make = |wall_ms, passed| TaskResult {
            model: "model".into(),
            reasoning_effort: crate::ReasoningEffort::Medium,
            run: 1,
            id: "task".into(),
            dialect: "none".into(),
            channel: "standard".into(),
            wall_ms,
            passed,
            failure_reason: None,
            rounds: 2,
            provider_calls: 2,
            system_prompt_tokens_first_call: Some(150),
            iterations: 2,
            executions: 0,
            expected_tool_call_count: 1,
            cost_unknown: false,
            max_task_cost_usd: 0.10,
            turn_wall_limit_secs: 120,
            tool_call_count: 1,
            submit_count: 1,
            submit_values: vec![Some(json!(1))],
            retries: 1,
            provider_attempts: 2,
            turn_outcome: None,
            error: None,
            failed_exec_iterations: 0,
            finish_value: Some(json!(1)),
            seed: crate::world::World::seeded(),
            checker: String::new(),
            usage: usage.clone(),
        };

        let mut rows = vec![make(1000, true), make(3000, false)];
        let summary = aggregate(&rows).remove(0);
        assert_eq!((summary.passed, summary.rows, summary.rounds), (1, 2, 4));
        assert_eq!(summary.usage.prompt_tokens_total, Some(700));
        assert_eq!(summary.usage.completion_tokens, Some(100));
        assert_eq!(summary.usage.reasoning_tokens, Some(80));
        assert_eq!(summary.usage.cost, Some(0.06));
        assert_eq!((summary.wall_total_s, summary.wall_median_s), (4.0, 2.0));
        assert_eq!(summary.prompt_per_task, Some(350.0));
        assert_eq!(summary.system_prompt_tokens_first_call_mean, Some(150.0));
        let report = markdown(&[summary]);
        assert!(report.contains("| Prompt total |"));
        assert!(report.contains("| Attempts |"));
        assert!(report.contains("| Attempts/task |"));
        assert!(report.contains("| Retries |"));
        assert!(report.contains("local recorder hop"));
        assert!(!report.contains("Rounds"));
        rows[1].usage.prompt_tokens_total = None;
        rows[1].usage.cost = None;
        rows[1].cost_unknown = true;
        assert_eq!(aggregate(&rows)[0].usage.prompt_tokens_total, None);
        assert_eq!(aggregate(&rows)[0].usage.cost, None);
    }
}
