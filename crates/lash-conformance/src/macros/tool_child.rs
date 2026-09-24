//! Registration macros for the tool-child and turn-runner laws (FIG-3397,
//! ADR 0099): tool calls running as effect-group children on every tier.
//! Split from `macros.rs` to keep each catalogue file inside the support-file
//! line budget; `scripts/check_law_execution_receipts.py` and
//! `scripts/check_conformance_law_registration.py` read both.

/// Register the laws that drive a real turn through the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): the public
/// signal-intent wake and the turn-cancel laws for tool calls running as
/// effect-group children.
///
/// The fixture hands back a guard, a session prefix, the tier's effect host, a
/// process registry, the process-work substrate, the tier's turn runner and a
/// post-law verification handed the law's name. Restate runs each turn inside a live handler
/// (`#[ignore]`d, deferred to `effect-group-conformance-e2e`).
#[macro_export]
macro_rules! turn_runner_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (public_signal_intent_wakes_parked_process, "public-signal-intent-wake"));
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (an_after_step_stop_during_a_child_retry_sleep_finishes_the_iteration, "tool-child-after-step-retry-sleep"));
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (a_follow_on_pending_child_waits_under_the_follow_on_turn_cancel_gate, "tool-child-follow-on-cancel-gate"));
    };
}

/// Register the turn-cancel law for a tool child that ignores cancellation,
/// with [`turn_runner_tests!`]'s fixture. The in-process tiers own their
/// children's tasks, so dropping one is theirs to prove.
#[macro_export]
macro_rules! tool_child_turn_cancel_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__turn_runner_register!([$(#[$attr])*] $fixture;
            (a_cancelled_turn_drops_a_tool_child_that_ignores_cancellation, "tool-child-turn-cancel-drops-child"));
    };
}

