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
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

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
    /// Maximum simultaneous task runs; start at 4–8 for OpenRouter.
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
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
        bail!("--runs and --repetitions must be at least 1");
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        bail!("OPENROUTER_API_KEY is not set; load the repository .env before running toolbench");
    }

    let tasks = selected_tasks(args.task.as_deref())?;
    if args.concurrency == 0 {
        bail!("--concurrency must be at least 1");
    }
    let writer =
        Mutex::new(std::fs::File::create(&args.results_file).context("create results JSONL")?);
    let supported = args.dialect.dialects();
    // Capability is a route-level probe, performed once before any work starts.
    if (args.paired || args.channel == ChannelSelection::Native)
        && let Err(reason) =
            runtime::preflight(&tasks[0], supported[0], &args.model, &api_key).await
    {
        let row = serde_json::json!({"kind":"excluded_route","route":"openrouter","model":args.model,"reason":reason});
        write_row(&mut *writer.lock().await, &row)?;
        if !args.allow_partial {
            bail!("native route preflight failed: {reason}");
        }
        return Ok(());
    }
    let runs = if args.paired {
        args.repetitions
    } else {
        args.runs
    };
    let mut random = std::fs::File::open("/dev/urandom").context("open random order source")?;
    let work = build_work_list(&tasks, supported, runs, args.paired, args.channel, || {
        let mut coin = [0u8];
        std::io::Read::read_exact(&mut random, &mut coin)?;
        Ok(coin[0] & 1 == 1)
    })?;
    let mut results = run_work_list(work, args.concurrency, |item| {
        let tasks = &tasks;
        let args = &args;
        let api_key = &api_key;
        let writer = &writer;
        async move {
        let task = &tasks[item.task_index];
        let (final_world, evidence) = runtime::run_task(
            task, item.dialect, &args.model, api_key, item.run, item.channel.channel(),
        ).await;
        let grade = grade(task, &final_world, &evidence);
        let mut file = writer.lock().await;
        for attempt in &evidence.attempts {
            let mut row = attempt.clone();
            let fields = row.as_object_mut().expect("attempt is an object");
            fields.extend(serde_json::json!({"kind":"attempt", "task":task.id,"model":args.model,"route":"openrouter","dialect":item.dialect.language_id(),"channel":item.channel.name(),"repetition":item.run,"success":grade.passed,"grade":grade,"task_wall_ms":evidence.wall_ms}).as_object().expect("metadata object").clone());
            write_row(&mut file, &row)?;
        }
        write_row(&mut file, &serde_json::json!({"kind":"task_result","task":task.id,"model":args.model,"route":"openrouter","dialect":item.dialect.language_id(),"channel":item.channel.name(),"repetition":item.run,"success":grade.passed,"grade":grade,"wall_ms":evidence.wall_ms}))?;
        file.flush()?;
        Ok(TaskResult {
            run: item.run,
            id: task.id.to_string(),
            dialect: item.dialect.language_id().to_string(),
            channel: item.channel.name().to_string(),
            wall_ms: evidence.wall_ms,
            passed: grade.passed,
            failure_reason: grade.failure_reason,
            iterations: evidence.iterations,
            tool_call_count: evidence.tool_call_count,
            failed_exec_iterations: evidence.failed_execution_errors.len(),
            finish_value: evidence.finish_value,
            seed: task.seed.clone(),
            checker: task.checker_description(),
        })
    }}).await?;
    results.sort_by(|a, b| {
        (a.run, &a.dialect, &a.id, &a.channel).cmp(&(b.run, &b.dialect, &b.id, &b.channel))
    });
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

