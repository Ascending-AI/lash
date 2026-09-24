use super::*;
use lash_core::ProcessQuery as _;
use lash_core::{
    ProcessEventLog as _, ProcessEventLogTestSupport as _, ProcessObserverRegistry as _,
    ProcessRetention as _, ProcessWakeOutbox as _,
};
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

// =============================================================================
// Runtime dependencies come from one backend
// =============================================================================
//
/// A standard-mode builder over a fresh memory backend with a model and
/// provider already named.
async fn peer_coherence_builder() -> crate::core::LashCoreBuilder {
    peer_coherence_builder_over(memory_backend().await).without_queued_work()
}

fn peer_coherence_builder_over(
    backend: Arc<dyn lash_core::Backend>,
) -> crate::core::LashCoreBuilder {
    LashCore::standard_builder(backend, crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .provider(mock_provider())
        .model(mock_model_spec())
}

#[tokio::test]
async fn commit_budget_is_required_for_builder_construction_and_deserialization() {
    let error = expect_build_error(
        LashCore::standard_builder(memory_backend().await, crate::TurnBudget::Unbounded)
            .without_queued_work()
            .provider(mock_provider())
            .model(mock_model_spec())
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

#[tokio::test]
async fn queued_work_action_reserve_is_required() {
    let error = expect_build_error(
        LashCore::standard_builder(memory_backend().await, crate::TurnBudget::Unbounded)
            .without_queued_work()
            .provider(mock_provider())
            .model(mock_model_spec())
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .build(crate::testing::runtime_lease_owner()),
        "builder must reject a missing queued-work action reserve",
    );
    assert!(matches!(error, EmbedError::MissingQueuedWorkBatching));
}

/// `LashCore` is not `Debug`, so `Result::expect_err` is unavailable; this
/// extracts the build error or panics with the given message.
fn expect_build_error<T>(result: std::result::Result<T, EmbedError>, message: &str) -> EmbedError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(err) => err,
    }
}

/// A backend whose binding identity names a different substrate than its
/// effect host binds to: every port is a real memory backend's, only the
/// identity lies.
struct MisboundBackend {
    inner: Arc<dyn lash_core::Backend>,
}

impl lash_core::Backend for MisboundBackend {
    fn binding_identity(&self) -> &str {
        "some-other-substrate"
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core::SessionStoreFactory> {
        self.inner.session_store_factory()
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.inner.effect_host()
    }

    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn trigger_store(&self) -> Arc<dyn lash_core::TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash_core::ProcessDefinitionRegistry> {
        self.inner.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        self.inner.process_work()
    }

    fn queued_work(&self) -> lash_core::BackendQueuedWork {
        self.inner.queued_work()
    }
}

/// A backend whose effect host binds to another identity than its own is
/// refused at build, before a record could name the wrong substrate.
#[tokio::test]
async fn a_backend_whose_host_binds_elsewhere_is_refused_at_build() {
    let inner: Arc<dyn lash_core::Backend> = memory_backend().await;
    let host_binding = inner.effect_host().turn_control_binding_id();
    let error = expect_build_error(
        peer_coherence_builder_over(Arc::new(MisboundBackend { inner }))
            .without_queued_work()
            .build(crate::testing::runtime_lease_owner()),
        "a misbound backend must be refused",
    );
    match error {
        EmbedError::BackendBindingMismatch {
            binding_identity,
            effect_host_binding,
        } => {
            assert_eq!(binding_identity, "some-other-substrate");
            assert_eq!(effect_host_binding, host_binding);
        }
        other => panic!("expected BackendBindingMismatch, got {other}"),
    }
}

/// FIG-3633: the RLM protocol keeps its Lashlang artifacts in the backend it
/// was built over, so a core over any other backend refuses it at build, and
/// a session refuses one supplied as a per-session factory. Otherwise a
/// resumed session would look for its modules in a substrate that never held
/// them, and the core's artifact cleanup would sweep a store nobody wrote.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_core_refuses_an_rlm_factory_built_over_another_backend() -> Result<()> {
    let artifacts = memory_backend().await;
    let core_backend = memory_backend().await;
    // Precondition: two memory backends are two substrates.
    assert_ne!(
        lash_core::Backend::binding_identity(artifacts.as_ref()),
        lash_core::Backend::binding_identity(core_backend.as_ref()),
        "two memory backends must name two substrates"
    );
    let build = |factory_backend: &lash_sqlite_store::SqliteBackend| {
        LashCore::rlm_builder(
            core_backend.clone(),
            crate::TurnBudget::Unbounded,
            rlm_factory(factory_backend),
        )
        .provider(mock_provider())
        .model(mock_model_spec())
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())
    };

    // Control: the same factory over the core's own backend builds.
    let core = build(core_backend.as_ref())?;

    let error = expect_build_error(
        build(artifacts.as_ref()),
        "an RLM factory over another backend must be refused",
    );
    match error {
        EmbedError::PluginBackendMismatch {
            plugin_id,
            plugin_backend,
            backend,
        } => {
            assert_eq!(plugin_id, lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
            assert_eq!(
                plugin_backend,
                lash_core::Backend::binding_identity(artifacts.as_ref())
            );
            assert_eq!(
                backend,
                lash_core::Backend::binding_identity(core_backend.as_ref())
            );
        }
        other => panic!("expected PluginBackendMismatch, got {other}"),
    }

    // A per-session factory and a worker's extra factory are held to the
    // same backend.
    let mut session = core.session("foreign-rlm-plugin");
    session
        .plugin_factories
        .push(Arc::new(rlm_factory(artifacts.as_ref())));
    let session_error = match session.open().await {
        Ok(_) => panic!("a per-session RLM factory over another backend must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(session_error, EmbedError::PluginBackendMismatch { .. }),
        "expected PluginBackendMismatch, got {session_error}"
    );
    let worker_error = expect_build_error(
        core.durable_process_worker_config_with_plugins([
            Arc::new(rlm_factory(artifacts.as_ref())) as Arc<dyn PluginFactory>,
        ]),
        "a worker's RLM factory over another backend must be refused",
    );
    assert!(
        matches!(worker_error, EmbedError::PluginBackendMismatch { .. }),
        "expected PluginBackendMismatch, got {worker_error}"
    );
    Ok(())
}

/// The backend's process registry stamps wake deliveries from the
/// backend's clock: the one clock the core and every store share.
#[tokio::test]
async fn the_backend_process_registry_stamps_from_the_backend_clock() {
    const NOW_MS: u64 = 4_200_000;
    let clock = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let core = LashCore::standard_builder(
        memory_backend_with_clock(clock).await,
        crate::TurnBudget::Unbounded,
    )
    .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
    .model(
        lash_core::ModelSpec::builder("clock-wiring-model")
            .context_window_tokens(4_096)
            .build()
            .expect("valid test model"),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("build core over a clocked memory backend");
    let registry = core.process_registry();
    let delivery_expiry_ms = registry.wake_delivery_config().delivery_expiry_ms;
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "builder-clock-process",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
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
            .with_wake_session_id(Some(SessionId::from("builder-clock-target"))),
        )
        .await
        .expect("register clock-wiring process");
    registry
        .append_event(
            &ProcessId::from("builder-clock-process"),
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
async fn backend_trigger_store_observes_the_backend_clock_for_inline_and_public_worker_configs()
-> Result<()> {
    const NOW_MS: u64 = 4_200_000;
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let core = explicit_ephemeral_facets_with_backend_work(peer_coherence_builder_over(
        memory_backend_with_clock(clock).await,
    ))
    .build(crate::testing::runtime_lease_owner())?;

    let inline_trigger_store = {
        let config = core
            .substrate_slot
            .process_worker_config()
            .expect("native worker config must be assembled at build");
        config.trigger_store()
    };
    let public_trigger_store = core.durable_process_worker_config()?.trigger_store();

    assert!(Arc::ptr_eq(&inline_trigger_store, &public_trigger_store));
    let receipt = public_trigger_store
        .ingest_occurrence(lash_core::TriggerOccurrenceRequest::new(
            "fig1882.clock",
            "public-worker-config",
            serde_json::Value::Null,
            "fig1882:public-worker-config",
        ))
        .await
        .expect("the backend's trigger store must ingest the clock probe");
    assert_eq!(receipt.occurrence.occurred_at_ms, NOW_MS);
    Ok(())
}

#[tokio::test]
async fn a_file_backend_builds_successfully() -> Result<()> {
    // Positive control: a durable file backend supplies every port.
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = lash_sqlite_store::SqliteBackend::open(dir.path())
        .await
        .expect("open the file backend");
    peer_coherence_builder_over(Arc::new(backend)).build(crate::testing::runtime_lease_owner())?;
    Ok(())
}

/// The process worker rebuilds session runtimes through the backend's
/// catalog, the same one the core opens and creates sessions through.
#[tokio::test]
async fn durable_process_worker_config_uses_the_backend_catalog() -> Result<()> {
    let core = explicit_ephemeral_facets_with_backend_work(peer_coherence_builder().await)
        .build(crate::testing::runtime_lease_owner())?;

    let inline_config = core
        .substrate_slot
        .process_worker_config()
        .expect("native process worker config must be assembled at build");
    let public_config = core.durable_process_worker_config()?;
    assert!(Arc::ptr_eq(
        &inline_config.session_store_factory(),
        &core.store_factory
    ));
    assert!(Arc::ptr_eq(
        &public_config.session_store_factory(),
        &core.store_factory
    ));
    Ok(())
}

#[tokio::test]
async fn attachment_limit_is_optional_host_policy_on_the_facade_builder() -> Result<()> {
    let unbounded = explicit_ephemeral_facets(peer_coherence_builder().await)
        .build(crate::testing::runtime_lease_owner())?;
    assert_eq!(
        unbounded
            .env
            .core
            .durability
            .attachment_store
            .max_attachment_bytes(),
        None
    );

    let bounded = explicit_ephemeral_facets(peer_coherence_builder().await)
        .max_attachment_bytes(Some(4096))
        .build(crate::testing::runtime_lease_owner())?;
    assert_eq!(
        bounded
            .env
            .core
            .durability
            .attachment_store
            .max_attachment_bytes(),
        Some(4096)
    );
    Ok(())
}

struct NoopProcessWork;

#[async_trait]
impl lash_core::ProcessWorkSubstrate for NoopProcessWork {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> std::result::Result<
        lash_core::facade_support::ProcessAdmissionReport,
        lash_core::PluginError,
    > {
        Ok(lash_core::facade_support::ProcessAdmissionReport::default())
    }

    async fn await_process_terminal(
        &self,
        process_ref: &lash_core::ProcessRef,
    ) -> std::result::Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        panic!("unexpected terminal wait for {process_ref}")
    }
}

/// A backend that runs its processes in `NoopProcessWork`, wired over the
/// backend's own registry.
async fn backend_with_external_process_work() -> DecoratedBackend {
    DecoratedBackend::over_sqlite(memory_backend().await).process_work(|registry| {
        lash_core::ProcessWorkWiring::new(
            lash_core::facade_support::watch_process_registry(registry),
            Arc::new(NoopProcessWork),
        )
    })
}

#[tokio::test]
async fn backend_process_work_configures_the_core_registry() -> Result<()> {
    let backend = backend_with_external_process_work().await;
    let driver_registry = lash_core::Backend::process_work(&backend)
        .expect("the backend supplies its process work")
        .registry()
        .clone();
    let core =
        explicit_ephemeral_facets_with_backend_work(peer_coherence_builder_over(Arc::new(backend)))
            .without_queued_work()
            .build(crate::testing::runtime_lease_owner())?;

    assert!(Arc::ptr_eq(&core.process_registry(), &driver_registry));
    assert!(core.processes().observer().is_ok());
    assert!(!core.substrate_slot.ports().await.drive_process_on_open);
    Ok(())
}

#[tokio::test]
async fn external_process_port_composes_native_queued_port_and_refreshes_after_ran() -> Result<()> {
    let core = explicit_ephemeral_facets_with_backend_work(peer_coherence_builder_over(Arc::new(
        backend_with_external_process_work().await,
    )))
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("external-process-native-queue").open().await?;
    let cursor_before = session
        .observe()
        .current_observation()
        .cursor
        .as_str()
        .to_string();
    session
        .admin()
        .commands()
        .refresh_tool_catalog("native queue regression guard", "native-queue-refresh")
        .await?;
    let cursor_after = session
        .observe()
        .current_observation()
        .cursor
        .as_str()
        .to_string();

    let ports = core.substrate_slot.ports().await;
    let outcome = ports
        .queued
        .drain_session_work(
            lash_core::SessionWorkTarget::Any,
            "figments-regression-guard",
        )
        .await?;

    assert_eq!(outcome, lash_core::SessionDrainOutcome::Ran);
    assert_ne!(cursor_after, cursor_before);
    Ok(())
}

#[tokio::test]
async fn default_process_work_driver_resolves_over_the_backend_registry() -> Result<()> {
    // Zero-ceremony path: a backend with no process work of its own gets
    // the default native process work port on first `session().open()`. The
    // driver's actual lease-protected execution of out-of-turn processes is
    // covered in lash-core
    // (`concurrent_workers_run_a_directly_registered_process_exactly_once`).
    let core = explicit_ephemeral_facets_with_backend_work(peer_coherence_builder().await)
        .build(crate::testing::runtime_lease_owner())?;
    core.session("main").open().await?;
    assert!(
        core.substrate_slot.ports().await.drive_process_on_open,
        "the default native process port must resolve over the backend's registry"
    );
    Ok(())
}

#[tokio::test]
async fn facade_native_process_wiring_shares_worker_change_hub() -> Result<()> {
    let core = explicit_ephemeral_facets_with_backend_work(peer_coherence_builder().await)
        .build(crate::testing::runtime_lease_owner())?;
    let worker_hub = core
        .substrate_slot
        .native_process_change_hub()
        .expect("native worker change hub");
    let ports = core.substrate_slot.ports().await;
    let wiring_registry = Arc::clone(ports.process.registry());
    let process_id = "facade-native-same-hub";
    let mut worker_changes = worker_hub.subscribe(&ProcessId::from(process_id));

    wiring_registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        ))
        .await?;

    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        worker_changes.changed(),
    )
    .await
    .expect("wiring registry mutation must wake the worker-side hub")
    .expect("worker-side hub remains live");
    Ok(())
}

