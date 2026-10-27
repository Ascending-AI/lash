use super::types::*;

fn route(provider: &str, endpoint: &str, model: &str) -> ProviderRouteIdentity {
    ProviderRouteIdentity::for_endpoint(provider, endpoint, model)
}

fn replay_request(blocks: Vec<LlmContentBlock>) -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: crate::llm_profile::LlmProfileConfig::new(
            crate::llm_profile::RecordedLlmProfile::mint(
                crate::llm_profile::LlmProfileKey::new("request-fixture"),
                crate::llm_profile::LlmProfileMetadata::builder("model-a".to_string())
                    .context_window_tokens(128_000)
                    .capability(Default::default())
                    .extra_body(Default::default())
                    .cache_retention(crate::llm::capability::CacheRetention::Short)
                    .build()
                    .expect("valid profile"),
            ),
        )
        .with_reasoning(Default::default()),
        messages: vec![LlmMessage::new(LlmRole::Assistant, blocks)],

        tools: std::sync::Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        attachment_acceptance: Default::default(),
        generation: Default::default(),
        scope: LlmRequestScope::new("session", "frame", "request"),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
        attempt_control: Default::default(),
    }
}

fn marked_text(role: LlmRole, text: &str, starts_user_segment: bool) -> LlmMessage {
    let mut message = LlmMessage::text(role, text);
    message.starts_user_segment = starts_user_segment;
    message
}

/// FIG-7506: changing call scope must not rebuild the immutable tool schema suffix.
#[test]
fn retained_canonical_calls_share_configuration_and_read_each_calls_scope() {
    let route = route("canonical", "https://canonical.example", "model-a");
    let cache = SegmentCache::new();
    let mut request = replay_request(Vec::new());
    request.tools = std::sync::Arc::new(vec![crate::llm::wire_corpus::corpus_tool()]);
    let first = RecordedRequestTemplate::of_retained_request(
        route.clone(),
        &RetainedLlmRequest::from_request(request.clone()),
        &cache,
    )
    .expect("first canonical call");
    let read = first.canonical_request().expect("first request");
    assert_eq!(read.scope, request.scope);
    request.scope.request_id = "second-call".into();
    let second = RecordedRequestTemplate::of_retained_request(
        route.clone(),
        &RetainedLlmRequest::from_request(request.clone()),
        &cache,
    )
    .expect("second canonical call");
    assert_eq!(
        second.canonical_request().expect("second request").scope,
        request.scope
    );
    let extended = second
        .clone()
        .edit_segments(|segments| {
            let last = segments.last_mut().expect("scope suffix");
            let mut text = last.text_lossy();
            assert_eq!(text.pop(), Some('}'));
            text.push_str(",\"retained_extension\":true}");
            *last = RequestSegment::text(text).expect("extended suffix");
        })
        .expect("a valid extended recorded body");
    assert_eq!(
        extended
            .canonical_request()
            .expect("extended request")
            .scope,
        request.scope,
        "a recorded body with fields after scope still uses the complete decoder"
    );
    assert!(
        RequestSegment::same(
            first.retained_items().expect("first item run").0,
            second.retained_items().expect("second item run").0,
        ),
        "the same configuration keeps its lowered owner across calls"
    );
    std::sync::Arc::make_mut(&mut request.tools)[0].description = "a changed tool".into();
    let changed = RecordedRequestTemplate::of_retained_request(
        route,
        &RetainedLlmRequest::from_request(request.clone()),
        &cache,
    )
    .expect("changed canonical call");
    assert_eq!(
        changed.canonical_request().expect("changed request").tools,
        request.tools
    );
    assert_eq!(
        first
            .canonical_request()
            .expect("retained first request")
            .tools,
        read.tools
    );
}

