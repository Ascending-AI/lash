//! The in-process live-replay buffer's registration of the live-replay laws.
//!
//! Live replay is an observation cache, not persistence (ADR 0102, D3): the
//! durable backends keep no live-replay store of their own, so the one
//! implementation is certified here.

use std::sync::Arc;
use std::time::Duration;

use crate::*;

crate::live_replay_tests!({
    let original = crate::InMemoryLiveReplayStore::default();
    let preserved = original.reopen_preserving_history();
    (
        (),
        || Arc::new(crate::InMemoryLiveReplayStore::default()) as Arc<dyn LiveReplayStore>,
        || {
            Arc::new(crate::InMemoryLiveReplayStore::with_bounds(
                1,
                Duration::from_secs(120),
            )) as Arc<dyn LiveReplayStore>
        },
        || {
            Arc::new(crate::InMemoryLiveReplayStore::with_bounds(
                16,
                Duration::from_millis(1),
            )) as Arc<dyn LiveReplayStore>
        },
        Duration::from_millis(20),
        (
            Arc::new(original) as Arc<dyn LiveReplayStore>,
            Arc::new(crate::InMemoryLiveReplayStore::default()) as Arc<dyn LiveReplayStore>,
            Arc::new(preserved) as Arc<dyn LiveReplayStore>,
        ),
    )
});
