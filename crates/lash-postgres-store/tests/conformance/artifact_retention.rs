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

lash_conformance::artifact_referrer_tests!(@catalogue {
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
                packages: Arc::new(open_storage.lash_vm_artifact_store()),
                artifacts: Arc::new(open_storage.lash_vm_artifact_store())
                    as Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
                process_env: Arc::new(open_storage.process_env_store()) as Arc<dyn ProcessEnvStore>,
                turn_preludes: Arc::new(open_storage.process_env_store())
                    as Arc<dyn lash_execution_contracts::turn_prelude::TurnPreludeStore>,
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
                        packages: Arc::new(reopened.lash_vm_artifact_store()),
                        artifacts: Arc::new(reopened.lash_vm_artifact_store())
                            as Arc<
                                dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore,
                            >,
                        process_env: Arc::new(reopened.process_env_store())
                            as Arc<dyn ProcessEnvStore>,
                        turn_preludes: Arc::new(reopened.process_env_store())
                            as Arc<dyn lash_execution_contracts::turn_prelude::TurnPreludeStore>,
                    }
                }),
            }
        })
    })
}; [
            (publication_racing_frame_end_is_fenced, "artifact-referrer-publication-race"),
            (host_pins_reclaim_and_fence, "artifact-referrer-host-pins"),
            (process_env_carry_retains_exact_origin, "process-env-origin-retention"),
            (attachment_only_referrers_cannot_acquire_artifacts, "artifact-referrer-kind-refusal"),
            (captured_environments_are_shared_until_the_last_referrer_ends, "captured-environment-referrers"),
            (process_env_spellings_read_back_exactly, "process-env-exact-spellings"),
            (environment_holders_answer_package_authority_as_direct_holders, "process-env-package-authority"),
            (environment_retention_survives_a_cut_at_every_step, "process-env-crash-cuts"),
            (module_share_outlives_environment_share_in_either_order, "module-share-end-orders"),
            (module_share_survives_a_cut_between_its_ends, "module-share-crash-cuts"),
            (carried_environments_refuse_in_the_callers_order, "process-env-carry-refusal-order"),

            (retry_idempotency_after_destination_ends, "artifact-referrer-retry-idempotency"),
        ]);
