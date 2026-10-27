//! Shared qualified Linux instruction counter and the replay host's lifetime.
use anyhow::{Context, Result};
pub use lash_vm_worker::core_support::instructions::{InstructionCounter, InstructionReading};

static HOST: std::sync::OnceLock<InstructionCounter> = std::sync::OnceLock::new();

/// Call before creating the replay runtime, so every host thread is included.
pub fn enable_host() -> Result<()> {
    HOST.set(InstructionCounter::open_threads()?)
        .map_err(|_| anyhow::anyhow!("host PMU already open"))
}

pub fn host_snapshot() -> Result<InstructionReading> {
    HOST.get()
        .context("host PMU must open before runtime startup")?
        .snapshot()
        .map_err(Into::into)
}
