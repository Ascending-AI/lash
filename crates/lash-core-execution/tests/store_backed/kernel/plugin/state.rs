mod tests {
    use crate::plugin::PluginSessionRequest;
    use std::sync::Arc;

    use crate::SessionId;

    /// A memory backend's catalog holding the session the test commits.
    async fn session_store(session_id: &str) -> Arc<dyn crate::RuntimeStore> {
        let catalog = crate::support::sqlite_memory_store_set()
            .await
            .session_store_factory();
        crate::SessionCatalogStore::admit_session(
            catalog.as_ref(),
            &crate::testing::store_fixtures::session_store_request(
                &SessionId::fixture(session_id),
                "model",
                crate::SessionRelation::Root,
            ),
        )
        .await
        .expect("admit the session");
        catalog
    }

    /// The session's current window on `store`, as runtime state.
    async fn load_state(
        store: &Arc<dyn crate::RuntimeStore>,
        session_id: &str,
    ) -> crate::RuntimeSessionState {
        let view =
            crate::store::SessionStore::new(Arc::clone(store), SessionId::fixture(session_id))
                .expect("a valid session id");
        crate::store::load_session_window_state(&view, crate::store::WindowSelector::Current)
            .await
            .unwrap()
            .unwrap()
            .state
    }

    /// Publish `value` to the `mock` namespace from the recorded body `step`.
    async fn publish(plugins: &Arc<crate::PluginSession>, step: &str, value: serde_json::Value) {
        crate::publish_plugin_state(
            plugins,
            "mock",
            crate::EffectAddress::new(crate::ExecutionScope::turn("capture-race", "run"), step)
                .unwrap(),
            crate::StateCommands::new().set("value", value),
        )
        .await
        .expect("the publication applies");
    }

    #[tokio::test]
    async fn publication_after_capture_survives_commit_receipt_adoption() {
        let plugins =
            crate::support::plugin_host(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
                crate::plugin::PluginDeclaration::initial("mock"),
                crate::plugin::PluginSpec::new(),
            ))])
            .build_session(PluginSessionRequest::creation(
                "capture-race",
                crate::plugin::SessionAuthorityContext::ambient_fixture(),
            ))
            .unwrap();
        let store = session_store("capture-race").await;
        let mut state = crate::RuntimeSessionState {
            session_id: "capture-race".into(),
            ..crate::RuntimeSessionState::ambient_fixture(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                lash_core_execution::NoProgressBudget::bounded(12),
            ))
        };
        publish(&plugins, "first", serde_json::json!(1)).await;
        state
            .refresh_plugin_states(&plugins)
            .expect("the live plugin state is captured");
        let captured = crate::RuntimeCommit::persisted_state_for_test(&state);
        publish(&plugins, "second", serde_json::json!(2)).await;
        let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
            &store,
            captured,
            "capture-race",
        )
        .await
        .unwrap();
        state.apply_persisted_commit_result(receipt);
        let loaded = load_state(&store, "capture-race").await;
        assert_eq!(
            loaded.plugin_state().unwrap().plugins["mock"].values["value"],
            serde_json::json!(1)
        );
        state
            .refresh_plugin_states(&plugins)
            .expect("the live plugin state is captured");
        let next = crate::RuntimeCommit::persisted_state_for_test(&state);
        assert!(matches!(
            next.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Changed { .. }
        ));
        let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
            &store,
            next,
            "capture-race-next",
        )
        .await
        .unwrap();
        state.apply_persisted_commit_result(receipt);
        let loaded = load_state(&store, "capture-race").await;
        assert_eq!(
            loaded.plugin_state().unwrap().plugins["mock"].values["value"],
            serde_json::json!(2)
        );
    }

    /// FIG-6084: fresh admission and reconstruction share the host's bounds;
    /// replay installs an already recorded candidate under a smaller policy.
    #[tokio::test]
    async fn host_state_limits_survive_sqlite_reopen_and_transition() {
        use crate::plugin::{
            PluginDeclaration, PluginSpec, PluginTransitionBase, PluginTransitionId,
            PluginTransitionRequest, SessionAuthorityContext, StaticPluginFactory,
        };
        use crate::tool_run::{StateCommandLimits, StateCommandRefusal, StateResolutionOutcome};

        let make_host = |limits| {
            let mut policy = crate::runtime::RuntimeDataLimits::standard();
            policy.plugin_state = limits;
            crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
                PluginDeclaration::initial("mock"),
                PluginSpec::new(),
            ))])
            .with_data_limits(policy)
        };
        let address = |step| {
            crate::EffectAddress::new(crate::ExecutionScope::turn("state-limits", "run"), step)
                .unwrap()
        };
        let wide = serde_json::Value::String("x".repeat(64 * 1024));
        let default_host = make_host(StateCommandLimits::PUBLISHED);
        let default_session = default_host
            .build_session(PluginSessionRequest::creation(
                "state-limits",
                SessionAuthorityContext::ambient_fixture(),
            ))
            .unwrap();
        let refused = crate::publish_plugin_state(
            &default_session,
            "mock",
            address("default-publish"),
            crate::StateCommands::new().set("value", wide.clone()),
        )
        .await
        .unwrap();
        let crate::RuntimeEffectOutcome::PluginState { state, .. } = refused else {
            panic!("the recorded publication carries its refusal");
        };
        assert!(matches!(
            state.resolutions[0].outcome,
            StateResolutionOutcome::Refused {
                refusal: StateCommandRefusal::ValueTooLarge { limit, .. }
            } if limit == StateCommandLimits::PUBLISHED.max_value_bytes
        ));
        assert!(
            crate::plugin_state_view(&default_session, "mock")
                .get("value")
                .is_none()
        );
        drop(default_session);

        let limits = StateCommandLimits {
            max_value_bytes: 96 * 1024,
            max_namespace_bytes: 256 * 1024,
            max_encoded_bytes: 256 * 1024,
            ..StateCommandLimits::PUBLISHED
        };
        let host = make_host(limits);
        let plugins = host
            .build_session(PluginSessionRequest::creation(
                "state-limits",
                SessionAuthorityContext::ambient_fixture(),
            ))
            .unwrap();
        // Three separately admitted values also exceed the old namespace bound.
        for key in ["value", "second", "third"] {
            crate::publish_plugin_state(
                &plugins,
                "mock",
                address(key),
                crate::StateCommands::new().set(key, wide.clone()),
            )
            .await
            .unwrap();
        }
        for key in ["value", "second", "third"] {
            assert_eq!(
                crate::plugin_state_view(&plugins, "mock").get(key),
                Some(wide.clone())
            );
        }
        let store = session_store("state-limits").await;
        let mut runtime = crate::RuntimeSessionState {
            session_id: "state-limits".into(),
            ..crate::RuntimeSessionState::ambient_fixture(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
            ))
        };
        runtime.refresh_plugin_states(&plugins).unwrap();
        crate::testing::store_fixtures::commit_runtime_state_for_test(
            &store,
            crate::RuntimeCommit::persisted_state_for_test(&runtime),
            "state-limits-commit",
        )
        .await
        .unwrap();
        drop(plugins);
        drop(host);

        let loaded = load_state(&store, "state-limits").await;
        let snapshot = loaded.plugin_state().unwrap();
        let reopened_host = make_host(limits);
        let reopened = reopened_host
            .build_session(PluginSessionRequest::rematerialization(
                "state-limits",
                snapshot,
                SessionAuthorityContext::ambient_fixture(),
            ))
            .expect("the configured bounds also admit the reopened state");
        assert_eq!(
            crate::plugin_state_view(&reopened, "mock").get("value"),
            Some(wide)
        );
        let request = PluginTransitionRequest {
            id: PluginTransitionId(address("transition")),
            owner: crate::RuntimeOwner::Session("state-limits".into()),
            base: PluginTransitionBase::Session {
                head: crate::store::SessionHeadRef {
                    generation: 0,
                    revision: 0,
                    leaf: None,
                    checkpoint: None,
                },
            },
            target: crate::store::plugin_writers::PluginAdmission::from_plugins(vec![
                crate::store::plugin_writers::AdmittedPlugin {
                    plugin: "mock".into(),
                    writer: crate::FormatVersion::ONE,
                },
            ]),
        };
        let record =
            reopened_host.transition_plugins(request.clone(), snapshot, &Default::default());
        assert_eq!(&record.candidate().unwrap().0, snapshot);
        let refusal = default_host.transition_plugins(request, snapshot, &Default::default());
        assert!(matches!(
            refusal.candidate(),
            Err(crate::PluginError::State(crate::PluginStateError::ValueTooLarge { limit, .. }))
                if limit == StateCommandLimits::PUBLISHED.max_value_bytes
        ));

        let namespace_host = make_host(StateCommandLimits {
            max_namespace_bytes: 128 * 1024,
            ..limits
        });
        assert!(matches!(
            namespace_host.decode_state(snapshot),
            Err(crate::PluginError::State(crate::PluginStateError::StoreTooLarge { limit, .. }))
                if limit == 128 * 1024
        ));

        // Reconstruct from a recorded transition without admitting its postimage again.
        let bytes = rmp_serde::to_vec_named(&record).unwrap();
        let recorded = rmp_serde::from_slice(&bytes).unwrap();
        let replay = default_host
            .defer_session(PluginSessionRequest::rematerialization(
                "state-limits",
                &Default::default(),
                SessionAuthorityContext::ambient_fixture(),
            ))
            .unwrap();
        replay.adopt_plugin_transition(&recorded).unwrap();
        replay
            .materialize()
            .expect("recorded postimages ignore the current admission limits");
        assert_eq!(&replay.export_state(), snapshot);
    }
}
