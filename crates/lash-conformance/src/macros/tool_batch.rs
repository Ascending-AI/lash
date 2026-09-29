/// Register the barrier laws (FIG-3400, ADR 0116 §7.1): every member of a
/// turn step's tool group starts before any finishes, the reverse-dependency
/// case, and the forced-serial negative control that must fail.
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test, the product producers reachable on that tier and
/// the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner). Every
/// producer runs every law, so "this tier overlaps a tool group" is one
/// statement per surface and not a family of look-alike tests. A
/// handler-bound tier supplies a runner that drives each turn inside a live
/// handler.
#[macro_export]
macro_rules! tool_batch_parallelism_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_batch_parallelism_tests!(@law [$(#[$attr])*] $fixture;
            (tool_group_members_start_before_any_finishes, "tool-group-members-start-before-any-finishes"));
        $crate::tool_batch_parallelism_tests!(@law [$(#[$attr])*] $fixture;
            (tool_group_reverse_dependency, "tool-group-reverse-dependency"));
        $crate::tool_batch_parallelism_tests!(@tier [$(#[$attr])*] $fixture;
            (forced_serial_host_fails_the_barrier, "forced-serial-host-fails-the-barrier"));
    };
    (@tier [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn $law() {
            let (_guard, prefix, host, stores, producers, runner) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, producers)
                .await;
        }
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn $law() {
            let (_guard, prefix, host, stores, producers, runner) = $fixture;
            assert!(
                !producers.is_empty(),
                "a tier registers at least one product producer, or the law \
                 runs on nothing"
            );
            for producer in producers {
                $crate::registration_macro_support::$law(
                    prefix,
                    std::sync::Arc::clone(&host),
                    std::sync::Arc::clone(&stores),
                    std::sync::Arc::clone(&runner),
                    producer,
                )
                .await;
            }
        }
    };
}

/// Register the `batch` sugar laws (ADR 0116 §7.2).
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test, the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) and the standard
/// protocol as [`BatchSugarFactories`](crate::BatchSugarFactories): offered
/// and withheld.
#[macro_export]
macro_rules! batch_sugar_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_admission_and_identity_contract, "batch-admission-and-identity-contract"));
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_replay_preserves_fold_and_ranks, "batch-replay-preserves-fold-and-ranks"));
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_redrive_reuses_children, "batch-redrive-reuses-children"));
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_cancel_preserves_committed_drains, "batch-cancel-preserves-committed-drains"));
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_all_refused_opens_no_group, "batch-all-refused-opens-no-group"));
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_folds_to_one_transcript_call, "batch-folds-to-one-transcript-call"));
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $label:literal)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner, factories) = $fixture;
            $crate::registration_macro_support::$law(prefix, host, stores, runner, factories)
                .await;
        }
    };
}