/// Register the FIG-1293 migrated-tools crash-redrive law. The fixture hands
/// back a guard, a prefix, the effect host, a process registry, the tier's
/// turn runner and the orchestration plugin factories (`spawn_agent`,
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
            let (_guard, prefix, host, registry, runner, orchestration) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, registry, runner, orchestration)
                .await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
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
            let (_guard, prefix, host, registry, work, runner, verify) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, registry, work, runner).await;
            verify(stringify!($law)).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the cross-tier tool-batch parallelism law (FIG-3400).
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the product producers reachable on that tier and the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner). Every producer
/// runs the same law, so "this tier overlaps a tool batch" is one statement
/// per surface and not a family of look-alike tests. A handler-bound tier
/// supplies a runner that drives each turn inside a live handler.
#[macro_export]
macro_rules! tool_batch_parallelism_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_batch_parallelism_tests!(@law [$(#[$attr])*] $fixture;
            (tool_batch_cross_tier_parallelism, "tool-batch-cross-tier-parallelism"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn $law() {
            let (_guard, prefix, host, producers, runner) = $fixture;
            assert!(
                !producers.is_empty(),
                "a tier registers at least one product producer, or the law \
                 runs on nothing"
            );
            for producer in producers {
                $crate::registration_macro_support::$law(
                    prefix,
                    std::sync::Arc::clone(&host),
                    std::sync::Arc::clone(&runner),
                    producer,
                )
                .await;
            }
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the handler-level tool-child invocation laws (FIG-2266, ADR 0099).
///
/// The fixture hands back a guard, a session prefix and a
/// [`ToolChildLawFixture`]: a world factory over one substrate (a law calls it
/// from a runtime it is about to destroy), a process-registry factory, and the
/// completion routing the tier records for a deferrable child. Restate
/// registers through the live e2e recipe (`conformance_and_poison.rs`) and its
/// world has no Lash-owned drain — Restate redrives the child invocation
/// itself — so the laws take their open-time shape there.
#[macro_export]
macro_rules! tool_child_invocation_tests {
    ($fixture:block) => {
        $crate::tool_child_invocation_tests!(@catalogue [] $fixture);
    };
    ($(#[$attr:meta])+ $fixture:block) => {
        $crate::tool_child_invocation_tests!(@catalogue [$(#[$attr])*] $fixture);
    };
    (@catalogue [$($attr:tt)*] $fixture:block) => {
        $crate::tool_child_invocation_tests!(@expand [$($attr)*] $fixture; [
            (
                tool_children_run_through_the_invocation_driver,
                "tool-child-invocation-driver"
            ),
            (
                an_unregistered_opener_leaves_the_child_accepted,
                "tool-child-unregistered-opener"
            ),
            (
                a_foreign_opener_cannot_drive_another_openers_child,
                "tool-child-foreign-opener"
            ),
            (
                a_same_name_process_incarnation_is_not_the_recorded_opener,
                "tool-child-process-incarnation"
            ),
            (
                a_crashed_child_replays_its_committed_attempts_facts,
                "tool-child-attempt-capture"
            ),
            (
                every_billed_provider_attempt_is_conserved_once_on_its_opener,
                "tool-child-usage-conservation"
            ),
            (
                a_committed_childs_final_is_protected_and_its_drain_is_finished,
                "tool-child-commit-boundary-crash"
            ),
            (
                drains_are_admitted_in_recorded_commit_order,
                "tool-child-commit-order"
            ),
            (
                a_cancel_decided_before_a_nested_sink_is_refused_at_the_sink,
                "tool-child-admission-fence"
            ),
            (
                a_late_completion_after_a_cancel_decision_is_refused,
                "tool-child-late-completion-refused"
            ),
            (
                a_deferred_childs_commit_point_is_its_resolution,
                "tool-child-deferred-commit-point"
            ),
            (
                a_group_prefix_incorporation_reincorporates_exactly_the_recorded_ranks,
                "tool-child-group-prefix-incorporation"
            ),
            (
                two_presentation_steps_compose_deterministically_on_first_run_and_replay,
                "tool-child-presentation-composition"
            ),
            (
                a_changed_presentation_environment_on_replay_does_not_change_the_recorded_presentation,
                "tool-child-presentation-recorded-env"
            ),
            (
                the_oracle_and_the_budget_plugin_coexist,
                "tool-child-presentation-coexistence"
            ),
            (
                a_retained_full_output_is_a_durable_artifact_not_a_path,
                "tool-child-presentation-artifact"
            ),
            (
                timer_and_durable_wait_children_are_admitted_beside_a_tool_child,
                "tool-child-timer-and-wait-siblings"
            ),
        ]);
    };
    (@expand $attrs:tt $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
        $(
            $crate::__tool_child_invocation_register!($attrs $fixture; ($law, $label));
        )*
    };
}

/// Register one shared tool-child invocation law.
#[macro_export]
macro_rules! __tool_child_invocation_register {
    ([$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, fixture) = $fixture;
            $crate::registration_macro_support::$law(&fixture, prefix).await;
            $crate::law_receipt::record(module_path!(), stringify!($law), $label);
        }
    };
}

/// Register the consumer-driven group laws (FIG-3397, ADR 0099 §5, §6, §7,
/// §10, §13): an `All` group of tool children answers every admission shape
/// with its own reply, keyed by input index, and a settlement order its
/// preparation prefix leads; and the opener's end finishes and incorporates
/// every group its aggregates formed — one a cancel handed back, one a failed
/// end left `closing`.
///
/// The fixture hands back a guard, a session prefix and a
/// [`ToolChildLawFixture`], the same shape `tool_child_invocation_tests!`
/// takes — the law needs a world factory over the tier's substrate and a
/// process-registry factory, nothing more. Restate is deliberately absent:
/// its deployment host executes no effects, so there is no batch consumer to
/// run.
#[macro_export]
macro_rules! tool_batch_group_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (an_all_group_of_tool_children_yields_the_batch_replies, "tool-batch-group-replies"));
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (a_cancelled_aggregates_committed_loser_is_incorporated_by_its_openers_end,
             "opener-end-cancelled-aggregate"));
        $crate::__tool_child_invocation_register!([$(#[$attr])*] $fixture;
            (a_retried_openers_end_finishes_the_closing_group_its_first_end_left,
             "opener-end-retried-end"));
    };
}
