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
    pub recovery_workload: bool,
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

// Fixture body observers report the provider-local name, without the root.
fn inputs(n: usize) -> Vec<ToolCallSpec> {
    (0..n)
        .map(|i| ToolCallSpec {
            tool: provider::LOOKUP.to_owned(),
            args: serde_json::json!({"key": format!("item-{i}")}),
        })
        .collect()
}

fn workload(n: usize, leg: Leg) -> Workload {
    // Model calls address the fixture through its registered root. The exact
    // useful-work check below compares the observer's provider-local names.
    let calls: Vec<_> = inputs(n)
        .into_iter()
        .map(|mut call| {
            call.tool = format!("tools.{}", call.tool);
            call
        })
        .collect();
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
                        format!("tools.fixture_lookup({})", call.args)
                    } else {
                        format!("tools.fixtureLookup({})", call.args)
                    }
                })
                .collect();
            let source = match leg {
                Leg::CellParallel => format!(
                    "await Promise.all([{}]);\nawait control.finish(\"Done.\");",
                    expressions.join(",")
                ),
                Leg::PythonParallel => format!(
                    "import asyncio\nawait asyncio.gather({})\nawait control.finish(\"Done.\")",
                    expressions.join(",")
                ),
                Leg::PythonSequential => {
                    expressions
                        .iter()
                        .map(|expr| format!("await {expr}\n"))
                        .collect::<String>()
                        + "await control.finish(\"Done.\")"
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
        instruction_limit: None,
        sessions: 1,
        compact_every: None,
        turns: vec![TurnGroup {
            capability_turn: None,
            repeat: 1,
            setup: false,
            oracle: None,
            user: "Run the lookups.".to_owned(),
            replies,
        }],
    }
}

/// Run one measured owner. Prefix owners die at the committed-outcome boundary.
pub async fn sample(args: &SampleArgs) -> Result<()> {
    ensure!(args.n > 0, "N must be positive");
    let workload = if args.recovery_workload {
        super::script::resolve("crash-resume")?
    } else {
        workload(args.n, args.leg)
    };
    let mut expanded = workload.expand(0, None);
    let selected = if args.recovery_workload { 3 } else { 0 };
    let turn = expanded.remove(selected);
    let mut book = ScriptBook::new();
    let setup: Vec<_> = expanded
        .into_iter()
        .take(selected)
        .map(|turn| {
            let input = book.register(0, &turn);
            (turn, input)
        })
        .collect();
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
        .config(lash::durable::DurableSettings::standard())
        .build()
        .map_err(|e| anyhow::anyhow!("build durable backend: {e}"))?;
    let workers = (workload.protocol != Protocol::Standard)
        .then(|| {
            super::workers::service(
                workload.protocol == Protocol::Python,
                &args.dir,
                None,
                false,
                false,
            )
        })
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
    let id = lash_kernel_value::identity::SessionId::from("code-mode-control");
    if args.phase != Phase::Resume {
        core.session(id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                provider::MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1_000_000),
                lash_core::NoProgressBudget::bounded(12),
            )))
            .await
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?;
    }
    let session = core
        .session(id)
        .open()
        .await
        .map_err(|e| anyhow::anyhow!("open session: {e}"))?;
    if args.phase != Phase::Resume {
        for (turn, input) in setup {
            let output = session
                .send(lash::TurnInput::text(input))
                .id(lash_vocabulary::identity::TurnId::try_from(format!(
                    "recovery-setup-{}",
                    turn.ordinal
                ))?)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("recovery setup: {e}"))?;
            ensure!(output.is_success(), "recovery setup failed");
            let calls = book.take_calls(0, turn.ordinal);
            ensure!(
                calls.scripted == turn.replies.len() && calls.past_script == 0,
                "recovery setup left its script"
            );
        }
        cleanup.settle().await?;
        observation.start()?;
    }
    let handle = if args.phase == Phase::Resume {
        session.attach_id(lash_vocabulary::identity::TurnId::from("control-input"))
    } else {
        session
            .send(lash::TurnInput::text(input))
            .id(lash_vocabulary::identity::TurnId::from("control-input"))
            .into_future()
            .await
            .map_err(|e| anyhow::anyhow!("send: {e}"))?
    };
    let outcome = tokio::time::timeout(Duration::from_secs(120), handle.outcome())
        .await
        .context("control turn timed out")?
        .map_err(|e| anyhow::anyhow!("outcome: {e}"))?;
    observation.stop_instructions()?;
    if args.phase == Phase::Resume {
        recovery_event("outcome");
    }
    ensure!(
        outcome.output().is_some_and(|output| output.is_success()),
        "turn failed: {outcome:?}"
    );
    let calls = book.take_calls(0, turn.ordinal);
    ensure!(
        calls.past_script == 0,
        "script deviation: {:?}",
        calls.last_past_script
    );
    // model.answer paid for the response before either prefix cut. Recovery
    // derives that retained answer without sending another model call.
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
    invoke_timed(n, leg, phase, dir, out, None)
}
fn invoke_timed(
    n: usize,
    leg: Leg,
    phase: Phase,
    dir: &Path,
    out: &Path,
    timeline: Option<&Path>,
) -> Result<Sample> {
    let phase_name = match phase {
        Phase::Fresh => "fresh",
        Phase::Prefix => "prefix",
        Phase::Resume => "resume",
    };
    let mut command = std::process::Command::new(std::env::current_exe()?);
    if let Some(path) = timeline {
        command.env("LASH_RECOVERY_TIMELINE", path);
    }
    command
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
        .arg(out);
    if timeline.is_some() {
        command.arg("--recovery-workload");
    }
    let status = command.status()?;
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
            "execution_policy": "Once", "owner_crash": "SIGKILL after committed round.outcome; k=floor(N/2); k=0 before first cell admission, including model.done+cell.snapshot+admit (admission writes x_start; model.answer retains the paid response, with no resend)",
        }),
        repeat_count: args.repeats,
        n: args.n.clone(),
        physical_syncs: None,
        physical_syncs_note: "No fsync/fdatasync counter in existing observers; WAL frames/bytes are reported instead, never inferred sync counts.",
        measurement: "fresh: send through outcome, setup excluded; resume: prefix window + fresh-owner entry through outcome, crash/reexec gap excluded; CPU=host process clock + pool charged worker CPU; bytes_read=durable SQL returned scalar payload; staged_bytes=graph node bytes prepared for owner commit attempts, including restaged/uncommitted nodes; commit_sql.installed_graph_bytes=canonical bytes actually inserted by successful owner writes; commit_sql.bound_bytes=expanded SQL spelling, not disk bytes; host allocation deltas include observation work and other host threads; IPC counters include framing in both directions; p90=nearest-rank; no warmups",
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

