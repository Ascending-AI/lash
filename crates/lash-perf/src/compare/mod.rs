//! Developer-only comparisons of resident interpreters and fresh kernel cells.
mod durable;
pub(crate) mod math;
mod reference;
mod workloads;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use lash_vm_worker::core_support::compare as kernel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::perf_support::instructions::{InstructionCounter, InstructionReading};

/// Comparative profiling is an explicit developer command, outside merge gates.
#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long)]
    pub out: PathBuf,
    /// Repeat for upstream QuickJS and dquickts; receipt retains binary hashes.
    #[arg(long)]
    pub qjs: Vec<PathBuf>,
    #[arg(long)]
    pub node: Option<PathBuf>,
    #[arg(long)]
    pub python: Option<PathBuf>,
    /// Monty CLI from the pinned reference fetcher; CPython checks equal work.
    #[arg(long, requires = "python")]
    pub monty: Option<PathBuf>,
    #[arg(long, default_value_t = 5)]
    pub runs: usize,
    #[arg(long, default_value_t = 1)]
    pub warmups: usize,
    /// High trip count is workload's low count times this factor.
    #[arg(long, default_value_t = 100)]
    pub factor: u64,
    #[arg(long, value_delimiter = ',')]
    pub workload: Vec<String>,
    #[arg(long, value_parser = ["typescript", "python"])]
    pub dialect: Option<String>,
    /// Opt in to measurement history; ordinary measurements never open a history file.
    #[arg(long)]
    pub history: Option<PathBuf>,
    /// Check qualified instructions/op against the last comparable recorded baseline.
    #[arg(long, requires_all = ["history", "instructions"])]
    pub check: bool,
    /// Count instructions around each fresh cell execution, excluding driver setup/I/O.
    #[arg(long)]
    pub instructions: bool,
    #[arg(long, default_value_t = 0.01)]
    pub tolerance: f64,
    /// Pin to this CPU; default is the first CPU in this process's allowed set.
    #[arg(long)]
    pub cpu: Option<usize>,
    #[arg(long)]
    pub no_pin: bool,
    /// Count work, publish context and record statement paths in timed cells.
    #[arg(long)]
    pub observe: bool,
    /// Diagnostic ablation of exact charge accounting; never records history.
    #[arg(long, conflicts_with = "history")]
    pub no_charge: bool,
    /// Diagnostic ablation of held-byte accounting; never records history.
    #[arg(long, conflicts_with = "history")]
    pub no_memory_meter: bool,
    /// Write perf.data and perf-script stacks for each lash workload/dialect.
    #[arg(long)]
    pub profile: Option<PathBuf>,
    /// Existing optimized durable-substrate executable; requires --vm-worker.
    #[arg(long, requires = "vm_worker")]
    pub durable_substrate: Option<PathBuf>,
    #[arg(long)]
    pub vm_worker: Option<PathBuf>,
    /// dquickts tests/snapshot/build/hibernate; requires --dqts-script.
    #[arg(long, requires = "dqts_script")]
    pub dqts_hibernate: Option<PathBuf>,
    #[arg(long)]
    pub dqts_script: Option<PathBuf>,
}

