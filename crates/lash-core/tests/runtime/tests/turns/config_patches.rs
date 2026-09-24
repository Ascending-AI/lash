use super::*;

#[tokio::test]
pub(super) async fn queued_config_patches_coalesce_into_one_head_commit() {
    let backend = memory_backend().await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, mock_provider(Vec::new())).await;
    let models = ["queued-model-a", "queued-model-b", "queued-model-c"];
    for model in models {
        enqueue_config_patch_command(
            store.as_ref(),
            &SessionId::from("root"),
            lash_core::runtime::ApplyConfigPatch {
                model: Some(
                    lash_core::ModelSpec::builder(model)
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..lash_core::runtime::ApplyConfigPatch::default()
            },
        )
        .await;
    }
    let commits_before = *store.runtime_commit_count.lock_recover();
    let owner = lease_owner("config-patch-coalescing");
    let lease = lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        &SessionId::from("root"),
        &owner,
        "config-patch-coalescing-executor",
        lash_core::facade_support::LeaseTimings::default().ttl_ms(),
    )
    .await
    .expect("claim session execution lease")
    .acquired()
    .expect("session execution lease");

    runtime
        .drain_next_session_command(&lease.fence())
        .await
        .expect("drain coalesced config patches")
        .expect("one receipt from the coalesced claim");

    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        commits_before + 1,
        "N config commands must share exactly one head commit"
    );
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("list settled config commands")
        .is_empty(),
        "every independently accepted command must settle its own batch"
    );
    assert_eq!(runtime.session_policy().model.id, "queued-model-c");
}

#[tokio::test]
pub(super) async fn config_settlement_distinguishes_enqueue_rejection_from_durable_completion() {
    let backend = memory_backend().await;
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let original_model = runtime.session_policy().model.clone();
    let outcome = runtime
        .submit_apply_config_patch_with_idempotency_key(
            lash_core::runtime::ApplyConfigPatch {
                model: Some(
                    lash_core::ModelSpec::builder("must-not-publish")
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..lash_core::runtime::ApplyConfigPatch::default()
            },
            "",
        )
        .await
        .expect("typed submission outcome");

    let lash_core::runtime::SessionCommandSettlement::Rejected(rejection) = outcome else {
        panic!("empty idempotency key must be rejected before durable acceptance");
    };
    assert_eq!(
        rejection.code,
        lash_core::RuntimeErrorCode::SessionCommandIdempotencyKey
    );
    assert_eq!(runtime.session_policy().model, original_model);

    let durable = runtime
        .submit_apply_config_patch_with_idempotency_key(
            lash_core::runtime::ApplyConfigPatch {
                model: Some(
                    lash_core::ModelSpec::builder("durable-inline")
                        .context_window_tokens(32_000)
                        .build()
                        .expect("model"),
                ),
                ..lash_core::runtime::ApplyConfigPatch::default()
            },
            "durable-inline",
        )
        .await
        .expect("durable settlement");
    assert!(matches!(
        durable,
        lash_core::runtime::SessionCommandSettlement::Durable(_)
    ));
    assert_eq!(runtime.session_policy().model.id, "durable-inline");
}

pub(super) fn turn_budget_config_mutator(
    turn_budget: lash_core::TurnBudget,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: Some(Arc::new(move |reg| {
                    reg.session()
                        .config_mutator(Arc::new(move |_ctx, mut policy| {
                            Box::pin(async move {
                                policy.turn_budget = turn_budget;
                                Ok(policy)
                            })
                        }));
                    Ok(())
                })),
            }))
        }),
    })
}

#[tokio::test]
pub(super) async fn plugin_turn_budget_mutation_survives_park_and_reload() {
    let backend = memory_backend().await;
    let store = unbound_recording_store(&backend).await;
    let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store.clone();
    let persisted_budget = lash_core::TurnBudget::bounded(7);
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        vec![turn_budget_config_mutator(persisted_budget)],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        Arc::clone(&runtime_store),
    )
    .await;

    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            model: Some(
                lash_core::ModelSpec::builder("turn-budget-mutation-trigger")
                    .context_window_tokens(32_000)
                    .build()
                    .expect("model"),
            ),
            ..lash_core::facade_support::SessionConfigPatch::default()
        })
        .await
        .expect("plugin turn-budget mutation settles");
    assert_eq!(runtime.session_policy().turn_budget, persisted_budget);
    drop(
        Box::pin(runtime.park())
            .await
            .expect("park mutated session"),
    );

    let reloaded_state =
        lash_core::testing::runtime_internals::load_persisted_session_state(runtime_store.as_ref())
            .await
            .expect("load parked session")
            .expect("parked session exists");
    let plugin_host =
        lash_core::testing::test_plugin_host(vec![turn_budget_config_mutator(persisted_budget)]);
    let plugins = match reloaded_state.plugin_state() {
        Some(snapshot) => plugin_host.rematerialize_session(
            "root",
            snapshot,
            lash_core::plugin::RecordedSessionConfig::new(
                reloaded_state.protocol_turn_options.clone(),
            ),
        ),
        None => plugin_host.build_session("root"),
    }
    .expect("reloaded plugins");
    let runtime_host = test_host_config(&backend);
    let runtime_services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugins,
        runtime_store,
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let reloaded = lash_core::facade_support::LashRuntime::from_persistent_embedded_state(
        standard_test_policy(),
        runtime_host,
        runtime_services,
        reloaded_state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("reload parked runtime");
    assert_eq!(
        reloaded.session_policy().turn_budget,
        persisted_budget,
        "plugin-mutated durable budget must survive cold reload"
    );
}

