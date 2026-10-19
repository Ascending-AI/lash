//! The bounds lash-perf's code-mode harnesses run their cells under.
//!
//! A deployment's instruction and memory limits are what its runs ask for,
//! and a worker pool's `run_bounds` are the ceiling it admits; a limit past
//! the ceiling is refused when the runtime assembles (FIG-6827). Every
//! harness takes its limits and its pool's ceiling from here, so the two
//! cannot drift apart.

use lash::rlm::{InstructionBound, MemoryBound};
use lash::vm::{WorkerPoolConfig, WorkerService};

/// The instruction limit a harness cell asks for.
pub const INSTRUCTION_LIMIT: InstructionBound = InstructionBound::instructions(100_000_000);

/// The largest logical heap a harness cell may ask for.
pub const MEMORY_LIMIT: MemoryBound = MemoryBound::mebibytes(512);

/// `config` with a run ceiling that admits [`INSTRUCTION_LIMIT`] and
/// [`MEMORY_LIMIT`]; a higher ceiling of its own is kept.
#[must_use]
pub fn admitting(mut config: WorkerPoolConfig) -> WorkerPoolConfig {
    let bounds = &mut config.run_bounds;
    if let Some(instructions) = INSTRUCTION_LIMIT.limit() {
        bounds.charge_limit = bounds.charge_limit.max(instructions.get());
    }
    if let Some(memory) = MEMORY_LIMIT.limit() {
        bounds.memory = bounds.memory.max(memory.get());
    }
    config
}

/// The worker service over the helper `executable`, admitting the harness
/// limits.
#[must_use]
pub fn subprocess(executable: impl Into<std::path::PathBuf>) -> WorkerService {
    WorkerService::new(admitting(
        WorkerService::subprocess(executable).config().clone(),
    ))
}
