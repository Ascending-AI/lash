fn main() {
    let _config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .wall_clock(lash::rlm::WallClockBound::secs(30))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
}