#[tokio::test]
async fn durable_process_worker_config_uses_the_backend_registry_and_trigger_store() -> Result<()> {
    let backend = memory_backend().await;
    let core_owner = lash_core::LeaseOwnerIdentity::opaque(
        "durable-worker-facade-owner",
        "durable-worker-facade-boot",
    );
    let native_substrate = lash_core::NativeSubstrateConfig {
        worker_sweep: lash_core::WorkerSweepPolicy {
            intake_page: std::num::NonZeroUsize::new(17).unwrap(),
            ..lash_core::WorkerSweepPolicy::default()
        },
        work_cadence: lash_core::WorkCadencePolicy {
            delivery_batch: std::num::NonZeroUsize::MIN,
            ..lash_core::WorkCadencePolicy::default()
        },
    };
    let core =
        explicit_ephemeral_facets_with_backend_work(peer_coherence_builder_over(backend.clone()))
            .native_substrate_config(native_substrate)
            .build(core_owner)?;

    assert!(core.processes().observer().is_ok());
    let config = core.durable_process_worker_config()?;
    assert!(Arc::ptr_eq(
        config.process_registry(),
        &core.process_registry()
    ));
    let backend_trigger_store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    assert!(Arc::ptr_eq(&config.trigger_store(), &backend_trigger_store));
    assert_eq!(config.lease_owner.owner_id, "durable-worker-facade-owner");
    assert_eq!(
        config.lease_owner.incarnation_id,
        "durable-worker-facade-boot"
    );
    assert_eq!(
        config.process_execution_concurrency(),
        lash_core_worker::DEFAULT_PROCESS_EXECUTION_CONCURRENCY
    );
    assert_eq!(config.native_substrate.worker_sweep.intake_page.get(), 17);
    assert_eq!(config.native_substrate.work_cadence.delivery_batch.get(), 1);
    Ok(())
}

