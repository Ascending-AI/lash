fn main() {
    // `build()` exists only once both bounds are named, so a config that
    // forgot its memory limit is a compile error rather than a silent default.
    let _config = lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .build();
}
