use super::*;

fn committed_prompt_layer() -> crate::PromptLayer {
    crate::PromptLayer::new().with_contribution(crate::PromptContribution::guidance(
        "Committed operating policy",
        "Continue with the session-specific policy.",
    ))
}

#[test]
fn legacy_config_keeps_prompt_absence_distinct() {
    let config = crate::PersistedSessionConfig {
        provider_id: "stored-provider".to_string(),
        model: crate::ModelSpec::default(),
        turn_budget: crate::TurnBudget::Unbounded,
        prompt: Some(committed_prompt_layer()),
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
    assert_eq!(
        restored.prompt, None,
        "an absent field must remain distinguishable from an explicit empty layer"
    );
}

#[test]
fn explicit_empty_prompt_is_serialized_as_present() {
    let value = serde_json::to_value(crate::PersistedSessionConfig {
        provider_id: "stored-provider".to_string(),
        model: crate::ModelSpec::default(),
        turn_budget: crate::TurnBudget::Unbounded,
        prompt: Some(crate::PromptLayer::new()),
    })
    .expect("serialize explicit empty prompt");

    assert_eq!(
        value.get("prompt"),
        Some(&serde_json::json!({})),
        "an explicit empty layer must not collapse into legacy absence"
    );
}

#[test]
fn committed_prompt_cold_loads_into_the_runtime_policy() {
    let expected_prompt = committed_prompt_layer();
    let committed_head_json = serde_json::to_string(&SessionHeadPayload {
        schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: "committed-session".to_string(),
        config: crate::PersistedSessionConfig {
            provider_id: "stored-provider".to_string(),
            model: crate::ModelSpec::default(),
            turn_budget: crate::TurnBudget::Unbounded,
            prompt: Some(expected_prompt.clone()),
        },
        current_frame_node_id: None,
    })
    .expect("serialize committed session head");
    let decoded: SessionHeadPayload = decode_versioned_json_record(
        &committed_head_json,
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect("decode committed session head");
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
    .expect("cold-load committed session");

    assert_eq!(restored.policy.prompt, expected_prompt);
}
