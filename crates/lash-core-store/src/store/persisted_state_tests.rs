use super::*;

#[test]
fn persisted_state_hydrates_the_recorded_llm_profile_without_live_rebinding() {
    let recorded = crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
        crate::LlmProfileKey::new("stored-key"),
        lash_core_llm::llm_profile::LlmProfileMetadata::builder("stored-wire-model")
            .cache_retention(lash_core_llm::provider::CacheRetention::Short)
            .context_window_tokens(4096)
            .build()
            .expect("model"),
    ));
    let state = persisted_session_state_from_head(
        SessionId::from("stored"),
        7,
        crate::PersistedSessionConfig {
            model: Some(recorded.clone()),
            attachment_acceptance: Default::default(),
            turn_budget: crate::TurnBudget::Unbounded,
            max_tool_calls: crate::MaxToolCalls::new(1024),
            no_progress_budget: crate::NoProgressBudget::bounded(12),
            charge_safety: crate::ChargeSafetyPolicy::default(),
            generation: crate::GenerationOptions::default(),
            tool_access: crate::SessionToolAccess::ambient(),
            prompt_plan: crate::prompt_sections::PromptPlan::default(),
            config_revision: 0,
            undelivered_change: None,
            plugin_config: crate::PluginConfig::default(),
        },
        None,
    )
    .expect("valid persisted state");

    assert_eq!(state.policy.model, Some(recorded));
    assert_eq!(state.head_revision, 7);
}

#[test]
fn session_head_decode_requires_recorded_identity_even_for_root() {
    let mut value = serde_json::to_value(SessionHeadPayload::default()).expect("head JSON");
    value
        .as_object_mut()
        .expect("head object")
        .remove("session_id");
    let json = serde_json::to_string(&value).expect("head without identity");
    let exact = decode_versioned_json_record::<SessionHeadPayload>(
        &json,
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    );
    let fleet = decode_versioned_json_record_for_fleet::<SessionHeadPayload>(
        &json,
        "SessionHeadMeta",
        crate::surface_format!(SESSION_HEAD_META_SCHEMA_VERSION),
        FleetFormat::current(),
    );
    for result in [exact, fleet] {
        let error = result.expect_err("missing identity must not decode as root");
        assert!(
            matches!(
                &error,
                StoreError::StoredDataCorrupt {
                    record_kind: "SessionHeadMeta",
                    message,
                } if message.contains("missing field `session_id`")
            ),
            "the refusal names the missing identity: {error:?}"
        );
        assert!(!error.is_transient(), "{error:?}");
    }
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
fn session_meta_rejects_removed_observer_inheritance() {
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
    .expect_err("removed selector must fail closed");

    assert!(
        error
            .to_string()
            .contains("unknown field `observer_inheritance`"),
        "removed selector must be named: {error}"
    );
}

#[test]
fn fig1123_reasoning_retention_policy_survives_session_head_cold_decode() {
    let retention = crate::ReasoningRetentionPolicy {
        capability: Some(crate::ReasoningRetentionCapability::OpenAiContext {
            supported: vec![crate::OpenAiReasoningContext::CurrentTurn],
        }),
        selection: crate::ReasoningRetentionSelection::OpenAiContext {
            context: crate::OpenAiReasoningContext::CurrentTurn,
        },
    };
    let mut config = crate::PersistedSessionConfig::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
        crate::SessionToolAccess::ambient(),
    );
    config.model = Some(crate::LlmProfileConfig::new(
        crate::RecordedLlmProfile::mint(
            crate::LlmProfileKey::new("model"),
            lash_core_llm::llm_profile::LlmProfileMetadata::builder("model")
                .cache_retention(lash_core_llm::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()
                .expect("model")
                .with_capability(crate::LlmProfileCapability {
                    reasoning_retention: Box::new(retention.clone()),
                    ..Default::default()
                }),
        ),
    ));
    let payload = SessionHeadPayload {
        schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
        session_id: SessionId::from("retention-cold-reopen"),
        config,
    };

    let json = serde_json::to_string(&payload).expect("head JSON");
    let decoded = decode_versioned_json_record::<SessionHeadPayload>(
        &json,
        "SessionHeadMeta",
        SESSION_HEAD_META_SCHEMA_VERSION,
    )
    .expect("current head decodes");

    assert_eq!(
        *decoded
            .config
            .model
            .expect("recorded model")
            .metadata()
            .capability
            .reasoning_retention,
        retention
    );
}

#[test]
fn fleet_reader_refuses_a_record_outside_the_read_window() {
    // The fleet's writers emit version 1 for this surface; the build knows 2.
    let surface = SurfaceFormat::of("PROBE_SURFACE_VERSION", 2);
    let fleet = FleetFormat::current().with_writer_pins(&[WriterPin {
        constant: "PROBE_SURFACE_VERSION",
        generation: FLEET_FORMAT_VERSION,
        version: 1,
    }]);
    // A version that is neither the build's newest nor `F`'s recorded writer
    // version is refused at admission, exactly as the exact-version check
    // refuses it.
    let err = decode_versioned_json_record_for_fleet::<serde_json::Value>(
        r#"{"schema_version":3,"payload":"x"}"#,
        "Probe",
        surface,
        fleet,
    )
    .expect_err("a version outside the {recorded, newest} window is refused");
    assert!(matches!(
        err,
        StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "Probe",
            actual: 3,
            expected: 2,
        }
    ));
}

