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

/// Why the crash points from the turn-cancel closure's store write onward
/// cannot recover on the level-one matrix: it drains through the queued-run
/// body, whose redrive returns the settled run's receipt from the store
/// without the commands the crashed execution journaled, so Restate reports a
/// journal mismatch (570) and retries the invocation until it pauses. The same
/// cuts recover through the session drive (the after-commit redrive and
/// turn-cancel closure laws below); FIG-3668 deletes the queued-run body.
const SETTLED_RUN_REDRIVE: &str = "the queued-run body's redrive returns the settled run from \
    the store, not its journaled commands: journal mismatch (570); the session drive recovers \
    these cuts";

/// The level-one crash points whose recovery hits the queued-run body's
/// settled-run redrive (see [`SETTLED_RUN_REDRIVE`]).
const PARKED_LEVEL_ONE_POINTS: &[lash_conformance::ParkedTurnCrashPoint] = &[
    lash_conformance::ParkedTurnCrashPoint {
        point: r#"{"operation":{"seam":"store","operation":{"kind":"apply_turn_cancel_effects_and_consume"}},"placement":"inside_call"}"#,
        ticket: "FIG-3668",
        reason: SETTLED_RUN_REDRIVE,
    },
    lash_conformance::ParkedTurnCrashPoint {
        point: r#"{"operation":{"seam":"store","operation":{"kind":"pending_queued_run"}},"placement":"boundary"}"#,
        ticket: "FIG-3668",
        reason: SETTLED_RUN_REDRIVE,
    },
    lash_conformance::ParkedTurnCrashPoint {
        point: r#"{"operation":{"seam":"store","operation":{"kind":"commit_final_head","settles_queue":false,"settles_turn_input":false,"releases_lease":false}},"placement":"boundary"}"#,
        ticket: "FIG-3668",
        reason: SETTLED_RUN_REDRIVE,
    },
    lash_conformance::ParkedTurnCrashPoint {
        point: r#"{"operation":{"seam":"store","operation":{"kind":"commit_final_head","settles_queue":false,"settles_turn_input":false,"releases_lease":false}},"placement":"inside_call"}"#,
        ticket: "FIG-3668",
        reason: SETTLED_RUN_REDRIVE,
    },
    lash_conformance::ParkedTurnCrashPoint {
        point: r#"{"operation":{"seam":"store","operation":{"kind":"release_session_execution_lease"}},"placement":"boundary"}"#,
        ticket: "FIG-3668",
        reason: SETTLED_RUN_REDRIVE,
    },
    lash_conformance::ParkedTurnCrashPoint {
        point: r#"{"operation":{"seam":"store","operation":{"kind":"release_session_execution_lease"}},"placement":"inside_call"}"#,
        ticket: "FIG-3668",
        reason: SETTLED_RUN_REDRIVE,
    },
];

lash_conformance::turn_crash_level_1_tests!(parked: PARKED_LEVEL_ONE_POINTS; {
    turn_crash_runner_fixture().await
});

lash_conformance::effect_layer_group_child_tests!({ turn_crash_runner_fixture().await });

// A drain crashed after its final commit and redriven through the session
// drive replays its recorded admission, seal and claim and reads the
// committed root back (FIG-3748).
lash_conformance::turn_crash_after_commit_redrive_tests!({ turn_crash_runner_fixture().await });

lash_conformance::turn_crash_generation_claim_tests!({ turn_crash_runner_fixture().await });

lash_conformance::turn_crash_direct_acceptance_tests!({ turn_crash_runner_fixture().await });

// The redrive of a turn the pre-cutover build left in flight is refused
// before any effect and parks, typed: its handler fails each retry
// retryably where the crashed execution's journal holds its next command,
// and the turn handler's retry policy pauses the invocation (FIG-3735).
lash_conformance::turn_crash_generation_redrive_tests!({ turn_crash_runner_fixture().await });

// Every closure cut recovers through the session drive, the one inside the
// store write that applies the input effects included (FIG-3736).
lash_conformance::turn_crash_cancel_closure_tests!({ turn_crash_runner_fixture().await });
