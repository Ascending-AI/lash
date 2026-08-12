// =============================================================================
// Explicit runtime dependency wiring
// =============================================================================
//
/// A standard-mode builder with a model + provider already named, ready for the
/// explicit dependency wiring under test.
fn peer_coherence_builder() -> crate::core::LashCoreBuilder {
    LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .provider(mock_provider())
        .model(mock_model_spec())
}

#[test]
fn commit_budget_is_required_for_builder_construction_and_deserialization() {
    let error = expect_build_error(
        LashCore::standard_builder(crate::TurnBudget::Unbounded)
            .provider(mock_provider())
            .model(mock_model_spec())
            .effect_host(Arc::new(
                lash_core::facade_support::InlineEffectHost::default(),
            ))
            .attachment_store(Arc::new(
                lash_core::facade_support::InMemoryAttachmentStore::new(),
            ))
            .process_env_store(Arc::new(
                lash_core::facade_support::InMemoryProcessExecutionEnvStore::new(),
            ))
            .build(crate::testing::runtime_lease_owner()),
        "builder must reject a missing commit budget",
    );
    assert!(matches!(error, EmbedError::MissingCommitBudget));

    let error = serde_json::from_value::<crate::CommitBudget>(serde_json::json!({
        "bytes": { "bounded": 1_048_576 }
    }))
    .expect_err("serialized host commit budget must include the node limit");
    assert!(error.to_string().contains("nodes"), "{error}");
}

#[test]
fn queued_work_action_reserve_is_required() {
    let error = expect_build_error(
        LashCore::standard_builder(crate::TurnBudget::Unbounded)
            .provider(mock_provider())
            .model(mock_model_spec())
            .effect_host(Arc::new(
                lash_core::facade_support::InlineEffectHost::default(),
            ))
            .attachment_store(Arc::new(
                lash_core::facade_support::InMemoryAttachmentStore::new(),
            ))
            .process_env_store(Arc::new(
                lash_core::facade_support::InMemoryProcessExecutionEnvStore::new(),
            ))
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .build(),
        "builder must reject a missing queued-work action reserve",
    );
    assert!(matches!(error, EmbedError::MissingQueuedWorkBatching));
}

fn durable_session_store_factory(dir: &std::path::Path) -> Arc<dyn lash_core::SessionStoreFactory> {
    Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.join("sessions"),
    ))
}

fn durable_attachment_store(dir: &std::path::Path) -> Arc<dyn lash_core::AttachmentStore> {
    Arc::new(crate::persistence::FileAttachmentStore::new(
        dir.join("attachments"),
    ))
}

/// `LashCore` is not `Debug`, so `Result::expect_err` is unavailable; this
/// extracts the build error or panics with the given message.
fn expect_build_error<T>(result: std::result::Result<T, EmbedError>, message: &str) -> EmbedError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(err) => err,
    }
}

async fn durable_process_env_store(
    dir: &std::path::Path,
) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
    Arc::new(
        lash_sqlite_store::Store::open(&dir.join("process-env.db"))
            .await
            .expect("open durable process env store"),
    )
}

async fn durable_trigger_store(dir: &std::path::Path) -> Arc<dyn lash_core::TriggerStore> {
    Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&dir.join("triggers.db"))
            .await
            .expect("open durable trigger store"),
    )
}

#[tokio::test]
async fn builder_rebinds_first_party_process_registry_to_runtime_clock() {
    const NOW_MS: u64 = 4_200_000;
    let clock = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let registry = lash_sqlite_store::SqliteProcessRegistry::memory()
        .await
        .expect("open SQLite process registry with its default clock");
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::with_clock(
        clock.clone(),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .model(
            lash_core::ModelSpec::builder("clock-wiring-model")
                .context_window_tokens(4_096)
                .build()
            .expect("valid test model"),
        )
        .store_factory(store_factory)
        .process_registry(Arc::new(registry))
        .advanced()
        .runtime_host_config(lash_core::facade_support::RuntimeHostConfig::in_memory(lash_core::CommitBudget::bounded(1024 * 1024, 512), lash_core::QueuedWorkBatchingConfig::new(1)).with_clock(clock))
        .build(crate::testing::runtime_lease_owner())
        .expect("build core with SQLite process registry");
    let registry = core.process_registry().expect("built process registry");
    let delivery_expiry_ms = registry.wake_delivery_config().delivery_expiry_ms;
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "builder-clock-process",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "builder.clock.wake".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec {
                    wake: Some(lash_core::ProcessWakeSpec {
                        when: None,
                        input: lash_core::ProcessValueSelector::Pointer("/wake_input".to_string()),
                    }),
                    ..lash_core::ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some("builder-clock-target".to_string())),
        )
        .await
        .expect("register clock-wiring process");
    registry
        .append_event(
            "builder-clock-process",
            lash_core::ProcessEventAppendRequest::new(
                "builder.clock.wake",
                serde_json::json!({"wake_input": "wake"}),
            ),
        )
        .await
        .expect("append clock-wiring wake");
    let delivery = registry
        .claim_pending_wake_deliveries(1)
        .await
        .expect("scan clock-wiring wake")
        .into_iter()
        .next()
        .expect("clock-wiring delivery");

    assert_eq!(delivery.wake.created_at_ms, NOW_MS);
    assert_eq!(delivery.expires_at_ms, NOW_MS + delivery_expiry_ms);
}

