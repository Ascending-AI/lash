//! Registration macro for the engine-owned usage accounting laws
//! (FIG-4236, ADR 0125).

/// Register one independently reported test per usage-accounting engine law.
///
/// Attributes before the block (an `#[ignore]` for a live tier) go on
/// every registered test. The fixture block yields `(guard, tier)`: a value kept alive for the
/// test's duration, and a
/// [`UsageAccountingTier`](crate::UsageAccountingTier) over a fresh engine.
#[macro_export]
macro_rules! usage_accounting_engine_tests {
    ($(#[$meta:meta])* $tier:block) => {
        $crate::usage_accounting_engine_tests!(@catalogue [$(#[$meta])*] $tier; [
            usage_of_an_unfinished_root_is_read_without_driving,
            each_paid_attempt_counts_once_under_any_boundary_grouping_and_replay,
            usage_crash_p1_committed_completed,
            usage_crash_p1_committed_cancelled,
            usage_crash_p1_committed_failed,
            usage_crash_p1_forked,
            usage_crash_p1_session_deleted,
            usage_crash_p1_parked_forever,
            usage_crash_p2_committed_completed,
            usage_crash_p2_committed_cancelled,
            usage_crash_p2_committed_failed,
            usage_crash_p2_forked,
            usage_crash_p2_session_deleted,
            usage_crash_p2_parked_forever,
            a_settlement_retried_after_its_projection_counts_once,
            committed_turn_totals_are_preserved,
        ]);
    };
    (@catalogue $attrs:tt $tier:block; [$($law:ident),* $(,)?]) => {
        $(
            $crate::usage_accounting_engine_tests!(@law $attrs $tier $law);
        )*
    };
    (@law [$(#[$meta:meta])*] $tier:block $law:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        $(#[$meta])*
        async fn $law() {
            let (_tier_guard, tier) = $tier;
            $crate::$law(&tier).await;
        }
    };
}
