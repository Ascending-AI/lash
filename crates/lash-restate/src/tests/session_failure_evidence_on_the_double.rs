//! The mid-stream failure-evidence law under the Restate effect engine's
//! in-process face: the law drives `stream_turn` itself, so its effect host is
//! the Restate controller over its recording context — journaled runs execute
//! locally through the same path a handler execution records — over a SQLite
//! memory store set on an injected clock, so the two settlements order by a
//! committed timestamp the fixture advances.

use std::sync::Arc;

use super::*;

lash_conformance::session_failure_evidence_tests!({
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let stores =
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
            .await
            .expect("open the failure-evidence store set");
    let host: Arc<dyn EffectHost> = Arc::new(RestateRuntimeEffectController::new_for_test(
        Arc::new(RecordingContext::default()),
    ));
    let backend = lash_conformance::backend_over(&stores, host);
    (stores, backend, move || clock.advance(1))
});
