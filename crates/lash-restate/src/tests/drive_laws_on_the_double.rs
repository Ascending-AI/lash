//! The session drive's laws on the in-process server double: the drive
//! admission laws (FIG-3600, ADR 0105), the root start marker (L-S8) and the
//! queued and frame-switch redrives (FIG-3748, FIG-3788). Each runs through
//! the endpoint's real handlers with the Restate server simulated in process.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

// The drive's admission laws (FIG-3600, ADR 0105 §2): admission and seal
// are recorded steps on the engine's journal, so a redelivered handler
// replays them instead of re-deciding from the store.
lash_conformance::drive_admission_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-drive-admission-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// The session config a root runs under is a recorded step (FIG-3600 S6):
// a redelivered handler replays the root under the config it recorded.
lash_conformance::turn_config_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-turn-config-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// L-S8: a fresh execution of a started root is SubstrateLost. Every run
// of the probe runner is a fresh invocation, so its second run of the
// same admission is the fresh execution.
lash_conformance::root_start_marker_tests!(
    #[ignore = "parked: no root start marker yet; a fresh execution re-seals its admission and re-runs the root (FIG-3815: root start marker)"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str = Box::leak(
            format!("restate-root-start-marker-{}", harness.run_nonce()).into_boxed_str(),
        );
        (harness, prefix, effect_host, stores, turn_runner)
    }
);

// FIG-3788: a driver turn that switched frames, crashed after the switch
// commit and redelivered replays its recorded admission and switched
// turn from the journal and runs only the follow-on frame.
lash_conformance::frame_switch_redrive_tests!(
    #[ignore = "parked: a queued drain redriven after its switch commit diverges from its journal (570 at call 2) until queued drains run through the recorded drive admission (FIG-3788)"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str =
            Box::leak(format!("restate-frame-switch-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }
);

// FIG-3748: a queued drive crashed after its first commit replays that
// root from its journal, and the input queued behind it runs once.
lash_conformance::queued_after_commit_redrive_tests!(
    #[ignore = "parked: a queued drive redriven after its first commit re-decides from the live queue and diverges from its journal until queued drains run through the recorded drive admission (FIG-3748)"]
    {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str =
            Box::leak(format!("restate-queued-redrive-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }
);
