//! The K1, first-winner and T1 laws of waits
//! (`lash_core_execution::runtime::actor::wait_laws`; L5, FIG-5173) over
//! PostgreSQL, each on its own isolated database.

// This file is test code; ambient env access is sanctioned here (the
// workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::runtime::actor::wait_laws;
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders};

use crate::testing::IsolatedDatabase;
use crate::{PostgresStorage, PostgresStoreSet};

macro_rules! law {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(database_url) = crate::postgres_test_support::database_url() else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
                return;
            };
            let database = IsolatedDatabase::create(&database_url).await;
            let storage = PostgresStorage::connect(database.url())
                .await
                .expect("open the isolated store");
            let backend = Backend::assemble(BackendParts {
                formats: Vec::new(),
                stores: Arc::new(PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core_execution::attachments::UnavailableAttachmentStore),
                )),
                settings: wait_laws::settings(),
                engines: Vec::new(),
                providers: Arc::new(NoProjectionProviders),
            })
            .expect("assemble the law backend");
            wait_laws::$name(&backend)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

law!(
    k1_a_key_that_is_not_an_issued_wait_id_is_refused_and_writes_nothing,
    the_first_resolution_wins,
    a_waiting_actor_past_its_deadline_times_out_within_the_claim_poll,
);
