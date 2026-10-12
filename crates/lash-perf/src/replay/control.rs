//! Optional control comparison over the replay instrument, outside its merge gate.
mod observation;
mod store;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::provider::{self, ScriptBook};
use super::receipt::Identity;
use super::script::{Protocol, Reply, Source, ToolCallSpec, TurnGroup, Workload};
use observation::{Observation, Sample};

/// Optional benchmark command; every sample gets a fresh SQLite file and owner.
#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long, value_delimiter = ',', default_value = "1,8,32")]
    pub n: Vec<usize>,
    #[arg(long, default_value_t = 5)]
    pub repeats: usize,
    /// A new directory; existing databases are never removed or reused.
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
}

/// Internal subprocess invocation: isolates owners and the global replay witness.
#[derive(Debug, clap::Args)]
pub struct SampleArgs {
    #[arg(long)]
    pub n: usize,
    #[arg(long, value_enum)]
    pub leg: Leg,
    #[arg(long, value_enum)]
    pub phase: Phase,
    #[arg(long)]
    pub dir: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Leg {
    PlainSequential,
    PlainParallel,
    CellSequential,
    CellParallel,
    CellResumeAtPrefix,
    PythonSequential,
    PythonParallel,
}

impl Leg {
    const ALL: [Self; 7] = [
        Self::PlainSequential,
        Self::PlainParallel,
        Self::CellSequential,
        Self::CellParallel,
        Self::CellResumeAtPrefix,
        Self::PythonSequential,
        Self::PythonParallel,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::PlainSequential => "plain-sequential",
            Self::PlainParallel => "plain-parallel",
            Self::CellSequential => "cell-sequential",
            Self::CellParallel => "cell-parallel",
            Self::CellResumeAtPrefix => "cell-resume-at-prefix",
            Self::PythonSequential => "python-sequential",
            Self::PythonParallel => "python-parallel",
        }
    }

