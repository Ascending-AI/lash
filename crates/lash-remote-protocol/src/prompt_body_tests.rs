use super::*;

#[test]
fn contribution_wire_body_rejects_a_second_slot_identity() {
    let wire = serde_json::json!({
        "title": "Runtime", "priority": 7, "gate": {"tools": ["lookup"]}, "content": "Use the session context."
    });
    let body: RemotePromptContribution = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&body).unwrap(), wire);
    let mut legacy = wire;
    legacy["slot"] = serde_json::json!("guidance");
    let error = serde_json::from_value::<RemotePromptContribution>(legacy).unwrap_err();
    assert!(
        error.to_string().contains("unknown field `slot`"),
        "{error}"
    );
}

#[test]
#[cfg(feature = "core-conversions")]
fn prompt_map_wire_round_trip_preserves_authoring() {
    let authored =
        lash_core::PromptContribution::guidance("Guide", "Keep this guidance.").with_priority(7);
    let core = lash_core::PromptLayer::new().with_contribution(authored.clone());
    let wire = RemotePromptLayer::from(core);
    assert_eq!(
        serde_json::to_value(&wire).unwrap(),
        serde_json::json!({
            "slots": {"guidance": {"reset": false, "contributions": [{
                "title": "Guide", "priority": 7, "content": "Keep this guidance."
            }]}}
        })
    );
    let restored: lash_core::PromptLayer =
        serde_json::from_value::<RemotePromptLayer>(serde_json::to_value(wire).unwrap())
            .unwrap()
            .into();
    let flat = lash_core::session_model::prompt::resolve_prompt_layers([&restored]).contributions;
    assert_eq!(flat, vec![authored.clone()]);
}
