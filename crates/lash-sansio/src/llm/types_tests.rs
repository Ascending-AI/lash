use super::types::*;

fn route(provider: &str, endpoint: &str, model: &str) -> ProviderRouteIdentity {
    ProviderRouteIdentity::for_endpoint(provider, endpoint, model)
}

fn replay_request(blocks: Vec<LlmContentBlock>) -> LlmRequest {
    LlmRequest {
        model: "model-a".to_string(),
        messages: vec![LlmMessage::new(LlmRole::Assistant, blocks)],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: std::sync::Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: Default::default(),
        generation: Default::default(),
        scope: LlmRequestScope::new("session", "frame", "request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}

#[test]
fn route_identity_normalizes_endpoint_without_collapsing_distinct_gateways() {
    assert_eq!(
        route(
            "openai-compatible",
            " HTTPS://Gateway.Example/v1/ ",
            "model-a"
        ),
        route("openai-compatible", "https://gateway.example/v1", "model-a")
    );
    assert_ne!(
        route(
            "openai-compatible",
            "https://gateway-a.example/v1",
            "model-a"
        ),
        route(
            "openai-compatible",
            "https://gateway-b.example/v1",
            "model-a"
        )
    );
}

#[test]
fn endpoint_normalization_attack_matrix_is_fail_closed() {
    let userinfo_upper = route(
        "openai-compatible",
        "https://User:Secret@Gateway.Example/v1",
        "model-a",
    );
    let userinfo_lower = route(
        "openai-compatible",
        "https://user:secret@gateway.example/v1",
        "model-a",
    );
    assert_ne!(userinfo_upper, userinfo_lower);
    assert_eq!(
        userinfo_upper.validate_endpoint(),
        Err(ProviderEndpointError::UserinfoNotAllowed)
    );
    assert_eq!(
        userinfo_lower.validate_endpoint(),
        Err(ProviderEndpointError::UserinfoNotAllowed)
    );

    assert_ne!(
        route("openai", "https://api.example", "model-a"),
        route("openai", "https://api.example:443", "model-a"),
        "explicit default ports remain distinct from implicit ports"
    );
    assert_eq!(
        route("openai", "https://API.EXAMPLE", "model-a"),
        route("openai", "https://api.example/", "model-a"),
        "host case and empty path versus slash normalize"
    );
    assert_ne!(
        route("openai", "https://api.example/V1", "model-a"),
        route("openai", "https://api.example/v1", "model-a"),
        "path case is identity-significant"
    );
    assert_ne!(
        route("openai", "https://api.example/v1?region=eu", "model-a"),
        route("openai", "https://api.example/v1?region=us", "model-a"),
        "query strings are identity-significant when configured"
    );
}

#[test]
fn replay_gate_keeps_shared_blocks_on_the_no_drop_path() {
    let native = route("openai-compatible", "https://gateway.example/v1", "model-a");
    let mut request = replay_request(vec![LlmContentBlock::ToolCall {
        call_id: "call".to_string(),
        tool_name: "tool".to_string(),
        input_json: "{}".to_string(),
        replay: Some(ProviderReplayMeta {
            opaque: Some("opaque".to_string()),
            origin: Some(native.clone()),
            ..Default::default()
        }),
    }]);
    let shared = std::sync::Arc::clone(&request.messages[0].blocks);

    assert!(request.drop_foreign_replay(&native).is_empty());
    assert!(std::sync::Arc::ptr_eq(&shared, &request.messages[0].blocks));
}

#[test]
fn replay_gate_covers_response_text_metadata_and_omits_empty_reasoning_fallbacks() {
    let foreign = route("openai", "https://api.openai.com/v1", "model-a");
    let serving = route(
        "google_oauth",
        "https://cloudcode-pa.googleapis.com/v1internal",
        "model-a",
    );
    let mut request = replay_request(vec![
        LlmContentBlock::Text {
            text: "answer".into(),
            response_meta: Some(ResponseTextMeta {
                id: Some("response-item".to_string()),
                status: Some("completed".to_string()),
                phase: Some("final_answer".to_string()),
                provider_payload: Some("signature".to_string()),
                origin: Some(foreign.clone()),
                ..Default::default()
            }),
            cache_breakpoint: false,
        },
        LlmContentBlock::Reasoning {
            text: String::new(),
            replay: Some(ProviderReasoningReplay {
                encrypted_content: Some("encrypted".to_string()),
                origin: Some(foreign),
                ..Default::default()
            }),
        },
    ]);

    let drops = request.drop_foreign_replay(&serving);

    assert_eq!(drops.len(), 2);
    assert!(matches!(drops[0].kind, ProviderReplayKind::ResponseText));
    assert_eq!(request.messages[0].blocks.len(), 1);
    assert!(matches!(
        &request.messages[0].blocks[0],
        LlmContentBlock::Text { response_meta: None, text, .. } if text.as_ref() == "answer"
    ));
}

#[test]
fn base_response_text_identity_decodes_without_becoming_native_provenance() {
    let base_json = r#"{
        "id":"response-id",
        "status":"complete",
        "phase":"final_answer",
        "provider_payload":"signature",
        "origin_provider":"google_oauth",
        "origin_model":"gemini-base"
    }"#;
    let meta: ResponseTextMeta = serde_json::from_str(base_json).expect("base JSON");
    assert!(
        meta.origin.is_none(),
        "endpoint-less legacy identity is not provenance"
    );
    assert_eq!(meta.legacy_origin_provider.as_deref(), Some("google_oauth"));
    assert_eq!(meta.legacy_origin_model.as_deref(), Some("gemini-base"));
    assert_eq!(
        serde_json::to_value(&meta).expect("legacy metadata reserializes"),
        serde_json::from_str::<serde_json::Value>(base_json).expect("base JSON value"),
        "base-era identity leaves survive a JSON decode/encode round trip"
    );

    let mut request = replay_request(vec![LlmContentBlock::Text {
        text: "answer".into(),
        response_meta: Some(meta),
        cache_breakpoint: false,
    }]);
    let serving = route(
        "google_oauth",
        "https://cloudcode-pa.googleapis.com/v1internal",
        "gemini-base",
    );
    let drops = request.drop_foreign_replay(&serving);
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].reason, ProviderReplayDropReason::Unstamped);
}

