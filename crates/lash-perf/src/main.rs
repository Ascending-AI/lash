//! `lash-perf` — developer-only synthetic runtime benchmark binary.
//!
//! Driven by `scripts/profile_runtime.py` and
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

    /// Concurrent workers for durable queued-work contention scenarios
    #[arg(long, default_value_t = 4)]
    runtime_perf_contention_workers: usize,

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

    #[arg(long, default_value_t = 0)]
    runtime_perf_load_arrival_rate: u64,

    /// Weighted high-traffic turn mix as comma-separated `kind=weight` pairs
    #[arg(
        long,
        default_value = "plain=1,tool=1,queued=1,child=1,wake=1,trigger=1"
    )]
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

        /// Limit the table to one benchmark size preset. Default: every preset
        /// present in the file, each as its own series.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
    },

    /// Run the send-to-completion latency gate (FIG-3843) against a live
    /// restate-server: `RESTATE_INGRESS_URL`/`RESTATE_ADMIN_URL` when a
    /// launcher provides one, a spawned private server otherwise.
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
    },

    /// The cross-worker child a latency case drives: serves lash's Restate
    /// services over the shared store directory. Not run by hand; `latency`
    /// spawns it.
    LatencyWorker {
        #[arg(long, value_name = "DIR")]
        store_dir: std::path::PathBuf,

        #[arg(long, value_name = "FILE")]
        ready_file: std::path::PathBuf,

        #[arg(long, value_name = "ADDR")]
        endpoint_bind: std::net::SocketAddr,
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
    let args = Args::parse();
    match &args.command {
        Some(Command::StringScaling { out }) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            return runtime.block_on(lash_perf::string_scaling::run(out.as_deref()));
        }
        Some(Command::DurationTrend { history, profile }) => {
            // Pure history reading: no runtime, no measurement, no exit code.
            return lash_perf::runtime_perf::run_duration_trend_cli(history, profile.as_deref());
        }
        Some(Command::Latency {
            out,
            samples_out,
            store_dir,
            cases,
            fast_samples,
            lanes,
            scale_down,
        }) => {
            let run = lash_perf::latency::LatencyRun {
                out: out.clone(),
                samples_out: samples_out.clone(),
                store_dir: store_dir.clone(),
                cases: cases.clone(),
                fast_samples: *fast_samples,
                lanes: *lanes,
                scale_down: *scale_down,
            };
            let mut runtime = tokio::runtime::Builder::new_multi_thread();
            runtime.enable_all();
            runtime.thread_stack_size(tokio_thread_stack_bytes(&args));
            let code = runtime.build()?.block_on(lash_perf::latency::run(run))?;
            std::process::exit(code);
        }
        Some(Command::LatencyWorker {
            store_dir,
            ready_file,
            endpoint_bind,
        }) => {
            let worker = lash_perf::latency::LatencyWorkerArgs {
                store_dir: store_dir.clone(),
                ready_file: ready_file.clone(),
                endpoint_bind: *endpoint_bind,
            };
            let mut runtime = tokio::runtime::Builder::new_multi_thread();
            runtime.enable_all();
            runtime.thread_stack_size(tokio_thread_stack_bytes(&args));
            runtime
                .build()?
                .block_on(lash_perf::latency::run_worker(worker))?;
            return Ok(());
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
        contention_workers: args.runtime_perf_contention_workers,
        checkpoint_transcript_bytes: args.runtime_perf_checkpoint_transcript_bytes,
        checkpoint_messages: args.runtime_perf_checkpoint_messages,
        checkpoint_graph_rows: args.runtime_perf_checkpoint_graph_rows,
        checkpoint_components: args.runtime_perf_checkpoint_components,
        high_traffic_population: args.runtime_perf_load_population,
        high_traffic_arrival_rate: args.runtime_perf_load_arrival_rate,
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
