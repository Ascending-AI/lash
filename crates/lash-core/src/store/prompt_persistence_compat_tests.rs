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
        generation: crate::GenerationOptions::default(),
        tool_access: crate::SessionToolAccess::default(),
        subagent: None,
        protocol_turn_options: None,
    };
    let mut old_writer_value = serde_json::to_value(config).expect("serialize current config");
    let old_writer_object = old_writer_value
        .as_object_mut()
        .expect("persisted config is an object");
    assert!(
        old_writer_object.remove("prompt").is_some(),
        "the compatibility probe strips exactly the field introduced by FIG-1376"
    );
    assert!(
        old_writer_object.remove("generation").is_some(),
        "the compatibility probe also strips the field introduced by FIG-1895"
    );
    assert!(
        old_writer_object.remove("tool_access").is_some(),
        "the compatibility probe strips authority made explicit by FIG-1954"
    );
    assert!(
        old_writer_object.remove("subagent").is_some(),
        "the compatibility probe strips subagent authority made explicit by FIG-1954"
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
    assert_eq!(
        restored.generation,
        crate::GenerationOptions::default(),
        "a head written before generation persistence must restore neutral intent"
    );
}

#[test]
fn legacy_config_without_authority_decodes_as_unrestricted_root() {
    let legacy = serde_json::json!({
        "provider_id": "provider",
        "model": {
            "id": "model",
            "variant": "provider_default",
            "limits": { "context_window_tokens": 4096 }
        },
        "turn_budget": "unbounded",
        "prompt": {},
        "generation": {}
    });

    let restored: crate::PersistedSessionConfig =
        serde_json::from_value(legacy).expect("legacy authority-free config must decode");

    assert_eq!(restored.tool_access, crate::SessionToolAccess::default());
    assert_eq!(restored.subagent, None);
}

#[test]
fn current_config_serializes_default_authority_explicitly() {
    let value = serde_json::to_value(crate::PersistedSessionConfig {
        provider_id: "stored-provider".to_string(),
        model: crate::ModelSpec::default(),
        turn_budget: crate::TurnBudget::Unbounded,
        prompt: Some(crate::PromptLayer::new()),
        generation: crate::GenerationOptions::default(),
        tool_access: crate::SessionToolAccess::default(),
        subagent: None,
        protocol_turn_options: None,
    })
    .expect("serialize current config");

    assert_eq!(value.get("tool_access"), Some(&serde_json::json!({})));
    assert_eq!(value.get("subagent"), Some(&serde_json::Value::Null));
}

#[test]
fn explicit_empty_prompt_is_serialized_as_present() {
    let value = serde_json::to_value(crate::PersistedSessionConfig {
        provider_id: "stored-provider".to_string(),
        model: crate::ModelSpec::default(),
        turn_budget: crate::TurnBudget::Unbounded,
        prompt: Some(crate::PromptLayer::new()),
        generation: crate::GenerationOptions::default(),
        tool_access: crate::SessionToolAccess::default(),
        subagent: None,
        protocol_turn_options: None,
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
            generation: crate::GenerationOptions::default(),
            tool_access: crate::SessionToolAccess::default(),
            subagent: None,
            protocol_turn_options: None,
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

#[test]
fn committed_generation_cold_loads_into_the_runtime_policy() {
    let expected_generation = crate::GenerationOptions {
        seed: Some(1895),
        output_token_cap: std::num::NonZeroUsize::new(1_895),
        ..crate::GenerationOptions::default()
    };
    let committed_head_json = serde_json::to_string(&SessionHeadPayload {
        schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: "committed-generation".to_string(),
        config: crate::PersistedSessionConfig {
            provider_id: "stored-provider".to_string(),
            model: crate::ModelSpec::default(),
            turn_budget: crate::TurnBudget::Unbounded,
            prompt: Some(crate::PromptLayer::new()),
            generation: expected_generation.clone(),
            tool_access: crate::SessionToolAccess::default(),
            subagent: None,
            protocol_turn_options: None,
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
    .expect("cold-load committed generation");

    assert_eq!(restored.policy.generation, expected_generation);
}

#[test]
fn persisted_head_and_frame_open_reject_legacy_slot_fields() {
    let prompt = committed_prompt_layer();
    let mut policy = crate::SessionPolicy::new(crate::TurnBudget::Unbounded);
    policy.prompt = prompt;
    let head = SessionHeadPayload {
        schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: "slot-body-session".into(),
        config: crate::PersistedSessionConfig::from(&policy),
        current_frame_node_id: None,
    };
    let mut head_json = serde_json::to_value(&head).unwrap();
    assert!(
        head_json["config"]["prompt"]["slots"]["guidance"]["contributions"][0]
            .get("slot")
            .is_none()
    );
    head_json["config"]["prompt"]["slots"]["guidance"]["contributions"][0]["slot"] =
        serde_json::json!("environment");
    let error = decode_versioned_json_record::<SessionHeadPayload>(
        &head_json.to_string(),
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("unknown field `slot`"),
        "{error}"
    );

    let node = crate::SessionNodeRecord {
        node_id: "slot-body-frame".into(),
        parent_node_id: None,
        timestamp: "2026-09-07T00:00:00Z".into(),
        payload: crate::SessionNodePayload::FrameOpen {
            frame_key: crate::FrameKey::from_caller_material("slot-body-frame").unwrap(),
            reason: crate::AgentFrameReason::initial(),
            assignment: crate::AgentFrameAssignment::from_policy(policy),
            protocol_turn_options: crate::ProtocolTurnOptions::default(),
        },
    };
    let current = node.encode_storage_body().unwrap();
    let restored =
        crate::SessionNodeRecord::decode_storage_body(node.node_id.clone(), None, &current)
            .unwrap();
    assert_eq!(restored.encode_storage_body().unwrap(), current);
    let mut legacy: serde_json::Value = serde_json::from_str(&current).unwrap();
    legacy["assignment"]["policy"]["prompt"]["slots"]["guidance"]["contributions"][0]["slot"] =
        serde_json::json!("environment");
    let error =
        crate::SessionNodeRecord::decode_storage_body(node.node_id, None, &legacy.to_string())
            .unwrap_err();
    assert!(
        error.to_string().contains("unknown field `slot`"),
        "{error}"
    );
}
