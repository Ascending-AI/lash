fn main() {
    // Each RLM execution bound is its own type, so transposing the instruction
    // budget and the memory limit is a compile error rather than a 64 MiB
    // instruction budget paired with a 1,000,000-byte heap.
    let _config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::MemoryBound::mebibytes(64))
        .wall_clock(lash::rlm::WallClockBound::secs(30))
        .memory_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .build();
}
