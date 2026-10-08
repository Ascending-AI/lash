use super::*;
use pretty_assertions::assert_eq;

// Seed generation five durably, assert cold construction is read-only, then
// publish through the registered plugin's recorded commands.
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
    let plugins = support::construct(
        &fixture.host(),
        id,
        None,
        lash_core::plugin::SessionAuthorityContext::ambient_fixture(),
    )
    .await;
    let handle = fixture.state(id);
    for (name, commands) in [
        (
            "seed-counter",
            StateCommands::new().set("counter", serde_json::json!(true)),
        ),
        (
            "seed-large-a",
            StateCommands::new().set("large-a", serde_json::json!("x".repeat(32766))),
        ),
        (
            "seed-large-b",
            StateCommands::new().set("large-b", serde_json::json!("x".repeat(32766))),
        ),
        (
            "seed-large-c",
            StateCommands::new().set("large-c", serde_json::json!("x".repeat(32766))),
        ),
        (
            "seed-padding",
            StateCommands::new().set("padding", serde_json::json!("x".repeat(100))),
        ),
    ] {
        support::publish(&plugins, MOCK, name, commands).await;
    }
    assert_eq!(handle.generation(), 5);
    let mut state = RuntimeSessionState {
        session_id: id.parse().unwrap(),
        ..RuntimeSessionState::ambient_fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
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
        lash_core::plugin::SessionAuthorityContext::ambient_fixture(),
    )
    .await;
    let handle = rebuilt.state(id);
    assert_eq!(handle.generation(), 5, "cold construction changes no state");
    match registration {
        Registration::Remove => {
            support::publish(
                &plugins,
                MOCK,
                "remove-counter",
                StateCommands::new().remove("counter").remove("absent"),
            )
            .await;
            assert_eq!(handle.generation(), 6);
            assert_eq!(handle.get("counter"), None);
        }
        Registration::Admission => {
            support::publish(
                &plugins,
                MOCK,
                "overflow",
                StateCommands::new()
                    .set("small", serde_json::json!(1))
                    .set("overflow", serde_json::json!("x".repeat(32766))),
            )
            .await;
            assert_eq!(handle.generation(), 6, "a refusal is one publication");
            assert_eq!(handle.get("overflow"), None);
            assert_eq!(handle.get("small"), None, "a refusal publishes no command");
            support::publish(
                &plugins,
                MOCK,
                "accepted",
                StateCommands::new().set("accepted", serde_json::json!("x".repeat(1024))),
            )
            .await;
            assert_eq!(handle.generation(), 7);
        }
        Registration::None => unreachable!(),
    }
    durable.refresh_plugin_states(&plugins).unwrap();
    commit(&store, &mut durable).await;
    let final_state =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(final_state.plugin_state(), Some(&plugins.export_state()));
    let namespace = &final_state.plugin_state().unwrap().plugins[MOCK];
    match registration {
        Registration::Remove => {
            assert_eq!(namespace.generation, 6);
            assert!(!namespace.values.contains_key("counter"));
        }
        Registration::Admission => {
            assert_eq!(namespace.generation, 7);
            assert!(!namespace.values.contains_key("overflow"));
            assert_eq!(
                namespace.values["accepted"],
                serde_json::json!("x".repeat(1024))
            );
        }
        Registration::None => unreachable!(),
    }
}
