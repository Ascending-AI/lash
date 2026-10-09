//! `lash-perf` — developer-only synthetic runtime benchmark binary.
//!
//! Executed by `scripts/profile_runtime.py` and
//! `scripts/profile_runtime_stack.py`.
//! It runs provider-free runtime scenarios against in-process fixtures and
//! writes a structured JSON report.

use clap::Parser;
#[cfg(not(feature = "dhat-heap"))]
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
#[cfg(not(feature = "dhat-heap"))]
use std::alloc::System;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static GLOBAL_ALLOCATOR: &lash_perf::DhatStatsAllocator = &lash_perf::GLOBAL_ALLOCATOR;

// The same `INSTRUMENTED_SYSTEM` instance that `lash_perf::GLOBAL_ALLOCATOR`
// reads its counters from.
#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const DEFAULT_TOKIO_THREAD_STACK_BYTES: usize = 2 * 1024 * 1024;
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Synthetic non-inference runtime performance benchmark for Lash.
#[derive(Debug, Parser)]
#[command(name = "lash-perf", version)]
struct Args {
    /// Omitted, the binary runs the benchmark.
    #[command(subcommand)]
    command: Option<Command>,

    /// Write the runtime benchmark JSON report to this file
    #[arg(long, value_name = "OUT.json")]
    runtime_perf_out: Option<std::path::PathBuf>,

    /// Write a dhat heap profile for the measured runtime benchmark window
    #[arg(long)]
    runtime_perf_dhat: bool,

    /// Destination for the dhat heap profile
    #[arg(long, value_name = "OUT.json")]
    runtime_perf_dhat_out: Option<std::path::PathBuf>,

    /// Trim dhat backtraces to this many frames
    #[arg(long, value_name = "FRAMES")]
    runtime_perf_dhat_frames: Option<usize>,

    /// Number of measured runs for the runtime benchmark
    #[arg(long, default_value_t = 5)]
    runtime_perf_runs: usize,

    /// Number of warmup runs for the runtime benchmark
    #[arg(long, default_value_t = 1)]
    runtime_perf_warmups: usize,

    /// Limit the runtime benchmark to one or more named scenarios
    #[arg(long, value_name = "SCENARIO")]
    runtime_perf_scenario: Vec<String>,

    /// Number of committed turns to run inside each measured runtime session
    #[arg(long, default_value_t = 12)]
    runtime_perf_turns: usize,

    /// Concurrent session population for high-traffic load scenarios
    #[arg(long, default_value_t = 4)]
    runtime_perf_load_population: usize,

    /// Fixed transcript/body byte target at the center of the durable checkpoint curve
    #[arg(long, default_value_t = 8 * 1024)]
    runtime_perf_checkpoint_transcript_bytes: usize,

    /// Messages represented in every durable checkpoint curve commit
    #[arg(long, default_value_t = 8)]
    runtime_perf_checkpoint_messages: usize,

    /// Graph rows represented in every durable checkpoint curve commit
    #[arg(long, default_value_t = 16)]
    runtime_perf_checkpoint_graph_rows: usize,

    /// Fixed component count at the center of the durable checkpoint curve
    #[arg(long, default_value_t = 32)]
    runtime_perf_checkpoint_components: usize,

    /// Weighted high-traffic turn mix as comma-separated `kind=weight` pairs
    #[arg(long, default_value = "plain=1,tool=1,queued=1,child=1")]
    runtime_perf_load_mix: String,

    /// Comma-separated populations for high-traffic knee-search scenarios
    #[arg(long, default_value = "4,8")]
    runtime_perf_knee_populations: String,

    /// First p95-vs-initial-step ratio reported as the saturation knee
    #[arg(long, default_value_t = 1.25)]
    runtime_perf_knee_threshold: f64,

    /// Tokio worker stack size for runtime benchmark processes
    #[arg(long, value_name = "BYTES")]
    runtime_perf_worker_stack_bytes: Option<usize>,

    #[arg(long)]
    runtime_perf_enforce_budgets: bool,

    /// Exit non-zero only on machine-independent inventory failures (missing
    /// required phases, emitted phases without a checked-in budget). Duration
    /// and allocation ceilings are calibrated on the release profile and are
    /// enforced by --runtime-perf-enforce-budgets at release time.
    /// The two enforcement flags name three modes, not four: passing both is
    /// refused rather than resolving to the wider one.
    #[arg(long, conflicts_with = "runtime_perf_enforce_budgets")]
    runtime_perf_enforce_inventory: bool,