#[test]
fn fleet_reader_fails_closed_on_an_admitted_older_version_without_an_upcaster() {
    // ADR 0106 §2's `[N-1, N]` window admits the version `F` records for the
    // surface — but no `RecordUpcaster` is registered for the walk, so the
    // admitted older payload refuses rather than decoding at a shape it was
    // never written for (no fake previous format is invented, FIG-3796).
    let surface = SurfaceFormat::of("PROBE_SURFACE_VERSION", 2);
    let fleet = FleetFormat::current().with_writer_pins(&[WriterPin {
        constant: "PROBE_SURFACE_VERSION",
        generation: FLEET_FORMAT_VERSION,
        version: 1,
    }]);
    let err = decode_versioned_json_record_for_fleet::<serde_json::Value>(
        r#"{"schema_version":1,"payload":"x"}"#,
        "Probe",
        surface,
        fleet,
    )
    .expect_err("an admitted older version with no upcaster refuses, not decodes");
    assert!(matches!(
        err,
        StoreError::UnsupportedRecordSchemaVersion {
            record_kind: "Probe",
            actual: 1,
            expected: 2,
        }
    ));
}

/// Bytes a versioned record's decoder refuses are refused on every read:
/// corrupt stored data, never a backend fault a retry repairs (FIG-4628).
#[test]
fn an_undecodable_versioned_record_is_corrupt_and_never_transient() {
    let mistyped = format!(r#"{{"schema_version":{SESSION_HEAD_META_SCHEMA_VERSION},"policy":7}}"#);
    for json in ["{not-current-json", mistyped.as_str()] {
        let exact = decode_versioned_json_record::<SessionHeadPayload>(
            json,
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("the exact-version decoder refuses the bytes");
        let fleet = decode_versioned_json_record_for_fleet::<SessionHeadPayload>(
            json,
            "SessionHeadMeta",
            SurfaceFormat::of(
                "SESSION_HEAD_META_SCHEMA_VERSION",
                SESSION_HEAD_META_SCHEMA_VERSION,
            ),
            FleetFormat::current(),
        )
        .expect_err("the fleet decoder refuses the bytes");
        for error in [exact, fleet] {
            assert!(
                matches!(
                    error,
                    StoreError::StoredDataCorrupt {
                        record_kind: "SessionHeadMeta",
                        ..
                    }
                ),
                "{json}: {error:?}"
            );
            assert!(!error.is_transient(), "{json}: {error:?}");
        }
    }
}
