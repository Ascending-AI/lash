// =============================================================================
// Explicit runtime dependency wiring
// =============================================================================
//
/// An RLM builder with a model + provider already named, ready for the explicit
/// dependency wiring under test.
fn peer_coherence_builder(
    artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>,
) -> crate::core::LashCoreBuilder {
    LashCore::rlm_builder(lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        artifact_store,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
}

/// The named in-memory Lashlang artifact store used by builder tests.
fn inline_artifact_store() -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
    Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new())
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

async fn durable_artifact_store(
    dir: &std::path::Path,
) -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
    Arc::new(
        lash_sqlite_store::Store::open(&dir.join("artifacts.db"))
            .await
            .expect("open durable artifact store"),
    )
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
    let clock =
        Arc::new(lash_core::testing::conformance::AttachmentOwnerConformanceClock::new(NOW_MS));
    let registry = lash_sqlite_store::SqliteProcessRegistry::memory()
        .await
        .expect("open SQLite process registry with its default clock");
    let store_factory = Arc::new(lash_core::InMemorySessionStoreFactory::with_clock(
        clock.clone(),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let core = LashCore::standard_builder()
        .model(
            lash_core::ModelSpec::from_token_limits(
                "clock-wiring-model",
                Default::default(),
                4_096,
                None,
            )
            .expect("valid test model"),
        )
        .store_factory(store_factory)
        .process_registry(Arc::new(registry))
        .advanced()
        .runtime_host_config(lash_core::RuntimeHostConfig::in_memory().with_clock(clock))
        .build()
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
    let result = peer_coherence_builder(inline_artifact_store())
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash_core::InMemoryAttachmentStore::new()))
        .build();
    let err = expect_build_error(
        result,
        "builder must reject missing process execution environment store",
    );

    assert!(matches!(err, EmbedError::MissingProcessEnvStore));
}

#[tokio::test]
async fn all_durable_stores_build_successfully() -> Result<()> {
    // Positive control: a coherent durable wiring (durable session store +
    // durable attachment + durable artifact + durable process registry +
    // durable trigger store) builds without error.
    let dir = tempfile::tempdir().expect("tempdir");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open durable registry"),
    );
    peer_coherence_builder(durable_artifact_store(dir.path()).await)
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .store_factory(durable_session_store_factory(dir.path()))
        .attachment_store(durable_attachment_store(dir.path()))
        .process_env_store(durable_process_env_store(dir.path()).await)
        .trigger_store(durable_trigger_store(dir.path()).await)
        .process_registry(registry)
        .build()?;
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
    peer_coherence_builder(durable_artifact_store(dir.path()).await)
        .effect_host(Arc::new(lash_core::InlineEffectHost::default()))
        .child_store_factory(durable_session_store_factory(dir.path()))
        .attachment_store(durable_attachment_store(dir.path()))
        .process_env_store(durable_process_env_store(dir.path()).await)
        .trigger_store(durable_trigger_store(dir.path()).await)
        .process_registry(registry)
        .build()?;
    Ok(())
}

#[tokio::test]
async fn explicit_ephemeral_facets_build_successfully() -> Result<()> {
    // An all-in-memory build succeeds, including the explicit session store
    // factory that backs process execution.
    explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
    Ok(())
}

struct NoopProcessRunHandle;

#[async_trait]
impl lash_core::ProcessRunHandle for NoopProcessRunHandle {
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
        lash_core::ProcessWorkDriver::new(Arc::clone(&registry), Arc::new(NoopProcessRunHandle));
    let driver_registry = driver.process_registry();
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .process_work_driver(driver)
        .build()?;

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
            ..Default::default()
        },
        ..Default::default()
    };
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::with_state(state));
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(ReusableStoreFactory { store }))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build()?;
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
        Arc::new(lash_core::InMemoryTriggerStore::default()) as Arc<dyn lash_core::TriggerStore>;
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .trigger_store(Arc::clone(&trigger_store))
        .process_registry(Arc::clone(&registry))
        .build()?;

    assert!(core.processes().observer().is_ok());
    let config = core.durable_process_worker_config()?;
    let core_registry = core
        .process_registry()
        .expect("process registry must be configured");
    assert!(Arc::ptr_eq(&config.process_registry, &core_registry));
    assert!(Arc::ptr_eq(&config.trigger_store, &trigger_store));
    assert_eq!(
        config.process_execution_concurrency(),
        lash_core::DEFAULT_PROCESS_EXECUTION_CONCURRENCY
    );
    Ok(())
}

#[tokio::test]
async fn fork_distinguishes_collected_point_from_retained_orphaned_source() -> Result<()> {
    use lash_core::SessionStoreFactory as _;

    let factory = Arc::new(lash_core::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .build()?;

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
        ..Default::default()
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
        ..Default::default()
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

    core.fork_at(&retained_node_id, "orphaned-fork-branch")
        .await
        .expect("retained graph frame must resolve policy after source deletion");
    let branch = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            session_id: "orphaned-fork-branch".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::default(),
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

    let factory = Arc::new(lash_core::InMemorySessionStoreFactory::new());
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build()?;
    let mut source_model = mock_model_spec();
    source_model.id = "fork-source-model".to_string();
    let policy = lash_core::SessionPolicy {
        provider_id: "fork-source-provider".to_string(),
        model: source_model,
        session_id: Some("fork-observer-source".to_string()),
        ..Default::default()
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
        ..Default::default()
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
            policy: lash_core::SessionPolicy::default(),
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
            ..Default::default()
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

#[test]
fn builder_rejects_invalid_process_execution_concurrency() {
    let err = expect_build_error(
        explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
            .process_execution_concurrency(0)
            .build(),
        "zero process execution concurrency must be rejected",
    );
    assert!(matches!(err, EmbedError::ProcessExecutionConcurrency(_)));
}

#[tokio::test]
async fn durable_process_worker_config_requires_core_process_registry() {
    let core = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .store_factory(Arc::new(lash_core::InMemorySessionStoreFactory::new()))
        .build()
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
    let result = explicit_ephemeral_facets(peer_coherence_builder(inline_artifact_store()))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build();
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
    };
    let factory = Arc::new(lash_core::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .generation(host_generation.clone())
        .store_factory(Arc::clone(&factory) as Arc<dyn lash_core::SessionStoreFactory>)
        .build()?;

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
        ..Default::default()
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
        ..Default::default()
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
