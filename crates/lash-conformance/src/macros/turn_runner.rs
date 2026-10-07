//! Registration macros for surviving turn-runner and binding laws.
//! Split from `macros.rs` to keep each catalogue file inside the support-file
//! line budget.

/// Register the laws that execute a real turn through the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the public
/// signal-intent wake.
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test (whose session catalog and process registry the
/// law's runtime uses), the process-work substrate, the tier's turn runner and
/// a post-law verification handed the law's name.
#[macro_export]
macro_rules! turn_runner_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (public_signal_intent_wakes_parked_process, "public-signal-intent-wake"));
    };
}

/// Register the FIG-1293 migrated-tools crash-redrive law. The fixture hands
/// back a guard, a prefix, the effect host, the store set under test, the tier's
/// turn runner and the plugin factories (the standard protocol, `spawn_agent`,
/// `cancel_process`) from the crates above this one.
#[macro_export]
macro_rules! migrated_tools_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::migrated_tools_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (public_migrated_tools_redrive_to_literal_outcomes, "migrated-tools-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, plugins) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, plugins).await;
        }
    };
}

/// Register the admitted-head redrive law (FIG-3682): a direct turn crashed
/// after its own commit and redriven replays at the head it was admitted on,
/// under the turn index its admission recorded. The fixture hands back a
/// guard, a prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner). The turn calls no
/// tool, so it runs wherever a direct turn runs.
#[macro_export]
macro_rules! admitted_head_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::admitted_head_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (a_turn_redriven_after_its_commit_replays_at_its_admitted_head, "admitted-head-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register the turn-config laws (FIG-3600 S6, D3 §5.2; FIG-3838, FIG-3842,
/// FIG-5093): a run resolves its run spec against its session config once, as a recorded
/// step, and every replay of the run executes under that record. The fixture is the admitted-head one: a guard, a
/// prefix, the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! turn_config_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (one_config_resolution_per_run, "turn-config-one-resolution"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (an_unbindable_llm_profile_retries_and_never_fails_the_turn, "turn-config-unbindable-retries"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redrive_runs_under_the_execution_controls_its_run_recorded, "turn-config-recorded-controls-redrive"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redrive_assembles_the_terminal_its_run_recorded_termination_decides, "turn-config-recorded-termination-redrive"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_missing_recorded_termination_is_a_typed_terminal_refusal, "turn-config-missing-recorded-termination"));
        $crate::turn_config_tests!(@law [$(#[$attr])*] $fixture;
            (a_redrive_calls_the_model_with_the_request_defaults_its_run_recorded, "turn-config-recorded-request-defaults-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}

/// Register the segment redrive law (FIG-3547): a redrive never
/// re-executes a recorded effect, an unrecorded one runs once more under the
/// same identity, and a segment whose engine lost its record ends
/// `Abandoned(SubstrateLost)` with no further effect. The fixture hands back a
/// guard, a prefix, the store set whose process registry the tier's engine
/// reads, and the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner),
/// which must run, crash and recover process segments.
#[macro_export]
macro_rules! segment_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::segment_redrive_tests!(@law [$(#[$attr])*] $fixture;
            (segment_redrive_never_reexecutes_a_recorded_effect, "segment-redrive"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, stores, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, stores, runner).await;
        }
    };
}

/// Register the served-process-start laws (FIG-3779): an RLM cell that
/// called `agents.spawn` is cut at one point of its declared process start —
/// after the start was issued, or after its registration and before its
/// workflow send — and redriven under a drifted `agents.spawn` binding. The
/// recorded start is served and the turn completes. The fixture hands back a
/// guard, a prefix, the tier's effect host, the store set under test, its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) — which must run
/// process segments — the RLM protocol plugin factories, and the
/// [`SubagentFactories`](crate::SubagentFactories) from the crates above this
/// one.
#[macro_export]
macro_rules! served_process_start_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_whose_start_was_issued_is_served, "served-process-start-issued"));
        $crate::served_process_start_tests!(@law [$(#[$attr])*] $fixture;
            (a_drifted_spawn_cut_before_its_send_is_served, "served-process-start-before-send"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, rlm, subagents) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, rlm, subagents)
                .await;
        }
    };
}

/// Register one turn-runner law.
#[macro_export]
macro_rules! __turn_runner_register {
    ([$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, work, runner, verify) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, work, runner).await;
            verify(stringify!($law)).await;
        }
    };
}