#[tokio::test]
pub(super) async fn every_session_config_patch_emits_a_lifecycle_event() {
    let backend = memory_backend().await;
    let observed = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let observed_hook = Arc::clone(&observed);
    let plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let observed = Arc::clone(&observed_hook);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: None,
                presentation_steps: vec![],
                runtime_event: Some(Arc::new(move |event| {
                    let observed = Arc::clone(&observed);
                    Box::pin(async move {
                        if let lash_core::plugin::PluginLifecycleEvent::SessionConfigChanged(ctx) =
                            event
                        {
                            observed.lock().await.push((ctx.previous, ctx.current));
                        }
                        Ok(())
                    })
                })),
                external_registrar: None,
            }))
        }),
    });
    let transport = mock_provider(Vec::new());
    let mut runtime = runtime_with_plugins(&backend, vec![plugin], transport).await;

    let alt_provider = TestProvider::builder()
        .kind("alt")
        .complete_error("alt provider not wired")
        .build()
        .into_handle();
    let alt_model = lash_core::ModelSpec::builder("alt-model")
        .context_window_tokens(123_456)
        .build()
        .expect("valid model spec");
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            model: Some(alt_model.clone()),
            ..Default::default()
        })
        .await
        .expect("update model config");
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider: Some(alt_provider),
            ..Default::default()
        })
        .await
        .expect("update provider config");

    assert_eq!(observed.lock().await.len(), 2);

    let combined_provider = TestProvider::builder()
        .kind("combined")
        .complete_error("combined provider not wired")
        .build()
        .into_handle();
    let combined_model = lash_core::ModelSpec::builder("combined-model")
        .context_window_tokens(234_567)
        .build()
        .expect("valid combined model spec");
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider: Some(combined_provider),
            model: Some(combined_model.clone()),
            ..Default::default()
        })
        .await
        .expect("update combined config");

    assert_eq!(observed.lock().await.len(), 3);

    let prompt = lash_core::PromptLayer::new().with_contribution(
        lash_core::PromptContribution::guidance("Patch", "prompt-only session config"),
    );
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch::with_prompt(
            prompt.clone(),
        ))
        .await
        .expect("update prompt config");

    assert_eq!(observed.lock().await.len(), 4);

    let generation = lash_core::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                generation.clone(),
            )),
            ..Default::default()
        })
        .await
        .expect("update generation config");

    assert_eq!(observed.lock().await.len(), 5);

    let helper_template =
        lash_core::PromptTemplate::new(vec![lash_core::PromptTemplateSection::untitled(vec![
            lash_core::PromptTemplateEntry::text("prompt helper template"),
        ])]);
    runtime
        .set_prompt_template(helper_template.clone())
        .await
        .expect("set prompt template");

    let changes = observed.lock().await;
    assert_eq!(changes.len(), 6);
    let (previous, current) = &changes[0];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(current.provider_id, "mock");
    assert_eq!(current.model.id, "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(previous.provider_id, "mock");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "alt");
    assert_eq!(current.model.id, "alt-model");
    let (previous, current) = &changes[2];
    assert_eq!(previous.provider_id, "alt");
    assert_eq!(previous.model.id, "alt-model");
    assert_eq!(current.provider_id, "combined");
    assert_eq!(current.model, combined_model);
    let (previous, current) = &changes[3];
    assert_eq!(previous.model.id, "combined-model");
    assert_eq!(current.prompt, prompt);
    let (previous, current) = &changes[4];
    assert_eq!(previous.prompt, prompt);
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[5];
    assert_eq!(previous.generation, generation);
    assert_eq!(
        current.prompt.template,
        Some(helper_template),
        "prompt helper changes emit SessionConfigChanged"
    );
}
