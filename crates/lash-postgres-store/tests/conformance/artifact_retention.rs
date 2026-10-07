//! Tool material and artifact dependency retention laws.
use super::*;

lash_conformance::tool_material_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres tool-material conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let database_url = database_fixture.url().to_owned();
    (database_fixture, move || {
        let storage = Arc::clone(&storage);
        let database_url = database_url.clone();
        sync_await(async move {
            reset(storage.pool()).await;
            let open = lash_postgres_store::testing::connect(&database_url)
                .await
                .expect("open first Postgres tool-material pool");
            let reopen_url = database_url.clone();
            lash_conformance::material_retention::ReopenableToolMaterialStore {
                open: Arc::new(open.tool_material_store())
                    as Arc<dyn lash_core::store::ToolMaterialStore>,
                reopen: Arc::new(move || {
                    let reopen_url = reopen_url.clone();
                    let reopened = sync_await(async move {
                        lash_postgres_store::testing::connect(&reopen_url)
                            .await
                            .expect("reopen Postgres tool-material pool")
                    });
                    Arc::new(reopened.tool_material_store())
                        as Arc<dyn lash_core::store::ToolMaterialStore>
                }),
            }
        })
    })
});

lash_conformance::artifact_referrer_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres artifact-referrer conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let storage = Arc::new(storage);
    let database_url = database_fixture.url().to_owned();
    (database_fixture, move || {
        let storage = Arc::clone(&storage);
        let database_url = database_url.clone();
        sync_await(async move {
            reset(storage.pool()).await;
            let open_storage = lash_postgres_store::testing::connect(&database_url)
                .await
                .expect("open first Postgres artifact pool");
            let open = lash_conformance::fused_artifact_store::ArtifactStoreHandles {
                artifacts: Arc::new(open_storage.lashlang_artifact_store())
                    as Arc<dyn lash_core::ModuleArtifactStore>,
                process_env: Arc::new(open_storage.process_env_store())
                    as Arc<dyn ProcessExecutionEnvStore>,
                turn_preludes: Arc::new(open_storage.process_env_store())
                    as Arc<dyn lash_core::TurnPreludeStore>,
            };
            let reopen_url = database_url.clone();
            lash_conformance::fused_artifact_store::ReopenableArtifactStore {
                open,
                reopen: Arc::new(move || {
                    let reopen_url = reopen_url.clone();
                    let reopened = sync_await(async move {
                        lash_postgres_store::testing::connect(&reopen_url)
                            .await
                            .expect("construct post-write Postgres artifact pool")
                    });
                    lash_conformance::fused_artifact_store::ArtifactStoreHandles {
                        artifacts: Arc::new(reopened.lashlang_artifact_store())
                            as Arc<dyn lash_core::ModuleArtifactStore>,
                        process_env: Arc::new(reopened.process_env_store())
                            as Arc<dyn ProcessExecutionEnvStore>,
                        turn_preludes: Arc::new(reopened.process_env_store())
                            as Arc<dyn lash_core::TurnPreludeStore>,
                    }
                }),
            }
        })
    })
});
