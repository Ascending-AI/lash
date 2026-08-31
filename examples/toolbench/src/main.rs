mod grading;
mod runtime;
mod tasks;
mod world;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::Value;

use crate::grading::grade;
use crate::tasks::{Task, task_pack};
use crate::world::World;

const DEFAULT_MODEL: &str = "z-ai/glm-5.3-flash";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DialectSelection {
    Both,
    Lashlang,
    Typescript,
}

impl DialectSelection {
    fn dialects(self) -> &'static [lash::rlm::RlmDialect] {
        match self {
            Self::Both => &lash::rlm::RlmDialect::ALL,
            Self::Lashlang => &[lash::rlm::RlmDialect::Lashlang],
            Self::Typescript => &[lash::rlm::RlmDialect::Typescript],
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Deterministic Lash RLM tool-calling bench")]
struct Args {
    /// OpenRouter model identifier.
    #[arg(long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    model: String,
    /// Number of independent attempts per task and dialect.
    #[arg(long, default_value_t = 1)]
    runs: usize,
    /// Run both dialects or select one.
    #[arg(long, value_enum, default_value_t = DialectSelection::Both)]
    dialect: DialectSelection,
    /// Run only one task id.
    #[arg(long)]
    task: Option<String>,
    /// Exit successfully even when one or more task rows fail.
    #[arg(long)]
    allow_partial: bool,
}

#[derive(Debug, Serialize)]
struct BenchResult {
    model: String,
    runs: usize,
    results: Vec<TaskResult>,
    summaries: Vec<Summary>,
    all_passed: bool,
}

#[derive(Debug, Serialize)]
struct TaskResult {
    run: usize,
    id: String,
    dialect: String,
    passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
    iterations: usize,
    tool_call_count: usize,
    failed_exec_iterations: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_value: Option<Value>,
    seed: World,
    checker: String,
}

#[derive(Debug, Serialize)]
struct Summary {
    dialect: String,
    passed: usize,
    total: usize,
    pass_rate: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    if args.runs == 0 {
        bail!("--runs must be at least 1");
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        bail!("OPENROUTER_API_KEY is not set; load the repository .env before running toolbench");
    }

    let tasks = selected_tasks(args.task.as_deref())?;
    let mut results = Vec::new();
    for run in 1..=args.runs {
        for &dialect in args.dialect.dialects() {
            for task in &tasks {
                eprintln!(
                    "running {}/{} {} {}",
                    run,
                    args.runs,
                    dialect.language_id(),
                    task.id
                );
                let (final_world, evidence) =
                    runtime::run_task(task, dialect, &args.model, &api_key, run).await;
                let grade = grade(task, &final_world, &evidence);
                results.push(TaskResult {
                    run,
                    id: task.id.to_string(),
                    dialect: dialect.language_id().to_string(),
                    passed: grade.passed,
                    failure_reason: grade.failure_reason,
                    iterations: evidence.iterations,
                    tool_call_count: evidence.tool_call_count,
                    failed_exec_iterations: evidence.failed_execution_errors.len(),
                    finish_value: evidence.finish_value,
                    seed: task.seed.clone(),
                    checker: task.checker_description(),
                });
            }
        }
    }

    print_table(&results);
    let summaries = summarize(&results);
    let all_passed = results.iter().all(|result| result.passed);
    let output = BenchResult {
        model: args.model,
        runs: args.runs,
        results,
        summaries,
        all_passed,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&output).context("serialize bench results")?
    );
    if !output.all_passed && !args.allow_partial {
        bail!("one or more toolbench tasks failed");
    }
    Ok(())
}

fn selected_tasks(task_id: Option<&str>) -> Result<Vec<Task>> {
    let tasks = task_pack();
    let Some(task_id) = task_id else {
        return Ok(tasks);
    };
    let selected = tasks
        .into_iter()
        .filter(|task| task.id == task_id)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("unknown task `{task_id}`");
    }
    Ok(selected)
}

fn print_table(results: &[TaskResult]) {
    eprintln!("\nrun dialect     task                 pass iter tools failed-exec reason");
    eprintln!("--- ----------- -------------------- ---- ---- ----- ----------- ------");
    for result in results {
        eprintln!(
            "{:<3} {:<11} {:<20} {:<4} {:>4} {:>5} {:>11} {}",
            result.run,
            result.dialect,
            result.id,
            if result.passed { "yes" } else { "no" },
            result.iterations,
            result.tool_call_count,
            result.failed_exec_iterations,
            result.failure_reason.as_deref().unwrap_or("")
        );
    }
    for summary in summarize(results) {
        eprintln!(
            "{}: {}/{} passed ({:.1}%)",
            summary.dialect,
            summary.passed,
            summary.total,
            summary.pass_rate * 100.0
        );
    }
}

fn summarize(results: &[TaskResult]) -> Vec<Summary> {
    lash::rlm::RlmDialect::ALL
        .iter()
        .filter_map(|dialect| {
            let matching = results
                .iter()
                .filter(|result| result.dialect == dialect.language_id())
                .collect::<Vec<_>>();
            let total = matching.len();
            (total > 0).then(|| {
                let passed = matching.iter().filter(|result| result.passed).count();
                Summary {
                    dialect: dialect.language_id().to_string(),
                    passed,
                    total,
                    pass_rate: passed as f64 / total as f64,
                }
            })
        })
        .collect()
}
