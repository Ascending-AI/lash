use super::*;

#[test]
fn persisted_state_hydrates_provider_id_without_live_provider_rebinding() {
    let state = persisted_session_state_from_head(
        SessionHead {
            session_id: "stored".to_string(),
            head_revision: 7,
            current_frame_node_id: None,
            graph: crate::SessionGraph::default(),
            config: crate::PersistedSessionConfig {
                provider_id: "stored-provider".to_string(),
                model: crate::ModelSpec::default(),
                turn_budget: crate::TurnBudget::Unbounded,
                prompt: Some(crate::PromptLayer::new()),
                generation: crate::GenerationOptions::default(),
                tool_access: crate::SessionToolAccess::default(),
                subagent: None,
                protocol_turn_options: None,
            },
            checkpoint_ref: None,
            token_ledger: Vec::new(),
        },
        None,
    )
    .expect("valid persisted state");

    assert_eq!(state.policy.recorded_provider_id(), "stored-provider");
    assert_eq!(state.head_revision, 7);
}

#[test]
fn versioned_json_record_rejects_missing_schema_version() {
    let err = decode_versioned_json_record::<SessionHeadPayload>(
        "{}",
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect_err("pre-versioned session head should fail");

    assert!(matches!(
        err,
        StoreError::MissingRecordSchemaVersion {
            record_kind: "SessionHeadMeta",
            expected: SESSION_HEAD_META_SCHEMA_VERSION
        }
    ));
}

#[test]
fn versioned_json_record_rejects_invalid_schema_version() {
    let err = decode_versioned_json_record::<SessionHeadPayload>(
        r#"{"schema_version":"1"}"#,
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect_err("invalid session head schema version should fail");

    assert!(matches!(
        err,
        StoreError::InvalidRecordSchemaVersion {
            record_kind: "SessionHeadMeta",
            expected: SESSION_HEAD_META_SCHEMA_VERSION,
            ..
        }
    ));
}

#[test]
fn versioned_json_record_rejects_unsupported_schema_version() {
    let unsupported = SESSION_HEAD_META_SCHEMA_VERSION + 1;
    let err = decode_versioned_json_record::<SessionHeadPayload>(
        &format!(r#"{{"schema_version":{unsupported}}}"#),
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect_err("unsupported session head schema version should fail");

    assert!(matches!(
        err,
        StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "SessionHeadMeta",
            actual,
            expected: SESSION_HEAD_META_SCHEMA_VERSION
        } if actual == unsupported
    ));
}

#[test]
fn session_meta_rejects_unknown_durable_fields() {
    let error = serde_json::from_str::<SessionMeta>(
        r#"{
            "session_id":"stored",
            "session_name":"stored",
            "created_at":"2026-08-01T00:00:00Z",
            "model":"example",
            "cwd":"/tmp",
            "relation":{"kind":"root"}
        }"#,
    )
    .expect_err("pre-cutover session metadata must not decode by omission");

    assert!(
        error.to_string().contains("unknown field `session_name`"),
        "strict decode must name the first obsolete field: {error}"
    );
}

#[test]
fn session_meta_rejects_unknown_fields_in_nested_relation() {
    let error = serde_json::from_str::<SessionMeta>(
        r#"{
            "session_id":"stored",
            "relation":{
                "kind":"child",
                "parent_session_id":"parent",
                "legacy":true
            }
        }"#,
    )
    .expect_err("nested durable relation fields must not decode by omission");

    assert!(
        error.to_string().contains("unknown field `legacy`"),
        "strict nested decode must name the obsolete relation field: {error}"
    );
}

#[test]
fn session_meta_rejects_unknown_fields_in_nested_causal_ref() {
    let error = serde_json::from_str::<SessionMeta>(
        r#"{
            "session_id":"stored",
            "relation":{
                "kind":"child",
                "parent_session_id":"parent",
                "caused_by":{
                    "type":"turn",
                    "session_id":"source",
                    "turn_id":"turn",
                    "legacy":true
                }
            }
        }"#,
    )
    .expect_err("nested durable causal fields must not decode by omission");

    assert!(
        error.to_string().contains("unknown field `legacy`"),
        "strict nested decode must name the obsolete causal field: {error}"
    );
}

