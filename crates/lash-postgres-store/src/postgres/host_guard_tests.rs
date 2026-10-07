//! Laws of the host configuration on a real server (FIG-5240): every role's
//! guard holds from its transaction's first lock, the writer fence's share
//! lock included, whoever built the pool; every role names itself; and a
//! deployment the server cannot hold is refused before the pools open.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use lash_durable::{CommitLabel, NodeSpec, Signals as _};
use sqlx::Connection as _;

use super::*;
use crate::host::{DeploymentBudget, PostgresHostConfig, ServerTimeout};
use crate::testing::IsolatedDatabase;

async fn database(law: &str) -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping {law}: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

/// Every lock limit short, so a held fence surfaces at once.
fn short_locks() -> PostgresHostConfig {
    let mut config = crate::testing::fixture_config();
    let lock = ServerTimeout::Limit(Duration::from_millis(300));
    config.guards.ordinary.lock = lock;
    config.guards.durable.lock = lock;
    config.guards.renewal.lock = ServerTimeout::Limit(Duration::from_millis(200));
    config.guards.scheduler.lock = ServerTimeout::Limit(Duration::from_millis(200));
    config
}

/// A connection holding the fleet-format row `FOR UPDATE`: every writer's
/// fence waits on it, as it does behind a finalize.
async fn hold_fence(url: &str) -> sqlx::PgConnection {
    let mut holder = sqlx::PgConnection::connect(url)
        .await
        .expect("connect the fence holder");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin the hold");
    sqlx::query(
        crate::session_sql::session_sql()
            .fleet_format
            .select_for_update
            .sql(),
    )
    .fetch_one(&mut holder)
    .await
    .expect("lock the fleet-format row");
    holder
}

/// Open a transaction on `label`'s capacity while the fence is held: the
/// role's lock guard must refuse it contended, well before the probe's own
/// bound.
async fn first_lock_refused(store: &PostgresDurableStore, label: CommitLabel) {
    let started = Instant::now();
    let opened = tokio::time::timeout(Duration::from_secs(5), store.open(label))
        .await
        .unwrap_or_else(|_| panic!("{label}: the first lock waited past every guard"));
    match opened {
        Err(DurableError::Store(failure)) if failure.kind == StoreFailureKind::Contended => {}
        Err(other) => panic!("{label}: expected a contended refusal, got {other}"),
        Ok(_) => panic!("{label}: the transaction passed a held fence"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{label}: refused only after {:?}",
        started.elapsed()
    );
}

async fn every_role_refuses_its_first_lock(storage: &crate::PostgresStorage, url: &str) {
    let store = storage.durable_store();
    let mut holder = hold_fence(url).await;
    for label in [
        CommitLabel::MAIL_SESSION,
        CommitLabel::PROCESS_TERMINAL,
        CommitLabel::CLAIM,
        CommitLabel::HEARTBEAT,
    ] {
        first_lock_refused(&store, label).await;
    }
    let started = Instant::now();
    let ordinary = tokio::time::timeout(
        Duration::from_secs(5),
        crate::begin_guarded(storage.pool(), &storage.fence),
    )
    .await
    .expect("the ordinary first lock waited past its guard");
    assert!(
        matches!(ordinary, Err(StoreError::Contended)),
        "an ordinary transaction passed a held fence"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    sqlx::query("ROLLBACK")
        .execute(&mut holder)
        .await
        .expect("release the fence");
    // Every role's capacity came back: each opens at once now.
    for label in [
        CommitLabel::MAIL_SESSION,
        CommitLabel::PROCESS_TERMINAL,
        CommitLabel::CLAIM,
        CommitLabel::HEARTBEAT,
    ] {
        let (tx, _) = store.open(label).await.expect("the role opens again");
        tx.rollback().await.expect("roll back");
    }
}

/// The writer fence is the first lock every transaction takes. Each role's
/// lock guard bounds it, because the guards ride the transaction's `BEGIN`:
/// before FIG-5240 the durable guards were installed after the fence, and
/// the reserved pool carried no session guard at all, so a heartbeat waited
/// on a held fence without bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_role_bounds_its_first_lock_by_its_guard() {
    let Some(database) = database("every_role_bounds_its_first_lock_by_its_guard").await else {
        return;
    };
    let storage = crate::testing::connect_with(database.url(), &short_locks())
        .await
        .expect("open the isolated store");
    every_role_refuses_its_first_lock(&storage, database.url()).await;
}

/// The same guard holds over pools the host built with no session settings
/// of its own: the prelude, not a connect hook, installs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_pool_set_bounds_its_first_lock_by_the_guard() {
    let Some(database) = database("an_imported_pool_set_bounds_its_first_lock_by_the_guard").await
    else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(database.url())
        .await
        .expect("the host builds its pool");
    let mut config = short_locks();
    config.roles.max_store_operations = 8;
    let storage = crate::testing::from_pool(pool, &config)
        .await
        .expect("open over the imported pool");
    every_role_refuses_its_first_lock(&storage, database.url()).await;
}