#[tokio::test]
async fn builder_requires_explicit_process_env_store_at_build() {
    let result = peer_coherence_builder()
        .effect_host(Arc::new(lash_core::facade_support::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()))
        .build(crate::testing::runtime_lease_owner());
    let err = expect_build_error(
        result,
        "builder must reject missing process execution environment store",
    );

    assert!(matches!(err, EmbedError::MissingProcessEnvStore));
}

#[tokio::test]
async fn all_durable_stores_build_successfully() -> Result<()> {
    // Positive control: a coherent standard-mode durable wiring (durable
    // session store + durable attachment + durable process environment +
    // durable process registry + durable trigger store) builds without error.
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open durable registry"),
    );
    peer_coherence_builder()
        .effect_host(Arc::new(lash_core::facade_support::InlineEffectHost::default()))
        .store_factory(durable_session_store_factory(dir.path()))
        .attachment_store(durable_attachment_store(dir.path()))
        .process_env_store(durable_process_env_store(dir.path()).await)
        .trigger_store(durable_trigger_store(dir.path()).await)
        .process_registry(registry)
        .build(crate::testing::runtime_lease_owner())?;
    Ok(())
}

#[tokio::test]
async fn durable_registry_with_only_child_store_factory_builds() -> Result<()> {
    // The CLI can wire a child store factory without a root store factory.
    // The process work runner must resolve the same effective factory.
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open durable registry"),
    );
    peer_coherence_builder()
        .effect_host(Arc::new(lash_core::facade_support::InlineEffectHost::default()))
        .child_store_factory(durable_session_store_factory(dir.path()))
        .attachment_store(durable_attachment_store(dir.path()))
        .process_env_store(durable_process_env_store(dir.path()).await)
        .trigger_store(durable_trigger_store(dir.path()).await)
        .process_registry(registry)
        .build(crate::testing::runtime_lease_owner())?;
    Ok(())
}

