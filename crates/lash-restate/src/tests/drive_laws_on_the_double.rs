//! The session drive's laws on the in-process server double: the drive
//! admission laws (FIG-3600, ADR 0105), the root start marker (L-S8) and the
//! queued, frame-switch and frame-open redrives (FIG-3748,
//! FIG-3788, FIG-4110), the queued input roots (FIG-4457) and the
//! bound-trigger duplicate (FIG-4297). Each runs
//! through the endpoint's real handlers with the Restate server simulated in
//! process.

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

lash_conformance::session_close_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let host = harness.endpoint_host();
    let stores = harness.law_stores();
    let runner = harness.turn_runner();
    let prefix: &'static str =
        Box::leak(format!("restate-session-close-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, host, stores, Some(runner))
});

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

// FIG-3607 contract 4 (FIG-4489): every logical turn a drive runs, a
// recovered follow-on's included, is owned by `Turn(logical root)`.
lash_conformance::driver_turn_ownership_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-driver-ownership-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// The ownership law where every await suspends and every resumption replays
// the handler's journal from its start (FIG-4514): a root replayed after its
// terminal-checkpoint follow-on committed names that follow-on's effects as
// its first execution did, so the drive ends.
mod driver_turn_ownership_under_replay {
    use super::{HarnessServer, LiveConformanceHarness};

    lash_conformance::driver_turn_ownership_tests!({
        let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
            unreachable!("in_process names the server double");
        };
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
                seed,
                always_replay: true,
            })
            .await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str = Box::leak(
            format!("restate-driver-ownership-replay-{}", harness.run_nonce()).into_boxed_str(),
        );
        (harness, prefix, effect_host, stores, turn_runner)
    });
}

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

// FIG-4376's execution-control laws where every await suspends and every
// resumption replays the handler's journal from its start: the root's
// recorded config, its turn budget included, is served from the journal on
// every resumption.
mod recorded_execution_controls_under_replay {
    use super::{HarnessServer, LiveConformanceHarness};

    async fn always_replay_harness(
        law: &str,
    ) -> (
        LiveConformanceHarness,
        &'static str,
        std::sync::Arc<dyn lash_core::EffectHost>,
        std::sync::Arc<dyn lash_core::StoreSet>,
        std::sync::Arc<dyn lash_conformance::ConformanceTurnRunner>,
    ) {
        let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
            unreachable!("in_process names the server double");
        };
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
                seed,
                always_replay: true,
            })
            .await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str =
            Box::leak(format!("restate-{law}-replay-{}", harness.run_nonce()).into_boxed_str());
        (harness, prefix, effect_host, stores, turn_runner)
    }

    lash_conformance::turn_config_tests!(@law [] {
        always_replay_harness("recorded-controls-redrive").await
    }; (a_redrive_runs_under_the_execution_controls_its_root_recorded, "turn-config-recorded-controls-redrive"));
}

// FIG-4389's recorded termination law on the double's other legs: under
// always-replay, where every resumption replays the root's journal from its
// start, and over a SQLite file store set, plain and always-replay. The plain
// SQLite memory leg runs with `turn_config_tests!` above; the PostgreSQL legs
// run with the PostgreSQL ingress laws.
mod recorded_termination {
    use super::{HarnessServer, LiveConformanceHarness};
    use std::sync::Arc;

