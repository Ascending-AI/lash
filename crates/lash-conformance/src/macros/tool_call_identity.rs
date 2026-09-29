/// Register the tool-call identity laws (FIG-4079, FIG-4073). The fixture
/// hands back a guard and a
/// [`ToolCallIdentityTier`](crate::ToolCallIdentityTier): the tier's effect
/// host and store set and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which must crash
/// and redrive a turn.
///
/// A law registered `held` states a contract that holds only once FIG-4080
/// cuts every tool-derived identity over to the lash-minted call id: its
/// failure is printed as the expected divergence and its pass fails the test,
/// so the hold goes the moment the law holds.
#[macro_export]
macro_rules! tool_call_identity_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_call_identity_tests!(@held [$(#[$attr])*] $fixture;
            repeated_provider_id_across_turns_is_distinct);
        $crate::tool_call_identity_tests!(@held [$(#[$attr])*] $fixture;
            same_scope_completion_collision);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            tool_identity_survives_unrecorded_effect_crash);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            reported_failure_retry_preserves_call_id);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            recorded_outcome_skips_execution);
        $crate::tool_call_identity_tests!(@law [$(#[$attr])*] $fixture;
            refusals_and_parallel_completion_never_renumber_identity);
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
    (@held [$($attr:tt)*] $fixture:block; $law:ident) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, tier) = $fixture;
            $crate::run_held_law(stringify!($law), "FIG-4080", async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(240),
                    $crate::registration_macro_support::$law(tier),
                )
                .await
                .expect(concat!(stringify!($law), " finishes within its bound"));
            })
            .await;
        }
    };
}
