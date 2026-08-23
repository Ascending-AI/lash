fn main() {
    // `build()` exists only once all three bounds are named, so a config that
    // forgot its memory limit is a compile error rather than a silent default.
    let _config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .wall_clock(lash::rlm::WallClockBound::secs(30))
        .build();
}