#[tokio::test]
async fn explicit_ephemeral_facets_build_successfully() -> Result<()> {
    // An all-in-memory build succeeds, including the explicit session store
    // factory that backs process execution.
    explicit_ephemeral_facets(peer_coherence_builder())
        .store_factory(Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    Ok(())
}

struct NoopProcessRunHandle;

#[async_trait]
impl lash_core::facade_support::ProcessRunHandle for NoopProcessRunHandle {
    async fn claim_and_run_pending(&self) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

#[tokio::test]
async fn process_work_driver_configures_external_runner_without_inline_store_factory() -> Result<()>
{
    let registry =
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn lash_core::ProcessRegistry>;
    let driver =
        lash_core::facade_support::ProcessWorkDriver::new(Arc::clone(&registry), Arc::new(NoopProcessRunHandle));
    let driver_registry = driver.process_registry();
    let core = explicit_ephemeral_facets(peer_coherence_builder())
        .process_work_driver(driver)
        .build(crate::testing::runtime_lease_owner())?;

    let configured = core
        .process_registry()
        .expect("external driver configures the core registry");
    assert!(Arc::ptr_eq(&configured, &driver_registry));
    assert!(core.processes().observer().is_ok());
    assert!(core.work_driver.drivers().await.process.is_some());
    Ok(())
}

#[tokio::test]
async fn default_process_work_driver_resolves_when_registry_and_store_factory_present() -> Result<()>
{
    // Zero-ceremony path: a registry + a store factory (so the inline worker can
    // rebuild session runtimes) and no explicit driver constructs the default
    // inline process work driver on first `session().open()`. The driver's actual
    // lease-protected execution of out-of-turn processes is covered in lash-core
    // (`concurrent_workers_run_a_directly_registered_process_exactly_once`).
    let state = RuntimeSessionState {
        session_id: "main".to_string(),
        policy: lash_core::SessionPolicy {
            provider_id: mock_provider().kind().to_string(),
            model: mock_model_spec(),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(peer_coherence_builder())
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    core.session("main").open().await?;
    assert!(
        core.work_driver.drivers().await.process.is_some(),
        "the default inline process driver must resolve when a registry + store factory are wired"
    );
    Ok(())
}

#[tokio::test]
async fn durable_process_worker_config_uses_core_process_registry() -> Result<()> {
    let registry =
        Arc::new(TestLocalProcessRegistry::default()) as Arc<dyn lash_core::ProcessRegistry>;
    let trigger_store =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default()) as Arc<dyn lash_core::TriggerStore>;
    let core_owner = lash_core::LeaseOwnerIdentity::opaque(
        "durable-worker-facade-owner",
        "durable-worker-facade-boot",
    );
    let core = explicit_ephemeral_facets(peer_coherence_builder())
        .store_factory(Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()))
        .trigger_store(Arc::clone(&trigger_store))
        .process_registry(Arc::clone(&registry))
        .build(core_owner)?;

    assert!(core.processes().observer().is_ok());
    let config = core.durable_process_worker_config()?;
    let core_registry = core
        .process_registry()
        .expect("process registry must be configured");
    assert!(Arc::ptr_eq(&config.process_registry, &core_registry));
    assert!(Arc::ptr_eq(&config.trigger_store, &trigger_store));
    assert_eq!(config.lease_owner.owner_id, "durable-worker-facade-owner");
    assert_eq!(
        config.lease_owner.incarnation_id,
        "durable-worker-facade-boot"
    );
    assert_eq!(
        config.process_execution_concurrency(),
        lash_core::facade_support::DEFAULT_PROCESS_EXECUTION_CONCURRENCY
    );
    Ok(())
}

#[tokio::test]
async fn fork_distinguishes_collected_point_from_retained_orphaned_source() -> Result<()> {
    use lash_core::SessionStoreFactory as _;

    let factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .build(crate::testing::runtime_lease_owner())?;

    let collected_error = core
        .fork_at("collected-fork-point", "collected-fork-branch")
        .await
        .expect_err("a collected point must remain classified as not retained");
    assert!(matches!(
        collected_error,
        EmbedError::Store(lash_core::StoreError::ForkPointNotRetained { node_id })
            if node_id == "collected-fork-point"
    ));

    let mut source_model = mock_model_spec();
    source_model.id = "orphaned-source-model".to_string();
    let source_policy = lash_core::SessionPolicy {
        provider_id: "orphaned-source-provider".to_string(),
        model: source_model,
        session_id: Some("orphaned-fork-source".to_string()),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let source_request = lash_core::SessionStoreCreateRequest {
        session_id: "orphaned-fork-source".to_string(),
        relation: lash_core::SessionRelation::Root,
        policy: source_policy.clone(),
    };
    let source = factory
        .create_store(&source_request)
        .await
        .expect("create source that will be deleted");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: source_request.session_id.clone(),
        policy: source_policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    source_state.ensure_agent_frame_initialized();
    source
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
            &[],
        ))
        .await
        .expect("commit orphaned source frame");
    let retained_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("orphaned source leaf");
    core.pin(&retained_node_id).await?;
    factory
        .delete_session(&source_request.session_id)
        .await
        .expect("delete pinned source session");

    let forked = core
        .fork_at(&retained_node_id, "orphaned-fork-branch")
        .await
        .expect("retained graph frame must resolve policy after source deletion");
    assert_eq!(forked.node_id, retained_node_id);
    assert_eq!(
        forked.source_session_id, source_request.session_id,
        "a successful orphaned-pin fork preserves deleted-source provenance"
    );
    let branch = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            session_id: "orphaned-fork-branch".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("open orphaned-source fork")
        .expect("orphaned-source fork exists");
    let branch_config = branch
        .load_session()
        .await?
        .expect("orphaned-source fork head")
        .config;
    assert_eq!(
        branch_config.provider_id, "orphaned-source-provider",
        "the retained frame carries provider identity after source deletion"
    );
    assert_eq!(
        branch_config.model.id, "orphaned-source-model",
        "the retained frame carries model identity after source deletion"
    );
    Ok(())
}

#[tokio::test]
async fn fork_observer_inheritance_is_recoverable_selective_and_wake_independent() -> Result<()> {
    use lash_core::{ProcessRegistry as _, SessionStoreFactory as _};

    let factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build(crate::testing::runtime_lease_owner())?;
    let mut source_model = mock_model_spec();
    source_model.id = "fork-source-model".to_string();
    let policy = lash_core::SessionPolicy {
        provider_id: "fork-source-provider".to_string(),
        model: source_model,
        session_id: Some("fork-observer-source".to_string()),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let source_store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "fork-observer-source".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("create fork observer source");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: "fork-observer-source".to_string(),
        policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    source_state.ensure_agent_frame_initialized();
    source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
            &[],
        ))
        .await
        .expect("commit fork observer source");
    let fork_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("fork observer source leaf");
    core.pin(&fork_node_id).await?;

    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "fork-visible-process",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "fork.wake".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec {
                    wake: Some(lash_core::ProcessWakeSpec {
                        when: Some(lash_core::ProcessValueSelector::Present(
                            "/wake_input".to_string(),
                        )),
                        input: lash_core::ProcessValueSelector::Pointer(
                            "/wake_input".to_string(),
                        ),
                    }),
                    ..lash_core::ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some("fork-observer-source".to_string())),
        )
        .await
        .expect("register fork-visible process");
    registry
        .add_observer(
            "fork-observer-source",
            "fork-visible-process",
            lash_core::ProcessObserverBy::host("fork-test-source"),
        )
        .await
        .expect("observe source process");

    core.fork_at(&fork_node_id, "fork-observer-branch").await?;
    let branch_store = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            session_id: "fork-observer-branch".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("open branch store")
        .expect("branch store exists");
    let branch_read = branch_store
        .load_session()
        .await
        .expect("load branch config")
        .expect("branch head exists");
    assert_eq!(branch_read.config.provider_id, "fork-source-provider");
    assert_eq!(branch_read.config.model.id, "fork-source-model");

    let inherited = registry
        .list_observed_by("fork-observer-branch")
        .await
        .expect("list inherited observations");
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].id, "fork-visible-process");

    registry
        .set_process_read_error(Some(lash_core::PluginError::Session(
            "transient fork observer registry failure".to_string(),
        )))
        .await;
    core.fork_at(&fork_node_id, "fork-transient-branch")
        .await
        .expect("transient observer registry failure must not fail fork_at");
    registry.set_process_read_error(None).await;
    assert!(
        registry
            .list_observed_by("fork-transient-branch")
            .await
            .expect("list transient-failure branch observations")
            .is_empty(),
        "a transiently unavailable process must not gain a fork observer edge"
    );
    let transient_branch_store = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            session_id: "fork-transient-branch".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("open transient-failure branch store")
        .expect("transient-failure branch store exists");
    let transient_meta = transient_branch_store
        .load_session_meta()
        .await
        .expect("load transient-failure fork metadata")
        .expect("transient-failure fork metadata exists");
    assert!(
        matches!(
            transient_meta.relation,
            lash_core::SessionRelation::Fork {
                ref pending_observer_process_ids,
                ..
            } if pending_observer_process_ids.is_empty()
        ),
        "fork_at must consume transiently unavailable observer intents"
    );

    let published_meta = branch_store
        .load_session_meta()
        .await
        .expect("load published fork metadata")
        .expect("published fork metadata exists");
    let lash_core::SessionRelation::Fork {
        pending_observer_process_ids,
        ..
    } = &published_meta.relation
    else {
        panic!("branch metadata must retain its fork relation");
    };
    assert!(
        pending_observer_process_ids.is_empty(),
        "successfully published observers must be consumed from the recovery intent"
    );

    let mut recovery_meta = published_meta;
    let lash_core::SessionRelation::Fork {
        pending_observer_process_ids,
        ..
    } = &mut recovery_meta.relation
    else {
        unreachable!("relation was checked above");
    };
    pending_observer_process_ids.push("fork-visible-process".to_string());
    registry
        .register_process(lash_core::ProcessRegistration::new(
            "fork-pruned-process",
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryDisposition::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .expect("register process that will be pruned during fork publication");
    let pruned_terminal = registry
        .complete_process(
            "fork-pruned-process",
            lash_core::ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete inherited process before recovery");
    registry
        .prune_terminal_processes(
            pruned_terminal.updated_at_ms.saturating_add(1),
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune inherited process before recovery");
    pending_observer_process_ids.push("fork-pruned-process".to_string());
    branch_store
        .save_session_meta(recovery_meta)
        .await
        .expect("simulate a crash before observer intent consumption");
    registry
        .remove_observer(
            "fork-observer-branch",
            "fork-visible-process",
            lash_core::ProcessObserverBy::host("fork-test-crash"),
        )
        .await
        .expect("remove the partially published observer");
    core.session("fork-observer-branch")
        .open_with_state(lash_core::RuntimeSessionState {
            session_id: "fork-observer-branch".to_string(),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded))
        })
        .await?;
    assert_eq!(
        registry
            .list_observed_by("fork-observer-branch")
            .await
            .expect("list recovered fork observations")
            .len(),
        1,
        "opening a durable fork must reconcile an unconsumed observer intent"
    );
    let recovered_meta = branch_store
        .load_session_meta()
        .await
        .expect("load recovered fork metadata")
        .expect("recovered fork metadata exists");
    let lash_core::SessionRelation::Fork {
        pending_observer_process_ids,
        ..
    } = &recovered_meta.relation
    else {
        unreachable!("relation was checked above");
    };
    assert!(
        pending_observer_process_ids.is_empty(),
        "recovery must consume the intent after idempotent publication"
    );
    registry
        .remove_observer(
            "fork-observer-branch",
            "fork-visible-process",
            lash_core::ProcessObserverBy::host("fork-test-revoke"),
        )
        .await
        .expect("deliberately remove the recovered observer");
    core.session("fork-observer-branch").open().await?;
    assert!(
        registry
            .list_observed_by("fork-observer-branch")
            .await
            .expect("list observations after deliberate removal")
            .is_empty(),
        "a later deliberate observer removal must remain removed"
    );

    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                "fork-selective-process",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            ),
            &["fork-observer-source".to_string()],
        )
        .await
        .expect("register second observed process");
    core.fork_at_with_observer_inheritance(
        &fork_node_id,
        "fork-only-branch",
        lash_core::ObserverInheritance::Only(vec!["fork-selective-process".to_string()]),
    )
    .await?;
    let only = registry
        .list_observed_by("fork-only-branch")
        .await
        .expect("list Only selector result");
    assert_eq!(
        only.iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fork-selective-process"]
    );
    let event_count_before = registry
        .events_after("fork-selective-process", 0)
        .await
        .expect("read observer audit before duplicate apply")
        .len();
    registry
        .add_observer(
            "fork-only-branch",
            "fork-selective-process",
            lash_core::ProcessObserverBy::ForkInheritance,
        )
        .await
        .expect("reapply fork observer");
    assert_eq!(
        registry
            .events_after("fork-selective-process", 0)
            .await
            .expect("read observer audit after duplicate apply")
            .len(),
        event_count_before,
        "double-apply must be an event-log no-op"
    );

    core.fork_at_with_observer_inheritance(
        &fork_node_id,
        "fork-none-branch",
        lash_core::ObserverInheritance::None,
    )
    .await?;
    assert!(
        registry
            .list_observed_by("fork-none-branch")
            .await
            .expect("list None selector result")
            .is_empty()
    );

    registry
        .append_event(
            "fork-visible-process",
            lash_core::ProcessEventAppendRequest::new(
                "fork.wake",
                serde_json::json!({"wake_input": "source-only"}),
            ),
        )
        .await
        .expect("append fork wake event");
    assert!(
        registry
            .list_wake_deliveries(None)
            .await
            .expect("list wake deliveries")
            .iter()
            .any(|delivery| {
                delivery.wake.process_id == "fork-visible-process"
                    && delivery.wake.target_session_id == "fork-observer-source"
            }),
        "observer inheritance must not retarget the source wake subscription"
    );
    Ok(())
}

