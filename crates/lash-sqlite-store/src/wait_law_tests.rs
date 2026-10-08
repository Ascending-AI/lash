//! The laws of waits and completion keys
//! (`lash_core_execution::runtime::actor::wait_laws`; L5, FIG-5173) over a
//! SQLite memory store set, and in `file` over a SQLite file store set.

use std::sync::Arc;

use lash_core_execution::runtime::actor::wait_laws;
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders};

use crate::SqliteStoreSet;

fn assemble(set: SqliteStoreSet) -> Backend {
    Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: Arc::new(set),
        settings: wait_laws::settings(),
        engines: Vec::new(),
        providers: Arc::new(NoProjectionProviders),
    })
    .expect("assemble the law backend")
}

async fn backend() -> Backend {
    assemble(
        SqliteStoreSet::memory()
            .await
            .expect("open the memory store set"),
    )
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

macro_rules! laws {
    () => {
        law!(
            k1_a_key_that_is_not_an_issued_wait_id_is_refused_and_writes_nothing,
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
            a_parked_call_is_listed_from_its_wait_row_alone,
        );
    };
}

laws!();

/// The same laws over a SQLite file store set, each in its own directory.
mod file {
    use super::{Backend, SqliteStoreSet, assemble, wait_laws};

    async fn backend() -> (tempfile::TempDir, Backend) {
        let root = tempfile::tempdir().expect("a store root");
        let set = SqliteStoreSet::open(
            root.path().join("lash.db"),
            crate::SqliteSynchronous::Normal,
        )
        .await
        .expect("open the file store set");
        (root, assemble(set))
    }

    macro_rules! law {
        ($($name:ident),* $(,)?) => {$(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                let (_root, backend) = backend().await;
                wait_laws::$name(&backend)
                    .await
                    .unwrap_or_else(|broken| panic!("{broken}"));
            }
        )*};
    }

    laws!();
}