#[test]
fn stamping_foreign_replay_reports_conflict_and_preserves_the_original_origin() {
    let foreign = route("openai", "https://api.openai.com/v1", "model-a");
    let serving = route("anthropic", "https://api.anthropic.com", "model-a");
    let mut part = LlmOutputPart::ToolCall {
        call_id: "call".to_string(),
        tool_name: "tool".to_string(),
        input_json: "{}".to_string(),
        replay: Some(ProviderReplayMeta {
            opaque: Some("opaque".to_string()),
            origin: Some(foreign.clone()),
            ..Default::default()
        }),
    };

    let conflict = part
        .stamp_replay_origin(&serving)
        .expect_err("a serving route must not recertify foreign replay");

    assert_eq!(conflict.actual, foreign);
    assert_eq!(conflict.expected, serving);
    assert!(matches!(
        part,
        LlmOutputPart::ToolCall { replay: Some(ProviderReplayMeta { origin: Some(origin), .. }), .. }
            if origin == conflict.actual
    ));
}

#[test]
fn only_requested_options_can_be_omitted() {
    let untouched = GenerationDisposition {
        output_token_cap: GenerationOptionDisposition::applied(false),
        temperature: GenerationOptionDisposition::sampling_pinned(false),
        seed: GenerationOptionDisposition::unsupported(false),
        stop_sequences: GenerationOptionDisposition::unsupported(false),
        cache: GenerationOptionDisposition::unsupported(false),
    };
    assert_eq!(untouched, GenerationDisposition::default());
    assert!(untouched.nothing_omitted());

    let suppressed = GenerationDisposition {
        stop_sequences: GenerationOptionDisposition::SuppressedProtocolOwned,
        ..Default::default()
    };
    assert!(!suppressed.nothing_omitted());
    assert!(!suppressed.fully_honored());

    let dropped = GenerationDisposition {
        output_token_cap: GenerationOptionDisposition::applied(true),
        temperature: GenerationOptionDisposition::sampling_pinned(true),
        seed: GenerationOptionDisposition::unsupported(true),
        stop_sequences: GenerationOptionDisposition::unsupported(false),
        cache: GenerationOptionDisposition::unsupported(true),
    };
    assert_eq!(
        dropped.output_token_cap,
        GenerationOptionDisposition::Applied
    );
    assert!(!dropped.output_token_cap.is_omitted());
    assert!(dropped.temperature.is_omitted());
    assert!(dropped.seed.is_omitted());
    assert!(!dropped.nothing_omitted());
    assert_eq!(
        serde_json::to_value(dropped).expect("serialize disposition"),
        serde_json::json!({
            "output_token_cap": "applied",
            "temperature": "omitted_sampling_pinned",
            "seed": "omitted_unsupported",
            "stop_sequences": "not_requested",
            "cache": "omitted_unsupported",
        })
    );
}

#[test]
fn attempt_contract_round_trips_closed_outcomes_and_preserves_optional_zero() {
    for (outcome, position) in [
        (
            AttemptOutcome::Completed,
            ProtocolPosition::TerminalObserved,
        ),
        (AttemptOutcome::Failed, ProtocolPosition::ResponseObserved),
        (AttemptOutcome::Aborted, ProtocolPosition::OutputStarted),
        (AttemptOutcome::Interrupted, ProtocolPosition::NoResponse),
    ] {
        let record = LlmCallRecord {
            call_id: LlmCallId("call-1".to_string()),
            label: Some("test".to_string()),
            replay_drops: Vec::new(),
            attempts: vec![AttemptRecord {
                ordinal: 1,
                started_at: 42,
                duration: std::time::Duration::from_millis(7),
                outcome,
                protocol_position: position,
                retry_budget_consumed: true,
                retry_decision: None,
                error: None,
                evidence: Some(ExecutionEvidence {
                    reasoning_output_tokens: Some(0),
                    ..ExecutionEvidence::default()
                }),
                generation_disposition: Some(GenerationDisposition {
                    output_token_cap: GenerationOptionDisposition::Applied,
                    temperature: GenerationOptionDisposition::OmittedSamplingPinned,
                    seed: GenerationOptionDisposition::OmittedUnsupported,
                    stop_sequences: GenerationOptionDisposition::NotRequested,
                    cache: GenerationOptionDisposition::Applied,
                }),
                usage: None,
            }],
        };
        let decoded: LlmCallRecord =
            serde_json::from_value(serde_json::to_value(&record).unwrap()).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.attempts[0]
                .evidence
                .as_ref()
                .unwrap()
                .reasoning_output_tokens,
            Some(0)
        );
    }

    let absent = ExecutionEvidence::default();
    assert_eq!(absent.reasoning_output_tokens, None);
}