    fn server(always_replay: bool) -> HarnessServer {
        let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
            unreachable!("in_process names the server double");
        };
        HarnessServer::InProcess {
            seed,
            always_replay,
        }
    }

    /// The double over its own SQLite memory store set.
    async fn memory_harness(
        always_replay: bool,
    ) -> (
        LiveConformanceHarness,
        &'static str,
        Arc<dyn lash_core::EffectHost>,
        Arc<dyn lash_core::StoreSet>,
        Arc<dyn lash_conformance::ConformanceTurnRunner>,
    ) {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(server(always_replay)).await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str = Box::leak(
            format!(
                "restate-recorded-termination-memory-{always_replay}-{}",
                harness.run_nonce()
            )
            .into_boxed_str(),
        );
        (harness, prefix, effect_host, stores, turn_runner)
    }

    /// The double with the law's runtime over a SQLite file store set.
    async fn file_harness(
        always_replay: bool,
    ) -> (
        (LiveConformanceHarness, tempfile::TempDir),
        &'static str,
        Arc<dyn lash_core::EffectHost>,
        Arc<dyn lash_core::StoreSet>,
        Arc<dyn lash_conformance::ConformanceTurnRunner>,
    ) {
        let directory = tempfile::tempdir().expect("SQLite file store directory");
        let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(directory.path())
                .await
                .expect("open the SQLite file store set"),
        );
        let harness = LiveConformanceHarness::start_for_tool_children_settling_into(
            server(always_replay),
            stores.usage_accounting(),
        )
        .await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let prefix: &'static str = Box::leak(
            format!(
                "restate-recorded-termination-file-{always_replay}-{}",
                harness.run_nonce()
            )
            .into_boxed_str(),
        );
        (
            (harness, directory),
            prefix,
            effect_host,
            stores,
            turn_runner,
        )
    }

    mod sqlite_memory_always_replay {
        lash_conformance::turn_config_tests!(@law [] {
            super::memory_harness(true).await
        }; (a_redrive_assembles_the_terminal_its_root_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::memory_harness(true).await
        }; (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    }

    mod sqlite_file {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_redrive_assembles_the_terminal_its_root_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    }

    mod sqlite_file_always_replay {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(true).await
        }; (a_redrive_assembles_the_terminal_its_root_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(true).await
        }; (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    }
}

// L-S8: a fresh execution of a started root is SubstrateLost. Every run
// of the probe runner is a fresh invocation, so its second run of the
// same admission is the fresh execution.
lash_conformance::root_start_marker_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-root-start-marker-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-3788: a driver turn that switched frames, crashed after the switch
// commit and redelivered replays its recorded admission and switched
// turn from the journal and runs only the follow-on frame.
lash_conformance::frame_switch_redrive_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-frame-switch-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-4110: every frame open (a context-pressure frame, a pressure frame
// followed by `continue_as`, an administrative compaction) killed at each
// crash point and redelivered opens once, chained in order, with one
// summarizer call.
lash_conformance::frame_open_redrive_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-frame-open-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-3552, FIG-3927: a row admitted to one root is answered only by that
// root, whatever path the drives after its worker's death take.
lash_conformance::root_answers_its_rows_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-root-rows-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-3748: a queued drive crashed after its first commit replays that
// root from its journal, and the input queued behind it runs once.
lash_conformance::queued_after_commit_redrive_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-queued-redrive-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-4457: two queued inputs, the second sent while the first one's drive
// is down, get their own roots under the default drain, and a cancel of one
// leaves the other untouched.
lash_conformance::queued_input_roots_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-queued-input-roots-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-4258: the frame-open laws where every await suspends and every
// resumption replays the handler's journal from its start, the e2e replay
// leg's mode. A drive that applies an administrative compaction and then runs
// the input queued behind it replays the compaction after the input's root
// sealed a newer drive epoch; the laws hold only if that replay never
// presents the command root's superseded fence again.
mod frame_open_under_replay {
    use super::{HarnessServer, LiveConformanceHarness};

    lash_conformance::frame_open_redrive_tests!({
        let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
            unreachable!("in_process names the server double");
        };
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
                seed,
                always_replay: true,
            })
            .await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str = Box::leak(
            format!("restate-frame-open-replay-{}", harness.run_nonce()).into_boxed_str(),
        );
        (harness, prefix, effect_host, stores, turn_runner)
    });
}

// FIG-4297: a duplicate of a bound trigger delivery's occurrence, emitted by a
// fresh invocation after the bound process was pruned, returns that process
// and starts nothing, and the original emission's replay still answers it.
// Every emission runs in a probe handler and the delivery's process in the
// endpoint's `LashProcessWorkflow`.
lash_conformance::bound_trigger_duplicate_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-bound-trigger-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-4297's law where every await suspends and every resumption replays the
// handler's journal from its start: each emission's admission and start are
// served from its journal on every resumption.
mod bound_trigger_duplicate_under_replay {
    use super::{HarnessServer, LiveConformanceHarness};

    lash_conformance::bound_trigger_duplicate_tests!({
        let HarnessServer::InProcess { seed, .. } = HarnessServer::in_process() else {
            unreachable!("in_process names the server double");
        };
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::InProcess {
                seed,
                always_replay: true,
            })
            .await;
        let effect_host = harness.endpoint_host();
        let turn_runner = harness.turn_runner();
        let stores = harness.law_stores();
        let prefix: &'static str = Box::leak(
            format!("restate-bound-trigger-replay-{}", harness.run_nonce()).into_boxed_str(),
        );
        (harness, prefix, effect_host, stores, turn_runner)
    });
}
