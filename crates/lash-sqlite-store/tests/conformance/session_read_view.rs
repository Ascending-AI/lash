use std::sync::Arc;

use super::SUBSTRATE;
use crate::deployment_fixture::TestDeployment;

lash_conformance::session_read_view_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let deployment = TestDeployment::open_with_clock(
        SUBSTRATE,
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let factory = deployment.session_store_factory();
    (deployment, factory, move || clock.advance(1))
});