    fn protocol(self) -> Protocol {
        match self {
            Self::PlainSequential | Self::PlainParallel => Protocol::Standard,
            Self::PythonSequential | Self::PythonParallel => Protocol::Python,
            _ => Protocol::Typescript,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Phase {
    Fresh,
    Prefix,
    Resume,
}

fn inputs(n: usize) -> Vec<ToolCallSpec> {
    (0..n)
        .map(|i| ToolCallSpec {
            tool: provider::LOOKUP.to_owned(),
            args: serde_json::json!({"key": format!("item-{i}")}),
        })
        .collect()
}

fn workload(n: usize, leg: Leg) -> Workload {
    let calls = inputs(n);
    let protocol = leg.protocol();
    let replies = match leg {
        Leg::PlainSequential => calls
            .into_iter()
            .map(|call| Reply::ToolCalls(vec![call]))
            .chain([Reply::Text("Done.".to_owned())])
            .collect(),
        Leg::PlainParallel => vec![Reply::ToolCalls(calls), Reply::Text("Done.".to_owned())],
        _ => {
            let expressions: Vec<String> = calls
                .iter()
                .map(|call| {
                    if protocol == Protocol::Python {
                        format!("tools_fixture_lookup({})", call.args)
                    } else {
                        format!("tools.fixture_lookup({})", call.args)
                    }
                })
                .collect();
            let source = match leg {
                Leg::CellParallel => format!(
                    "await Promise.all([{}]);\nawait control.finish(\"Done.\");",
                    expressions.join(",")
                ),
                Leg::PythonParallel => format!(
                    "import asyncio\nawait asyncio.gather({})\nawait control_finish(\"Done.\")",
                    expressions.join(",")
                ),
                Leg::PythonSequential => {
                    expressions
                        .iter()
                        .map(|expr| format!("await {expr}\n"))
                        .collect::<String>()
                        + "await control_finish(\"Done.\")"
                }
                _ => {
                    expressions
                        .iter()
                        .map(|expr| format!("await {expr};\n"))
                        .collect::<String>()
                        + "await control.finish(\"Done.\");"
                }
            };
            vec![Reply::Cell(Source(source))]
        }
    };
    Workload {
        format: super::script::FORMAT.to_owned(),
        name: leg.name().to_owned(),
        description: "Identical lookup inputs; orchestration control.".to_owned(),
        protocol,
        sessions: 1,
        compact_every: None,
        turns: vec![TurnGroup {
            repeat: 1,
            setup: false,
            user: "Run the lookups.".to_owned(),
            replies,
        }],
    }
}

/// Run one measured owner. Prefix owners die at the committed-outcome boundary.
pub async fn sample(args: &SampleArgs) -> Result<()> {
    ensure!(args.n > 0, "N must be positive");
    let workload = workload(args.n, args.leg);
    let turn = workload.expand(0, None).remove(0);
    let mut book = ScriptBook::new(workload.protocol);
    let input = book.register(0, &turn);
    let book = Arc::new(book);
    let observation = Arc::new(Observation::new(args, Arc::clone(&book))?);
    let mut options =
        lash_sqlite_store::SqliteStoreSetOptions::standard(lash::sqlite::SqliteSynchronous::Full);
    // The SQL observer retains commit labels; the decorator below counts staged
    // graph bytes and read payloads, including hydration on a fresh owner.
    let observer = Arc::clone(&observation);
    options.observer = lash_core::operational_metrics::StoreObserver::default()
        .with_sql_sink(move |receipt, outcome| observer.commit(receipt, outcome));
    let stores = lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
        args.dir.join("lash.db"),
        options,
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await
    .map_err(|e| anyhow::anyhow!("open FULL SQLite: {e}"))?;
    observation.set_page_size(&stores)?;
    let observer = Arc::clone(&observation);
    let stores = lash_core::testing::runtime_helpers::LayeredStores::over(Arc::new(stores))
        .map_durable_store(move |inner| {
            Arc::new(store::ObservedStore {
                inner,
                observation: observer,
            })
        })
        .into_store_set();
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .map_err(|e| anyhow::anyhow!("build durable backend: {e}"))?;
    let workers = (workload.protocol != Protocol::Standard)
        .then(|| super::workers::service(workload.protocol == Protocol::Python, &args.dir))
        .transpose()?;
    let observer = Arc::clone(&observation);
    let tools =
        provider::observed_fixtures(Some(Arc::new(move |tool, args| observer.tool(tool, args))))?;
    if let Some(workers) = &workers {
        observation.set_pool(
            workers
                .pool()
                .map_err(|e| anyhow::anyhow!("worker pool: {e}"))?,
        );
    }
    let core = super::runner::build_core(
        &workload,
        &backend,
        Arc::clone(&book),
        workers.as_ref(),
        tools,
    )?;
    let cleanup = lash::testing::quiesce_artifact_cleanup(&core).await;
    let id = lash::SessionId::from("code-mode-control");
    if args.phase != Phase::Resume {
        core.session(id.clone())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                lash::SessionSpec::new(
                    provider::MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1_000_000),
                )
                .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            ))
            .await
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?;
    }
    let session = core
        .session(id)
        .open()
        .await
        .map_err(|e| anyhow::anyhow!("open session: {e}"))?;
    if args.phase != Phase::Resume {
        cleanup.settle().await?;
        observation.start();
    }
    let handle = if args.phase == Phase::Resume {
        session.attach_id(lash::TurnId::from("control-input"))
    } else {
        session
            .send(lash::TurnInput::text(input))
            .id(lash::TurnId::from("control-input"))
            .into_future()
            .await
            .map_err(|e| anyhow::anyhow!("send: {e}"))?
    };
    let outcome = tokio::time::timeout(Duration::from_secs(120), handle.outcome())
        .await
        .context("control turn timed out")?
        .map_err(|e| anyhow::anyhow!("outcome: {e}"))?;
    ensure!(
        outcome.output().is_some_and(|output| output.is_success()),
        "turn failed: {outcome:?}"
    );
    let calls = book.take_calls(0, 1);
    ensure!(
        calls.past_script == 0,
        "script deviation: {:?}",
        calls.last_past_script
    );
    // A resumed cell needs no second model call: its executable is stored.
    let expected = if args.phase == Phase::Resume {
        0
    } else {
        turn.replies.len()
    };
    ensure!(
        calls.scripted == expected,
        "{} model calls, expected {expected}",
        calls.scripted
    );
    // take_calls consumes the evidence; preserve its count in the snapshot.
    let mut sample = observation.snapshot();
    sample
        .metrics
        .insert("model_calls".to_owned(), calls.scripted as f64);
    write(&args.out, &sample)?;
    session
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("close: {e}"))?;
    core.shutdown()
        .await
        .map_err(|e| anyhow::anyhow!("shutdown: {e}"))?;
    Ok(())
}