#[test]
fn fig1123_client_side_retention_cuts_only_at_genuine_user_segments() {
    let serving = route("openai-compatible", "https://gateway.example/v1", "model-a");
    let mut request = replay_request(Vec::new());
    request.messages = vec![
        marked_text(LlmRole::User, "first genuine input", true),
        LlmMessage::new(
            LlmRole::Assistant,
            vec![LlmContentBlock::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
        ),
        LlmMessage::new(
            LlmRole::User,
            vec![LlmContentBlock::ToolResult {
                status: crate::ToolCallStatus::Success,
                call_id: "call-1".to_string(),
                content: vec![crate::ModelToolReturnPart::text("result")],
                tool_name: Some("lookup".to_string()),
            }],
        ),
        marked_text(LlmRole::User, "synthetic observation", false),
        marked_text(LlmRole::User, "second genuine input", true),
        marked_text(LlmRole::Assistant, "second answer", false),
        marked_text(LlmRole::User, "third genuine input", true),
    ];
    *request.model.metadata_mut().capability.reasoning_retention = ReasoningRetentionPolicy {
        capability: Some(ReasoningRetentionCapability::ClientSideUserSegments),
        selection: ReasoningRetentionSelection::ClientSideUserSegments {
            max_segments: std::num::NonZeroUsize::new(2).unwrap(),
        },
    };

    let safe = request
        .reasoning_retention_safe_for(
            &serving,
            "OpenAI Chat Completions",
            ProviderReasoningRetentionSupport::ClientSideUserSegments,
        )
        .expect("fallback is supported");

    assert_eq!(safe.messages.len(), 3);
    assert!(safe.messages[0].starts_user_segment);
    assert!(matches!(
        &safe.messages[0].blocks[0],
        LlmContentBlock::Text { text, .. } if text.as_ref() == "second genuine input"
    ));
    assert!(safe.messages.iter().all(|message| !message.blocks.iter().any(
        |block| matches!(block, LlmContentBlock::ToolCall { call_id, .. } | LlmContentBlock::ToolResult { call_id, .. } if call_id == "call-1")
    )));
}

#[test]
fn fig1123_native_retention_keeps_http_history_and_rejects_cross_primitive_approximation() {
    let serving = route("openai", "https://api.openai.com/v1", "model-a");
    let mut request = replay_request(Vec::new());
    request.messages = vec![
        marked_text(LlmRole::User, "old", true),
        marked_text(LlmRole::Assistant, "answer", false),
        marked_text(LlmRole::User, "new", true),
    ];
    *request.model.metadata_mut().capability.reasoning_retention = ReasoningRetentionPolicy {
        capability: Some(ReasoningRetentionCapability::OpenAiContext {
            supported: vec![OpenAiReasoningContext::CurrentTurn],
        }),
        selection: ReasoningRetentionSelection::OpenAiContext {
            context: OpenAiReasoningContext::CurrentTurn,
        },
    };

    let safe = request
        .reasoning_retention_safe_for(
            &serving,
            "OpenAI Responses",
            ProviderReasoningRetentionSupport::OpenAiContext,
        )
        .expect("native policy is exact");
    assert_eq!(
        safe.messages, request.messages,
        "native pruning is model-side"
    );

    let error = request
        .reasoning_retention_safe_for(
            &serving,
            "OpenAI Chat Completions",
            ProviderReasoningRetentionSupport::ClientSideUserSegments,
        )
        .expect_err("native units must not be approximated as user segments");
    assert_eq!(
        error.category,
        ReasoningRetentionValidationCategory::UnsupportedSelection
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
                phase: Some(ResponsePhase::FinalAnswer),
                provider_payload: Some("signature".to_string()),
                origin: Some(foreign.clone()),
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
    let untouched = GenerationReceipt {
        output_token_cap: GenerationOptionOutcome::from_emission(false, true),
        temperature: GenerationOptionOutcome::from_emission(false, false),
        ..GenerationReceipt::default()
    };
    assert_eq!(untouched, GenerationReceipt::default());
    assert!(untouched.nothing_omitted());
    assert!(untouched.fully_honored());

    let suppressed = GenerationReceipt {
        stop_sequences: GenerationOptionOutcome::SuppressedProtocolOwned,
        ..Default::default()
    };
    assert!(!suppressed.nothing_omitted());
    assert!(!suppressed.fully_honored());

    let clamped = GenerationReceipt {
        output_token_cap: GenerationOptionOutcome::ClampedToCapacity,
        ..Default::default()
    };
    assert!(clamped.nothing_omitted());
    assert!(!clamped.fully_honored());

    let reported = GenerationReceipt {
        output_token_cap: GenerationOptionOutcome::from_emission(true, true),
        temperature: GenerationOptionOutcome::from_emission(true, true),
        cache: GenerationOptionOutcome::from_emission(true, false),
        reasoning: GenerationOptionOutcome::Applied,
        reasoning_retention: GenerationOptionOutcome::Applied,
        parallel_tool_calls: GenerationOptionOutcome::Applied,
        thinking_summary: GenerationOptionOutcome::NotRequested,
        thinking_visibility: GenerationOptionOutcome::Applied,
        ..GenerationReceipt::default()
    };
    assert_eq!(reported.output_token_cap, GenerationOptionOutcome::Applied);
    assert!(reported.cache.is_omitted());
    assert!(!reported.nothing_omitted());
    assert_eq!(
        serde_json::to_value(reported).expect("serialize disposition"),
        serde_json::json!({
            "output_token_cap": "applied",
            "temperature": "applied",
            "seed": "not_requested",
            "stop_sequences": "not_requested",
            "cache": "omitted_unsupported",
            "reasoning": "applied",
            "reasoning_retention": "applied",
            "parallel_tool_calls": "applied",
            "thinking_summary": "not_requested",
            "thinking_visibility": "applied",
            "passthrough": "not_requested",
        })
    );
    // The removed sampling-pinned disposition is refused: pinning now refuses
    // the call before any I/O.
    assert!(
        serde_json::from_value::<GenerationOptionOutcome>(serde_json::json!(
            "omitted_sampling_pinned"
        ))
        .is_err()
    );
}

#[test]
fn attempt_usage_disposition_follows_outcome_when_usage_is_absent() {
    let mut record: AttemptRecord = serde_json::from_value(serde_json::json!({
        "ordinal": 1, "outcome": "aborted", "protocol_position": "output_started", "retry_budget_consumed": true
    })).expect("attempt");
    for outcome in [
        AttemptOutcome::Completed,
        AttemptOutcome::Failed,
        AttemptOutcome::Aborted,
        AttemptOutcome::Interrupted,
    ] {
        record.outcome = outcome;
        assert_eq!(
            record.usage_disposition(),
            AttemptUsageOutcome::for_attempt(outcome, None)
        );
        assert!(
            serde_json::to_value(&record)
                .expect("encode")
                .get("usage_disposition")
                .is_none()
        );
    }
    record.usage = Some(LlmUsage::default());
    assert_eq!(record.usage_disposition(), AttemptUsageOutcome::Reported);

    let usage = LlmUsage {
        input_tokens: 1,
        ..LlmUsage::default()
    };
    for outcome in [
        AttemptOutcome::Completed,
        AttemptOutcome::Failed,
        AttemptOutcome::Aborted,
        AttemptOutcome::Interrupted,
    ] {
        assert_eq!(
            AttemptUsageOutcome::for_attempt(outcome, Some(&usage)),
            AttemptUsageOutcome::Reported
        );
    }
    assert_eq!(
        AttemptUsageOutcome::for_attempt(AttemptOutcome::Completed, None),
        AttemptUsageOutcome::UnreportedByProvider
    );
    assert_eq!(
        AttemptUsageOutcome::for_attempt(AttemptOutcome::Aborted, None),
        AttemptUsageOutcome::UnreportedAfterAbort
    );
    assert_eq!(
        AttemptUsageOutcome::for_attempt(AttemptOutcome::Failed, None),
        AttemptUsageOutcome::UnreportedAfterFailure
    );
    assert_eq!(
        AttemptUsageOutcome::for_attempt(AttemptOutcome::Interrupted, None),
        AttemptUsageOutcome::UnreportedAfterFailure
    );
    assert!(AttemptUsageOutcome::UnreportedAfterAbort.is_unreported_after_interruption());
    assert!(!AttemptUsageOutcome::UnreportedByProvider.is_unreported_after_interruption());
}

/// FIG-6570: a completed retry cannot erase unreported earlier work.
#[test]
fn a_completed_retry_keeps_interrupted_usage_unknown() {
    let mut call: LlmCallRecord = serde_json::from_value(serde_json::json!({
        "call_id": "retry-after-interruption",
        "attempts": [
            {"ordinal": 1, "outcome": "interrupted", "protocol_position": "output_started", "retry_budget_consumed": false},
            {"ordinal": 2, "outcome": "completed", "protocol_position": "terminal_observed", "retry_budget_consumed": false, "usage": LlmUsage { input_tokens: 3, output_tokens: 2, ..Default::default() }}
        ]
    })).expect("call");
    let usage = call.usage().expect("usage");
    assert_eq!(
        usage.unknown_calls, 1,
        "earlier interrupted work is not free"
    );
    assert_eq!(usage.input_tokens, 3);
    call.attempts[0].usage = Some(LlmUsage::default());
    assert_eq!(call.usage().expect("reported zero").unknown_calls, 0);
}

#[test]
fn a_completed_call_with_absent_usage_is_unknown_and_explicit_zero_is_known() {
    let mut call: LlmCallRecord = serde_json::from_value(serde_json::json!({
        "call_id": "provider-omitted-usage", "attempts": [
            {"ordinal":1,"outcome":"completed","protocol_position":"terminal_observed","retry_budget_consumed":true}
        ]
    })).expect("completed call");
    assert_eq!(
        call.attempts[0].usage_disposition(),
        AttemptUsageOutcome::UnreportedByProvider
    );
    assert_eq!(call.usage().expect("usage").unknown_calls, 1);
    call.attempts[0].usage = Some(LlmUsage::default());
    assert_eq!(call.usage().expect("explicit zero").unknown_calls, 0);
}
