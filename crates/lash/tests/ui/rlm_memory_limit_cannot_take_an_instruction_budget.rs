// FIG-1979: the transposition guard.
//
// Every execution bound is its own type and is named at its call site, so a
// memory limit can never bind an instruction budget — not by transposing the
// arguments of a positional constructor, and not by spelling a heap ceiling
// through `instructions(..)`. Both attempts below are type errors.

use lash::rlm::{
    ExecutionBounds, InstructionBound, MemoryBound, RlmProtocolPluginConfig,
};

fn a_memory_limit_cannot_be_an_instruction_budget() {
    let _ = RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(InstructionBound::instructions(1_000_000))
        .memory_limit(InstructionBound::instructions(64 * 1024 * 1024))
        .build();
}

fn an_instruction_limit_cannot_be_a_memory_bound() {
    let _ = RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(MemoryBound::mebibytes(64))
        .memory_limit(MemoryBound::mebibytes(64))
        .build();
}

fn bounds_cannot_be_transposed() {
    let _ = ExecutionBounds::new(
        MemoryBound::mebibytes(64),
        InstructionBound::instructions(1_000_000),
    );
}

fn main() {
    let _ = a_memory_limit_cannot_be_an_instruction_budget;
    let _ = an_instruction_limit_cannot_be_a_memory_bound;
    let _ = bounds_cannot_be_transposed;
}
