//! The C1, batched-cascade and step-lifecycle laws of process actors
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
    a_repeatable_step_that_fails_retryably_once_succeeds_on_its_second_ordinal,
    a_step_parked_on_its_wait_settles_when_the_wait_resolves,
);

/// FIG-5235: a deterministic 40P01 inside the terminal is retried, not refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deadlock_inside_a_process_terminal_is_retried() {
    let database_url =
        crate::postgres_test_support::database_url().expect("hermetic PostgreSQL URL");
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("isolated store");
    sqlx::raw_sql(
        r#"
        CREATE SEQUENCE terminal_attempts;
        CREATE FUNCTION fail_first_terminal() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF NEW.status NOT IN ('running', 'waiting') AND nextval('terminal_attempts') = 1 THEN
                RAISE EXCEPTION 'injected terminal deadlock' USING ERRCODE = '40P01';
            END IF;
            RETURN NEW;
        END $$;
        CREATE TRIGGER terminal_contention BEFORE UPDATE OF status ON lash_processes
            FOR EACH ROW EXECUTE FUNCTION fail_first_terminal();
    "#,
    )
    .execute(storage.pool())
    .await
    .expect("install one-shot deadlock");
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
    .expect("assemble law backend");
    process_laws::a_process_terminal_keeps_its_real_outcome_after_contention(&backend)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
    let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM terminal_attempts")
        .fetch_one(storage.pool())
        .await
        .expect("count terminal attempts");
    assert_eq!(
        attempts, 2,
        "the injected failure and one real terminal commit"
    );
}
