// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

//! The turn crash laws on the PostgreSQL engine, which live until FIG-3667.

use super::*;

/// A journaled invocation over the database's journal. Its redrive opens a
/// successor controller over the same journal, the way a restarted process
/// does: its own group-executor registration, the journal's completed effects
/// replayed. Effect leases lapse on the recovery timings, so the successor
/// reclaims what a crashed controller held.
fn journaled_crash_invocation(
    database_url: &str,
    scope: ExecutionScope,
) -> lash_conformance::ConformanceInvocation {
    let database_url = database_url.to_string();
    let storage = sync_await(async move {
        PostgresStorage::connect(&database_url)
            .await
            .expect("construct Postgres crash-law journal pool")
    });
    let options = PostgresEffectReplayOptions {
        lease_timings: lash_core_execution::facade_support::LeaseTimings::new(
            std::time::Duration::from_millis(600),
            std::time::Duration::from_millis(100),
        )
        .expect("crash-law effect lease timings"),
        ..PostgresEffectReplayOptions::default()
    };
    let open = {
        let scope = scope.clone();
        move || {
            PostgresRuntimeEffectController::with_options(&storage, scope.clone(), options.clone())
        }
    };
    let controller = open();
    let faults = controller.effect_journal_faults();
    lash_conformance::ConformanceInvocation::new(
        Arc::new(controller) as Arc<dyn RuntimeEffectController>,
        scope,
        || {},
        move || Arc::new(open()) as Arc<dyn RuntimeEffectController>,
    )
    .with_effect_journal_faults(faults)
}

/// A pooled session store over `database_url` for one crash-law scenario.
fn crash_law_store(
    database_url: &str,
    scenario: &str,
) -> Arc<lash_postgres_store::PostgresSessionStore> {
    let database_url = database_url.to_string();
    let storage = sync_await(async move {
        PostgresStorage::connect(&database_url)
            .await
            .expect("construct fresh Postgres crash-law pool")
    });
    Arc::new(storage.session_store(format!("trace-derived-real-turn:{scenario}")))
}

lash_conformance::turn_crash_matrix_tests!({
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres real-turn crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let (attachments, stores) = pg_law_stores(&storage);
    let store_url = database_url.clone();
    let matrix_url = database_url.clone();
    (
        (database_lock, attachments),
        stores,
        move |scenario: &str| crash_law_store(&store_url, scenario) as Arc<dyn RuntimePersistence>,
        move |_: &str, scope: ExecutionScope| journaled_crash_invocation(&matrix_url, scope),
        move |_: &str, scope: ExecutionScope| {
            // FIG-3524: the error-return sweep arms journal faults on its
            // controller; the short renew interval lets a `renew` fault fire
            // while the parked tool attempt is still open.
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("construct Postgres error-return journal pool")
            });
            let controller = PostgresRuntimeEffectController::with_options(
                &storage,
                scope.clone(),
                PostgresEffectReplayOptions {
                    lease_timings: lash_core_execution::facade_support::LeaseTimings::new(
                        std::time::Duration::from_secs(60),
                        std::time::Duration::from_millis(50),
                    )
                    .expect("error-return effect lease timings"),
                    ..PostgresEffectReplayOptions::default()
                },
            );
            postgres_conformance_invocation(controller.clone(), scope)
                .with_effect_journal_faults(controller.effect_journal_faults())
        },
    )
});

// The level-one matrix simulates each crash in process. On the journaled
// PostgreSQL engine the crashed attempt's group child keeps running and
// renewing its effect lease, which nothing in the process can stop, so the
// successor waits on it forever. The native host this matrix ran on is gone;
// the real SIGKILL matrix covers this engine's crash recovery.
lash_conformance::turn_crash_level_1_tests!(
    #[ignore = "parked: an in-process crash cannot stop the journaled engine's attempt (FIG-3667)"]
    {
        let Some((database_lock, storage)) = storage().await else {
            eprintln!(
                "skipping Postgres real-turn crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        };
        reset(storage.pool()).await;
        let database_url = database_url().expect("configured Postgres database URL");
        let (attachments, stores) = pg_law_stores(&storage);
        let store_url = database_url.clone();
        let matrix_url = database_url.clone();
        (
            (database_lock, attachments),
            stores,
            move |scenario: &str| {
                crash_law_store(&store_url, scenario) as Arc<dyn RuntimePersistence>
            },
            move |_: &str, scope: ExecutionScope| journaled_crash_invocation(&matrix_url, scope),
            move |_: &str, scope: ExecutionScope| journaled_crash_invocation(&database_url, scope),
        )
    }
);

/// FIG-3571: a turn the pre-cutover build left in flight is refused, typed,
/// before any effect when this build redrives it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_pre_cutover_generation_turn_redrive_is_refused_before_any_effect_when_configured()
{
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres pre-cutover generation law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let (_attachments, stores) = pg_law_stores(&storage);
    Box::pin(
        lash_conformance::pre_cutover_generation_turn_redrive_is_refused_before_any_effect(
            stores,
            |scenario| crash_law_store(&database_url, scenario),
            |_, scope| journaled_crash_invocation(&database_url, scope),
        ),
    )
    .await;
}

/// FIG-3619: a runtime already open on a session whose marker moves behind
/// this build is refused, typed and terminal, at the turn-lane claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_pre_cutover_generation_turn_claim_is_refused_typed_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres pre-cutover generation claim law: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let (_attachments, stores) = pg_law_stores(&storage);
    Box::pin(
        lash_conformance::pre_cutover_generation_turn_claim_is_refused_typed(
            stores,
            |scenario| crash_law_store(&database_url, scenario),
            |_, scope| journaled_crash_invocation(&database_url, scope),
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_held_turn_input_visibility_survives_claim_holder_crash_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres held-input crash law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(storage.pool()).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let (_attachments, stores) = pg_law_stores(&storage);
    Box::pin(
        lash_conformance::held_turn_input_visibility_survives_claim_holder_crash(
            stores,
            |scenario| crash_law_store(&database_url, scenario) as Arc<dyn RuntimePersistence>,
            |_, scope| journaled_crash_invocation(&database_url, scope),
        ),
    )
    .await;
}

#[tokio::test]
async fn postgres_real_turn_satisfies_cold_process_crash_matrix_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping PostgreSQL cold-process real-turn matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let url = database_url().expect("configured PostgreSQL database URL");
    let dir = tempfile::tempdir().expect("PostgreSQL cold-process real-turn tempdir");
    cold_process_turn_parent::assert_real_turn_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command = tokio::process::Command::new(lash_conformance::helper_executable(
                "postgres-await-event-helper",
            ));
            command
                .env("LASH_POSTGRES_DATABASE_URL", &url)
                .arg(action)
                .arg(nonce)
                .arg(marker);
            command
        },
    )
    .await;
}
