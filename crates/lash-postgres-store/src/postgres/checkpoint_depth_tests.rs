use crate::*;
use sqlx::postgres::PgPoolOptions;

fn checkpoint_with_changed_components(depth: usize) -> HydratedSessionCheckpoint {
    HydratedSessionCheckpoint {
        components: (0..depth)
            .map(|index| {
                (
                    format!("arbitrary/depth-invariance/{index:05}"),
                    lash_core::HydratedCheckpointComponent::changed(
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
                    lash_core::HydratedCheckpointComponent::unchanged(descriptor),
                )
            })
            .collect(),
        plugin_snapshot_revision: manifest.plugin_snapshot_revision,
    }
}

#[tokio::test]
async fn checkpoint_component_statement_count_is_depth_invariant_when_configured() {
    let Some(database_url) = postgres_test_support::database_url() else {
        eprintln!("skipping Postgres checkpoint depth invariance: database URL is not set");
        return;
    };
    let _database_lock = postgres_test_support::SharedDatabaseLock::acquire(&database_url).await;
    let statement_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect PostgreSQL statement-statistics pool");
    sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .execute(&statement_pool)
        .await
        .expect("enable pg_stat_statements for checkpoint depth invariance");
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect checkpoint depth-invariance storage");
    let mut observed = Vec::new();
    for depth in [10, 100, 1_000, 4_000] {
        let mut tx = storage.pool().begin().await.expect("begin checkpoint test");
        let (_, seed_manifest) =
            support::put_checkpoint_tx(&mut tx, &checkpoint_with_changed_components(depth))
                .await
                .expect("seed checkpoint component bodies");
        let unchanged = checkpoint_with_unchanged_components(&seed_manifest);

        let commit_started = std::time::Instant::now();
        let (committed, commit_statements) = support::count_checkpoint_data_statements(
            &statement_pool,
            support::put_checkpoint_tx(&mut tx, &unchanged),
        )
        .await;
        let commit_elapsed = commit_started.elapsed();
        let (checkpoint_ref, _) = committed.expect("commit unchanged checkpoint refs");

        let load_started = std::time::Instant::now();
        let (loaded, load_statements) = support::count_checkpoint_data_statements(
            &statement_pool,
            support::get_checkpoint_tx(&mut tx, &checkpoint_ref),
        )
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
    assert!(
        observed
            .iter()
            .all(|(_, commit, load)| { *commit == observed[0].1 && *load == observed[0].2 }),
        "checkpoint commit/load statement counts must be independent of component depth: {observed:?}"
    );
}