/// Every role's connection names itself `<prefix>/<role>` in
/// `pg_stat_activity`, so an operator can tell them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_role_names_itself_in_pg_stat_activity() {
    let Some(database) = database("every_role_names_itself_in_pg_stat_activity").await else {
        return;
    };
    let prefix = format!("law-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let mut config = crate::testing::fixture_config();
    config.connection.application_name_prefix = prefix.clone();
    let storage = crate::testing::connect_with(database.url(), &config)
        .await
        .expect("open the isolated store");
    let store = storage.durable_store();
    let lease = store
        .register_node(&NodeSpec {
            node: NodeId::new("named"),
            decodes: vec![FormatSet::new("law-formats")],
            ttl_millis: 15_000,
        })
        .await
        .expect("register on the renewal connection");
    let _feed = storage
        .durable_signals()
        .listen(&lease)
        .await
        .expect("listen on the listener session");
    let pools = &storage.pools;
    let _held = (
        pools.work.acquire().await.expect("work"),
        pools.scheduler.acquire().await.expect("scheduler"),
        pools.critical.acquire().await.expect("critical"),
        pools.renewal.acquire().await.expect("renewal"),
        pools.session.acquire().await.expect("session"),
    );
    let mut probe = sqlx::PgConnection::connect(database.url())
        .await
        .expect("connect the probe");
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT application_name FROM pg_stat_activity
         WHERE application_name LIKE $1 ORDER BY 1",
    )
    .bind(format!("{prefix}/%"))
    .fetch_all(&mut probe)
    .await
    .expect("read pg_stat_activity");
    let expected: Vec<String> = [
        "critical",
        "listener",
        "renewal",
        "scheduler",
        "session",
        "work",
    ]
    .iter()
    .map(|role| format!("{prefix}/{role}"))
    .collect();
    assert_eq!(names, expected);
}

/// A declared deployment the server cannot hold is refused with the
/// budget's report, before any role pool connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_over_the_server_capacity_is_refused_before_the_pools_open() {
    let Some(database) =
        database("a_deployment_over_the_server_capacity_is_refused_before_the_pools_open").await
    else {
        return;
    };
    let mut config = crate::testing::fixture_config();
    config.connection.application_name_prefix = format!(
        "budget-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    config.deployment = Some(DeploymentBudget {
        processes_per_generation: 100_000,
        generations: 2,
        other_clients: 0,
        admin_headroom: 10,
        other_host_connections: 0,
        operator_connections: 0,
    });
    let endpoints = crate::PostgresEndpoints::from_url(database.url()).expect("parses");
    let refused = crate::PostgresStorage::connect(&endpoints, &config, Default::default())
        .await
        .err();
    assert!(
        matches!(
            refused,
            Some(crate::PostgresHostError::Budget(
                crate::PostgresConnectionBudgetRefusal::ConnectionBudgetExceeded { .. }
            ))
        ),
        "{refused:?}"
    );
    let mut probe = sqlx::PgConnection::connect(database.url())
        .await
        .expect("connect the probe");
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE application_name LIKE $1 AND application_name <> $2",
    )
    .bind(format!("{}/%", config.connection.application_name_prefix))
    .bind(format!(
        "{}/work",
        config.connection.application_name_prefix
    ))
    .fetch_one(&mut probe)
    .await
    .expect("read pg_stat_activity");
    assert_eq!(open, 0, "only the capacity probe's work connection opened");
}
