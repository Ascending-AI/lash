//! The session shift's laws on the in-process server double: the shift
//! admission laws (FIG-3600, ADR 0105), the run start marker (L-S8) and the
//! queued, frame-switch and frame-open redrives (FIG-3748,
//! FIG-3788, FIG-4110), the queued input runs (FIG-4457) and the
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

// The shift's admission laws (FIG-3600, ADR 0105 §2): admission and seal
// are recorded steps on the engine's journal, so a redelivered handler
// replays them instead of re-deciding from the store.
lash_conformance::shift_admission_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-shift-admission-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-3607 contract 4 (FIG-4489): every logical turn a shift runs, a
// recovered follow-on's included, is owned by `Turn(logical run)`.
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
// the handler's journal from its start (FIG-4514): a run replayed after its
// terminal-checkpoint follow-on committed names that follow-on's effects as
// its first execution did, so the shift ends.
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

// The session config a run executes under is a recorded step (FIG-3600 S6):
// a redelivered handler replays the run under the config it recorded.
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
// resumption replays the handler's journal from its start: the run's
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
    }; (a_redrive_runs_under_the_execution_controls_its_run_recorded, "turn-config-recorded-controls-redrive"));

    // FIG-4567: so are the request defaults of the run's model binding,
    // capture allowlists included.
    lash_conformance::turn_config_tests!(@law [] {
        always_replay_harness("recorded-request-defaults-redrive").await
    }; (a_redrive_calls_the_model_with_the_request_defaults_its_run_recorded, "turn-config-recorded-request-defaults-redrive"));
}

// FIG-4389's recorded termination law on the double's other legs: under
// always-replay, where every resumption replays the run's journal from its
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
    pub(super) async fn memory_harness(
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
    pub(super) async fn file_harness(
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
        }; (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::memory_harness(true).await
        }; (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    }

    mod sqlite_file {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    }

    mod sqlite_file_always_replay {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(true).await
        }; (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(true).await
        }; (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
    }
}

// FIG-4518's stale-fence boundary laws on the double's other legs. A committed
// run redriven after a model change answers from its receipt, and any other
// commit under its older admission is fenced out. The plain SQLite memory leg
// runs with `turn_config_tests!` above; the PostgreSQL legs run with the
// PostgreSQL ingress laws.
mod stale_fence_boundary {
    use super::recorded_termination::{file_harness, memory_harness};

    mod sqlite_memory_always_replay {
        lash_conformance::turn_config_tests!(@law [] {
            super::memory_harness(true).await
        }; (a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt, "turn-config-stale-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::memory_harness(true).await
        }; (an_older_admission_redriven_after_a_profile_change_is_fenced_out, "turn-config-stale-fenced-out"));
    }

    mod sqlite_file {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt, "turn-config-stale-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (an_older_admission_redriven_after_a_profile_change_is_fenced_out, "turn-config-stale-fenced-out"));
    }

    mod sqlite_file_always_replay {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(true).await
        }; (a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt, "turn-config-stale-redrive"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(true).await
        }; (an_older_admission_redriven_after_a_profile_change_is_fenced_out, "turn-config-stale-fenced-out"));
    }
}

// FIG-4646's recorded-view and recorded-bound laws on the double's SQLite
// file leg: a config command after a pinned run resolves over the sticky
// config, and a redriven switch owes its follow-on under the bound its run
// resolved. The plain SQLite memory leg runs with `turn_config_tests!` above;
// the PostgreSQL leg runs with the PostgreSQL ingress laws.
mod recorded_run_view_and_bound {
    use super::recorded_termination::file_harness;

    mod sqlite_file {
        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_config_command_after_a_pinned_run_resolves_over_the_sticky_config, "run-spec-sticky-command"));

        lash_conformance::turn_config_tests!(@law [] {
            super::file_harness(false).await
        }; (a_redriven_switch_owes_its_follow_on_under_the_bound_its_run_resolved, "run-spec-follow-on-bound"));
    }
}

// L-S8: a fresh execution of a started run is SubstrateLost. Every run
// of the probe runner is a fresh invocation, so its second run of the
// same admission is the fresh execution.
lash_conformance::run_start_marker_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-run-start-marker-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-3788: a `SessionShifts` turn that switched frames, crashed after the switch
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

// FIG-3552, FIG-3927: a row admitted to one run is answered only by that
// run, whatever path the shifts after its worker's death take.
lash_conformance::run_answers_its_rows_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-run-rows-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-3748: a queued shift crashed after its first commit replays that
// run from its journal, and the input queued behind it runs once.
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

// FIG-4457: two queued inputs, the second sent while the first one's shift
// is down, get their own runs under the default drain, and a cancel of one
// leaves the other untouched.
lash_conformance::queued_input_runs_tests!({
    let harness =
        LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
    let effect_host = harness.endpoint_host();
    let turn_runner = harness.turn_runner();
    let stores = harness.law_stores();
    let prefix: &'static str =
        Box::leak(format!("restate-queued-input-runs-{}", harness.run_nonce()).into_boxed_str());
    (harness, prefix, effect_host, stores, turn_runner)
});

// FIG-4258: the frame-open laws where every await suspends and every
// resumption replays the handler's journal from its start, the e2e replay
// leg's mode. A shift that applies an administrative compaction and then runs
// the input queued behind it replays the compaction after the input's run
// sealed a newer shift epoch; the laws hold only if that replay never
// presents the command run's superseded fence again.
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
