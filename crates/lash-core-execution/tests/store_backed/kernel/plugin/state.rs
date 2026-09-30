mod tests {
    use crate::plugin::PluginSessionRequest;
    use std::sync::Arc;

    use crate::SessionId;

    /// A memory backend's catalog holding the session the test commits.
    async fn session_store(session_id: &str) -> Arc<dyn crate::RuntimeStore> {
        let catalog = crate::support::memory_store_set()
            .await
            .session_store_factory();
        crate::SessionCatalogStore::admit_session(
            catalog.as_ref(),
            &crate::testing::store_fixtures::session_store_request(
                &SessionId::from(session_id),
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
        let view = crate::store::SessionStore::new(Arc::clone(store), SessionId::from(session_id))
            .expect("a valid session id");
        crate::store::load_session_window_state(&view, crate::store::WindowSelector::Current)
            .await
            .unwrap()
            .unwrap()
            .state
    }

    #[tokio::test]
    async fn checkpoint_component_changes_iff_mediated_generation_moves() {
        let host = crate::support::plugin_host(Vec::new());
        let plugins = host
            .build_session(PluginSessionRequest::creation(
                "generation-gate",
                Default::default(),
            ))
            .unwrap();
        let handle =
            crate::plugin_state_store(&plugins, &SessionId::from("generation-gate"), "mock");
        let store = session_store("generation-gate").await;
        let mut state = crate::RuntimeSessionState {
            session_id: "generation-gate".into(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.refresh_plugin_states(&plugins);
        let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
            &store,
            crate::RuntimeCommit::persisted_state_for_test(&state, &[]),
            "generation-gate",
        )
        .await
        .unwrap();
        state.apply_persisted_commit_result(receipt);
        state.refresh_plugin_states(&plugins);
        assert!(matches!(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .checkpoint
                .components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Unchanged { .. }
        ));
        handle.set("value", serde_json::json!(1)).unwrap();
        state.refresh_plugin_states(&plugins);
        assert!(
            matches!(
                crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                    .checkpoint
                    .components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
                crate::HydratedCheckpointComponent::Changed { .. }
            ),
            "accepted write must make the checkpoint component Changed"
        );
    }

    #[tokio::test]
    async fn write_after_capture_survives_commit_receipt_adoption() {
        let plugins = crate::support::plugin_host(Vec::new())
            .build_session(PluginSessionRequest::creation(
                "capture-race",
                Default::default(),
            ))
            .unwrap();
        let handle = crate::plugin_state_store(&plugins, &SessionId::from("capture-race"), "mock");
        let store = session_store("capture-race").await;
        let mut state = crate::RuntimeSessionState {
            session_id: "capture-race".into(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        handle.set("value", serde_json::json!(1)).unwrap();
        state.refresh_plugin_states(&plugins);
        let captured = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
        handle.set("value", serde_json::json!(2)).unwrap();
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
        state.refresh_plugin_states(&plugins);
        let next = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
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
