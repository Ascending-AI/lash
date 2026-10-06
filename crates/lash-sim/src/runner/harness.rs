/// The retention of the live replay a generated world's cores observe
/// through: bounded by count, never by age.
///
/// The world's host reads a turn's activity from the replay once the turn's
/// shift has stopped, and a suspended turn's only after the whole workload
/// has drained. An age bound would make a seed's evidence depend on how long
/// the host took to run the workload: a loaded run drops activity an idle
/// run keeps, and a turn's oracle sees no activity at all (FIG-5148).
#[cfg(test)]
pub(super) fn world_live_replay_config() -> lash::observe::InMemoryLiveReplayStoreConfig {
    lash::observe::InMemoryLiveReplayStoreConfig {
        max_age: std::time::Duration::MAX,
        ..lash::observe::InMemoryLiveReplayStoreConfig::default()
    }
}
