use super::*;
use lash_sansio::SessionId;

// =============================================================================
// Runtime dependencies come from one backend
// =============================================================================
//
fn peer_coherence_builder_over(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
}

/// `LashCore` is not `Debug`, so `Result::expect_err` is unavailable; this
/// extracts the build error or panics with the given message.
fn expect_build_error<T>(result: std::result::Result<T, EmbedError>, message: &str) -> EmbedError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(err) => err,
    }
}

/// FIG-3633: the RLM protocol keeps its Lashlang artifacts in the backend it
/// was built over, so a core over any other backend refuses it at build. A
/// core's plugin set is the only one its sessions and workers run
/// (FIG-4396). Otherwise a
/// resumed session would look for its modules in a substrate that never held
/// them, and the core's artifact cleanup would sweep a store nobody wrote.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_core_refuses_an_rlm_factory_built_over_another_backend() -> Result<()> {
    let artifacts = sqlite_memory_store_backend().await;
    let core_backend = sqlite_memory_store_backend().await;
    // Precondition: two memory store sets are two substrates.
    assert_ne!(
        artifacts.binding_identity(),
        core_backend.binding_identity(),
        "two memory store sets must name two substrates"
    );
    let build = |factory_backend: &lash_core::Backend| {
        LashCore::rlm_builder(core_backend.clone(), rlm_factory(factory_backend))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .build(crate::testing::runtime_lease_owner())
    };

    // Control: the same factory over the core's own backend builds.
    build(&core_backend)?;

    let error = expect_build_error(
        build(&artifacts),
        "an RLM factory over another backend must be refused",
    );
    match error {
        EmbedError::PluginBackendMismatch {
            plugin_id,
            plugin_backend,
            backend,
        } => {
            assert_eq!(plugin_id, lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
            assert_eq!(plugin_backend, artifacts.binding_identity().to_string());
            assert_eq!(backend, core_backend.binding_identity().to_string());
        }
        other => panic!("expected PluginBackendMismatch, got {other}"),
    }

    Ok(())
}

/// The backend's process registry stamps a wake's event, and the wake it
/// carries, from the backend's clock: the one clock the core and every store
/// share.
#[tokio::test]
async fn the_backend_process_registry_stamps_from_the_backend_clock() {
    const NOW_MS: u64 = 4_200_000;
    let clock = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let core = LashCore::standard_builder(store_backend_with_clock(clock).await)
        .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
        .build(crate::testing::runtime_lease_owner())
        .expect("build core over a clocked memory backend");
    let registry = core.process_registry();
    let builder_clock_process_id = registry
        .register_process(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([lash_core::ProcessEventType {
                name: "builder.clock.wake".to_string(),
                payload_schema: lash_core::JsonSchema::any(),
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
        .expect("register clock-wiring process")
        .id;
    let appended = registry
        .append_event(
            &builder_clock_process_id,
            lash_core::ProcessEventAppendRequest::new(
                "builder.clock.wake",
                serde_json::json!({"wake_input": "wake"}),
            ),
        )
        .await
        .expect("append clock-wiring wake");
    let wake = appended.wake_delivery.expect("clock-wiring wake");

    assert_eq!(appended.event.occurred_at, NOW_MS);
    assert_eq!(wake.created_at_ms, NOW_MS);
}

#[tokio::test]
async fn backend_trigger_store_observes_the_backend_clock_for_the_worker_config() -> Result<()> {
    const NOW_MS: u64 = 4_200_000;
    let clock: Arc<dyn lash_core::Clock> = Arc::new(lash_core::testing::TestClock::new(NOW_MS));
    let core = explicit_ephemeral_facets(peer_coherence_builder_over(
        store_backend_with_clock(clock).await,
    ))
    .build(crate::testing::runtime_lease_owner())?;

    let public_trigger_store = core.durable_process_worker_config()?.trigger_store();

    let plan = public_trigger_store
        .plan_occurrence(&lash_core::TriggerOccurrenceRequest::new(
            "fig1882.clock",
            "public-worker-config",
            serde_json::Value::Null,
            "fig1882:public-worker-config",
        ))
        .await
        .expect("the backend's trigger store must plan the clock probe");
    let lash_core::TriggerOccurrencePlan::Fresh { occurrence, .. } = plan else {
        panic!("a fresh store holds no occurrence: {plan:?}");
    };
    assert_eq!(occurrence.occurred_at_ms, NOW_MS);
    Ok(())
}