#[tokio::test]
async fn fork_distinguishes_collected_point_from_retained_orphaned_source() -> Result<()> {
    let backend = memory_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let collected_error = core
        .fork_at(crate::ForkRequest {
            session_id: ("collected-fork-branch").into(),
            node_id: ("collected-fork-point").into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("collected-source").into(),
                source_node_id: ("collected-fork-point").into(),
            },
            observed_processes: Vec::new(),
        })
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
        session_id: Some(SessionId::from("orphaned-fork-source")),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let source_request = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("orphaned-fork-source"),
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
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
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
        .fork_at(crate::ForkRequest {
            session_id: ("orphaned-fork-branch").into(),
            node_id: (&retained_node_id).into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("orphaned-fork-source").into(),
                source_node_id: (&retained_node_id).into(),
            },
            observed_processes: Vec::new(),
        })
        .await
        .expect("retained graph frame must resolve policy after source deletion");
    assert_eq!(forked.node_id, retained_node_id);
    assert_eq!(
        forked.source_session_id, source_request.session_id,
        "a successful orphaned-pin fork preserves deleted-source provenance"
    );
    let branch = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("orphaned-fork-branch"),
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
async fn fork_observer_selection_is_recoverable_selective_and_wake_independent() -> Result<()> {
    // The registry's read-fault hook is what injects the transient observer
    // failure below; no SQLite registry seam reaches that read.
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let fault_registry = Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>;
    let backend = DecoratedBackend::over_sqlite(memory_backend().await)
        .process_registry(move |_| fault_registry);
    let factory = lash_core::Backend::session_store_factory(&backend);
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        Arc::new(backend),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let mut source_model = mock_model_spec();
    source_model.id = "fork-source-model".to_string();
    let policy = lash_core::SessionPolicy {
        // The host and the branch point agree on the provider: a durable
        // pin is a fact, so a host naming a different one is refused at
        // open rather than silently discarded (FIG-1558).
        provider_id: "embed-test".to_string(),
        model: source_model,
        session_id: Some(SessionId::from("fork-observer-source")),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let source_store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-observer-source"),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await
        .expect("create fork observer source");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: SessionId::from("fork-observer-source"),
        policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
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
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "fork.wake".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec {
                    wake: Some(lash_core::ProcessWakeSpec {
                        when: Some(lash_core::ProcessValueSelector::Present(
                            "/wake_input".to_string(),
                        )),
                        input: lash_core::ProcessValueSelector::Pointer("/wake_input".to_string()),
                    }),
                    ..lash_core::ProcessEventSemanticsSpec::default()
                },
            }])
            .with_wake_session_id(Some(SessionId::from("fork-observer-source"))),
        )
        .await
        .expect("register fork-visible process");
    registry
        .add_observer(
            &SessionId::from("fork-observer-source"),
            &ProcessId::from("fork-visible-process"),
            lash_core::ProcessObserverBy::host("fork-test-source"),
        )
        .await
        .expect("observe source process");

    let fork_receipt = core
        .fork_at(crate::ForkRequest {
            session_id: ("fork-observer-branch").into(),
            node_id: (&fork_node_id).into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: ("fork-observer-source").into(),
                source_node_id: (&fork_node_id).into(),
            },
            observed_processes: vec![
                registry
                    .resolve_process_ref(&ProcessId::from("fork-visible-process"))
                    .await?,
            ],
        })
        .await?;
    assert_eq!(fork_receipt.observed_processes.len(), 1);
    assert_eq!(
        fork_receipt.observed_processes[0].process_id,
        "fork-visible-process"
    );
    let branch_store = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-observer-branch"),
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
    assert_eq!(branch_read.config.provider_id, "embed-test");
    assert_eq!(branch_read.config.model.id, "fork-source-model");

    let inherited = registry
        .list_observed_by(
            &SessionId::from("fork-observer-branch"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list inherited observations");
    assert_eq!(inherited.len(), 1);
    assert_eq!(inherited[0].id, "fork-visible-process");

    registry
        .set_process_read_error(Some(lash_core::PluginError::Session(
            "transient fork observer registry failure".to_string(),
        )))
        .await;
    core.fork_at(crate::ForkRequest {
        session_id: ("fork-transient-branch").into(),
        node_id: (&fork_node_id).into(),
        relation: lash_core::SessionRelation::Fork {
            source_session_id: ("fork-observer-source").into(),
            source_node_id: (&fork_node_id).into(),
        },
        observed_processes: inherited
            .iter()
            .map(lash_core::ProcessRef::from_record)
            .collect(),
    })
    .await
    .expect("transient observer registry failure must not fail fork_at");
    registry.set_process_read_error(None).await;
    assert!(
        registry
            .list_observed_by(
                &SessionId::from("fork-transient-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("list transient-failure branch observations")
            .is_empty(),
        "a transiently unavailable process must not gain a fork observer edge"
    );
    let transient_branch_store = factory
        .open_existing_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("fork-transient-branch"),
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
        transient_meta.pending_observer_intents.is_empty(),
        "fork_at must consume transiently unavailable observer intents"
    );

    let published_meta = branch_store
        .load_session_meta()
        .await
        .expect("load published fork metadata")
        .expect("published fork metadata exists");
    assert!(matches!(
        published_meta.relation,
        lash_core::SessionRelation::Fork { .. }
    ));
    assert!(
        published_meta.pending_observer_intents.is_empty(),
        "successfully published observers must be consumed from the recovery intent"
    );

    let mut recovery_meta = published_meta;
    recovery_meta.pending_observer_intents.push(
        lash_core::facade_support::SessionObserverIntent::host_requested("fork-visible-process"),
    );
    registry
        .register_process(lash_core::ProcessRegistration::new(
            "fork-pruned-process",
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        ))
        .await
        .expect("register process that will be pruned during fork publication");
    let pruned_terminal = registry
        .complete_process(
            &ProcessId::from("fork-pruned-process"),
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
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
    recovery_meta.pending_observer_intents.push(
        lash_core::facade_support::SessionObserverIntent::host_requested("fork-pruned-process"),
    );
    branch_store
        .save_session_meta(recovery_meta)
        .await
        .expect("simulate a crash before observer intent consumption");
    registry
        .remove_observer(
            &SessionId::from("fork-observer-branch"),
            &ProcessId::from("fork-visible-process"),
            lash_core::ProcessObserverBy::host("fork-test-crash"),
        )
        .await
        .expect("remove the partially published observer");
    core.session("fork-observer-branch")
        .open_with_state(lash_core::RuntimeSessionState {
            session_id: SessionId::from("fork-observer-branch"),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        })
        .await?;
    assert_eq!(
        registry
            .list_observed_by(
                &SessionId::from("fork-observer-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
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
    assert!(
        recovered_meta.pending_observer_intents.is_empty(),
        "recovery must consume the intent after idempotent publication"
    );
    registry
        .remove_observer(
            &SessionId::from("fork-observer-branch"),
            &ProcessId::from("fork-visible-process"),
            lash_core::ProcessObserverBy::host("fork-test-revoke"),
        )
        .await
        .expect("deliberately remove the recovered observer");
    core.session("fork-observer-branch").open().await?;
    assert!(
        registry
            .list_observed_by(
                &SessionId::from("fork-observer-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
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
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ),
            &[SessionId::from("fork-observer-source")],
        )
        .await
        .expect("register second observed process");
    core.fork_at(crate::ForkRequest {
        session_id: ("fork-only-branch").into(),
        node_id: (&fork_node_id).into(),
        relation: lash_core::SessionRelation::Fork {
            source_session_id: ("fork-observer-source").into(),
            source_node_id: (&fork_node_id).into(),
        },
        observed_processes: vec![
            registry
                .resolve_process_ref(&ProcessId::from("fork-selective-process"))
                .await?,
        ],
    })
    .await?;
    let only = registry
        .list_observed_by(
            &SessionId::from("fork-only-branch"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list Only selector result");
    assert_eq!(
        only.iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fork-selective-process"]
    );
    let event_count_before = registry
        .full_event_window(&ProcessId::from("fork-selective-process"), 0)
        .await
        .expect("read observer audit before duplicate apply")
        .len();
    registry
        .add_observer(
            &SessionId::from("fork-only-branch"),
            &ProcessId::from("fork-selective-process"),
            lash_core::ProcessObserverBy::host("observer-test"),
        )
        .await
        .expect("reapply fork observer");
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from("fork-selective-process"), 0)
            .await
            .expect("read observer audit after duplicate apply")
            .len(),
        event_count_before,
        "double-apply must be an event-log no-op"
    );

    core.fork_at(crate::ForkRequest {
        session_id: ("fork-none-branch").into(),
        node_id: (&fork_node_id).into(),
        relation: lash_core::SessionRelation::Fork {
            source_session_id: ("fork-observer-source").into(),
            source_node_id: (&fork_node_id).into(),
        },
        observed_processes: Vec::new(),
    })
    .await?;
    assert!(
        registry
            .list_observed_by(
                &SessionId::from("fork-none-branch"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("list None selector result")
            .is_empty()
    );

    registry
        .append_event(
            &ProcessId::from("fork-visible-process"),
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

async fn duplicate_only_fork_intents_are_canonical(
    case: &str,
    backend: Arc<dyn lash_core::Backend>,
) -> Result<()> {
    let source_session_id = SessionId::from(format!("duplicate-only-source-{case}"));
    let branch_session_id = SessionId::from(format!("duplicate-only-branch-{case}"));
    let process_id = ProcessId::from(format!("duplicate-only-process-{case}"));
    let factory = backend.session_store_factory();
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let policy = lash_core::SessionPolicy {
        session_id: Some(source_session_id.clone()),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let source_store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: source_session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: policy.clone(),
        })
        .await?;
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: source_session_id.clone(),
        policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    source_state.ensure_agent_frame_initialized();
    source_store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
            &source_state,
            &[],
        ))
        .await?;
    let fork_node_id = source_state
        .session_graph
        .leaf_node_id
        .clone()
        .expect("source has a forkable frame node");
    core.pin(&fork_node_id).await?;

    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                &process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ),
            std::slice::from_ref(&source_session_id),
        )
        .await?;

    let receipt = core
        .fork_at(crate::ForkRequest {
            session_id: (&branch_session_id).into(),
            node_id: (&fork_node_id).into(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: (&source_session_id).into(),
                source_node_id: (&fork_node_id).into(),
            },
            observed_processes: vec![
                registry.resolve_process_ref(&process_id).await?,
                registry.resolve_process_ref(&process_id).await?,
            ],
        })
        .await?;

    assert_eq!(
        receipt.observed_processes.len(),
        1,
        "duplicate host selection must persist and settle one observer intent"
    );
    assert_eq!(receipt.observed_processes[0].process_id, process_id);
    assert_eq!(
        registry
            .list_observed_by(
                &branch_session_id,
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await?
            .len(),
        1,
        "the fork must expose one observer edge"
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_only_fork_intents_are_canonical_in_memory() -> Result<()> {
    duplicate_only_fork_intents_are_canonical("memory", memory_backend().await).await
}

#[tokio::test]
async fn duplicate_only_fork_intents_are_canonical_in_sqlite() -> Result<()> {
    let root = tempfile::tempdir().expect("create SQLite fixture directory");
    duplicate_only_fork_intents_are_canonical(
        "file",
        Arc::new(
            lash_sqlite_store::SqliteBackend::open(root.path())
                .await
                .expect("open the file backend"),
        ),
    )
    .await
}

#[tokio::test]
async fn session_create_observer_intent_replays_idempotently_on_open() -> Result<()> {
    let session_id = "session-create-observer-recovery";
    let process_id = "session-create-observed-process";
    let backend = memory_backend().await;
    let factory = backend.session_store_factory();
    let registry = backend.process_registry();
    registry
        .register_process(lash_core::ProcessRegistration::new(
            process_id,
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        ))
        .await?;
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: vec![
                lash_core::facade_support::SessionObserverIntent::host_requested(process_id),
            ],
            session_id: SessionId::from(session_id.to_string()),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await?;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    assert!(
        !registry
            .is_observer(&SessionId::from(session_id), &ProcessId::from(process_id))
            .await?,
        "the fixture must preserve the real crash gap before publication"
    );
    core.session(session_id).open().await?;
    assert!(
        registry
            .is_observer(&SessionId::from(session_id), &ProcessId::from(process_id))
            .await?,
        "open must publish the observer edge left pending by a create crash"
    );
    let observer_event_count = registry
        .full_event_window(&ProcessId::from(process_id), 0)
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
            .full_event_window(&ProcessId::from(process_id), 0)
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
            &SessionId::from(session_id),
            &ProcessId::from(process_id),
            lash_core::ProcessObserverBy::host("post-recovery-removal"),
        )
        .await?;
    core.session(session_id).open().await?;
    assert!(
        !registry
            .is_observer(&SessionId::from(session_id), &ProcessId::from(process_id))
            .await?,
        "consumed create intent must not recreate a deliberately removed edge"
    );
    Ok(())
}

#[tokio::test]
async fn session_observer_intents_settle_in_one_pass_before_open_returns() -> Result<()> {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    for (case, simulate_crash_between_layers) in [("fresh", false), ("crash-resume", true)] {
        let session_id = SessionId::from(format!("nested-observer-intent-{case}"));
        let create_process_id = ProcessId::from(format!("nested-create-process-{case}"));
        let fork_process_id = ProcessId::from(format!("nested-fork-process-{case}"));
        for process_id in [&create_process_id, &fork_process_id] {
            registry
                .register_process(lash_core::ProcessRegistration::new(
                    process_id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    lash_core::RecoveryContract::ExternallyOwned,
                    lash_core::ProcessProvenance::host(),
                    lash_core::ProcessLifecyclePolicy::new(
                        lash_core::ParentScope::Host,
                        lash_core::OnParentEnd::Abandon,
                    ),
                ))
                .await?;
        }
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                pending_observer_intents: vec![
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        create_process_id.clone(),
                    ),
                    lash_core::facade_support::SessionObserverIntent::host_requested(
                        fork_process_id.clone(),
                    ),
                ],
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: SessionId::from(format!("nested-source-{case}")),
                    source_node_id: format!("nested-source-node-{case}").into(),
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
                !registry.is_observer(&session_id, &fork_process_id).await?,
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
            registry.is_observer(&session_id, &fork_process_id).await?,
            "open must settle the inner fork observer intent"
        );
        let meta = store
            .load_session_meta()
            .await?
            .expect("attributed session metadata");
        assert!(
            matches!(meta.relation, lash_core::SessionRelation::Fork { .. }),
            "open must preserve the base fork relation, got {:?}",
            meta.relation
        );
        assert!(meta.pending_observer_intents.is_empty());

        let create_events = registry
            .full_event_window(&create_process_id, 0)
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
            .full_event_window(&fork_process_id, 0)
            .await?
            .into_iter()
            .filter(|event| event.event_type == "process.observer_added")
            .collect::<Vec<_>>();
        assert_eq!(fork_events.len(), 1);
        assert_eq!(
            fork_events[0].payload["by"],
            serde_json::json!({"kind": "host", "operation_id": format!("session-create:{session_id}")})
        );
    }
    Ok(())
}

#[tokio::test]
async fn builder_rejects_invalid_process_execution_concurrency() {
    let err = expect_build_error(
        explicit_ephemeral_facets(peer_coherence_builder().await)
            .process_execution_concurrency(0)
            .build(crate::testing::runtime_lease_owner()),
        "zero process execution concurrency must be rejected",
    );
    assert!(matches!(err, EmbedError::ProcessExecutionConcurrency(_)));
}

#[tokio::test]
async fn builder_rejects_invalid_queued_work_execution_concurrency() {
    let err = expect_build_error(
        explicit_ephemeral_facets(peer_coherence_builder().await)
            .queued_work_execution_concurrency(0)
            .build(crate::testing::runtime_lease_owner()),
        "zero queued-work execution concurrency must be rejected",
    );
    assert!(matches!(err, EmbedError::QueuedWorkExecutionConcurrency(_)));
}

#[tokio::test]
async fn builder_rejects_incoherent_native_pacing_durations() {
    type Edit = fn(&mut lash_core::NativeSubstrateConfig);
    let cases: [(&str, Edit); 13] = [
        ("worker_sweep.fetch_retry_base", |config| {
            config.worker_sweep.fetch_retry_base = std::time::Duration::ZERO;
        }),
        ("work_cadence.retry_initial", |config| {
            config.work_cadence.retry_initial = std::time::Duration::ZERO;
        }),
        ("work_cadence.retry_max", |config| {
            config.work_cadence.retry_max = std::time::Duration::ZERO;
        }),
        ("work_cadence.retry_initial", |config| {
            config.work_cadence.retry_initial = std::time::Duration::from_secs(2);
        }),
        ("work_cadence.poll_initial", |config| {
            config.work_cadence.poll_initial = std::time::Duration::ZERO;
        }),
        ("work_cadence.poll_max", |config| {
            config.work_cadence.poll_max = std::time::Duration::ZERO;
        }),
        ("work_cadence.poll_initial", |config| {
            config.work_cadence.poll_initial = std::time::Duration::from_secs(2);
        }),
        ("work_cadence.slow_wake_threshold", |config| {
            config.work_cadence.slow_wake_threshold = std::time::Duration::ZERO;
        }),
        ("work_cadence.slow_wake_threshold", |config| {
            config.work_cadence.slow_wake_threshold = std::time::Duration::from_micros(500);
        }),
        ("work_cadence.delivery_retry_initial", |config| {
            config.work_cadence.delivery_retry_initial = std::time::Duration::ZERO;
        }),
        ("work_cadence.delivery_retry_initial", |config| {
            config.work_cadence.delivery_retry_initial = std::time::Duration::from_micros(500);
        }),
        ("work_cadence.delivery_retry_max", |config| {
            config.work_cadence.delivery_retry_max = std::time::Duration::ZERO;
        }),
        ("work_cadence.delivery_retry_max", |config| {
            config.work_cadence.delivery_retry_max = std::time::Duration::from_micros(500);
        }),
    ];

    for (field, edit) in cases {
        let mut config = lash_core::NativeSubstrateConfig::default();
        edit(&mut config);
        let err = expect_build_error(
            explicit_ephemeral_facets_with_backend_work(peer_coherence_builder().await)
                .native_substrate_config(config)
                .build(crate::testing::runtime_lease_owner()),
            "incoherent native pacing must be rejected",
        );
        let EmbedError::NativeSubstrateConfig(source) = err else {
            panic!("native pacing must use its typed build error, got {err}");
        };
        assert!(
            source.to_string().contains(field),
            "error must identify {field}: {source}"
        );
    }
}

#[tokio::test]
async fn durable_process_worker_rejects_incoherent_native_pacing_directly() {
    let core = explicit_ephemeral_facets_with_backend_work(peer_coherence_builder().await)
        .build(crate::testing::runtime_lease_owner())
        .expect("build core with process support");
    let mut config = core
        .durable_process_worker_config()
        .expect("build durable process-worker config");
    config.native_substrate.worker_sweep.fetch_retry_base = std::time::Duration::ZERO;

    let Err(error) = lash_core_worker::DurableProcessWorker::new(config) else {
        panic!("direct worker construction must reject zero-delay pacing");
    };
    assert!(
        error.to_string().contains("worker_sweep.fetch_retry_base"),
        "error must identify the rejected worker pacing field: {error}"
    );
}

struct RejectedCadenceRunHandle;

#[async_trait::async_trait]
impl lash_core::facade_support::QueuedWorkRunHandle for RejectedCadenceRunHandle {
    async fn run_queued_work(
        &self,
        _request: lash_core::facade_support::QueuedWorkRunRequest,
    ) -> std::result::Result<(), lash_core::facade_support::QueuedWorkRunError> {
        unreachable!("invalid cadence must be rejected before the driver can run")
    }
}

#[test]
fn explicit_cadence_constructor_rejects_zero_poll_delay_directly() {
    let work_cadence = lash_core::WorkCadencePolicy {
        poll_initial: std::time::Duration::ZERO,
        ..lash_core::WorkCadencePolicy::default()
    };

    let Err(error) =
        lash_core::facade_support::native_queued_work_with_execution_concurrency_and_work_cadence(
            Arc::new(RejectedCadenceRunHandle),
            1,
            work_cadence,
        )
    else {
        panic!("explicit-cadence construction must reject zero-delay polling");
    };
    let lash_core::facade_support::NativeQueuedWorkConfigError::NativeSubstrateConfig(source) =
        error
    else {
        panic!("zero-delay polling must preserve its typed pacing cause: {error}");
    };
    assert!(
        source.to_string().contains("work_cadence.poll_initial"),
        "error must identify the rejected poll field: {source}"
    );
}

#[tokio::test]
async fn builder_allows_harmless_native_pacing_boundaries() {
    let mut config = lash_core::NativeSubstrateConfig::default();
    config.worker_sweep.intake_page = std::num::NonZeroUsize::MIN;
    config.worker_sweep.fetch_attempts = std::num::NonZeroUsize::MIN;
    config.work_cadence.max_transient_attempts = std::num::NonZeroU32::MIN;
    config.work_cadence.delivery_batch = std::num::NonZeroUsize::MIN;
    config.work_cadence.slow_wake_threshold = std::time::Duration::from_millis(1);
    config.work_cadence.delivery_retry_initial = std::time::Duration::from_secs(2);
    config.work_cadence.delivery_retry_max = std::time::Duration::from_secs(1);

    explicit_ephemeral_facets_with_backend_work(peer_coherence_builder().await)
        .native_substrate_config(config)
        .build(crate::testing::runtime_lease_owner())
        .expect(
            "non-zero count minima, a one-millisecond slow-wake threshold, and clamped delivery retry are valid",
        );
}

#[tokio::test]
async fn a_fork_runs_under_the_hosts_generation_intent_not_the_branch_points() -> Result<()> {
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
    let backend = memory_backend().await;
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .generation(host_generation.clone())
    .build(crate::testing::runtime_lease_owner())?;

    let mut source_model = mock_model_spec();
    source_model.id = "fork-source-model".to_string();
    let source_policy = lash_core::SessionPolicy {
        // The host and the branch point agree on the provider: a durable
        // pin is a fact, so a host naming a different one is refused at
        // open rather than silently discarded (FIG-1558).
        provider_id: "embed-test".to_string(),
        model: source_model,
        session_id: Some(SessionId::from("generation-fork-source")),
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
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("generation-fork-source"),
            relation: lash_core::SessionRelation::Root,
            policy: source_policy.clone(),
        })
        .await
        .expect("create fork source");
    let mut source_state = lash_core::RuntimeSessionState {
        session_id: SessionId::from("generation-fork-source"),
        policy: source_policy,
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
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

    core.fork_at(crate::ForkRequest {
        session_id: ("generation-fork-branch").into(),
        node_id: (&fork_node_id).into(),
        relation: lash_core::SessionRelation::Fork {
            source_session_id: ("generation-fork-source").into(),
            source_node_id: (&fork_node_id).into(),
        },
        observed_processes: Vec::new(),
    })
    .await?;

    let branch = core.session("generation-fork-branch").open().await?;
    let branch_state = branch.admin().state().persist_current().await?;
    assert_eq!(
        branch_state.policy.generation, host_generation,
        "a branch resolves the host's generation intent, like every other reopen"
    );
    assert_eq!(
        branch_state.policy.model.id, "fork-source-model",
        "the branch still records the model that produced the history it continues"
    );
    Ok(())
}
