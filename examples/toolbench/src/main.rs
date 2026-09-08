mod grading;
mod runtime;
mod tasks;
mod telemetry;
mod world;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde::Serialize;
use serde_json::Value;
use std::io::Write as _;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ChannelSelection {
    Cell,
    #[value(alias = "native_tool")]
    Native,
}
impl ChannelSelection {
    fn channel(self) -> lash::rlm::RlmChannel {
        match self {
            Self::Cell => lash::rlm::RlmChannel::Cell,
            Self::Native => lash::rlm::RlmChannel::NativeTool,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Cell => "cell",
            Self::Native => "native",
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Deterministic Lash RLM tool-calling bench")]
struct Args {
    #[arg(long, env = "LASH_RLM_CHANNEL", value_enum, default_value_t = ChannelSelection::Cell)]
    channel: ChannelSelection,
    /// Pair the same task/model/dialect in randomized channel order.
    #[arg(long)]
    paired: bool,
    #[arg(long, default_value_t = 1)]
    repetitions: usize,
    /// Per-provider-attempt machine-readable evidence, including retries.
    #[arg(long, default_value = "toolbench-results.jsonl")]
    results_file: std::path::PathBuf,
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
    channel: String,
    wall_ms: u128,
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
    channel: String,
    passed: usize,
    total: usize,
    pass_rate: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    if args.runs == 0 || args.repetitions == 0 {
        bail!("--runs must be at least 1");
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        bail!("OPENROUTER_API_KEY is not set; load the repository .env before running toolbench");
    }

    let tasks = selected_tasks(args.task.as_deref())?;
    let mut results = Vec::new();
    let mut results_file =
        std::fs::File::create(&args.results_file).context("create results JSONL")?;
    let mut supported = Vec::new();
    for &dialect in args.dialect.dialects() {
        if (args.paired || args.channel == ChannelSelection::Native)
            && let Err(reason) = runtime::preflight(&tasks[0], dialect, &args.model, &api_key).await
        {
            writeln!(
                results_file,
                "{}",
                serde_json::json!({"kind":"excluded_route","route":"openrouter","model":args.model,"dialect":dialect.language_id(),"reason":reason})
            )?;
            eprintln!(
                "excluded {} / {}: {reason}",
                args.model,
                dialect.language_id()
            );
            continue;
        }
        supported.push(dialect);
    }
    let runs = if args.paired {
        args.repetitions
    } else {
        args.runs
    };
    let mut random = std::fs::File::open("/dev/urandom").context("open random order source")?;
    for run in 1..=runs {
        for &dialect in &supported {
            for task in &tasks {
                let mut channels = if args.paired {
                    vec![ChannelSelection::Cell, ChannelSelection::Native]
                } else {
                    vec![args.channel]
                };
                let mut coin = [0u8];
                std::io::Read::read_exact(&mut random, &mut coin)?;
                if coin[0] & 1 == 1 {
                    channels.reverse();
                }
                for channel in channels {
                    eprintln!(
                        "running {run}/{runs} {} {} {}",
                        channel.name(),
                        dialect.language_id(),
                        task.id
                    );
                    let (final_world, evidence) = runtime::run_task(
                        task,
                        dialect,
                        &args.model,
                        &api_key,
                        run,
                        channel.channel(),
                    )
                    .await;
                    let grade = grade(task, &final_world, &evidence);
                    for attempt in &evidence.attempts {
                        let mut row = attempt.clone();
                        let fields = row.as_object_mut().expect("attempt is an object");
                        fields.extend(serde_json::json!({"kind":"attempt", "task":task.id,"model":args.model,"route":"openrouter","dialect":dialect.language_id(),"channel":channel.name(),"repetition":run,"success":grade.passed,"grade":grade,"task_wall_ms":evidence.wall_ms}).as_object().expect("metadata object").clone());
                        writeln!(results_file, "{row}")?;
                    }
                    writeln!(
                        results_file,
                        "{}",
                        serde_json::json!({"kind":"task_result","task":task.id,"model":args.model,"route":"openrouter","dialect":dialect.language_id(),"channel":channel.name(),"repetition":run,"success":grade.passed,"grade":grade,"wall_ms":evidence.wall_ms})
                    )?;
                    results_file.flush()?;
                    results.push(TaskResult {
                        run,
                        id: task.id.to_string(),
                        dialect: dialect.language_id().to_string(),
                        channel: channel.name().to_string(),
                        wall_ms: evidence.wall_ms,
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
    }
    print_table(&results);
    let summaries = summarize(&results);
    let all_passed = !results.is_empty() && results.iter().all(|result| result.passed);
    let output = BenchResult {
        model: args.model,
        runs,
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
    eprintln!("\nrun channel dialect     task                 pass iter tools failed-exec reason");
    eprintln!("--- ------- ----------- -------------------- ---- ---- ----- ----------- ------");
    for result in results {
        eprintln!(
            "{:<3} {:<7} {:<11} {:<20} {:<4} {:>4} {:>5} {:>11} {}",
            result.run,
            result.channel,
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
            "{} / {}: {}/{} passed ({:.1}%)",
            summary.dialect,
            summary.channel,
            summary.passed,
            summary.total,
            summary.pass_rate * 100.0
        );
    }
}

fn summarize(results: &[TaskResult]) -> Vec<Summary> {
    let mut summaries = Vec::new();
    for dialect in lash::rlm::RlmDialect::ALL {
        for channel in ["cell", "native"] {
            let matching = results
                .iter()
                .filter(|row| row.dialect == dialect.language_id() && row.channel == channel)
                .collect::<Vec<_>>();
            if matching.is_empty() {
                continue;
            }
            let passed = matching.iter().filter(|row| row.passed).count();
            summaries.push(Summary {
                dialect: dialect.language_id().to_string(),
                channel: channel.to_string(),
                passed,
                total: matching.len(),
                pass_rate: passed as f64 / matching.len() as f64,
            });
        }
    }
    summaries
}
