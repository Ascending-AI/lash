//! The C1 and batched-cascade laws of process actors
//! (`lash_core_execution::runtime::actor::process_laws`; L6, FIG-5175) over
//! PostgreSQL, each on its own isolated database.

// This file is test code; ambient env access is sanctioned here (the
// workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::runtime::actor::process_laws;
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
                settings: process_laws::settings(),
                engines: Vec::new(),
                providers: Arc::new(NoProjectionProviders),
            })
            .expect("assemble the law backend");
            process_laws::$name(&backend)
                .await
                .unwrap_or_else(|broken| panic!("{broken}"));
        }
    )*};
}

law!(
    c1_a_parked_child_ends_engine_free_and_its_child_receives_parent_ended,
    a_cascade_wider_than_its_batch_ends_a_tree_three_levels_deep,
);
