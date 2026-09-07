use super::*;

#[test]
fn google_dialect_is_closed_host_data_and_survives_the_wire_mirror() {
    use lash_core::{GoogleDialect, ModelCapability};
    assert_eq!(
        serde_json::from_value::<ModelCapability>(serde_json::json!({}))
            .unwrap()
            .google_dialect,
        GoogleDialect::Legacy
    );
    assert_eq!(
        serde_json::from_value::<RemoteModelCapability>(serde_json::json!({}))
            .unwrap()
            .google_dialect,
        RemoteGoogleDialect::Legacy
    );
    assert_eq!(
        serde_json::to_value(ModelCapability::default()).unwrap(),
        serde_json::json!({})
    );
    assert_eq!(
        serde_json::to_value(RemoteModelCapability::default()).unwrap(),
        serde_json::json!({})
    );
    assert!(
        serde_json::from_value::<ModelCapability>(serde_json::json!({"google_dialect":"future"}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<RemoteModelCapability>(
            serde_json::json!({"google_dialect":"future"})
        )
        .is_err()
    );
    for (core, remote, literal) in [
        (GoogleDialect::Legacy, RemoteGoogleDialect::Legacy, "legacy"),
        (
            GoogleDialect::Gemini3,
            RemoteGoogleDialect::Gemini3,
            "gemini3",
        ),
        (
            GoogleDialect::ClaudeOnVertex,
            RemoteGoogleDialect::ClaudeOnVertex,
            "claude_on_vertex",
        ),
    ] {
        assert_eq!(serde_json::to_value(core).unwrap(), literal);
        assert_eq!(serde_json::to_value(remote).unwrap(), literal);
        let capability = ModelCapability {
            attachment_acceptance: Default::default(),
            google_dialect: core,
            ..Default::default()
        };
        assert_eq!(capability.is_empty(), core == GoogleDialect::Legacy);
        let wire: RemoteModelCapability = capability.clone().into();
        assert_eq!(wire.google_dialect, remote);
        assert_eq!(wire.is_empty(), core == GoogleDialect::Legacy);
        let decoded: RemoteModelCapability =
            serde_json::from_value(serde_json::to_value(wire).unwrap()).unwrap();
        let restored: ModelCapability = decoded.into();
        assert_eq!(restored, capability);
    }
}
