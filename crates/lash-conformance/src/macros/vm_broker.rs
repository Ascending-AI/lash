/// Register the worker-broker laws (FIG-4159, ADR 0123): the kill-point
/// matrix of a worker running model code, the refusal of unauthorised, stale
/// and duplicate worker messages, the journaled cancellation winner, frame
/// opening, and slot release. The fixture hands back a guard, a prefix that
/// names this run's sessions apart from every other, and the tier's
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), which must run a
/// turn and crash and redrive one.
///
/// The worker is the in-process fake behind the protocol types; its effects
/// journal on the tier's own controller.
#[macro_export]
macro_rules! vm_broker_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::vm_broker_tests!(@laws [$(#[$attr])*] $fixture; [
            worker_kill_before_start_runs_no_effect,
            worker_kill_mid_compute_redrives_through_the_substrate,
            worker_kill_after_request_before_record_settles_the_admitted_operation_once,
            worker_kill_after_record_before_delivery_replays_with_zero_dispatch,
            worker_kill_mid_serialization_keeps_the_last_checkpoint,
            worker_kill_after_complete_before_commit_commits_once,
            unauthorized_worker_effect_request_is_refused_without_invoking_a_tool,
            stale_epoch_and_duplicate_worker_messages_are_refused,
            a_refused_run_is_terminal_and_a_broken_exchange_is_redriven,
            cancellation_winner_is_the_journaled_checkpoint_across_worker_kill,
            frame_open_retires_worker_state_and_old_globals_are_undefined,
            one_slot_nested_effect_does_not_deadlock,
        ]);
    };
    (@laws $attrs:tt $fixture:block; [$($law:ident),* $(,)?]) => {
        $($crate::vm_broker_tests!(@law $attrs $fixture; $law);)*
    };
    (@law [$($attr:tt)*] $fixture:block; $law:ident) => {
        $($attr)*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, runner) = $fixture;
            $crate::registration_macro_support::$law(&prefix, runner).await;
        }
    };
}