#[tokio::test]
async fn session_create_observer_intent_replays_idempotently_on_open() -> Result<()> {
    use lash_core::{ProcessRegistry as _, SessionStoreFactory as _};

    let session_id = "session-create-observer-recovery";
    let process_id = "session-create-observed-process";
    let factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let registry = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryDisposition::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
        ))
        .await?;
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: lash_core::SessionRelation::ObserverIntent {
                relation: Box::new(lash_core::SessionRelation::Root),
                pending_observer_process_ids: vec![process_id.to_string()],
            },
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await?;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build(crate::testing::runtime_lease_owner())?;

    assert!(
        !registry.is_observer(session_id, process_id).await?,
        "the fixture must preserve the real crash gap before publication"
    );
    core.session(session_id).open().await?;
    assert!(
        registry.is_observer(session_id, process_id).await?,
        "open must publish the observer edge left pending by a create crash"
    );
    let observer_event_count = registry
        .events_after(process_id, 0)
        .await?
        .into_iter()
        .filter(|event| event.event_type == "process.observer_added")
        .count();
    assert_eq!(
        observer_event_count, 1,
        "recovery must publish the missing observer edge exactly once"
    );
    core.session(session_id).open().await?;
    assert_eq!(
        registry
            .events_after(process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.event_type == "process.observer_added")
            .count(),
        observer_event_count,
        "recovery after edge publication must be idempotent"
    );
    assert!(matches!(
        store
            .load_session_meta()
            .await?
            .expect("session metadata")
            .relation,
        lash_core::SessionRelation::Root
    ));

    registry
        .remove_observer(
            session_id,
            process_id,
            lash_core::ProcessObserverBy::host("post-recovery-removal"),
        )
        .await?;
    core.session(session_id).open().await?;
    assert!(
        !registry.is_observer(session_id, process_id).await?,
        "consumed create intent must not recreate a deliberately removed edge"
    );
    Ok(())
}

