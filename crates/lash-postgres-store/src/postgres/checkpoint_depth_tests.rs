use crate::*;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::prelude::*;

tokio::task_local! {
    static STATEMENT_COUNT: std::cell::Cell<usize>;
}

struct StatementWitness;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for StatementWitness {
    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        if metadata.target() == "sqlx::query" {
            tracing::subscriber::Interest::always()
        } else {
            tracing::subscriber::Interest::never()
        }
    }

    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(tracing::metadata::LevelFilter::TRACE)
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() == "sqlx::query" {
            let _ = STATEMENT_COUNT.try_with(|count| count.set(count.get() + 1));
        }
    }
}

// SQLx emits this event when the query future finishes. One process-wide
// observer keeps callsite interest independent of which test first uses it;
// each measured future owns its counter, including when it changes workers.
async fn count_checkpoint_data_statements<F: std::future::Future>(future: F) -> (F::Output, usize) {
    static OBSERVER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    OBSERVER.get_or_init(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(StatementWitness),
        )
        .expect("install checkpoint statement observer");
        tracing::callsite::rebuild_interest_cache();
    });
    STATEMENT_COUNT
        .scope(std::cell::Cell::new(0), async {
            let output = future.await;
            let count = STATEMENT_COUNT.with(std::cell::Cell::get);
            (output, count)
        })
        .await
}

fn checkpoint_with_changed_components(depth: usize) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        components: (0..depth)
            .map(|index| {
                (
                    format!("arbitrary/depth-invariance/{index:05}"),
                    lash_core_execution::HydratedCheckpointComponent::changed(
                        format!("depth-invariance-body-{index:05}").into_bytes(),
                    ),
                )
            })
            .collect(),
        ..Default::default()
    }
}

fn checkpoint_with_unchanged_components(manifest: &SessionCheckpoint) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        turn_state: manifest.turn_state.clone(),
        components: manifest
            .components
            .iter()
            .map(|(key, descriptor)| {
                (
                    key.clone(),
                    lash_core_execution::HydratedCheckpointComponent::unchanged(descriptor),
                )
            })
            .collect(),
    }
}

#[tokio::test]
async fn checkpoint_component_statement_count_is_depth_invariant_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint depth invariance: database URL is not set");
        return;
    };
    let isolated_database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = PostgresStorage::connect(isolated_database.url())
        .await
        .expect("connect checkpoint depth-invariance storage");
    let mut observed = Vec::new();
    for depth in [10, 100, 1_000, 4_000] {
        let mut tx = storage.pool().begin().await.expect("begin checkpoint test");
        let (_, seed_manifest) = support::put_checkpoint_tx(
            &mut tx,
            &checkpoint_with_changed_components(depth),
            lash_core_execution::FleetFormat::current(),
        )
        .await
        .expect("seed checkpoint component bodies");
        let unchanged = checkpoint_with_unchanged_components(&seed_manifest);

        let commit_started = std::time::Instant::now();
        let (committed, commit_statements) =
            count_checkpoint_data_statements(support::put_checkpoint_tx(
                &mut tx,
                &unchanged,
                lash_core_execution::FleetFormat::current(),
            ))
            .await;
        let commit_elapsed = commit_started.elapsed();
        let (checkpoint_ref, _) = committed.expect("commit unchanged checkpoint refs");

        let load_started = std::time::Instant::now();
        let (loaded, load_statements) =
            count_checkpoint_data_statements(support::get_checkpoint_tx(
                &mut tx,
                &checkpoint_ref,
                lash_core_execution::FleetFormat::current(),
            ))
            .await;
        let load_elapsed = load_started.elapsed();
        let loaded = loaded
            .expect("load checkpoint component bodies")
            .expect("stored checkpoint root");

        assert_eq!(loaded.components.len(), depth);
        observed.push((depth, commit_statements, load_statements));
        eprintln!(
            "postgres checkpoint depth={depth} commit_statements={commit_statements} load_statements={load_statements} commit_ms={:.3} load_ms={:.3}",
            commit_elapsed.as_secs_f64() * 1_000.0,
            load_elapsed.as_secs_f64() * 1_000.0,
        );
        tx.rollback().await.expect("rollback checkpoint test rows");
    }
    storage.pool().close().await;
    drop(isolated_database);
    assert!(
        observed
            .iter()
            .all(|(_, commit, load)| (*commit, *load) == (3, 2)),
        "checkpoint commit/load statement counts must be independent of component depth: {observed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_statement_measurements_do_not_reset_each_other() {
    let Some(database_url) = postgres_test_support::database_url() else {
        return;
    };
    let database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database.url())
        .await
        .expect("connect statement witness");
    let mut outer_connection = pool
        .acquire()
        .await
        .expect("acquire outer measured connection");
    let mut inner_connection = pool
        .acquire()
        .await
        .expect("acquire inner measured connection");
    let (_, outer_count) = count_checkpoint_data_statements(async {
        sqlx::query("SELECT $1::integer")
            .bind(1_i32)
            .execute(&mut *outer_connection)
            .await
            .expect("execute outer statement");
        let inner_count = tokio::spawn(async move {
            let (_, count) = count_checkpoint_data_statements(async {
                sqlx::query("SELECT $1::bigint")
                    .bind(2_i64)
                    .execute(&mut *inner_connection)
                    .await
                    .expect("execute first inner statement");
                sqlx::query("SELECT $1::text")
                    .bind("three")
                    .execute(&mut *inner_connection)
                    .await
                    .expect("execute second inner statement");
            })
            .await;
            count
        })
        .await
        .expect("join independent measurement");
        assert_eq!(inner_count, 2, "inner witness lost a statement");
    })
    .await;
    assert_eq!(
        outer_count, 1,
        "independent measurement changed the outer witness"
    );
    drop(outer_connection);
    pool.close().await;
}
