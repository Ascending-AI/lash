//! `lash-perf` — developer-only synthetic runtime benchmark binary.
//!
//! Driven by `scripts/profile_runtime.py` and
//! `scripts/profile_runtime_stack.py`.
//! It runs provider-free runtime scenarios against in-process fixtures and
//! writes a structured JSON report.

use clap::Parser;
#[cfg(feature = "dhat-heap")]
use dhat::Alloc as DhatAlloc;
#[cfg(not(feature = "dhat-heap"))]
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
#[cfg(not(feature = "dhat-heap"))]
use std::alloc::System;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static GLOBAL_ALLOCATOR: DhatAlloc = DhatAlloc;

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
        args.runtime_perf_enforce_budgets,
        args.runtime_perf_enforce_inventory,
        APP_VERSION,
    ))
}