#[tokio::test]
async fn nested_session_observer_intents_settle_every_layer_before_open_returns() -> Result<()> {
    use lash_core::{ProcessRegistry as _, SessionStoreFactory as _};

    let factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build(crate::testing::runtime_lease_owner())?;

    for (case, simulate_crash_between_layers) in [("fresh", false), ("crash-resume", true)] {
        let session_id = format!("nested-observer-intent-{case}");
        let create_process_id = format!("nested-create-process-{case}");
        let fork_process_id = format!("nested-fork-process-{case}");
        for process_id in [&create_process_id, &fork_process_id] {
            registry
                .register_process(lash_core::ProcessRegistration::new(
                    process_id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    lash_core::RecoveryDisposition::ExternallyOwned,
                    lash_core::ProcessProvenance::host(),
                ))
                .await?;
        }
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::ObserverIntent {
                    relation: Box::new(lash_core::SessionRelation::Fork {
                        source_session_id: format!("nested-source-{case}"),
                        source_node_id: format!("nested-source-node-{case}"),
                        observer_inheritance: lash_core::ObserverInheritance::All,
                        pending_observer_process_ids: vec![fork_process_id.clone()],
                    }),
                    pending_observer_process_ids: vec![create_process_id.clone()],
                },
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await?;

        if simulate_crash_between_layers {
            registry
                .add_observer(
                    &session_id,
                    &create_process_id,
                    lash_core::ProcessObserverBy::host(format!("session-create:{session_id}")),
                )
                .await
                .expect("simulate outer publication before a crash between layers");
            assert!(
                !registry
                    .is_observer(&session_id, &fork_process_id)
                    .await?,
                "the crash fixture must leave the inner fork layer unpublished"
            );
        }

        core.session(&session_id).open().await?;

        assert!(
            registry
                .is_observer(&session_id, &create_process_id)
                .await?,
            "open must settle the outer session-create observer intent"
        );
        assert!(
            registry
                .is_observer(&session_id, &fork_process_id)
                .await?,
            "open must settle the inner fork observer intent"
        );
        let relation = store
            .load_session_meta()
            .await?
            .expect("nested session metadata")
            .relation;
        assert!(
            matches!(
                relation,
                lash_core::SessionRelation::Fork {
                    ref pending_observer_process_ids,
                    ..
                } if pending_observer_process_ids.is_empty()
            ),
            "open must persist a fully settled base fork relation, got {relation:?}"
        );

        let create_events = registry
            .events_after(&create_process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.event_type == "process.observer_added")
            .collect::<Vec<_>>();
        assert_eq!(create_events.len(), 1);
        assert_eq!(
            create_events[0].payload["by"],
            serde_json::json!({
                "kind": "host",
                "operation_id": format!("session-create:{session_id}")
            })
        );
        let fork_events = registry
            .events_after(&fork_process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.event_type == "process.observer_added")
            .collect::<Vec<_>>();
        assert_eq!(fork_events.len(), 1);
        assert_eq!(
            fork_events[0].payload["by"],
            serde_json::json!({"kind": "fork_inheritance"})
        );
    }
    Ok(())
}