#[derive(Serialize)]
struct Receipt {
    format: &'static str,
    identity: Identity,
    build_profile: &'static str,
    compiler: &'static str,
    executable_sha256: String,
    store: &'static str,
    durability: serde_json::Value,
    repeat_count: usize,
    n: Vec<usize>,
    physical_syncs: Option<u64>,
    physical_syncs_note: &'static str,
    measurement: &'static str,
    legs: Vec<LegReceipt>,
}

#[derive(Serialize)]
struct LegReceipt {
    leg: Leg,
    n: usize,
    committed_prefix: Option<usize>,
    script_digest: String,
    summary: BTreeMap<String, Distribution>,
    samples: Vec<Sample>,
}

#[derive(Serialize)]
struct Distribution {
    median: f64,
    p90: f64,
}

pub(super) fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)? + "\n")?;
    Ok(())
}

fn invoke(n: usize, leg: Leg, phase: Phase, dir: &Path, out: &Path) -> Result<Sample> {
    let phase_name = match phase {
        Phase::Fresh => "fresh",
        Phase::Prefix => "prefix",
        Phase::Resume => "resume",
    };
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "code-mode-control-sample",
            "--n",
            &n.to_string(),
            "--leg",
            leg.name(),
            "--phase",
            phase_name,
            "--dir",
        ])
        .arg(dir)
        .arg("--out")
        .arg(out)
        .status()?;
    if phase == Phase::Prefix {
        use std::os::unix::process::ExitStatusExt as _;
        ensure!(
            status.signal() == Some(libc::SIGKILL),
            "prefix owner was not SIGKILLed: {status}"
        );
    } else {
        ensure!(
            status.success(),
            "{}/{phase_name} failed: {status}",
            leg.name()
        );
    }
    serde_json::from_slice(&std::fs::read(out)?).context("read the owner's sample")
}

