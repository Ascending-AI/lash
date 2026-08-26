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
    /// Read-only companion commands. Omitted, the binary runs the benchmark.
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

    /// Open-loop arrivals per second for high-traffic scenarios; zero starts
    /// every session immediately
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

    /// Exit non-zero when a runtime perf budget is exceeded
    #[arg(long)]
    runtime_perf_enforce_budgets: bool,

    /// Exit non-zero only on machine-independent inventory failures (missing
    /// required phases, emitted phases without a checked-in budget). Duration
    /// and allocation ceilings are calibrated on the release profile and are
    /// enforced by --runtime-perf-enforce-budgets at release time.
    #[arg(long)]
    runtime_perf_enforce_inventory: bool,

    /// Append this run's per-scenario wall-clock and whole-window duration
    /// medians to a history and print the trend table. Advisory in every
    /// context: drift is warned about, never enforced (FIG-1385).
    #[arg(long, value_name = "HISTORY.jsonl")]
    runtime_perf_duration_history: Option<std::path::PathBuf>,

    /// Benchmark size preset recorded with each history entry. Durations are
    /// only comparable within one preset, so it keys the trend series.
    #[arg(long, value_name = "PROFILE", default_value = "custom")]
    runtime_perf_duration_profile: String,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Print the advisory duration trend table for an existing history file
    /// without running the benchmark.
    DurationTrend {
        /// Append-only JSONL history written by
        /// `--runtime-perf-duration-history`.
        #[arg(long, value_name = "HISTORY.jsonl")]
        history: std::path::PathBuf,

        /// Limit the table to one benchmark size preset. Default: every preset
        /// present in the file, each as its own series.
        #[arg(long, value_name = "PROFILE")]
        profile: Option<String>,
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
    if let Some(Command::DurationTrend { history, profile }) = &args.command {
        // Pure history reading: no runtime, no measurement, no exit code.
        return lash_perf::runtime_perf::run_duration_trend_cli(history, profile.as_deref());
    }
    let worker_stack_bytes = tokio_thread_stack_bytes(&args);
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    runtime.thread_stack_size(worker_stack_bytes);
    runtime.build()?.block_on(lash_perf::runtime_perf::run_cli(
        args.runtime_perf_out,
        args.runtime_perf_dhat,
        args.runtime_perf_dhat_out,
        args.runtime_perf_dhat_frames,
        worker_stack_bytes,
        args.runtime_perf_runs,
        args.runtime_perf_warmups,
        args.runtime_perf_scenario,
        args.runtime_perf_turns,
        args.runtime_perf_contention_workers,
        args.runtime_perf_checkpoint_transcript_bytes,
        args.runtime_perf_checkpoint_messages,
        args.runtime_perf_checkpoint_graph_rows,
        args.runtime_perf_checkpoint_components,
        args.runtime_perf_load_population,
        args.runtime_perf_load_arrival_rate,
        args.runtime_perf_load_mix,
        args.runtime_perf_knee_populations,
        args.runtime_perf_knee_threshold,
        args.runtime_perf_enforce_budgets,
        args.runtime_perf_enforce_inventory,
        args.runtime_perf_duration_history,
        args.runtime_perf_duration_profile,
        APP_VERSION,
    ))
}
