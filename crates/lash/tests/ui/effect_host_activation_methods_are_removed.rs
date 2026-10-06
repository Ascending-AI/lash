async fn check(
    session: lash::LashSession,
    scope: lash::runtime::ActorContext,
) {
    let _ = session
        .admin().triggers()
        .emit_with_effect_host("Button", "ui.button", "pressed", serde_json::json!({}), scope)
        .await;
    let _ = session
        .admin().triggers()
        .activate_with_effect_host("trigger:1", serde_json::json!({}), scope)
        .await;
    let _ = session
        .admin().triggers()
        .activate_source_type_with_effect_host("ui.button.pressed", serde_json::json!({}), scope)
        .await;
}

fn main() {}