    /// Check scenario completion and delivery witnesses without benchmark deadlines.
    #[arg(long)]
    runtime_perf_smoke: bool,

    /// Advisory in every context: drift is warned about, never enforced (FIG-1385).
    #[arg(long, value_name = "HISTORY.jsonl")]
    runtime_perf_duration_history: Option<std::path::PathBuf>,

    /// Benchmark size preset recorded with each history entry. Durations are
    /// only comparable within one preset, so it keys the trend series.
    #[arg(long, value_name = "PROFILE", default_value = "custom")]
    runtime_perf_duration_profile: String,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Read raw operation tails from a runtime or latency receipt.
    ReceiptTail {
        #[arg(long)]
        receipt: std::path::PathBuf,
        /// Explicit raw latency ledger, when stored separately from its receipt.
        #[arg(long)]
        samples: Option<std::path::PathBuf>,
        #[arg(long, default_value_t = 5)]
        slowest: usize,
    },
    /// Run one synthetic 1.0 boundary population and write its functional receipt.
    Boundary(lash_perf::boundary::Args),
    #[command(hide = true)]
    BoundaryWorker(lash_perf::boundary::WorkerArgs),
    /// Sweep scheduled arrival rates over one population and write the
    /// offered-load receipt and its operation ledger.
    OfferedLoad(lash_perf::offered_load::Args),
    /// Regenerate the strict synthetic workload v1 JSON Schema.
    WorkloadSchema {
        #[arg(long, value_name = "SCHEMA.json")]
        out: std::path::PathBuf,
    },
    /// Validate a synthetic workload without starting any services.
    WorkloadValidate {
        #[arg(long, value_name = "WORKLOAD.json")]
        file: std::path::PathBuf,
    },
    /// Measure TypeScript string loops in the VM and enforce their scaling budget.
    StringScaling {
        /// Write the measurements and budget results to this JSON file.
        #[arg(long, value_name = "OUT.json")]
        out: Option<std::path::PathBuf>,
    },
    /// Print the advisory duration trend table for an existing history file
    /// without running the benchmark.
    DurationTrend {
        #[arg(long, value_name = "HISTORY.jsonl")]
        history: std::path::PathBuf,

        /// Export the retained observations and identities as CSV.
        #[arg(long)]
        csv: Option<std::path::PathBuf>,

        /// Limit the table to one benchmark size preset. Default: every preset
        /// present in the file, each as its own series.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },

    /// Serve the child node used by the remote latency cases.
    #[command(hide = true)]
    LatencyWorker {
        #[arg(long)]
        store_dir: std::path::PathBuf,
        #[arg(long)]
        startup_out: Option<std::path::PathBuf>,
    },

    /// Functional simultaneous 1/2/4 process startup phases; never a timing gate.
    Startup {
        #[arg(long)]
        out: std::path::PathBuf,
        #[arg(long)]
        store_dir: std::path::PathBuf,
    },
    /// Functional loopback HTTP phases, retry/body-size and arrival-rate sweeps.
    ProviderHttp(lash_perf::runtime_perf::http_population::HttpPopulationArgs),

    /// Run the send-to-completion latency gate (FIG-3843) on lash's durable
    /// engine over SQLite store sets under `--store-dir`.
    Latency {
        /// Write the latency gate JSON report to this file.
        #[arg(long, value_name = "OUT.json")]
        out: std::path::PathBuf,

        /// Write the raw per-sample ledger here; default `<out>.samples.json`.
        #[arg(long, value_name = "SAMPLES.json")]
        samples_out: Option<std::path::PathBuf>,

        /// Directory the run's SQLite store sets and the cross-worker's live in.
        #[arg(long, value_name = "DIR")]
        store_dir: std::path::PathBuf,

        /// Limit the run to these comma-separated cases.
        #[arg(long, value_delimiter = ',', value_name = "CASE")]
        cases: Vec<String>,

        /// Sample count for the gated `fast` case.
        #[arg(long, default_value_t = lash_perf::latency::GATE_MIN_SAMPLES)]
        fast_samples: usize,

        /// Concurrent session lanes per case.
        #[arg(long, default_value_t = 64)]
        lanes: usize,

        /// Shrink every case to a smoke-sized sample count for development.
        #[arg(long)]
        scale_down: bool,

        /// Write a dhat heap profile of the measured cases here. Needs a
        /// `--features dhat-heap` build; meant for `--lanes 1` growth runs.
        #[arg(long, value_name = "OUT.json")]
        dhat_out: Option<std::path::PathBuf>,

        /// Trim dhat backtraces to this many frames.
        #[arg(long, value_name = "FRAMES", requires = "dhat_out")]
        dhat_frames: Option<usize>,
    },
}

fn tokio_thread_stack_bytes(args: &Args) -> usize {
    if let Some(stack_bytes) = args.runtime_perf_worker_stack_bytes {
        return stack_bytes;
    }
    std::env::var("LASH_TOKIO_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOKIO_THREAD_STACK_BYTES)
}

fn main() -> anyhow::Result<()> {
    lash_core::perf_witness::startup::initialize_epoch();
    let args = Args::parse();
    let startup_recorder = if matches!(
        &args.command,
        Some(Command::LatencyWorker {
            startup_out: Some(_),
            ..
        })
    ) {
        Some(lash_core::perf_witness::startup::Recorder::install()?)
    } else {
        None
    };
    match &args.command {
        Some(Command::ReceiptTail {
            receipt,
            samples,
            slowest,
        }) => {
            return lash_perf::receipt_tail::run(receipt, samples.as_deref(), *slowest);
        }
        Some(Command::Boundary(options)) => {
            let mut runtime = tokio::runtime::Builder::new_multi_thread();
            runtime.enable_all();
            if let Some(stack_bytes) = options.worker_stack_bytes {
                runtime.thread_stack_size(stack_bytes);
            }
            let runtime = runtime.build()?;
            lash_perf::perf_support::dhat::ensure_dhat_parent(options.dhat_out.as_ref())?;
            let profiler = lash_perf::perf_support::dhat::start_dhat_profiler(
                options.dhat_out.clone(),
                options.dhat_frames,
                "boundary --dhat-out requires a build with `--features dhat-heap`",
            )?;
            let result = runtime.block_on(lash_perf::boundary::run(options));
            // The profile is written when the profiler drops, on either outcome.
            lash_perf::perf_support::dhat::finish_dhat_profiler(profiler);
            result?;
            return Ok(());
        }
        Some(Command::BoundaryWorker(options)) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            return runtime.block_on(lash_perf::boundary::run_worker(options));
        }
        Some(Command::OfferedLoad(options)) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(tokio_thread_stack_bytes(&args))
                .build()?;
            runtime.block_on(lash_perf::offered_load::run(options))?;
            return Ok(());
        }
        Some(Command::WorkloadSchema { out }) => {
            let schema = serde_json::to_string_pretty(&lash_perf::workload::schema())?;
            std::fs::write(out, format!("{schema}\n"))?;
            return Ok(());
        }
        Some(Command::WorkloadValidate { file }) => {
            let workload = lash_perf::workload::Workload::parse(&std::fs::read_to_string(file)?)?;
            println!(
                "validated workload v1: {} sessions, {} fields with provenance",
                workload.spec().sessions,
                workload.spec().provenance.fields.len()
            );
            return Ok(());
        }
        Some(Command::StringScaling { out }) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            return runtime.block_on(lash_perf::string_scaling::run(out.as_deref()));
        }
        Some(Command::DurationTrend {
            history,
            profile,
            csv,
        }) => {
            if let Some(out) = csv {
                lash_perf::runtime_perf::export_duration_history_csv(
                    history,
                    profile.as_deref(),
                    out,
                )?;
            }
            // Pure history reading: no runtime, no measurement, no exit code.
            return lash_perf::runtime_perf::run_duration_trend_cli(history, profile.as_deref());
        }
        Some(Command::LatencyWorker {
            store_dir,
            startup_out,
        }) => {
            let mut runtime = tokio::runtime::Builder::new_multi_thread();
            runtime
                .enable_all()
                .thread_stack_size(tokio_thread_stack_bytes(&args));
            if startup_out.is_some() {
                runtime.worker_threads(2);
            }
            let runtime = runtime.build()?;
            if let (Some(out), Some(recorder)) = (startup_out, &startup_recorder) {
                return runtime.block_on(lash_perf::latency::startup::run_worker(
                    store_dir, out, recorder,
                ));
            }
            return runtime.block_on(lash_perf::latency::run_worker(store_dir));
        }
        Some(Command::Startup { out, store_dir }) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()?;
            return runtime.block_on(lash_perf::latency::startup::run(out, store_dir));
        }
        Some(Command::ProviderHttp(options)) => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()?;
            return runtime.block_on(lash_perf::runtime_perf::http_population::run(options));
        }
        Some(Command::Latency {
            out,
            samples_out,
            store_dir,
            cases,
            fast_samples,
            lanes,
            scale_down,
            dhat_out,
            dhat_frames,
        }) => {
            let run = lash_perf::latency::LatencyRun {
                out: out.clone(),
                samples_out: samples_out.clone(),
                store_dir: store_dir.clone(),
                cases: cases.clone(),
                fast_samples: *fast_samples,
                lanes: *lanes,
                scale_down: *scale_down,
                dhat_out: dhat_out.clone(),
                dhat_frames: *dhat_frames,
            };
            let mut runtime = tokio::runtime::Builder::new_multi_thread();
            runtime.enable_all();
            runtime.thread_stack_size(tokio_thread_stack_bytes(&args));
            let code = runtime.build()?.block_on(lash_perf::latency::run(run))?;
            std::process::exit(code);
        }
        None => {}
    }
    let worker_stack_bytes = tokio_thread_stack_bytes(&args);
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    runtime.thread_stack_size(worker_stack_bytes);
    // Every argument is named at this one call site: the parameters used to be
    // positional, and seven consecutive `usize` type-check in any permutation.
    let run = lash_perf::runtime_perf::RuntimePerfRun {
        out: args.runtime_perf_out,
        enable_dhat: args.runtime_perf_dhat,
        dhat_out: args.runtime_perf_dhat_out,
        dhat_frames: args.runtime_perf_dhat_frames,
        worker_stack_bytes,
        runs: args.runtime_perf_runs,
        warmups: args.runtime_perf_warmups,
        scenario_filters: args.runtime_perf_scenario,
        chat_turns: args.runtime_perf_turns,
        checkpoint_transcript_bytes: args.runtime_perf_checkpoint_transcript_bytes,
        checkpoint_messages: args.runtime_perf_checkpoint_messages,
        checkpoint_graph_rows: args.runtime_perf_checkpoint_graph_rows,
        checkpoint_components: args.runtime_perf_checkpoint_components,
        high_traffic_population: args.runtime_perf_load_population,
        high_traffic_mix: args.runtime_perf_load_mix,
        high_traffic_knee_populations: args.runtime_perf_knee_populations,
        high_traffic_knee_threshold: args.runtime_perf_knee_threshold,
        enforcement: lash_perf::runtime_perf::BudgetEnforcement::from_flags(
            args.runtime_perf_enforce_budgets,
            args.runtime_perf_enforce_inventory,
        )?,
        smoke: args.runtime_perf_smoke,
        duration_history: args.runtime_perf_duration_history,
        duration_profile: args.runtime_perf_duration_profile,
        version: APP_VERSION.to_string(),
    };
    runtime
        .build()?
        .block_on(lash_perf::runtime_perf::run_cli(run))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATENCY: [&str; 6] = [
        "lash-perf",
        "latency",
        "--out",
        "latency.json",
        "--store-dir",
        "stores",
    ];

    #[test]
    fn latency_runs_without_a_heap_profile_by_default() {
        let args = Args::try_parse_from(LATENCY).expect("parse");

        assert!(matches!(
            args.command,
            Some(Command::Latency {
                dhat_out: None,
                dhat_frames: None,
                ..
            })
        ));
    }

    #[test]
    fn latency_takes_a_heap_profile_path_and_frame_trim() {
        let args = Args::try_parse_from(LATENCY.into_iter().chain([
            "--dhat-out",
            "dhat.json",
            "--dhat-frames",
            "24",
        ]))
        .expect("parse");

        let Some(Command::Latency {
            dhat_out,
            dhat_frames,
            ..
        }) = args.command
        else {
            panic!("expected the latency command");
        };
        assert_eq!(dhat_out, Some(std::path::PathBuf::from("dhat.json")));
        assert_eq!(dhat_frames, Some(24));
    }

    #[test]
    fn latency_refuses_a_frame_trim_without_a_heap_profile() {
        let parsed = Args::try_parse_from(LATENCY.into_iter().chain(["--dhat-frames", "24"]));

        assert!(parsed.is_err());
    }
}
