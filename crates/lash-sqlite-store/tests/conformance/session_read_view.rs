use std::sync::Arc;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

lash_conformance::session_read_view_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let backend = TestBackend::open_with_clock(
        SUBSTRATE,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let law_backend = backend.dyn_backend();
    (backend, law_backend, move || clock.advance(1))
});