#[test]
fn session_meta_rejects_extra_observer_inheritance_variants() {
    let error = serde_json::from_str::<SessionMeta>(
        r#"{
            "session_id":"stored",
            "relation":{
                "kind":"fork",
                "source_session_id":"source",
                "source_node_id":"node",
                "observer_inheritance":{
                    "only":["process"],
                    "legacy":true
                }
            }
        }"#,
    )
    .expect_err("externally tagged nested enums must reject extra variants");

    assert!(
        error.to_string().contains("expected map with a single key"),
        "externally tagged enum must reject the second variant key: {error}"
    );
}

fn options(payload: serde_json::Value) -> crate::ProtocolTurnOptions {
    crate::ProtocolTurnOptions { payload }
}

fn head_with_protocol_turn_options(
    config_options: Option<crate::ProtocolTurnOptions>,
) -> SessionHead {
    let mut config = crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded);
    config.protocol_turn_options = config_options;
    SessionHead {
        session_id: "stored".to_string(),
        head_revision: 3,
        current_frame_node_id: None,
        graph: crate::SessionGraph::default(),
        config,
        checkpoint_ref: None,
        token_ledger: Vec::new(),
    }
}

fn checkpoint_with_protocol_turn_options(
    options: crate::ProtocolTurnOptions,
) -> HydratedSessionCheckpoint {
    let mut state =
        crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    state.checkpoint_components =
        crate::runtime::state::RuntimeCheckpointComponents::complete_empty();
    state.protocol_turn_options = options;
    build_checkpoint_from_persisted_state(&state).expect("build fixture checkpoint")
}

/// FIG-2479: the commanded head value (SESSION_HEAD_META v6) is authoritative
/// over the checkpoint's persisted-turn-state copy on cold load.
#[test]
fn head_protocol_turn_options_override_the_checkpoint_copy_on_load() {
    let head_options = options(serde_json::json!({"dialect": "head-settled"}));
    let checkpoint = checkpoint_with_protocol_turn_options(options(
        serde_json::json!({"dialect": "stale-checkpoint-copy"}),
    ));
    let state = persisted_session_state_from_head(
        head_with_protocol_turn_options(Some(head_options.clone())),
        Some(checkpoint),
    )
    .expect("valid persisted state");
    assert_eq!(state.protocol_turn_options, head_options);
}

/// A pre-v6-content head (`protocol_turn_options: None`) keeps the legacy
/// checkpoint fallback.
#[test]
fn absent_head_protocol_turn_options_fall_back_to_the_checkpoint_copy() {
    let checkpoint_options = options(serde_json::json!({"dialect": "checkpoint-copy"}));
    let checkpoint = checkpoint_with_protocol_turn_options(checkpoint_options.clone());
    let state =
        persisted_session_state_from_head(head_with_protocol_turn_options(None), Some(checkpoint))
            .expect("valid persisted state");
    assert_eq!(state.protocol_turn_options, checkpoint_options);
}

/// Refusal witness (FIG-2479): a v5 head — the immediate predecessor of the
/// protocol-turn-options head generation — is refused by the strict
/// schema-version fence every store backend decodes through.
#[test]
fn immediate_predecessor_head_meta_v5_is_refused() {
    const PREDECESSOR: u32 = 5;
    assert_eq!(
        PREDECESSOR + 1,
        SESSION_HEAD_META_SCHEMA_VERSION,
        "session-head schema adjacency pin"
    );
    let err = decode_versioned_json_record::<SessionHeadPayload>(
        &format!(r#"{{"schema_version":{PREDECESSOR}}}"#),
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect_err("v5 session head must be refused");
    assert!(matches!(
        err,
        StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "SessionHeadMeta",
            actual: PREDECESSOR,
            expected: SESSION_HEAD_META_SCHEMA_VERSION
        }
    ));
}
