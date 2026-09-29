/// Register the tool-call identity laws (FIG-4079, FIG-4073). The fixture
/// hands back a guard and a
/// [`ToolCallIdentityTier`](crate::ToolCallIdentityTier): the tier's effect
/// host and store set and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which must crash
/// and redrive a turn, and the RLM protocol plugin factories.
///
/// Every law reads the lash-minted `ToolCallId` an attempt sees through
/// `AttemptContext::call_id()` (ADR 0117).
#[macro_export]
macro_rules! tool_call_identity_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            repeated_provider_id_across_turns_is_distinct);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            same_scope_completion_collision);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            tool_identity_survives_unrecorded_effect_crash);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            reported_failure_retry_preserves_call_id);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            recorded_outcome_skips_execution);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            refusals_and_parallel_completion_never_renumber_identity);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            code_cells_keep_identity_and_distinguish_fresh_calls);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            frames_keep_identity_and_distinguish_fresh_calls);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            compaction_keeps_identity_and_distinguishes_fresh_calls);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            retained_payload_drift_is_refused_before_effects);
    };
    (@law [$($attr:tt)*] $fixture:block; $law:ident) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, tier) = $fixture;
            // A law that hangs fails with its own output rather than
            // silencing the whole target at its timeout.
            tokio::time::timeout(
                std::time::Duration::from_secs(240),
                $crate::registration_macro_support::$law(tier),
            )
            .await
            .expect(concat!(stringify!($law), " finishes within its bound"));
        }
    };
}

/// Register the process-admission law (FIG-4079): a call a Lashlang process
/// body issues is named by its process. The fixture hands back a guard, a
/// [`ToolCallIdentityTier`](crate::ToolCallIdentityTier) whose runner serves
/// process segments on the law's worker, and the RLM protocol with its
/// process lifecycle on beside the process controls.
///
/// The law's worker runs in the test process, so a tier whose process
/// workflow runs a deployment's own engine (live Restate) cannot carry it.
#[macro_export]
macro_rules! tool_call_identity_process_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn process_admission_names_each_call_and_survives_replay() {
            let (_guard, tier, process_rlm) = $fixture;
            tokio::time::timeout(
                std::time::Duration::from_secs(240),
                $crate::registration_macro_support::process_admission_names_each_call_and_survives_replay(
                    tier,
                    process_rlm,
                ),
            )
            .await
            .expect("process_admission_names_each_call_and_survives_replay finishes within its bound");
        }
    };
}
