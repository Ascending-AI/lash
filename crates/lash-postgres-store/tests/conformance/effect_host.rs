use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_cold_instance_await_event_conformance_when_configured() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-instance AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    lash_conformance::effect_host_await_events_cold_instance(|| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("cold PostgreSQL effect host")
        });
        Arc::new(storage.effect_host()) as Arc<dyn EffectHost>
    })
    .await;
    drop(database_lock);
}

/// The durable PostgreSQL tier answers the effect-group contract the same way
/// the in-memory reference host does (FIG-1564).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_the_effect_group_contract_when_configured() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres effect-group conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    lash_conformance::effect_group_host_conformance(|executors| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("PostgreSQL effect-group host")
        });
        let host = storage.effect_host();
        // Registration is what makes the host support groups at all: since
        // FIG-1578 a group carries envelopes, and what runs a child is the
        // resolver its host was built with. `None` is the unregistered host two
        // laws are about, over the same database as the wired ones.
        if let Some(executors) = executors {
            host.register_group_executors(executors)
                .expect("a freshly connected host has no resolver yet");
        }
        Arc::new(host) as Arc<dyn EffectHost>
    })
    .await;
    drop(database_lock);
}

/// A cancelled child's cancellation is journaled as its terminal, and a host
/// that was not running when the close happened reads it back (FIG-1564).
///
/// The reading host is the point: it holds none of the closing host's memory,
/// so the terminal it serves came out of the effect journal or from nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_journals_a_cancelled_child_as_its_terminal_when_configured() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cancelled-child terminal test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    drop(storage);
    let database_url = database_url().expect("configured Postgres database URL");
    lash_conformance::effect_group_cancelled_child_terminal_is_durable(|executors| {
        let database_url = database_url.clone();
        let storage = sync_await(async move {
            PostgresStorage::connect(&database_url)
                .await
                .expect("PostgreSQL effect-group host")
        });
        let host = storage.effect_host();
        // Registration is what makes the host support groups at all: since
        // FIG-1578 a group carries envelopes, and what runs a child is the
        // resolver its host was built with.
        if let Some(executors) = executors {
            host.register_group_executors(executors)
                .expect("a freshly connected host has no resolver yet");
        }
        Arc::new(host) as Arc<dyn EffectHost>
    })
    .await;
    drop(database_lock);
}

/// Retiring a runtime-operation scope removes its group and child rows in one
/// transaction and leaves the fence, while an in-flight operation keeps every
/// row (FIG-2500).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_retires_a_runtime_operation_journal_atomically() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres runtime-operation retirement test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let (retired, in_flight) =
        lash_conformance::effect_group_runtime_operation_retirement_is_atomic(|executors| {
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("PostgreSQL effect-group host")
            });
            let host = storage.effect_host();
            if let Some(executors) = executors {
                host.register_group_executors(executors)
                    .expect("a freshly connected host has no resolver yet");
            }
            Arc::new(host) as Arc<dyn EffectHost>
        })
        .await;
    let count = |sql: &'static str, scope_id: String| {
        let pool = storage.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(scope_id)
                .fetch_one(&pool)
                .await
                .expect("count journal rows")
        }
    };
    let groups = "SELECT COUNT(*) FROM lash_runtime_effect_group WHERE scope_id = $1";
    let children = "SELECT COUNT(*) FROM lash_runtime_effect_replay WHERE scope_id = $1";
    let fences = "SELECT COUNT(*) FROM lash_effect_scope_retirements WHERE scope_id = $1";
    assert_eq!(
        count(groups, retired.clone()).await,
        0,
        "retired scope keeps no group row"
    );
    assert_eq!(
        count(children, retired.clone()).await,
        0,
        "retired scope keeps no child row"
    );
    assert_eq!(
        count(fences, retired).await,
        1,
        "retired scope leaves one fence"
    );
    assert_eq!(
        count(groups, in_flight.clone()).await,
        1,
        "in-flight scope keeps its group"
    );
    assert_eq!(
        count(children, in_flight.clone()).await,
        2,
        "in-flight scope keeps its children"
    );
    assert_eq!(
        count(fences, in_flight).await,
        0,
        "in-flight scope is not fenced"
    );
    drop(database_lock);
}