#[test]
fn builder_rejects_invalid_process_execution_concurrency() {
    let err = expect_build_error(
        explicit_ephemeral_facets(peer_coherence_builder())
            .process_execution_concurrency(0)
            .build(crate::testing::runtime_lease_owner()),
        "zero process execution concurrency must be rejected",
    );
    assert!(matches!(err, EmbedError::ProcessExecutionConcurrency(_)));
}

#[test]
fn builder_rejects_invalid_queued_work_execution_concurrency() {
    let err = expect_build_error(
        explicit_ephemeral_facets(peer_coherence_builder())
            .queued_work_execution_concurrency(0)
            .build(crate::testing::runtime_lease_owner()),
        "zero queued-work execution concurrency must be rejected",
    );
    assert!(matches!(
        err,
        EmbedError::QueuedWorkExecutionConcurrency(_)
    ));
}

#[tokio::test]
async fn durable_process_worker_config_requires_core_process_registry() {
    let core = explicit_ephemeral_facets(peer_coherence_builder())
        .store_factory(Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()))
        .build(crate::testing::runtime_lease_owner())
        .expect("build core without process support");

    let Err(err) = core.durable_process_worker_config() else {
        panic!("worker config must require process support");
    };
    assert!(matches!(err, EmbedError::MissingProcessRegistry));
}