fn write_row(file: &mut std::fs::File, row: &Value) -> Result<()> {
    writeln!(file, "{row}")?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{row}")?;
    stdout.flush()?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct WorkItem {
    run: usize,
    dialect: lash::rlm::RlmDialect,
    task_index: usize,
    channel: ChannelSelection,
}

fn build_work_list(
    tasks: &[Task],
    dialects: &[lash::rlm::RlmDialect],
    runs: usize,
    paired: bool,
    channel: ChannelSelection,
    mut reverse_pair: impl FnMut() -> Result<bool>,
) -> Result<Vec<WorkItem>> {
    let mut work = Vec::new();
    for run in 1..=runs {
        for &dialect in dialects {
            for task_index in 0..tasks.len() {
                let channels = if paired {
                    if reverse_pair()? {
                        vec![ChannelSelection::Native, ChannelSelection::Cell]
                    } else {
                        vec![ChannelSelection::Cell, ChannelSelection::Native]
                    }
                } else {
                    vec![channel]
                };
                work.extend(channels.into_iter().map(|channel| WorkItem {
                    run,
                    dialect,
                    task_index,
                    channel,
                }));
            }
        }
    }
    Ok(work)
}

// Scoped futures keep model credentials and the writer borrowed. The semaphore
// bounds active runs; all futures are polled together without spawning threads.
async fn run_work_list<T, F: std::future::Future<Output = Result<T>>>(
    work: Vec<WorkItem>,
    concurrency: usize,
    run: impl Fn(WorkItem) -> F,
) -> Result<Vec<T>> {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut pending = work
        .into_iter()
        .map(|item| {
            let semaphore = Arc::clone(&semaphore);
            let run = &run;
            Box::pin(async move {
                let _permit = semaphore.acquire().await.context("acquire task permit")?;
                run(item).await
            })
        })
        .collect::<Vec<_>>();
    let mut results = Vec::new();
    std::future::poll_fn(|cx| {
        let mut index = 0;
        while index < pending.len() {
            match pending[index].as_mut().poll(cx) {
                std::task::Poll::Ready(result) => {
                    drop(pending.remove(index));
                    match result {
                        Ok(result) => results.push(result),
                        Err(error) => return std::task::Poll::Ready(Err(error)),
                    }
                }
                std::task::Poll::Pending => index += 1,
            }
        }
        if pending.is_empty() {
            std::task::Poll::Ready(Ok(()))
        } else {
            std::task::Poll::Pending
        }
    })
    .await?;
    Ok(results)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(item: &WorkItem) -> (usize, &str, usize, &str) {
        (
            item.run,
            item.dialect.language_id(),
            item.task_index,
            item.channel.name(),
        )
    }

    #[test]
    fn paired_repetitions_keep_both_channels_consecutive() {
        let args = Args::parse_from(["toolbench", "--paired", "--repetitions", "2"]);
        let tasks = task_pack();
        let mut coins = 0;
        let work = build_work_list(
            &tasks,
            args.dialect.dialects(),
            args.repetitions,
            args.paired,
            args.channel,
            || {
                coins += 1;
                Ok(coins % 2 == 0)
            },
        )
        .unwrap();
        assert_eq!(args.concurrency, 1);
        assert_eq!(
            work.len(),
            tasks.len() * 2 * args.dialect.dialects().len() * 2
        );
        assert_eq!(coins, work.len() / 2);
        for (index, pair) in work.chunks_exact(2).enumerate() {
            assert_eq!(key(&pair[0]).0, key(&pair[1]).0);
            assert_eq!(key(&pair[0]).1, key(&pair[1]).1);
            assert_eq!(pair[0].task_index, pair[1].task_index);
            assert_ne!(pair[0].channel, pair[1].channel);
            assert_eq!(
                pair[0].channel,
                if index % 2 == 0 {
                    ChannelSelection::Cell
                } else {
                    ChannelSelection::Native
                }
            );
        }
    }

    #[tokio::test]
    async fn concurrent_fake_runs_preserve_rows_and_bound_active_work() {
        let args = Args::parse_from([
            "toolbench",
            "--paired",
            "--repetitions",
            "2",
            "--concurrency",
            "4",
        ]);
        let work = build_work_list(
            &task_pack(),
            args.dialect.dialects(),
            args.repetitions,
            args.paired,
            args.channel,
            || Ok(false),
        )
        .unwrap();
        let mut expected = work.iter().map(key).collect::<Vec<_>>();
        expected.sort();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let actual = run_work_list(work.clone(), args.concurrency, |item| {
            let active = &active;
            let peak = &peak;
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                for _ in 0..=item.task_index % 3 {
                    tokio::task::yield_now().await;
                }
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(item)
            }
        })
        .await
        .unwrap();
        let mut actual_keys = actual.iter().map(key).collect::<Vec<_>>();
        actual_keys.sort();
        assert_eq!(actual_keys, expected);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        let serial = run_work_list(work.clone(), 1, |item| async move { Ok(item) })
            .await
            .unwrap();
        assert_eq!(
            serial.iter().map(key).collect::<Vec<_>>(),
            work.iter().map(key).collect::<Vec<_>>()
        );
    }
}