/// Run all control legs, write raw samples and distributions, and print a table.
pub fn run(args: &Args) -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "the control requires an optimized build (--config=optimized)"
    );
    ensure!(
        args.repeats > 0 && !args.n.is_empty() && args.n.iter().all(|n| *n > 0),
        "positive N and repeats required"
    );
    // Never delete a user's existing store directory, including symlink targets.
    ensure!(
        !args.store_dir.exists(),
        "--store-dir must be new: {}",
        args.store_dir.display()
    );
    std::fs::create_dir_all(&args.store_dir)?;
    let mut receipt = Receipt {
        format: "lash-perf-code-mode-control/1",
        identity: Identity::current(),
        build_profile: env!("LASH_PERF_BUILD_PROFILE"),
        compiler: env!("LASH_PERF_COMPILER"),
        executable_sha256: {
            use sha2::Digest as _;
            format!(
                "{:x}",
                sha2::Sha256::digest(std::fs::read(std::env::current_exe()?)?)
            )
        },
        store: "SQLite file; WAL; synchronous=FULL",
        durability: serde_json::json!({
            "settings": format!("{:?}", lash_durable::DurableSettings::standard()),
            "sqlite_options": format!("{:?}", lash_sqlite_store::StoreOptions::standard(lash::sqlite::SqliteSynchronous::Full)),
            "execution_policy": "Once", "owner_crash": "SIGKILL after committed round.outcome; k=floor(N/2); k=0 before first cell.snapshot+admit (admission writes x_start)",
        }),
        repeat_count: args.repeats,
        n: args.n.clone(),
        physical_syncs: None,
        physical_syncs_note: "No fsync/fdatasync counter in existing observers; WAL frames/bytes are reported instead, never inferred sync counts.",
        measurement: "fresh: send through outcome, setup excluded; resume: prefix window + fresh-owner entry through outcome, crash/reexec gap excluded; CPU=host process clock + pool charged worker CPU; bytes_read=durable SQL returned scalar payload; staged_bytes=graph node bytes prepared for owner commit attempts, including restaged/uncommitted nodes; p90=nearest-rank; no warmups",
        legs: Vec::new(),
    };
    for &n in &args.n {
        for leg in Leg::ALL {
            let mut samples = Vec::new();
            for repeat in 0..args.repeats {
                let dir = args
                    .store_dir
                    .join(format!("{}-n{n}-r{repeat}", leg.name()));
                std::fs::create_dir(&dir)?;
                let out = dir.join("sample.json");
                let sample = if leg == Leg::CellResumeAtPrefix {
                    let prefix = invoke(n, leg, Phase::Prefix, &dir, &dir.join("prefix.json"))?;
                    ensure!(
                        prefix.tool_inputs == inputs(n / 2),
                        "owner did not cut at the committed prefix"
                    );
                    let mut resumed = invoke(n, leg, Phase::Resume, &dir, &out)?;
                    resumed.prepend(prefix);
                    resumed
                } else {
                    invoke(n, leg, Phase::Fresh, &dir, &out)?
                };
                let mut actual = sample.tool_inputs.clone();
                actual.sort_by_key(|call| call.args.to_string());
                let mut expected = inputs(n);
                expected.sort_by_key(|call| call.args.to_string());
                ensure!(
                    actual == expected,
                    "{} N={n} ran different tool inputs: {actual:?}",
                    leg.name()
                );
                if leg == Leg::CellResumeAtPrefix {
                    ensure!(
                        sample.resume.is_some(),
                        "resume never reached the next call"
                    );
                }
                samples.push(sample);
            }
            let summary = summarize(&samples);
            receipt.legs.push(LegReceipt {
                leg,
                n,
                committed_prefix: (leg == Leg::CellResumeAtPrefix).then_some(n / 2),
                script_digest: workload(n, leg).digest(),
                summary,
                samples,
            });
            write(&args.out, &receipt)?;
        }
    }
    println!(
        "{}; {}; repeats={}; no warmups; syncs unavailable (WAL fallback)",
        receipt.identity.commit.as_deref().unwrap_or("unknown SHA"),
        receipt.store,
        args.repeats
    );
    println!(
        "| N | leg | wall ms med/p90 | CPU ms med/p90 | owner commits | syncs | WAL frames/bytes | SQL | rows read/write | staged bytes | model | resume ms med/p90 | resume read bytes | restored nodes | repeated tools |"
    );
    println!("|---:|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for leg in &receipt.legs {
        let value = |key: &str| {
            leg.summary
                .get(key)
                .map(|v| format!("{:.0}", v.median))
                .unwrap_or_else(|| "—".to_owned())
        };
        let timing = |key: &str| {
            leg.summary
                .get(key)
                .map(|v| format!("{:.2}/{:.2}", v.median, v.p90))
                .unwrap_or_else(|| "—".to_owned())
        };
        println!(
            "| {} | {} | {} | {} | {} | n/a | {}/{} | {} | {}/{} | {} | {} | {} | {} | {} | {} |",
            leg.n,
            leg.leg.name(),
            timing("wall_ms"),
            timing("cpu_ms"),
            value("logical_owner_commits"),
            value("wal_frames"),
            value("wal_bytes"),
            value("sql_statements"),
            value("sql_rows_read"),
            value("sql_rows_written"),
            value("staged_bytes"),
            value("model_calls"),
            timing("resume_latency_ms"),
            value("resume_bytes_read"),
            value("resume_restored_nodes"),
            value("repeated_tool_calls")
        );
    }
    println!("receipt: {}", args.out.display());
    Ok(())
}

fn summarize(samples: &[Sample]) -> BTreeMap<String, Distribution> {
    let mut values = BTreeMap::<String, Vec<f64>>::new();
    for sample in samples {
        let mut metrics = sample.metrics.clone();
        if let Some(resume) = &sample.resume {
            metrics.extend([
                ("resume_latency_ms".to_owned(), resume.latency_ms),
                ("resume_bytes_read".to_owned(), resume.bytes_read as f64),
                (
                    "resume_restored_nodes".to_owned(),
                    resume.restored_nodes as f64,
                ),
                (
                    "repeated_tool_calls".to_owned(),
                    sample.tool_inputs.len().saturating_sub(sample.n) as f64,
                ),
            ]);
        }
        for (key, value) in metrics {
            values.entry(key).or_default().push(value);
        }
    }
    values
        .into_iter()
        .map(|(key, mut values)| {
            values.sort_by(f64::total_cmp);
            let mid = values.len() / 2;
            let median = if values.len().is_multiple_of(2) {
                (values[mid - 1] + values[mid]) / 2.0
            } else {
                values[mid]
            };
            let p90 = values[(values.len() * 9).div_ceil(10) - 1];
            (key, Distribution { median, p90 })
        })
        .collect()
}
