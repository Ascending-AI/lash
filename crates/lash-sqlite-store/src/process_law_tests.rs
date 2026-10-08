//! The laws of process actors
//! (`lash_core_execution::runtime::actor::process_laws`; L6, FIG-5175) over
//! a SQLite memory store set.

use std::sync::Arc;

use lash_core_execution::runtime::actor::process_laws;
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders};

use crate::SqliteStoreSet;

async fn backend() -> Backend {
    let set = SqliteStoreSet::memory()
        .await
        .expect("open the memory store set");
    Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: Arc::new(set),
        settings: process_laws::settings(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
    })
    .expect("assemble the law backend")
}

macro_rules! law {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            process_laws::$name(&backend().await)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

law!(
    a_transition_in_another_format_commits_no_state,
    c1_a_parked_child_ends_engine_free_and_its_child_receives_parent_ended,
    c1_a_waiting_child_ends_within_its_grace_with_one_cancelled_advance,
    c2_a_cancel_the_engine_ignores_is_forced_at_its_grace,
    w1_await_process_times_out_and_its_awaiters_cancel_ends_it,
    w1_an_await_cycle_ends_by_a_timeout_and_is_cancellable,
    engine_free_end_runs_no_engine_code,
    p1_a_crash_loop_parks_at_its_budget_and_progress_resets_the_count,
    a_cascade_wider_than_its_batch_ends_a_tree_three_levels_deep,
    a_repeatable_step_that_fails_retryably_once_succeeds_on_its_second_ordinal,
    a_step_parked_on_its_wait_settles_when_the_wait_resolves,
    an_awaited_engine_key_records_a_waiting_fact,
    a_pinned_engine_key_is_listed_from_its_wait_after_a_restart_and_a_handover,
);
