use super::*;
use pretty_assertions::assert_eq;

/// Execute the plugin-state boundary and fork laws, returning decoded checkpoint
/// bodies for independent cross-backend comparison.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn plugin_state_boundary_trace(
    store: Arc<dyn RuntimeStore>,
    parent_id: &str,
    child_store: Arc<dyn RuntimeStore>,
    child_id: &str,
) -> Vec<lash_core::PluginState> {
    Box::pin(async move {
        let fixture = MockPlugin::default();
        let host = fixture.host();
        let plugins = support::construct(&host, parent_id, None, Default::default()).await;
        let handle = fixture.state(parent_id);
        let mut state = RuntimeSessionState {
            session_id: parent_id.parse().unwrap(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        state
            .refresh_plugin_states(&plugins)
            .expect("the live plugin state is captured");
        commit(&store, &mut state).await;
        let before = state.plugin_state_ref().cloned();
        let first = support::publish(
            &plugins,
            MOCK,
            "first-write",
            StateCommands::new().set("counter", serde_json::json!(1)),
        )
        .await;
        assert_eq!(handle.get("counter"), Some(serde_json::json!(1)));
        assert_eq!(handle.generation(), 1);
        plugins
            .publish_effect_state(first)
            .expect("a repeated delivery is accepted");
        assert_eq!(
            handle.generation(),
            1,
            "a repeated delivery applies nothing"
        );
        let crash_state =
            crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(parent_id))
                .await
                .unwrap()
                .unwrap();
        let rebuilt_fixture = MockPlugin::default();
        let rebuilt_host = rebuilt_fixture.host();
        let rebuilt = support::construct(
            &rebuilt_host,
            parent_id,
            crash_state.plugin_state(),
            Default::default(),
        )
        .await;
        assert_eq!(
            rebuilt_fixture.state(parent_id).get("counter"),
            None,
            "a publication the head does not carry is not in a rebuild from it"
        );
        drop(rebuilt);
        state
            .refresh_plugin_states(&plugins)
            .expect("the live plugin state is captured");
        let changed = RuntimeCommit::persisted_state_for_test(&state);
        assert!(
            matches!(
                changed.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
                crate::HydratedCheckpointComponent::Changed { .. }
            ),
            "generation moved: checkpoint must be Changed"
        );
        commit(&store, &mut state).await;
        assert_ne!(state.plugin_state_ref(), before.as_ref());
        state
            .refresh_plugin_states(&plugins)
            .expect("the live plugin state is captured");
        let unchanged = RuntimeCommit::persisted_state_for_test(&state);
        assert!(
            matches!(
                unchanged.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
                crate::HydratedCheckpointComponent::Unchanged { .. }
            ),
            "generation unchanged: checkpoint must use its resident reference"
        );
        let durable =
            crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(parent_id))
                .await
                .unwrap()
                .unwrap();
        let rebuilt = support::construct(
            &rebuilt_host,
            parent_id,
            durable.plugin_state(),
            Default::default(),
        )
        .await;
        assert_eq!(
            rebuilt_fixture.state(parent_id).get("counter"),
            Some(serde_json::json!(1)),
            "committed publication survives process reconstruction"
        );
        assert_eq!(
            rebuilt_fixture.ready_values.lock_recover()[parent_id],
            Some(serde_json::json!(1)),
            "session_ready itself must see the committed value"
        );
        assert_eq!(rebuilt.export_state(), plugins.export_state());
        support::publish(
            &plugins,
            MOCK,
            "parent-fork-write",
            StateCommands::new().set("counter", serde_json::json!(2)),
        )
        .await;
        let child = plugins
            .fork_for_session(SessionId::fixture(child_id), Default::default())
            .unwrap();
        let child_handle = fixture.state(child_id);
        assert_eq!(child_handle.generation(), handle.generation());
        assert_eq!(
            child_handle.get("counter"),
            Some(serde_json::json!(2)),
            "fork includes the parent's published state"
        );
        support::publish(
            &plugins,
            MOCK,
            "parent-isolated-write",
            StateCommands::new().set("counter", serde_json::json!(3)),
        )
        .await;
        support::publish(
            &child,
            MOCK,
            "child-isolated-write",
            StateCommands::new().set("child-only", serde_json::json!(true)),
        )
        .await;
        assert_eq!(child_handle.get("counter"), Some(serde_json::json!(2)));
        assert_eq!(handle.get("child-only"), None);
        let mut child_state = RuntimeSessionState {
            session_id: child_id.parse().unwrap(),
            ..RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        child_state
            .refresh_plugin_states(&child)
            .expect("the live plugin state is captured");
        commit(&child_store, &mut child_state).await;
        let child_durable = crate::conformance::helpers::load_window_state(
            &child_store,
            &SessionId::fixture(child_id),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(child_durable.plugin_state(), Some(&child.export_state()));
        vec![
            crash_state.plugin_state().unwrap().clone(),
            durable.plugin_state().unwrap().clone(),
            child_durable.plugin_state().unwrap().clone(),
        ]
    })
    .await
}
