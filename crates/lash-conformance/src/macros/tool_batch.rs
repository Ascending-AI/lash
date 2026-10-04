/// Register the barrier laws (FIG-3400, ADR 0116 §7.1): every member of a
/// turn step's tool group starts before any finishes, the reverse-dependency
/// case.
///
/// The fixture hands back a guard, a session prefix, the tier's effect host,
/// the store set under test, the product producers reachable on that tier and
/// the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner). Every
/// producer runs every law, so "this tier overlaps a tool group" is one
/// statement per surface and not a family of look-alike tests. A
/// handler-bound tier supplies a runner that executes each turn inside a live
/// handler.
#[macro_export]
macro_rules! tool_batch_parallelism_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_batch_parallelism_tests!(@law [$(#[$attr])*] $fixture;
            (tool_group_members_start_before_any_finishes, "tool-group-members-start-before-any-finishes"));
        $crate::tool_batch_parallelism_tests!(@law [$(#[$attr])*] $fixture;
            (tool_group_reverse_dependency, "tool-group-reverse-dependency"));
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

/// Register the `max_tool_calls` laws (FIG-4546): a session's recorded
/// tool-call limit admits every call under it untouched and refuses the call
/// past it as the program's own failure, naming the limit; calls issued in
/// sequence are counted the way the producer's surface says (a cell's total,
/// a step's group, what a process holds at once); and a turn that crashes
/// and is redriven refuses the same call.
///
/// The fixture is [`tool_batch_parallelism_tests!`]'s: a guard, a session
/// prefix, the tier's effect host, the store set under test, the product
/// producers reachable on that tier and the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which must be able
/// to crash and redrive a turn. Every producer runs the first law; the staged
/// laws run over the producers that can issue two groups in sequence, and a
/// tier must register at least one.
#[macro_export]
macro_rules! tool_call_limit_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_call_limit_tests!(@law [$(#[$attr])*] $fixture;
            (tool_call_limit_admits_the_limit_and_refuses_the_group_past_it, all));
        $crate::tool_call_limit_tests!(@law [$(#[$attr])*] $fixture;
            (tool_call_limit_staged_calls, staged_producers));
        $crate::tool_call_limit_tests!(@law [$(#[$attr])*] $fixture;
            (tool_call_limit_refuses_the_same_call_across_a_crash, turn_staged_producers));
    };
    (@producers all, $producers:expr) => {
        $producers
    };
    (@producers $select:ident, $producers:expr) => {
        $crate::registration_macro_support::$select($producers)
    };
    (@law [$($attr:tt)*] $fixture:block; ($law:ident, $select:ident)) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn $law() {
            let (_guard, prefix, host, stores, producers, runner) = $fixture;
            let producers: Vec<$crate::ToolBatchProducer> =
                $crate::tool_call_limit_tests!(@producers $select, producers);
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

/// Register the `max_tool_calls` law of a process's holding (FIG-4546): a
/// process that holds `max_tool_calls` calls is refused one more while it
/// holds them, and counts them once however many executions of its segment
/// formed their group.
///
/// The fixture is [`tool_call_limit_tests!`]'s. The law runs over the
/// producers that issue their groups from a process body, and a tier must
/// register at least one, with a runner that can kill a process's worker.
#[macro_export]
macro_rules! tool_call_limit_process_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::tool_call_limit_tests!(@law [$(#[$attr])*] $fixture;
            (tool_call_limit_counts_what_a_process_holds_across_a_worker_kill, holding_producers));
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
            (standard_rounds_and_batches_use_the_run, "standard-rounds-and-batches-use-the-run"));
        $crate::batch_sugar_tests!(@law [$(#[$attr])*] $fixture;
            (batch_admission_and_identity_contract, "batch-admission-and-identity-contract"));
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