#[tokio::test]
async fn registry_without_store_factory_fails_loudly() {
    // A registry but no store factory: the default work runner rebuilds a
    // session runtime per process and cannot do so without a store factory, so
    // build must fail loudly rather than silently leave processes unexecuted
    // (a process started in such a host would otherwise hang forever).
    let result = explicit_ephemeral_facets(peer_coherence_builder())
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner());
    let err = expect_build_error(
        result,
        "a process registry with no store factory must be rejected",
    );
    assert!(matches!(
        err,
        EmbedError::ProcessRegistryRequiresStoreFactory
    ));
}

#[tokio::test]
async fn a_fork_runs_under_the_hosts_generation_intent_not_the_branch_points() -> Result<()> {
    use lash_core::SessionStoreFactory as _;

    // Forking creates a session head at a retained point; it does not create a
    // second authority over configuration. The branch resolves the host's spec
    // when it opens, exactly as a reopen of the source would, so the sampling a
    // benchmark pinned on the core reaches the branch too. Only `provider_id`
    // comes from the record, because it names which provider produced the
    // history the branch continues.
    let host_generation = lash_core::GenerationOptions {
        output_token_cap: std::num::NonZeroUsize::new(4_096),
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        projection_provenance: Default::default(),
    };
    let factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .generation(host_generation.clone())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .build(crate::testing::runtime_lease_owner())?;

    let mut source_model = mock_model_spec();
    source_model.id = "fork-source-model".to_string();
    let source_policy = lash_core::SessionPolicy {
        provider_id: "fork-source-provider".to_string(),
        model: source_model,
        session_id: Some("generation-fork-source".to_string()),
        // The branch point ran with sampling of its own. It is not a second
        // source of truth for the branch.
        generation: lash_core::GenerationOptions {
            seed: Some(9),
            ..Default::default()
        },
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let source_store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            session_id: "generation-fork-source".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: source_policy.clone(),
        })
        .await
        .expect("create fork source");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: "generation-fork-source".to_string(),
        policy: source_policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    source_state.ensure_agent_frame_initialized();
    source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
            &[],
        ))
        .await
        .expect("commit fork source");
    let fork_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("fork source leaf");
    core.pin(&fork_node_id).await?;

    core.fork_at(&fork_node_id, "generation-fork-branch")
        .await?;

    let branch = core.session("generation-fork-branch").open().await?;
    let branch_state = branch.admin().state().persist_current().await?;
    assert_eq!(
        branch_state.policy.generation, host_generation,
        "a branch resolves the host's generation intent, like every other reopen"
    );
    assert_eq!(
        branch_state.policy.recorded_provider_id(),
        "fork-source-provider",
        "the branch still records the provider that produced the history it continues"
    );
    Ok(())
}
