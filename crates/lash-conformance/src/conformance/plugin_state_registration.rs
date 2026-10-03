use super::*;
use pretty_assertions::assert_eq;

#[expect(
    clippy::unwrap_used,
    reason = "conformance-law fixture: the unwrap mirrors the setup above"
)]
pub(super) async fn registration_state_law(
    store: Arc<dyn RuntimeStore>,
    id: &str,
    registration: Registration,
) {
    let fixture = MockPlugin::default();
    let plugins = support::construct(&fixture.host(), id, None, Default::default()).await;
    let handle = fixture.state(id);
    support::callback(&plugins, "registration-seed", {
        let handle = handle.clone();
        async move {
            handle.set("counter", serde_json::json!(true)).unwrap();
            for key in ["large-a", "large-b", "large-c"] {
                handle
                    .set(key, serde_json::json!("x".repeat(32766)))
                    .unwrap();
            }
            handle
                .set("padding", serde_json::json!("x".repeat(100)))
                .unwrap();
        }
    })
    .await;
    assert_eq!(handle.generation(), 5);
    let mut state = RuntimeSessionState {
        session_id: id.parse().unwrap(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.refresh_plugin_states(&plugins).unwrap();
    commit(&store, &mut state).await;
    drop(plugins);
    let mut durable =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(id))
            .await
            .unwrap()
            .unwrap();
    let rebuilt = MockPlugin {
        registration,
        ..Default::default()
    };
    let plugins = support::construct(
        &rebuilt.host(),
        id,
        durable.plugin_state(),
        Default::default(),
    )
    .await;
    let handle = rebuilt.state(id);
    assert_eq!(handle.generation(), 5, "cold construction changes no state");
    support::callback(&plugins, "registered-callback", async move {
        match registration {
            Registration::Remove => {
                assert_eq!(handle.remove("counter").unwrap(), 6);
                assert_eq!(handle.remove("absent").unwrap(), 6);
                assert_eq!(handle.get("counter"), None);
            }
            Registration::Admission => {
                assert!(matches!(
                    handle.set("overflow", serde_json::json!("x".repeat(32766))),
                    Err(PluginStateError::StoreTooLarge { .. })
                ));
                assert_eq!(handle.generation(), 5);
                assert_eq!(handle.get("overflow"), None);
                assert_eq!(
                    handle
                        .set("accepted", serde_json::json!("x".repeat(1024)))
                        .unwrap(),
                    6
                );
            }
            Registration::None => unreachable!(),
        }
    })
    .await;
    assert_eq!(rebuilt.state(id).generation(), 6);
    durable.refresh_plugin_states(&plugins).unwrap();
    commit(&store, &mut durable).await;
    let final_state =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(final_state.plugin_state(), Some(&plugins.export_state()));
    let namespace = &final_state.plugin_state().unwrap().plugins["mock-state"];
    assert_eq!(namespace.generation, 6);
    match registration {
        Registration::Remove => assert!(!namespace.values.contains_key("counter")),
        Registration::Admission => {
            assert!(!namespace.values.contains_key("overflow"));
            assert_eq!(
                namespace.values["accepted"],
                serde_json::json!("x".repeat(1024))
            );
        }
        Registration::None => unreachable!(),
    }
}
