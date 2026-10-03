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
                Default::default(),
            ))
            .unwrap();
        let store = session_store("capture-race").await;
        let mut state = crate::RuntimeSessionState {
            session_id: "capture-race".into(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
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
}
