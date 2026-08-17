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
