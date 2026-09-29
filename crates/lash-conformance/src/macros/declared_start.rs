/// Register the declared-start and `spawn_agent` laws (ADR 0116 §7.3). The
/// fixture hands back a guard and a [`DeclaredStartTier`](crate::DeclaredStartTier):
/// the tier's effect host and store set, its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) — which must crash
/// and redrive a turn and run process segments — the RLM protocol plugin
/// factories and the subagent plugin from the crates above this one.
#[macro_export]
macro_rules! declared_start_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::declared_start_tests!(@laws [$(#[$attr])*] $fixture);
    };
    (@laws [$($attr:tt)*] $fixture:block) => {
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_crash_at_every_launch_boundary);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            spawn_agent_record_carries_child_identity);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_timeout_cancels_the_child);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_cancel_at_each_point);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_cancel_at_the_claim_answer_delivers_the_start);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_retention_hold_blocks_prune_until_consumed);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_prune_after_hold_release_before_settlement_replays_terminal);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_rejects_foreign_or_reused_serialized_identity_before_launch);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_early_terminal_resolves_before_wait);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_rearm_is_idempotent);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            batch_of_spawns_overlaps);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_discarded_retry_launches_nothing);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_refusal_settles_the_call);
        $crate::declared_start_tests!(@law [$($attr)*] $fixture;
            declared_start_scope_close_cancels_until_children);
    };
    (@law [$($attr:tt)*] $fixture:block; $law:ident) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, tier) = $fixture;
            // The deadlock watchdog, and no part of the law: the law's waits
            // have no deadline, so a run a loaded pool slows is waited for.
            // A law that never ends is a hang, and the watchdog fails it
            // with its own output before the target's timeout silences the
            // whole target. It is far above any law's run, loaded or not.
            tokio::time::timeout(
                std::time::Duration::from_secs(240),
                $crate::registration_macro_support::$law(tier),
            )
            .await
            .expect(concat!(
                "deadlock watchdog: ",
                stringify!($law),
                " hung on a wait that never ended"
            ));
        }
    };
}
