mod grading;
mod runtime;
mod summary;
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
    Standard,
}
impl ChannelSelection {
    fn name(self) -> &'static str {
        match self {
            Self::Cell => "cell",
            Self::Native => "native",
            Self::Standard => "standard",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ChannelSet {
    Paired,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
}
impl ReasoningEffort {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
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
    #[arg(long, alias = "runs", default_value_t = 1)]
    repetitions: usize,
    /// Maximum simultaneous task runs; start at 4–8 for OpenRouter.
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    /// Per-provider-attempt machine-readable evidence, including retries.
    #[arg(long, default_value = "toolbench-results.jsonl")]
    results_file: std::path::PathBuf,
    /// OpenRouter model identifier.
    #[arg(long, env = "OPENROUTER_MODEL", default_value = DEFAULT_MODEL)]
    model: Vec<String>,
    /// Include standard alongside the paired RLM cohorts.
    #[arg(long, value_enum, default_value_t = ChannelSet::Paired)]
    channel_set: ChannelSet,
    #[arg(long, value_enum, default_value_t = ReasoningEffort::None)]
    reasoning_effort: ReasoningEffort,
    /// Run both dialects or select one.
    #[arg(long, value_enum, default_value_t = DialectSelection::Both)]
    dialect: DialectSelection,
    /// Run only these task ids; repeat --task to select a subset.
    #[arg(long)]
    task: Vec<String>,
    /// Exit successfully even when one or more task rows fail.
    #[arg(long)]
    allow_partial: bool,
}

#[derive(Debug, Serialize)]
struct TaskResult {
    model: String,
    reasoning_effort: ReasoningEffort,
    run: usize,
    id: String,
    dialect: String,
    channel: String,
    wall_ms: u128,
    passed: bool,
    failure_reason: Option<String>,
    rounds: usize,
    iterations: usize,
    tool_call_count: usize,
    submit_count: usize,
    failed_exec_iterations: usize,
    finish_value: Option<Value>,
    seed: World,
    checker: String,
    usage: summary::Usage,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();
    if args.repetitions == 0 || args.concurrency == 0 {
        bail!("--repetitions/--runs and --concurrency must be at least 1");
    }
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    if api_key.trim().is_empty() {
        bail!("OPENROUTER_API_KEY is not set");
    }
    let tasks = selected_tasks(&args.task)?;
    let writer =
        Mutex::new(std::fs::File::create(&args.results_file).context("create results JSONL")?);
    let mut random = std::fs::File::open("/dev/urandom").context("open random order source")?;
    let mut template = build_work_list(
        &tasks,
        args.dialect.dialects(),
        args.repetitions,
        args.paired || args.channel_set == ChannelSet::All,
        args.channel,
        || {
            let mut coin = [0u8];
            std::io::Read::read_exact(&mut random, &mut coin)?;
            Ok(coin[0] & 1 == 1)
        },
    )?;
    if args.channel_set == ChannelSet::All {
        template.extend(build_work_list(
            &tasks,
            args.dialect.dialects(),
            args.repetitions,
            false,
            ChannelSelection::Standard,
            || Ok(false),
        )?);
    }
    let probes = args
        .model
        .iter()
        .enumerate()
        .flat_map(|(model_index, _)| {
            [ChannelSelection::Native, ChannelSelection::Standard]
                .into_iter()
                .filter(|channel| template.iter().any(|item| item.channel == *channel))
                .map(move |channel| WorkItem {
                    model_index,
                    run: 0,
                    dialect: args.dialect.dialects()[0],
                    task_index: 0,
                    channel,
                })
        })
        .collect();
    let exclusions = run_work_list(probes, args.concurrency, |item| {
        let args = &args;
        let tasks = &tasks;
        let api_key = &api_key;
        let writer = &writer;
        async move {
            let model = &args.model[item.model_index];
            match runtime::preflight(&tasks[0], item.dialect, model, api_key, item.channel, args.reasoning_effort).await {
                Ok(()) => Ok(None),
                Err(reason) => {
                    write_row(&mut *writer.lock().await, &serde_json::json!({"kind":"excluded_route","route":"openrouter","model":model,"channel":item.channel.name(),"dialect":item.dialect_name(),"reasoning_effort":args.reasoning_effort,"rounds":0,"reason":reason}))?;
                    Ok(Some((item.model_index, item.channel)))
                }
            }
        }
    }).await?.into_iter().flatten().collect::<Vec<_>>();
    // Interleave models so a busy route cannot serialize entire model cohorts.
    let work = template
        .into_iter()
        .flat_map(|item| {
            (0..args.model.len()).map(move |model_index| WorkItem {
                model_index,
                ..item
            })
        })
        .filter(|item| !exclusions.contains(&(item.model_index, item.channel)))
        .collect();
    let mut results = run_work_list(work, args.concurrency, |item| {
        let tasks = &tasks;
        let args = &args;
        let api_key = &api_key;
        let writer = &writer;
        async move {
            let task = &tasks[item.task_index];
            let model = &args.model[item.model_index];
            let (final_world, evidence) = runtime::run_task(task, item.dialect, model, api_key, item.run, item.channel, args.reasoning_effort).await;
            let grade = grade(task, &final_world, &evidence);
            let usage = summary::Usage::from_attempts(&evidence.attempts);
            let mut file = writer.lock().await;
            for attempt in &evidence.attempts {
                let mut row = attempt.clone();
                row.as_object_mut().expect("attempt object").extend(serde_json::json!({"kind":"attempt","task":task.id,"model":model,"route":"openrouter","dialect":item.dialect_name(),"channel":item.channel.name(),"reasoning_effort":args.reasoning_effort,"rounds":evidence.rounds,"repetition":item.run,"success":grade.passed,"grade":grade,"task_wall_ms":evidence.wall_ms}).as_object().expect("metadata object").clone());
                write_row(&mut file, &row)?;
            }
            let result = TaskResult {
                model: model.clone(), reasoning_effort: args.reasoning_effort,
                run: item.run, id: task.id.into(), dialect: item.dialect_name().into(), channel: item.channel.name().into(),
                wall_ms: evidence.wall_ms, passed: grade.passed, failure_reason: grade.failure_reason,
                rounds: evidence.rounds, iterations: evidence.iterations, tool_call_count: evidence.tool_call_count,
                submit_count: evidence.submit_count, failed_exec_iterations: evidence.failed_execution_errors.len(),
                finish_value: evidence.finish_value, seed: task.seed.clone(),
                checker: if item.channel == ChannelSelection::Standard { format!("{}; exact world; exactly {} host calls; submit once; at most 3 model responses; 120 s", task.finish.describe(), task.tool_calls) } else { task.checker_description() }, usage,
            };
            let mut row = serde_json::to_value(&result)?;
            row.as_object_mut().expect("result object").extend(serde_json::json!({"kind":"task_result","task":task.id,"route":"openrouter","repetition":item.run,"success":result.passed,"grade":{"passed":result.passed,"failure_reason":result.failure_reason}}).as_object().expect("metadata object").clone());
            write_row(&mut file, &row)?;
            file.flush()?;
            Ok(result)
        }
    }).await?;
    results.sort_by(|a, b| {
        (&a.model, a.run, &a.dialect, &a.id, &a.channel)
            .cmp(&(&b.model, b.run, &b.dialect, &b.id, &b.channel))
    });
    let summaries = summary::aggregate(&results);
    for summary in &summaries {
        let mut row = serde_json::to_value(summary)?;
        row["kind"] = "summary".into();
        write_row(&mut *writer.lock().await, &row)?;
    }
    let markdown = summary::markdown(&summaries);
    print!("{markdown}");
    let mut summary_path = args.results_file.as_os_str().to_os_string();
    summary_path.push(".summary.md");
    std::fs::write(summary_path, markdown).context("write summary Markdown")?;
    if (!exclusions.is_empty() || results.is_empty() || results.iter().any(|row| !row.passed))
        && !args.allow_partial
    {
        bail!("one or more toolbench tasks failed or cohorts were excluded");
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
    model_index: usize,
    run: usize,
    dialect: lash::rlm::RlmDialect,
    task_index: usize,
    channel: ChannelSelection,
}

impl WorkItem {
    fn dialect_name(&self) -> &'static str {
        if self.channel == ChannelSelection::Standard {
            "none"
        } else {
            self.dialect.language_id()
        }
    }
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
        for &dialect in if !paired && channel == ChannelSelection::Standard {
            &dialects[..1]
        } else {
            dialects
        } {
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
                    model_index: 0,
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

fn selected_tasks(task_ids: &[String]) -> Result<Vec<Task>> {
    let tasks = task_pack();
    for task_id in task_ids {
        if !tasks.iter().any(|task| task.id == task_id) {
            bail!("unknown task `{task_id}`");
        }
    }
    Ok(tasks
        .into_iter()
        .filter(|task| task_ids.is_empty() || task_ids.iter().any(|id| id == task.id))
        .collect())
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
    fn repeatable_models_and_run_alias_parse() {
        let args = Args::parse_from([
            "toolbench",
            "--model",
            "a",
            "--model",
            "b",
            "--runs",
            "2",
            "--reasoning-effort",
            "medium",
        ]);
        assert_eq!(args.model, ["a", "b"]);
        assert_eq!(args.repetitions, 2);
        assert_eq!(args.reasoning_effort, ReasoningEffort::Medium);
        let work = build_work_list(
            &task_pack(),
            args.dialect.dialects(),
            1,
            false,
            ChannelSelection::Standard,
            || Ok(false),
        )
        .unwrap();
        assert_eq!(work.len(), 16);
        assert!(work.iter().all(|item| item.dialect_name() == "none"));
    }

    #[test]
    fn task_selection_defaults_to_full_pack_and_accepts_repeated_flags() {
        let defaults = Args::parse_from(["toolbench"]);
        assert_eq!(selected_tasks(&defaults.task).unwrap().len(), 16);
        let args = Args::parse_from([
            "toolbench",
            "--task",
            "kv-read",
            "--task",
            "weather-condition",
            "--task",
            "kv-read",
        ]);
        let selected = selected_tasks(&args.task).unwrap();
        assert_eq!(
            selected.iter().map(|task| task.id).collect::<Vec<_>>(),
            ["weather-condition", "kv-read"]
        );
        assert!(selected_tasks(&["kv-read".to_string(), "unknown".to_string()]).is_err());
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
        for (index, pair) in work.as_chunks::<2>().0.iter().enumerate() {
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
