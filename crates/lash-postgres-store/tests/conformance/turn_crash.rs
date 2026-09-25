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

/// A turn-crash runner fixture over a PostgreSQL effect host whose leases run
/// on `lease_timings`: the runner cuts turns with that host's journal fault
/// injector. `None` when no database is configured.
async fn journal_runner_fixture(
    lease_timings: lash_core_execution::facade_support::LeaseTimings,
) -> Option<(
    impl Sized,
    Arc<dyn lash_core_execution::StoreSet>,
    impl Fn(&str) -> Arc<lash_postgres_store::PostgresSessionStore> + Send + Sync + 'static,
    Arc<dyn lash_core_execution::EffectHost>,
    Arc<dyn lash_conformance::ConformanceTurnRunner>,
)> {
    let (database_lock, storage) = storage().await?;
    reset(storage.pool()).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let (attachments, stores) = pg_law_stores(&storage);
    let host = Arc::new(lash_postgres_store::PostgresEffectHost::with_options(
        &storage,
        PostgresEffectReplayOptions {
            lease_timings,
            ..PostgresEffectReplayOptions::default()
        },
    ));
    let faults = host.effect_journal_faults();
    let host = host as Arc<dyn lash_core_execution::EffectHost>;
    Some((
        (database_lock, attachments),
        stores,
        move |scenario: &str| crash_law_store(&database_url, scenario),
        Arc::clone(&host),
        lash_conformance::HostTurnRunner::with_journal_faults(host, faults),
    ))
}

/// Effect leases that lapse on the recovery timings, so a successor reclaims
/// what a crashed turn held.
fn crash_lease_timings() -> lash_core_execution::facade_support::LeaseTimings {
    lash_core_execution::facade_support::LeaseTimings::new(
        std::time::Duration::from_millis(600),
        std::time::Duration::from_millis(100),
    )
    .expect("crash-law effect lease timings")
}

// FIG-3524: the error-return sweep arms journal faults on the host; the short
// renew interval lets a `renew` fault fire while the parked tool attempt is
// still open.
lash_conformance::turn_crash_matrix_tests!({
    let Some(fixture) = journal_runner_fixture(
        lash_core_execution::facade_support::LeaseTimings::new(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_millis(50),
        )
        .expect("error-return effect lease timings"),
    )
    .await
    else {
        eprintln!(
            "skipping Postgres real-turn crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    fixture
});

// The level-one matrix crashes each turn in process. On the journaled
// PostgreSQL engine the crashed attempt's group child keeps running and
// renewing its effect lease, which nothing in the process can stop, so the
// successor waits on it forever. The real SIGKILL matrix covers this engine's
// crash recovery.
lash_conformance::turn_crash_level_1_tests!(
    #[ignore = "parked: an in-process crash cannot stop the journaled engine's attempt (FIG-3667)"]
    {
        let Some(fixture) = journal_runner_fixture(crash_lease_timings()).await else {
            eprintln!(
                "skipping Postgres real-turn crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
            );
            return;
        };
        fixture
    }
);

// The turn crash laws that run their turns on a turn runner: the FIG-3571
// generation-refusal pair, the direct-acceptance crash and the cancel-closure
// cuts, on a PostgreSQL effect host whose leases lapse on the recovery
// timings. A crash drops the turn's task, and the recovery is a fresh runtime
// over the same database.
lash_conformance::turn_crash_runner_tests!({
    let Some(fixture) = journal_runner_fixture(crash_lease_timings()).await else {
        eprintln!(
            "skipping Postgres runner turn crash laws: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    fixture
});

lash_conformance::effect_layer_group_child_tests!({
    let Some(fixture) = journal_runner_fixture(crash_lease_timings()).await else {
        eprintln!("skipping Postgres host-layer law: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    fixture
});

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
