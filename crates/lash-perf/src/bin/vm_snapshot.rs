//! VM continuation size and cost at quiet points (S1).

use clap::Parser;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::{alloc::System, path::PathBuf};

#[path = "vm_snapshot/mod.rs"]
mod benchmark;

// Match the runtime performance harness's default allocator instrumentation.
#[global_allocator]
static GLOBAL_ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    out: PathBuf,
    /// At least 100 samples for nearest-rank p99 below the maximum.
    #[arg(long, default_value_t = 100)]
    samples: usize,
    /// Measure only these named cases; omitted, measure all ten.
    #[arg(long = "case")]
    cases: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    benchmark::run(&args.out, args.samples, &args.cases)
}
