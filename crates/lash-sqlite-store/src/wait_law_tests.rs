//! The laws of waits and completion keys
//! (`lash_core_execution::runtime::actor::wait_laws`; L5, FIG-5173) over a
//! SQLite memory store set.

use std::sync::Arc;

use lash_core_execution::runtime::actor::wait_laws;
use lash_core_execution::{Backend, BackendParts, CompletionKeySecrets, NoProjectionProviders};

use crate::SqliteStoreSet;

async fn backend() -> Backend {
    let set = SqliteStoreSet::memory()
        .await
        .expect("open the memory store set");
    Backend::assemble(BackendParts {
        stores: Arc::new(set),
        settings: wait_laws::settings(),
        secrets: Some(CompletionKeySecrets::for_testing()),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
    })
    .expect("assemble the law backend")
}

macro_rules! law {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            wait_laws::$name(&backend().await)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

law!(
    k1_forged_keys_are_refused_and_write_nothing,
    the_first_resolution_wins,
    a_waiting_actor_past_its_deadline_times_out_within_the_claim_poll,
    an_unresolved_wait_suspends_and_resumes_on_resolution,
    a_completion_before_the_await_is_already_resolved,
    a_duplicate_resolution_keeps_the_first,
    the_awaiters_cancel_ends_its_wait,
    a_timeout_racing_a_completion_has_one_winner,
    a_wait_survives_its_owners_death_with_the_same_key_and_deadline,
    a_key_that_never_resolves_times_out,
    await_process_is_bounded_and_cancellable,
    keys_verify_under_their_own_version_until_it_is_removed,
);
