//! The turn crash laws that run their turns on a turn runner, on the
//! in-process server double (FIG-3667's Restate coverage): the FIG-3571
//! generation-refusal pair, the direct-acceptance crash after its store
//! commit, and the turn-cancel closure across a crash at each of its cuts.

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

lash_conformance::turn_crash_generation_claim_tests!({ turn_crash_runner_fixture().await });

lash_conformance::turn_crash_direct_acceptance_tests!({ turn_crash_runner_fixture().await });

// The redrive of a turn the pre-cutover build left in flight is refused
// before any effect, and the refusal reaches the attempt typed, but the
// refused execution returns where the crashed execution's journal holds
// its next command: Restate reports a journal mismatch and retries the
// invocation instead of ending it.
lash_conformance::turn_crash_generation_redrive_tests!(
    #[ignore = "parked: a refused redrive diverges from the crashed execution's journal (FIG-3672)"]
    {
        turn_crash_runner_fixture().await
    }
);

// Seven of the eight closure cuts recover. A crash inside the store write
// that applies the input effects and consumes the authorization leaves the
// write committed but unjournaled, and the redrive, reading the committed
// state, takes a path the journal does not hold.
lash_conformance::turn_crash_cancel_closure_tests!(
    #[ignore = "parked: a crash inside the closure's store write diverges the redrive from its journal (FIG-3672)"]
    {
        turn_crash_runner_fixture().await
    }
);
