//! The in-process process-replay buffer's registration of the replay laws,
//! beside the session buffer's (`live_replay_store_tests`).

use std::sync::Arc;
use std::time::Duration;

use crate::*;

fn bounded(max_events_per_process: usize, max_age: Duration) -> Arc<dyn ProcessReplayStore> {
    Arc::new(InMemoryProcessReplayStore::new(
        InMemoryProcessReplayStoreConfig {
            max_events_per_process,
            max_age,
            ..InMemoryProcessReplayStoreConfig::standard()
        },
    ))
}

crate::process_replay_tests!({
    let original = InMemoryProcessReplayStore::new(InMemoryProcessReplayStoreConfig::standard());
    let preserved = original.reopen_preserving_history();
    (
        (),
        || bounded(2048, Duration::from_secs(120)),
        || bounded(1, Duration::from_secs(120)),
        || bounded(16, Duration::from_millis(1)),
        Duration::from_millis(20),
        (
            Arc::new(original) as Arc<dyn ProcessReplayStore>,
            bounded(2048, Duration::from_secs(120)),
            Arc::new(preserved) as Arc<dyn ProcessReplayStore>,
        ),
    )
});
