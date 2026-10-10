//! The C1, batched-cascade and step-lifecycle laws of process actors
//! (`lash_core_execution::runtime::actor::process_laws`; L6, FIG-5175) over
//! PostgreSQL, each on its own isolated database.

// This file is test code; ambient env access is sanctioned here (the
// workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_core_execution::runtime::actor::process_laws;
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders};

use crate::PostgresStoreSet;
use crate::testing::IsolatedDatabase;

macro_rules! law {
    ($($name:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(database_url) = crate::postgres_test_support::database_url() else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
                return;
            };
            let database = IsolatedDatabase::create(&database_url).await;
            let storage = crate::testing::connect(database.url())
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
    an_awaited_engine_key_records_a_waiting_fact,
    a_pinned_engine_key_is_listed_from_its_wait_after_a_restart_and_a_handover,
    a_parked_call_is_reopened_from_its_completion_wait_after_a_restart_and_a_handover,
    a_sleeping_process_reads_waiting_on_its_sleep_and_its_site,
    a_process_awaiting_a_child_reads_waiting_on_that_process,
    a_process_with_two_parked_calls_lists_both_without_their_keys,
    a_process_parked_on_an_unknown_engine_shows_its_park_reason_beside_its_lifecycle,
);

/// FIG-5235: a deterministic 40P01 inside the terminal is retried, not refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deadlock_inside_a_process_terminal_is_retried() {
    let database_url =
        crate::postgres_test_support::database_url().expect("hermetic PostgreSQL URL");
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
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
        CREATE TRIGGER terminal_contention AFTER UPDATE OF record_json ON lash_processes
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

/// FIG-5855: a deterministic 40P01 inside a cancel's commit, where the
/// cancel appends its mail, is retried as the identical commit, not refused
/// as a terminal plugin error: the process still ends, cancelled once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deadlock_inside_a_cancel_is_retried() {
    let database_url =
        crate::postgres_test_support::database_url().expect("hermetic PostgreSQL URL");
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("isolated store");
    sqlx::raw_sql(super::DEADLOCK_FIRST_CANCEL_MAIL_SQL)
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
    process_laws::a_contended_cancel_commit_still_ends_the_process_once(&backend)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
    let attempts: i64 = sqlx::query_scalar("SELECT last_value FROM cancel_attempts")
        .fetch_one(storage.pool())
        .await
        .expect("count cancel attempts");
    assert_eq!(
        attempts, 2,
        "the injected failure and one real cancel commit"
    );
}

/// FIG-5855: a registry cancel takes its process's actor row before the
/// process row, the order the actor's owner commit takes them in (its
/// fence, then its `advance`). While an owner commit holds the actor row,
/// the waiting cancel holds no lock on the process row, so the owner's next
/// write never waits on the cancel and the two never deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_behind_an_owner_commit_holds_no_lock_on_its_process() {
    use lash_core_execution::{ProcessLifecycle as _, ProcessRegistrar as _};

    let database_url =
        crate::postgres_test_support::database_url().expect("hermetic PostgreSQL URL");
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("isolated store");
    lash_core_execution::testing::process_execution_env_fixture(&storage.process_env_store()).await;
    let process = storage
        .process_registry()
        .register_process(lash_core_execution::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core_execution::ProcessProvenance::host(),
            lash_core_execution::Lifetime::Detached,
        ))
        .await
        .expect("register the process")
        .id;
    let actor = lash_durable::ActorKey::process(process.as_str()).expect("a process actor key");

    // The owner's commit takes the actor row first, as its fence does.
    let mut owner = storage
        .pool()
        .begin()
        .await
        .expect("begin the owner commit");
    sqlx::query(super::OWNER_FENCE_SQL)
        .bind(actor.as_str())
        .execute(&mut *owner)
        .await
        .expect("the owner's fence");
    let cancel = tokio::spawn({
        let registry = storage.process_registry();
        let process = process.clone();
        async move {
            registry
                .request_process_cancel(
                    &process,
                    lash_core_execution::CancelOrigin::OperatorRequested,
                    "lock-order-law".to_owned(),
                    None,
                )
                .await
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(storage.pool())
        .await
        .expect("count lock waiters");
        if waiting > 0 {
            break;
        }
        assert!(
            !cancel.is_finished(),
            "the cancel finished while the owner held its actor"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the cancel never waited on the owner"
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    // The owner's next write takes the process row: the cancel waiting
    // behind the owner must not hold it.
    let locked =
        sqlx::query("SELECT 1 FROM lash_processes WHERE process_id = $1 FOR UPDATE NOWAIT")
            .bind(process.as_str())
            .execute(&mut *owner)
            .await;
    assert!(
        locked.is_ok(),
        "the waiting cancel held the process row ahead of its actor: {locked:?}"
    );
    owner.rollback().await.expect("end the owner commit");
    let record = cancel
        .await
        .expect("the cancel task")
        .expect("the cancel commits once the owner ends");
    assert!(
        record.cancel_request.is_some(),
        "the cancel recorded its request"
    );
}