impl Args {
    fn kernel(&self) -> Result<kernel::Kernel> {
        Ok(kernel::Kernel::new()
            .map_err(anyhow::Error::from_boxed)?
            .with_metering(
                if self.no_charge {
                    lash_kernel_vm::Charge::Off
                } else {
                    lash_kernel_vm::Charge::Exact
                },
                if self.no_memory_meter {
                    lash_kernel_vm::MemoryMeter::Off
                } else {
                    lash_kernel_vm::MemoryMeter::On
                },
            ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Sample {
    pub(crate) ns: f64,
    pub(crate) result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instruction_window: Option<InstructionReading>,
}

#[derive(Debug, Serialize)]
pub(crate) struct InstructionSlopes {
    minimum_per_operation: f64,
    pub(crate) median_per_operation: f64,
}
#[derive(Debug, Serialize)]
pub(crate) struct Row {
    pub engine: String,
    pub dialect: String,
    pub workload: String,
    pub source_hash: String,
    pub engine_identity: String,
    pub trips: [u64; 2],
    pub operations: [u64; 2],
    pub(crate) samples: Vec<[Sample; 2]>,
    pub minimum_ns: [f64; 2],
    pub median_ns: [f64; 2],
    pub minimum: math::Fit,
    pub median: math::Fit,
    pub(crate) instructions: Option<InstructionSlopes>,
    pub fixed_scopes_ns: BTreeMap<String, f64>,
    ratios: BTreeMap<String, f64>,
    /// Census of the separate fresh diagnostic cell at trips[0].
    execution_census: Option<lash_kernel_vm::perf_witness::execution::Census>,
}
#[derive(Serialize)]
struct UnsupportedRow {
    engine: String,
    workload: String,
    source_hash: String,
    engine_identity: String,
    error: String,
}
#[derive(Serialize)]
pub(crate) struct Receipt {
    pub recorded_at: String,
    pub commit: String,
    pub run_id: String,
    pub cpu: String,
    pub load_before: String,
    load_after: String,
    pub geometry: serde_json::Value,
    pub rows: Vec<Row>,
    errors: Vec<String>,
    unsupported: Vec<UnsupportedRow>,
    durable: Vec<serde_json::Value>,
}
fn elapsed(at: Instant) -> f64 {
    at.elapsed().as_secs_f64() * 1e9
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_else(|e| e.to_string())
        .trim()
        .into()
}
fn pin(args: &Args) -> String {
    if args.no_pin {
        return "disabled by caller".into();
    }
    let cpu = args.cpu.or_else(|| {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let list = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?;
        list.trim().split(['-', ',']).next()?.parse().ok()
    });
    let Some(cpu) = cpu else {
        return "unavailable: no allowed CPU information".into();
    };
    match std::process::Command::new("taskset")
        .args(["-pc", &cpu.to_string(), &std::process::id().to_string()])
        .output()
    {
        Ok(output) if output.status.success() => format!("pinned:{cpu}"),
        Ok(output) => format!(
            "unavailable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => format!("unavailable: {e}"),
    }
}
fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    if samples.len() % 2 == 1 {
        samples[samples.len() / 2]
    } else {
        (samples[samples.len() / 2 - 1] + samples[samples.len() / 2]) / 2.0
    }
}
fn same_result(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    if a == b {
        return true;
    }
    match (a.as_f64(), b.as_f64()) {
        (Some(a), Some(b)) => (a - b).abs() <= 1e-10 * a.abs().max(b.abs()).max(1.0),
        _ => false,
    }
}
struct Engine<'a> {
    name: &'a str,
    binary_hash: &'a str,
}
fn row(
    engine: Engine<'_>,
    dialect: &str,
    workload: &workloads::Workload,
    trips: [u64; 2],
    samples: Vec<[Sample; 2]>,
    fixed_scopes_ns: BTreeMap<String, f64>,
) -> Result<Row> {
    ensure!(!samples.is_empty(), "empty sample population");
    for pair in &samples {
        for i in 0..2 {
            ensure!(
                pair[i].ns.is_finite() && pair[i].ns > 0.0,
                "invalid duration"
            );
            ensure!(
                same_result(&pair[i].result, &samples[0][i].result),
                "nondeterministic result"
            );
        }
    }
    let operations = [
        trips[0]
            .checked_mul(workload.ops)
            .context("operation overflow")?,
        trips[1]
            .checked_mul(workload.ops)
            .context("operation overflow")?,
    ];
    let minimum_ns = [0, 1].map(|i| {
        samples
            .iter()
            .map(|p| p[i].ns)
            .fold(f64::INFINITY, f64::min)
    });
    let median_ns = [0, 1].map(|i| median(samples.iter().map(|p| p[i].ns).collect()));
    let instructions = samples
        .iter()
        .map(|pair| Some([pair[0].instructions? as f64, pair[1].instructions? as f64]))
        .collect::<Option<Vec<_>>>()
        .map(|counts| {
            let minimum = [0, 1].map(|i| {
                counts
                    .iter()
                    .map(|pair| pair[i])
                    .fold(f64::INFINITY, f64::min)
            });
            let medians = [0, 1].map(|i| median(counts.iter().map(|pair| pair[i]).collect()));
            // The same affine model applies to counts; its slope's unit is instructions/op.
            InstructionSlopes {
                minimum_per_operation: math::fit(operations, minimum).slope_ns,
                median_per_operation: math::fit(operations, medians).slope_ns,
            }
        });
    Ok(Row {
        engine: engine.name.into(),
        dialect: dialect.into(),
        workload: workload.name.into(),
        source_hash: hash(if dialect == "python" {
            workload.py.as_bytes()
        } else {
            workload.js.as_bytes()
        }),
        engine_identity: engine.binary_hash.into(),
        trips,
        operations,
        samples,
        minimum_ns,
        median_ns,
        minimum: math::fit(operations, minimum_ns),
        median: math::fit(operations, median_ns),
        instructions,
        fixed_scopes_ns,
        ratios: BTreeMap::new(),
        execution_census: None,
    })
}
fn lash(
    args: &Args,
    kernel: &kernel::Kernel,
    workload: &workloads::Workload,
    python: bool,
    trips: [u64; 2],
) -> Result<Row> {
    let counter = args
        .instructions
        .then(InstructionCounter::open_inherited)
        .transpose()?;
    let source = if python { workload.py } else { workload.js };
    let programs = trips.map(|n| {
        let source = if python {
            format!("{source}\nprint(benchmark({n}))\n")
        } else {
            format!("{source}\nconsole.log(JSON.stringify(benchmark({n})));\n")
        };
        kernel
            .prepare(&source, python)
            .map_err(anyhow::Error::from_boxed)
    });
    let [low, high] = programs;
    let (low, mut phases) = low?;
    let (high, _) = high?;
    let mut samples = Vec::new();
    for run in 0..args.runs + args.warmups {
        let a = execution(&low, args.observe, counter.as_ref())?;
        let b = execution(&high, args.observe, counter.as_ref())?;
        if run >= args.warmups {
            samples.push([a, b]);
        }
    }
    // Count first-use preparation and take an actual first checkpoint only in
    // a separate diagnostic machine; sealing perturbs subsequent execution.
    let preparation = std::rc::Rc::new(std::cell::Cell::new(0.0));
    let total = std::rc::Rc::clone(&preparation);
    let mut stack = Vec::new();
    lash_kernel_vm::perf_witness::observe_preparation(Some(Box::new(move |event| match event {
        lash_kernel_vm::perf_witness::Preparation::Begin { .. } => stack.push(Instant::now()),
        lash_kernel_vm::perf_witness::Preparation::End => {
            if let Some(at) = stack.pop() {
                total.set(total.get() + elapsed(at));
            }
        }
    })));
    struct Observer;
    impl Drop for Observer {
        fn drop(&mut self) {
            lash_kernel_vm::perf_witness::observe_preparation(None);
        }
    }
    let _observer = Observer;
    let diagnostic_kernel = args.kernel()?;
    let (diagnostic_program, _) = diagnostic_kernel
        .prepare(
            &if python {
                format!("{source}\nprint(benchmark({}))", trips[0])
            } else {
                format!(
                    "{source}\nconsole.log(JSON.stringify(benchmark({})));",
                    trips[0]
                )
            },
            python,
        )
        .map_err(anyhow::Error::from_boxed)?;
    let (diagnostic_sample, diagnostic) = kernel::execute(&diagnostic_program, true, args.observe)
        .map_err(anyhow::Error::from_boxed)?;
    phases.extend(diagnostic);
    phases.insert("library_preparation_inclusive".into(), preparation.get());
    phases.insert("embedding_process_once".into(), kernel.setup_ns);
    let mut row = row(
        Engine {
            name: "lash",
            binary_hash: &hash(&std::fs::read(std::env::current_exe()?)?),
        },
        if python { "python" } else { "typescript" },
        workload,
        trips,
        samples,
        phases,
    )?;
    ensure!(
        same_result(&diagnostic_sample.result, &row.samples[0][0].result),
        "diagnostic result differs from quiet result"
    );
    row.execution_census = Some(
        diagnostic_sample
            .execution_census
            .context("diagnostic census missing")?,
    );
    Ok(row)
}

fn execution(
    program: &kernel::PreparedCell,
    observe: bool,
    counter: Option<&InstructionCounter>,
) -> Result<Sample> {
    // Read only across the synchronous fresh-cell call. Lowering, embedding,
    // diagnostics, executable hashing and optional history are outside this
    // interval, rather than two multi-billion-instruction process intercepts.
    let before = counter.map(InstructionCounter::snapshot).transpose()?;
    let sample = kernel::execute(program, false, observe).map_err(anyhow::Error::from_boxed)?;
    let after = counter.map(InstructionCounter::snapshot).transpose()?;
    let window = before
        .zip(after)
        .map(|(before, after)| after.since(before))
        .transpose()?;
    Ok(Sample {
        instruction_window: window,
        ns: sample.0.ns,
        result: sample.0.result,
        instructions: window.map(|window| window.instructions),
    })
}

/// Run the explicit comparison, archiving partial observations on refusals.
///
/// # Errors
/// Invalid populations, incorrect results, failed engines or requested drift checks.
pub fn run(args: &Args) -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "compare requires an optimized build"
    );
    ensure!(
        args.runs > 0 && args.warmups > 0 && args.factor > 1,
        "runs/warmups must be positive; factor must exceed one"
    );
    ensure!(
        args.tolerance.is_finite() && args.tolerance >= 0.0,
        "invalid tolerance"
    );
    for name in &args.workload {
        ensure!(
            workloads::ALL.iter().any(|w| w.name == name),
            "unknown workload: {name}"
        );
    }
    if let Some(parent) = args.out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let dir = args.out.with_extension("sources");
    std::fs::create_dir_all(&dir)?;
    let cpu = pin(args);
    let mut receipt = Receipt {
        recorded_at: chrono::Utc::now().to_rfc3339(),
        commit: crate::perf_support::git::head_commit().unwrap_or_default(),
        run_id: format!("compare-{}", std::process::id()),
        cpu,
        load_before: load(),
        load_after: String::new(),
        geometry: serde_json::json!({"version":2,"state":"fresh in-memory cell; no persisted history or state input","instructions":if args.instructions { "instructions:u around fresh cell construction/run/drop/result validation; preparation/diagnostics/driver I/O excluded" } else { "off" },"observation":if args.observe { "work/context/path/counts/sites" } else { "off" },"factor":args.factor,"runs":args.runs,"warmups":args.warmups,"charge":if args.no_charge {"off"} else {"exact"},"memory_meter":if args.no_memory_meter {"off"} else {"on"},"memory_bytes":if args.no_memory_meter {None} else {Some(512_u64<<20)},"charge_limit":if args.no_charge {None} else {Some(10_000_000_000_u64)},"lash":"fresh cell start+run; lower/link/drop/result validation excluded","reference":"resident interpreter; function call; process startup/JSON transport excluded","reference_instructions":"paired perf stat instructions:u whole processes, unscaled >=99% running; fixed low-count warmups; startup/parse/teardown cancel to first order; low/high order alternates","minimum":"two-point fit of per-count minima","median":"two-point fit of per-count medians","diagnostic":"separate fresh machine; first checkpoint before run; preparation inclusive"}),
        rows: Vec::new(),
        errors: Vec::new(),
        unsupported: Vec::new(),
        durable: Vec::new(),
    };
    let kernel = args.kernel()?;
    let references = reference::engines(args)?;
    for workload in workloads::ALL
        .iter()
        .filter(|w| args.workload.is_empty() || args.workload.iter().any(|n| n == w.name))
    {
        let trips = [
            workload.low,
            workload
                .low
                .checked_mul(args.factor)
                .context("trip overflow")?,
        ];
        for python in [false, true] {
            let dialect = if python { "python" } else { "typescript" };
            if args.dialect.as_deref().is_some_and(|d| d != dialect) {
                continue;
            }
            let start = receipt.rows.len();
            match lash(args, &kernel, workload, python, trips) {
                Ok(row) => receipt.rows.push(row),
                Err(e) => receipt
                    .errors
                    .push(format!("lash {dialect} {}: {e:#}", workload.name)),
            }
            for reference in references.iter().filter(|r| r.python == python) {
                let measured = reference
                    .measure(args, workload, trips, &dir)
                    .and_then(|samples| {
                        row(
                            Engine {
                                name: &reference.name,
                                binary_hash: &reference.identity,
                            },
                            dialect,
                            workload,
                            trips,
                            samples,
                            BTreeMap::new(),
                        )
                    });
                match measured {
                    Ok(row) => receipt.rows.push(row),
                    Err(e) => {
                        if let Some(unsupported) = e.downcast_ref::<reference::Unsupported>() {
                            receipt.unsupported.push(UnsupportedRow {
                                engine: reference.name.clone(),
                                workload: workload.name.into(),
                                source_hash: hash(workload.py.as_bytes()),
                                engine_identity: reference.identity.clone(),
                                error: unsupported.0.clone(),
                            });
                        } else {
                            receipt.errors.push(format!(
                                "{} {dialect} {}: {e:#}",
                                reference.name, workload.name
                            ));
                        }
                    }
                }
            }
            let group = &mut receipt.rows[start..];
            let oracle = group
                .iter()
                .position(|r| r.engine == "cpython")
                .or_else(|| group.iter().position(|r| r.engine == "lash"));
            if let Some(oracle) = oracle {
                for i in 0..group.len() {
                    if i != oracle
                        && group[i]
                            .samples
                            .iter()
                            .zip(&group[oracle].samples)
                            .any(|(a, b)| (0..2).any(|j| !same_result(&a[j].result, &b[j].result)))
                    {
                        receipt.errors.push(format!(
                            "{} {dialect} {}: result differs from {}",
                            group[i].engine, workload.name, group[oracle].engine
                        ));
                    }
                }
            }
            if let Some(lash) = group.iter().position(|r| r.engine == "lash") {
                for i in 0..group.len() {
                    if i != lash
                        && group[i].median.slope_ns > 0.0
                        && group[lash].median.slope_ns > 0.0
                    {
                        let ratio = group[lash].median.slope_ns / group[i].median.slope_ns;
                        let name = group[i].engine.clone();
                        group[lash].ratios.insert(name, ratio);
                    }
                }
            }
            // Keep evidence even when a later engine, workload or profiler fails.
            std::fs::write(&args.out, serde_json::to_vec_pretty(&receipt)?)?;
        }
    }
    match durable::run(args, &dir) {
        Ok(rows) => receipt.durable = rows,
        Err(error) => receipt
            .errors
            .push(format!("durable comparison: {error:#}")),
    }
    receipt.load_after = load();
    let mut drift = Vec::new();
    if args.check {
        for row in receipt.rows.iter().filter(|r| {
            r.engine == "lash"
                && r.instructions
                    .as_ref()
                    .is_none_or(|r| r.median_per_operation <= 0.0)
        }) {
            drift.push(format!(
                "{}/{}: nonpositive instruction slope; increase trip separation",
                row.dialect, row.workload
            ));
        }
    }
    let history_drift = if receipt.errors.is_empty() && drift.is_empty() {
        crate::runtime_perf::compare_history::record(args, &receipt)?
    } else {
        Vec::new()
    };
    drift.extend(history_drift);
    std::fs::write(&args.out, serde_json::to_vec_pretty(&receipt)?)?;
    std::fs::write(args.out.with_extension("md"), table(&receipt))?;
    if let Some(dir) = &args.profile {
        profiles(args, dir)?;
    }
    ensure!(
        receipt.errors.is_empty(),
        "comparison incomplete: {}",
        receipt.errors.join("; ")
    );
    ensure!(
        drift.is_empty(),
        "slope regression check: {}",
        drift.join("; ")
    );
    Ok(())
}
fn table(receipt: &Receipt) -> String {
    let mut text = python_table(receipt);
    if receipt.geometry["charge"] != "exact" || receipt.geometry["memory_meter"] != "on" {
        text = format!(
            "Diagnostic accounting control: charge={}, memory_meter={}.\n\n{text}",
            receipt.geometry["charge"], receipt.geometry["memory_meter"]
        );
    }
    text.push_str(
        "| Dialect | Workload | Engine | Min slope ns/op | Median slope ns/op | Fixed intercept ns | Lash/reference (median slope) |\n|---|---|---|---:|---:|---:|---|\n",
    );
    for row in &receipt.rows {
        use std::fmt::Write as _;
        let _ = writeln!(
            text,
            "| {} | {} | {} | {:.2} | {:.2} | {} | {} |",
            row.dialect,
            row.workload,
            row.engine,
            row.minimum.slope_ns,
            row.median.slope_ns,
            if row.workload == "object_sort" {
                "n/a (nonlinear)".into()
            } else {
                format!("{:.0}", row.median.intercept_ns)
            },
            row.ratios
                .iter()
                .map(|(n, r)| format!("{n}: {r:.2}×"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if receipt.rows.iter().any(|row| row.instructions.is_some()) {
        text.push_str("\n| Dialect | Workload | Engine | Min instructions/op | Median instructions/op |\n|---|---|---|---:|---:|\n");
        for row in &receipt.rows {
            if let Some(counts) = &row.instructions {
                use std::fmt::Write as _;
                let _ = writeln!(
                    text,
                    "| {} | {} | {} | {:.2} | {:.2} |",
                    row.dialect,
                    row.workload,
                    row.engine,
                    counts.minimum_per_operation,
                    counts.median_per_operation
                );
            }
        }
    }
    text.push_str("\nExact physical execution census: separate diagnostic cell at the low trip count, after its first checkpoint. Ordinary slope samples are uncounted. Scalar regions are separate from the continuous register program. Statement counts include helper-plan loop-test statement boundaries; region speculation counts only committed statements.\n\n");
    text.push_str("| Dialect | Workload | Trips | Owner | Statements | Register entries (run) |\n|---|---|---:|---|---:|---:|\n");
    for row in &receipt.rows {
        if let Some(census) = &row.execution_census {
            use std::fmt::Write as _;
            for (owner, statements) in &census.statements {
                let _ = writeln!(
                    text,
                    "| {} | {} | {} | {:?} | {} | {} |",
                    row.dialect,
                    row.workload,
                    row.trips[0],
                    owner,
                    statements,
                    census.register_entries
                );
            }
            let _ = writeln!(
                text,
                "\n{} / {} transitions: `{:?}`; guard/slow exits: `{:?}`.\n",
                row.dialect, row.workload, census.transitions, census.slow_exits
            );
        }
    }
    for e in &receipt.errors {
        text.push_str(&format!("\nRefused: {e}\n"));
    }
    text
}
fn python_table(receipt: &Receipt) -> String {
    use std::fmt::Write as _;
    let mut text = String::from(
        "| Python workload | Lash-Python instr/op | Monty instr/op | Lash/Monty | Lash-TS/QuickJS |\n|---|---:|---:|---:|---:|\n",
    );
    let counts = |name: &str, dialect: &str, workload: &str| {
        receipt
            .rows
            .iter()
            .find(|r| r.engine == name && r.dialect == dialect && r.workload == workload)
            .and_then(|r| r.instructions.as_ref())
            .map(|r| r.median_per_operation)
    };
    let number = |n: Option<f64>| n.map_or_else(|| "unavailable".into(), |n| format!("{n:.2}"));
    let ratio = |a: Option<f64>, b: Option<f64>| {
        a.zip(b)
            .filter(|(a, b)| *a > 0.0 && *b > 0.0)
            .map_or_else(|| "unavailable".into(), |(a, b)| format!("{:.2}×", a / b))
    };
    let mut workloads = workloads::ALL
        .iter()
        .filter(|w| {
            receipt
                .rows
                .iter()
                .any(|r| r.workload == w.name && r.dialect == "python")
        })
        .collect::<Vec<_>>();
    workloads.sort_by(|a, b| {
        let ratio = |w: &workloads::Workload| {
            counts("lash", "python", w.name)
                .zip(counts("monty", "python", w.name))
                .filter(|(a, b)| *a > 0.0 && *b > 0.0)
                .map_or(f64::NEG_INFINITY, |(a, b)| a / b)
        };
        ratio(b).total_cmp(&ratio(a))
    });
    for workload in workloads {
        let name = workload.name;
        let lash = counts("lash", "python", name);
        let monty = counts("monty", "python", name);
        let unsupported = receipt
            .unsupported
            .iter()
            .find(|r| r.engine == "monty" && r.workload == name);
        let monty_text = unsupported.map_or_else(
            || number(monty),
            |r| {
                format!(
                    "unsupported: {}",
                    r.error
                        .lines()
                        .last()
                        .unwrap_or(&r.error)
                        .replace('|', "\\|")
                )
            },
        );
        let _ = writeln!(
            text,
            "| {name} | {} | {monty_text} | {} | {} |",
            number(lash),
            ratio(lash, monty),
            ratio(
                counts("lash", "typescript", name),
                counts("qjs-0", "typescript", name)
            )
        );
    }
    for row in &receipt.unsupported {
        let _ = writeln!(
            text,
            "\n{} / {} unsupported:\n```text\n{}\n```",
            row.engine,
            row.workload,
            row.error.trim()
        );
    }
    text.push('\n');
    text
}

fn profiles(args: &Args, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for workload in workloads::ALL
        .iter()
        .filter(|w| args.workload.is_empty() || args.workload.iter().any(|n| n == w.name))
    {
        for dialect in ["typescript", "python"] {
            if args.dialect.as_deref().is_some_and(|d| d != dialect) {
                continue;
            }
            let stem = dir.join(format!("{dialect}-{}", workload.name));
            let data = stem.with_extension("perf.data");
            let output = std::process::Command::new("perf")
                .args([
                    "record",
                    "-e",
                    "cycles:u",
                    "--call-graph",
                    "dwarf",
                    "--no-buildid-cache",
                    "-o",
                ])
                .arg(&data)
                .arg("--")
                .arg(std::env::current_exe()?)
                .arg("compare")
                .arg("--out")
                .arg(stem.with_extension("json"))
                .args([
                    "--workload",
                    workload.name,
                    "--dialect",
                    dialect,
                    "--runs",
                    "20",
                    "--factor",
                    &args.factor.to_string(),
                ])
                .output()?;
            std::fs::write(stem.with_extension("perf.log"), &output.stderr)?;
            ensure!(
                output.status.success(),
                "perf record failed for {dialect} {}",
                workload.name
            );
            let stacks = std::process::Command::new("perf")
                .args(["script", "-i"])
                .arg(&data)
                .output()?;
            ensure!(stacks.status.success(), "perf script failed");
            let script = stem.with_extension("stacks.txt");
            std::fs::write(&script, stacks.stdout)?;
            let folded = std::process::Command::new("inferno-collapse-perf")
                .arg(&script)
                .output()?;
            ensure!(
                folded.status.success() && !folded.stdout.is_empty(),
                "no folded perf samples"
            );
            std::fs::write(stem.with_extension("folded"), folded.stdout)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct Cli {
        #[command(flatten)]
        args: super::Args,
    }

    #[test]
    fn comparison_does_not_select_persistent_history_by_default() {
        let cli = Cli::parse_from(["compare", "--out", "receipt.json"]);
        assert!(
            cli.args.history.is_none(),
            "ordinary measurements must not read or append a persistent history"
        );
        assert!(Cli::try_parse_from(["compare", "--out", "receipt.json", "--check"]).is_err());
        for diagnostic in ["--no-charge", "--no-memory-meter"] {
            assert!(
                Cli::try_parse_from([
                    "compare",
                    "--out",
                    "receipt.json",
                    "--history",
                    "baseline.jsonl",
                    diagnostic,
                ])
                .is_err(),
                "an accounting ablation must not become a regression baseline"
            );
        }
    }

    #[test]
    fn instruction_slopes_remove_fixed_cell_execution_cost() {
        let samples = [900_000, 1_000_000, 800_000]
            .map(|fixed| {
                [100, 10_000].map(|n| super::Sample {
                    ns: 900_000.0 + 17.0 * n as f64,
                    result: serde_json::json!(0),
                    instructions: Some(fixed + 23 * n),
                    instruction_window: None,
                })
            })
            .to_vec();
        let row = super::row(
            super::Engine {
                name: "lash",
                binary_hash: "",
            },
            "typescript",
            &super::workloads::Workload {
                name: "synthetic",
                low: 100,
                ops: 1,
                js: "",
                py: "",
            },
            [100, 10_000],
            samples,
            Default::default(),
        )
        .unwrap();
        use crate::perf_support::instructions::InstructionReading;
        for (running, accepted) in [(98, false), (99, true), (101, false)] {
            assert_eq!(
                InstructionReading {
                    instructions: 100,
                    time_enabled: 100,
                    time_running: running,
                }
                .qualify()
                .is_ok(),
                accepted
            );
        }
        assert!(InstructionReading::default().qualify().is_err());
        let counts = row.instructions.unwrap();
        assert_eq!(counts.minimum_per_operation, 23.0);
        assert_eq!(counts.median_per_operation, 23.0);
    }
}