/// Profile a real host-owner death at a committed cell prefix on SQLite.
pub fn profile_recovery(dir: &Path, out: &Path) -> Result<()> {
    ensure!(!dir.exists(), "recovery requires a fresh store directory");
    std::fs::create_dir_all(dir)?;
    let timeline = dir.join("recovery.jsonl");
    let prefix = invoke_timed(
        2,
        Leg::CellResumeAtPrefix,
        Phase::Prefix,
        dir,
        &dir.join("prefix.json"),
        Some(&timeline),
    )?;
    recovery_event_at(Some(&timeline), "detection")?;
    recovery_event_at(Some(&timeline), "re-exec")?;
    let resumed = invoke_timed(
        2,
        Leg::CellResumeAtPrefix,
        Phase::Resume,
        dir,
        &dir.join("resumed.json"),
        Some(&timeline),
    )?;
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&timeline)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let kill = events
        .iter()
        .find(|e| e["phase"] == "kill")
        .and_then(|e| e["unix_ns"].as_u64())
        .context("missing owner kill timestamp")?;
    println!("recovery phase       since_kill_ms");
    for name in [
        "kill",
        "detection",
        "re-exec",
        "ready",
        "claim",
        "first progress",
        "outcome",
    ] {
        let event = events
            .iter()
            .find(|e| e["phase"] == name && e["unix_ns"].as_u64().is_some_and(|at| at >= kill))
            .with_context(|| format!("missing recovery {name}"))?;
        let at = event["unix_ns"]
            .as_u64()
            .context("invalid recovery clock")?;
        println!("{name:<20} {:>13.3}", at.saturating_sub(kill) as f64 / 1e6);
    }
    write(
        out,
        &serde_json::json!({"kind":"lash.profile-recovery", "clock":"CLOCK_REALTIME; same host across exec", "cut":"SIGKILL owner after first committed tool of crash-resume corpus turn 4", "events":events, "prefix":prefix, "resumed":resumed}),
    )?;
    Ok(())
}

pub(super) fn recovery_event(phase: &str) {
    if let Some(path) = std::env::var_os("LASH_RECOVERY_TIMELINE")
        && let Err(error) = recovery_event_at(Some(Path::new(&path)), phase)
    {
        eprintln!("recovery clock: {error:#}");
    }
}
fn recovery_event_at(path: Option<&Path>, phase: &str) -> Result<()> {
    use std::io::Write as _;
    if let Some(path) = path {
        let unix_ns = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos(),
        )?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(
            file,
            "{}",
            serde_json::json!({"phase":phase,"unix_ns":unix_ns,"pid":std::process::id()})
        )?;
    }
    Ok(())
}
