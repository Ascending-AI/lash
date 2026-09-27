//! The turn crash laws on the in-process server double, every one running its
//! turns on the endpoint's turn runner: the golden-trace drift check, the
//! level-one crash matrix, the FIG-3524 error-return sweep and the
//! after-commit redrive (FIG-3547, the FIG-3561 port), the host-layer law
//! over group children, the FIG-3571 generation-refusal pair, the
//! direct-acceptance crash after its store commit, and the turn-cancel
//! closure across a crash at each of its cuts.

use std::sync::Arc;

use lash_core::EffectHost;

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

/// The turn crash laws' fixture on the server double: the endpoint's
/// own turn runner, host and session catalog. A crash kills the turn's
/// handler execution where it stands, and the recovery is Restate's
/// redelivery of the same invocation, replaying its journal.
async fn turn_crash_runner_fixture() -> (
    LiveConformanceHarness,
    Arc<dyn lash_core::StoreSet>,
    impl Fn(&str) -> Arc<lash_sqlite_store::Store> + Send + Sync + 'static,
    Arc<dyn EffectHost>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
) {
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let host = harness.endpoint_host();
    let runner = harness.turn_runner();
    let stores = harness.law_stores();
    let make = harness.law_persistence();
    (harness, stores, make, host, runner)
}

lash_conformance::turn_crash_trace_tests!({ turn_crash_runner_fixture().await });

lash_conformance::turn_crash_error_return_tests!({ turn_crash_runner_fixture().await });

lash_conformance::turn_crash_level_1_tests!(parked: &[]; {
    turn_crash_runner_fixture().await
});

lash_conformance::effect_layer_group_child_tests!({ turn_crash_runner_fixture().await });

// A drain crashed after its final commit and redriven through the session
// drive replays its recorded admission, seal and claim and reads the
// committed root back (FIG-3748).
lash_conformance::turn_crash_after_commit_redrive_tests!({ turn_crash_runner_fixture().await });

lash_conformance::turn_crash_direct_acceptance_tests!({ turn_crash_runner_fixture().await });

// Every closure cut recovers through the session drive, the one inside the
// store write that applies the input effects included (FIG-3736).
lash_conformance::turn_crash_cancel_closure_tests!({ turn_crash_runner_fixture().await });
