use super::*;

fn yesterday_prompt_layer() -> crate::PromptLayer {
    crate::PromptLayer::new().with_contribution(crate::PromptContribution::guidance(
        "Yesterday's operating policy",
        "Continue with the session-specific policy.",
    ))
}

#[test]
fn persisted_session_config_without_prompt_keeps_the_historical_empty_layer() {
    let config = crate::PersistedSessionConfig {
        provider_id: "stored-provider".to_string(),
        model: crate::ModelSpec::default(),
        turn_budget: crate::TurnBudget::Unbounded,
        prompt: yesterday_prompt_layer(),
    };
    let mut old_writer_value = serde_json::to_value(config).expect("serialize current config");
    let old_writer_object = old_writer_value
        .as_object_mut()
        .expect("persisted config is an object");
    assert!(
        old_writer_object.remove("prompt").is_some(),
        "the compatibility probe strips exactly the field introduced by FIG-1376"
    );
    assert_eq!(
        old_writer_value,
        serde_json::json!({
            "provider_id": "stored-provider",
            "model": {
                "id": "",
                "variant": "provider_default",
                "limits": { "context_window_tokens": 1 }
            },
            "turn_budget": "unbounded"
        }),
        "the remaining value must be exactly the pre-FIG-1376 writer shape"
    );

    let restored: crate::PersistedSessionConfig =
        serde_json::from_value(old_writer_value).expect("old config remains readable");
    assert!(
        restored.prompt.is_empty(),
        "an absent field must restore the empty layer old code reconstructed"
    );
}

#[test]
fn session_created_yesterday_cold_loads_its_persisted_prompt_layer() {
    let expected_prompt = yesterday_prompt_layer();
    let yesterday_head_json = serde_json::to_string(&SessionHeadPayload {
        schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: "created-yesterday".to_string(),
        config: crate::PersistedSessionConfig {
            provider_id: "stored-provider".to_string(),
            model: crate::ModelSpec::default(),
            turn_budget: crate::TurnBudget::Unbounded,
            prompt: expected_prompt.clone(),
        },
        current_frame_node_id: None,
    })
    .expect("serialize yesterday's session head");
    let decoded: SessionHeadPayload = decode_versioned_json_record(
        &yesterday_head_json,
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect("decode yesterday's session head");
    let restored = persisted_session_state_from_head(
        SessionHead {
            session_id: decoded.session_id,
            head_revision: 7,
            current_frame_node_id: decoded.current_frame_node_id,
            graph: crate::SessionGraph::default(),
            config: decoded.config,
            checkpoint_ref: None,
            token_ledger: Vec::new(),
        },
        None,
    )
    .expect("cold-load yesterday's session");

    assert_eq!(restored.policy.prompt, expected_prompt);
}
