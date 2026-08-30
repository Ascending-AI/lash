use super::*;
use lash_sansio::sync::MutexExt;

pub(super) async fn prove_openai_compatible_tool_stream() -> Result<ProofRun, FixedScriptRunnerError>
{
    let (mut provider, transport) = openai_compatible_provider(OPENAI_COMPAT_TOOL_CALL)?;
    let response = provider.complete(openai_compatible_request(true)).await?;
    require(
        response.terminal_reason == LlmTerminalReason::ToolUse,
        "OpenAI-compatible tool stream terminal reason was not tool_use",
    )?;
    require(
        response.full_text() == "café ",
        "OpenAI-compatible tool stream text did not preserve split UTF-8",
    )?;
    require(
        response.parts.iter().any(|part| {
            matches!(
                part,
                LlmOutputPart::ToolCall {
                    tool_name,
                    input_json,
                    ..
                } if tool_name == "lookup" && input_json == "{\"q\":\"x\"}"
            )
        }),
        "OpenAI-compatible tool stream did not produce normalized lookup tool call",
    )?;
    proof(
        "openai-compatible.chat-tool-call-split-stream",
        "openai-compatible",
        OPENAI_COMPAT_TOOL_CALL,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
            "full_text": response.full_text(),
        }),
    )
}

pub(super) async fn prove_openai_responses_text_stream() -> Result<ProofRun, FixedScriptRunnerError>
{
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(
        OPENAI_RESPONSES_TEXT,
    )?);
    let mut provider =
        OpenAiProvider::new("test-key").with_transport(provider_transport(&transport));
    let response = provider.complete(openai_responses_request()).await?;
    require(
        response.terminal_reason == LlmTerminalReason::Stop,
        "OpenAI Responses stream terminal reason was not stop",
    )?;
    require(
        response.full_text() == "Direct answer.",
        "OpenAI Responses stream did not produce expected text",
    )?;
    proof(
        "openai.responses-text-stream",
        "openai",
        OPENAI_RESPONSES_TEXT,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
            "full_text": response.full_text(),
            "usage": response.usage,
        }),
    )
}

fn codex_provider(
    script: &str,
) -> Result<(CodexProvider, Arc<ScriptedLlmHttpTransport>), FixedScriptRunnerError> {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(script)?);
    // Pin the HTTP/SSE path so the injected scripted transport serves the
    // request; Codex's default Auto path would try the WebSocket transport.
    let provider = CodexProvider::new("access-token", "refresh-token", 0)
        .force_sse_transport()
        .with_http_transport(provider_transport(&transport));
    Ok((provider, transport))
}

fn codex_request(tools: bool, stream_events: Option<LlmEventSender>) -> LlmRequest {
    let tool_specs = if tools {
        vec![LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "q": { "type": "string" } }
            })
            .into(),
            output_schema: json!({}).into(),
        }]
    } else {
        Vec::new()
    };
    LlmRequest {
        model: "gpt-5.4-codex".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "lookup x")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(tool_specs),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(stream_events.unwrap_or_else(|| LlmEventSender::new(|_event| {}))),
        provider_trace: None,
    }
}

pub(super) async fn prove_codex_responses_text_stream() -> Result<ProofRun, FixedScriptRunnerError>
{
    let (mut provider, transport) = codex_provider(CODEX_RESPONSES_TEXT)?;
    let response = provider.complete(codex_request(false, None)).await?;
    require(
        response.terminal_reason == LlmTerminalReason::Stop,
        "Codex Responses stream terminal reason was not stop",
    )?;
    require(
        response.full_text() == "Codex direct answer.",
        "Codex Responses stream did not produce expected text",
    )?;
    proof(
        "codex.responses-text-stream",
        "codex",
        CODEX_RESPONSES_TEXT,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
            "full_text": response.full_text(),
            "usage": response.usage,
        }),
    )
}

pub(super) async fn prove_codex_responses_tool_call_stream()
-> Result<ProofRun, FixedScriptRunnerError> {
    let (mut provider, transport) = codex_provider(CODEX_RESPONSES_TOOL_CALL)?;
    let response = provider.complete(codex_request(true, None)).await?;
    require(
        response.terminal_reason == LlmTerminalReason::ToolUse,
        "Codex Responses tool-call stream terminal reason was not tool_use",
    )?;
    require(
        response.parts.iter().any(|part| {
            matches!(
                part,
                LlmOutputPart::ToolCall {
                    tool_name,
                    input_json,
                    ..
                } if tool_name == "lookup" && input_json == "{\"q\":\"x\"}"
            )
        }),
        "Codex Responses tool-call stream did not produce normalized lookup tool call",
    )?;
    proof(
        "codex.responses-tool-call-stream",
        "codex",
        CODEX_RESPONSES_TOOL_CALL,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
        }),
    )
}

pub(super) async fn prove_codex_responses_rate_limit() -> Result<ProofRun, FixedScriptRunnerError> {
    let (mut provider, transport) = codex_provider(CODEX_RESPONSES_RATE_LIMIT)?;
    let err = provider
        .complete(codex_request(false, None))
        .await
        .expect_err("codex rate-limit script should fail");
    require(
        err.status == Some(429),
        "Codex rate limit script did not preserve 429 status",
    )?;
    let classified = DefaultProviderFailureClassifier.classify(err.clone());
    proof(
        "codex.responses-rate-limit-429",
        "codex",
        CODEX_RESPONSES_RATE_LIMIT,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "status": err.status,
            "headers": redacted_headers(&err.headers),
            "raw_body_bytes": err.raw.as_ref().map(|body| body.len()),
            "retry_after_ms": err.retry_after().map(|duration| duration.as_millis() as u64),
            "request_body_snapshot": err.request_body.is_some(),
            "provider_error_retryable": err.is_retryable(),
            "classification": failure_classification(&classified),
        }),
    )
}

pub(super) async fn prove_codex_responses_disconnect() -> Result<ProofRun, FixedScriptRunnerError> {
    let (mut provider, transport) = codex_provider(CODEX_RESPONSES_DISCONNECT)?;
    let err = provider
        .complete(codex_request(false, None))
        .await
        .expect_err("codex disconnect script should fail");
    require(
        err.kind == ProviderFailureKind::Stream && err.is_retryable(),
        "Codex disconnect script did not surface retryable stream failure",
    )?;
    let classified = DefaultProviderFailureClassifier.classify(err.clone());
    proof(
        "codex.responses-mid-stream-disconnect",
        "codex",
        CODEX_RESPONSES_DISCONNECT,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "kind": format!("{:?}", err.kind),
            "retryable": err.is_retryable(),
            "classification": failure_classification(&classified),
        }),
    )
}

pub(super) async fn prove_anthropic_messages_text_stream()
-> Result<ProofRun, FixedScriptRunnerError> {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(
        ANTHROPIC_MESSAGES_TEXT,
    )?);
    let mut provider = AnthropicProvider::new("test-key")
        .with_base_url(Some("https://anthropic.test".to_string()))
        .with_transport(provider_transport(&transport));
    let response = provider.complete(anthropic_messages_request()).await?;
    require(
        response.terminal_reason == LlmTerminalReason::Stop,
        "Anthropic Messages stream terminal reason was not stop",
    )?;
    require(
        response.full_text() == "Anthropic scripted answer.",
        "Anthropic Messages stream did not produce expected text",
    )?;
    proof(
        "anthropic.messages-text-stream",
        "anthropic",
        ANTHROPIC_MESSAGES_TEXT,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
            "full_text": response.full_text(),
            "usage": response.usage,
        }),
    )
}

pub(super) async fn prove_google_stream_generate_text() -> Result<ProofRun, FixedScriptRunnerError>
{
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(
        GOOGLE_STREAM_GENERATE_TEXT,
    )?);
    let mut provider = GoogleOAuthProvider::new(
        "access-token",
        "refresh-token",
        0,
        lash_provider_google::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("project-1".to_string()))
    .with_transport(provider_transport(&transport));
    let response = provider.complete(google_request(true)).await?;
    require(
        response.terminal_reason == LlmTerminalReason::Stop,
        "Google streamGenerateContent terminal reason was not stop",
    )?;
    require(
        response.full_text() == "Google scripted answer.",
        "Google streamGenerateContent did not produce expected text",
    )?;
    require(
        response.usage.input_tokens == 6
            && response.usage.output_tokens == 4
            && response.usage.cache_read_input_tokens == 0
            && response.usage.cache_write_input_tokens == 0
            && response.usage.reasoning_output_tokens == 1,
        "Google streamGenerateContent did not normalize usage metadata",
    )?;
    proof(
        "google.stream-generate-content-text-stream",
        "google_oauth",
        GOOGLE_STREAM_GENERATE_TEXT,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
            "full_text": response.full_text(),
            "usage": response.usage,
        }),
    )
}

pub(super) async fn prove_google_generate_text() -> Result<ProofRun, FixedScriptRunnerError> {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(
        GOOGLE_GENERATE_TEXT,
    )?);
    let mut provider = GoogleOAuthProvider::new(
        "access-token",
        "refresh-token",
        0,
        lash_provider_google::GoogleOAuthClient {
            id: "oauth-client-id".into(),
            secret: "oauth-client-secret".into(),
        },
    )
    .with_project_id(Some("project-1".to_string()))
    .with_transport(provider_transport(&transport));
    let response = provider.complete(google_request(false)).await?;
    require(
        response.terminal_reason == LlmTerminalReason::Stop,
        "Google generateContent terminal reason was not stop",
    )?;
    require(
        response.full_text() == "Google buffered answer.",
        "Google generateContent did not produce expected text",
    )?;
    require(
        response.usage.input_tokens == 4
            && response.usage.output_tokens == 3
            && response.usage.cache_read_input_tokens == 2
            && response.usage.cache_write_input_tokens == 0
            && response.usage.reasoning_output_tokens == 0,
        "Google generateContent did not normalize buffered usage metadata",
    )?;
    proof(
        "google.generate-content-text",
        "google_oauth",
        GOOGLE_GENERATE_TEXT,
        transport_exchanges(transport.as_ref())?,
        success_terminal(&response),
        json!({
            "classification": "success",
            "terminal_reason": response.terminal_reason.code(),
            "full_text": response.full_text(),
            "usage": response.usage,
        }),
    )
}

pub(super) async fn prove_openai_compatible_rate_limit() -> Result<ProofRun, FixedScriptRunnerError>
{
    let (mut provider, transport) = openai_compatible_provider(OPENAI_COMPAT_RATE_LIMIT)?;
    let err = provider
        .complete(openai_compatible_request(false))
        .await
        .expect_err("rate-limit script should fail");
    require(
        err.status == Some(429),
        "OpenAI-compatible rate limit script did not preserve 429 status",
    )?;
    let classified = DefaultProviderFailureClassifier.classify(err.clone());
    proof(
        "openai-compatible.chat-rate-limit-429",
        "openai-compatible",
        OPENAI_COMPAT_RATE_LIMIT,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "status": err.status,
            "headers": redacted_headers(&err.headers),
            "raw_body_bytes": err.raw.as_ref().map(|body| body.len()),
            "retry_after_ms": err.retry_after().map(|duration| duration.as_millis() as u64),
            "request_body_snapshot": err.request_body.is_some(),
            "provider_error_retryable": err.is_retryable(),
            "classification": failure_classification(&classified),
        }),
    )
}

pub(super) async fn prove_openai_compatible_validation() -> Result<ProofRun, FixedScriptRunnerError>
{
    let (mut provider, transport) = openai_compatible_provider(OPENAI_COMPAT_VALIDATION)?;
    let err = provider
        .complete(openai_compatible_request(false))
        .await
        .expect_err("validation script should fail");
    require(
        err.status == Some(400),
        "OpenAI-compatible validation script did not preserve 400 status",
    )?;
    let classified = DefaultProviderFailureClassifier.classify(err.clone());
    proof(
        "openai-compatible.chat-validation-error",
        "openai-compatible",
        OPENAI_COMPAT_VALIDATION,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "status": err.status,
            "headers": redacted_headers(&err.headers),
            "raw_body_bytes": err.raw.as_ref().map(|body| body.len()),
            "request_body_snapshot": err.request_body.is_some(),
            "provider_error_retryable": err.is_retryable(),
            "classification": failure_classification(&classified),
        }),
    )
}

pub(super) async fn prove_openai_compatible_disconnect() -> Result<ProofRun, FixedScriptRunnerError>
{
    let (mut provider, transport) = openai_compatible_provider(OPENAI_COMPAT_DISCONNECT)?;
    let err = provider
        .complete(openai_compatible_request(true))
        .await
        .expect_err("disconnect script should fail");
    require(
        err.kind == ProviderFailureKind::Stream && err.is_retryable(),
        "OpenAI-compatible disconnect script did not surface retryable stream failure",
    )?;
    let classified = DefaultProviderFailureClassifier.classify(err.clone());
    proof(
        "openai-compatible.chat-mid-stream-disconnect",
        "openai-compatible",
        OPENAI_COMPAT_DISCONNECT,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "kind": format!("{:?}", err.kind),
            "retryable": err.is_retryable(),
            "classification": failure_classification(&classified),
        }),
    )
}

pub(super) async fn prove_openai_compatible_response_start_timeout()
-> Result<ProofRun, FixedScriptRunnerError> {
    let (mut provider, transport) =
        openai_compatible_provider(OPENAI_COMPAT_RESPONSE_START_TIMEOUT)?;
    let err = provider
        .complete(openai_compatible_request(true))
        .await
        .expect_err("response-start timeout script should fail");
    require(
        err.kind == ProviderFailureKind::Timeout
            && err.code.as_deref() == Some("timeout")
            && err.is_retryable()
            && err.status.is_none(),
        "OpenAI-compatible response-start timeout did not match production timeout envelope",
    )?;
    proof(
        "openai-compatible.chat-response-start-timeout",
        "openai-compatible",
        OPENAI_COMPAT_RESPONSE_START_TIMEOUT,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "classification": failure_classification(&err),
            "timeout_phase": "response_start",
            "reported_successful_partial_response": false,
        }),
    )
}

pub(super) async fn prove_openai_compatible_stream_chunk_timeout()
-> Result<ProofRun, FixedScriptRunnerError> {
    let (events, sender) = event_collector();
    let (mut provider, transport) = openai_compatible_provider(OPENAI_COMPAT_STREAM_CHUNK_TIMEOUT)?;
    let err = provider
        .complete(openai_compatible_request_with_events(Some(sender)))
        .await
        .expect_err("stream chunk timeout script should fail");
    let committed_events = events.lock_recover();
    let evidence_events = committed_events
        .iter()
        .filter(|event| matches!(event, LlmStreamEvent::Evidence(_)))
        .count();
    let partial_response_events = committed_events.len() - evidence_events;
    require(
        err.kind == ProviderFailureKind::Timeout
            && err.code.as_deref() == Some("timeout")
            && err.is_retryable()
            && err.status.is_none(),
        "OpenAI-compatible stream chunk timeout did not match production timeout envelope",
    )?;
    require(
        evidence_events == 1 && partial_response_events == 0,
        "stream chunk timeout did not retain exactly one request evidence event without partial provider output",
    )?;
    proof(
        "openai-compatible.chat-stream-chunk-timeout",
        "openai-compatible",
        OPENAI_COMPAT_STREAM_CHUNK_TIMEOUT,
        transport_exchanges(transport.as_ref())?,
        error_terminal(&err),
        json!({
            "classification": failure_classification(&err),
            "timeout_phase": "stream_chunk",
            "stream_events_committed": committed_events.len(),
            "evidence_events_committed": evidence_events,
            "partial_response_events_committed": partial_response_events,
            "reported_successful_partial_response": false,
        }),
    )
}

pub(super) async fn prove_openai_compatible_cancel_before_response_start()
-> Result<ProofRun, FixedScriptRunnerError> {
    let schedule = ScriptedTransportSchedule::new();
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_json_str(OPENAI_COMPAT_TOOL_CALL)?
            .with_event_schedule(schedule.clone()),
    );
    let (events, sender) = event_collector();
    let mut provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(provider_transport(&transport));

    let task = tokio::spawn(async move {
        provider
            .complete(openai_compatible_request_with_events(Some(sender)))
            .await
    });
    schedule.wait_until_blocked(0, 0).await;
    task.abort();

    let join_err = task.await.expect_err("cancelled provider task");
    require(
        join_err.is_cancelled(),
        "cancellation before response start did not cancel the provider task",
    )?;
    let committed_events = events.lock_recover().len();
    require(
        committed_events == 0,
        "cancellation before response start committed stream events",
    )?;
    proof(
        "openai-compatible.cancel-before-response-start",
        "openai-compatible",
        OPENAI_COMPAT_TOOL_CALL,
        transport_exchanges(transport.as_ref())?,
        cancelled_terminal(),
        json!({
            "classification": "cancelled_before_response_start",
            "stream_events_committed": committed_events,
        }),
    )
}

pub(super) async fn prove_openai_compatible_retry_exhaustion()
-> Result<ProofRun, FixedScriptRunnerError> {
    let attempt_budget = 2;
    let mut rate_limit_script = ProviderWireScript::from_json_str(OPENAI_COMPAT_RATE_LIMIT)?;
    for event in rate_limit_script.timeline_mut() {
        if let ProviderWireEvent::HttpError { headers, .. } = event {
            headers.retain(|header| !header.name.eq_ignore_ascii_case("retry-after"));
        }
    }
    let rate_limit_script_content = serde_json::to_string_pretty(&rate_limit_script)?;
    let transport = ScriptedLlmHttpTransport::from_scripts(
        (0..attempt_budget).map(|_| rate_limit_script.clone()),
    )?;
    let transport_for_assert = transport.clone();
    let retry_options = ProviderOptions {
        reliability: lash_core::provider::ProviderReliability::default()
            .max_attempts(attempt_budget)
            .base_delay_ms(0)
            .max_delay_ms(0)
            .retry_after_cap_ms(Some(0))
            // This proof measures attempt-budget exhaustion, so disable the
            // ladder's throttle deference (attempt-free Retry-After waits).
            .throttle_wait_budget_ms(0),
        ..ProviderOptions::default()
    };
    let provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_options(retry_options.clone())
        .with_transport(Arc::new(transport));
    let mut handle = ProviderHandle::new(provider.into_components());
    handle.set_options(retry_options);
    let err = handle
        .complete_with_charge_safety(
            openai_compatible_request(false),
            lash_core::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: u8::try_from(attempt_budget - 1)
                    .expect("the two-attempt proof has one unsafe retry"),
                max_duplicate_cost_tokens: None,
            },
        )
        .await
        .expect_err("retry exhaustion should fail");
    require(
        err.status == Some(429) && err.is_retryable(),
        format!(
            "retry exhaustion did not return classified retryable 429: status={:?} retryable={} kind={:?} code={:?} message={}",
            err.status,
            err.is_retryable(),
            err.kind,
            err.code,
            err.message
        ),
    )?;
    require(
        transport_for_assert.remaining_scripts()? == 0,
        "retry exhaustion did not consume the isolated two-attempt retry script budget",
    )?;
    let exchanges = transport_exchanges(&transport_for_assert)?;
    require(
        exchanges.len() == attempt_budget as usize,
        format!(
            "retry exhaustion executed {} scripted HTTP exchanges, expected the isolated two-attempt retry budget {attempt_budget}",
            exchanges.len()
        ),
    )?;
    require(
        exchanges
            .iter()
            .all(|exchange| exchange.response.status == Some(429)),
        "retry exhaustion did not preserve 429 status on every scripted retry attempt",
    )?;
    proof(
        "openai-compatible.retry-exhaustion",
        "openai-compatible",
        &rate_limit_script_content,
        exchanges,
        error_terminal(&err),
        json!({
            "status": err.status,
            "retryable": err.is_retryable(),
            "classification": failure_classification(&err),
            "attempts_consumed": attempt_budget,
        }),
    )
}

fn proof(
    name: impl Into<String>,
    provider_kind: impl Into<String>,
    script_content: &str,
    http_exchanges: Vec<ScriptedLlmHttpExchange>,
    terminal: TranscriptTerminal,
    observed: serde_json::Value,
) -> Result<ProofRun, FixedScriptRunnerError> {
    let name = name.into();
    let provider_kind = provider_kind.into();
    let transcript = transcript_for_script(
        &name,
        &provider_kind,
        script_content,
        http_exchanges,
        terminal,
        observed.clone(),
    )?;
    let endpoint = transcript.endpoint.path.clone();
    Ok(ProofRun {
        name,
        provider_kind,
        endpoint,
        outcome: "passed".to_string(),
        transcript,
        observed,
    })
}

fn success_terminal(response: &LlmResponse) -> TranscriptTerminal {
    TranscriptTerminal {
        classification: "success",
        provider_result: Some(TranscriptProviderResult {
            terminal_reason: response.terminal_reason.code().to_string(),
            full_text_bytes: response.full_text().len(),
            part_count: response.parts.len(),
            usage: serde_json::to_value(&response.usage).unwrap_or(serde_json::Value::Null),
        }),
        error_envelope: None,
    }
}

fn cancelled_terminal() -> TranscriptTerminal {
    TranscriptTerminal {
        classification: "cancelled_before_response_start",
        provider_result: None,
        error_envelope: None,
    }
}

fn error_terminal(error: &LlmTransportError) -> TranscriptTerminal {
    TranscriptTerminal {
        classification: "error",
        provider_result: None,
        error_envelope: Some(TranscriptErrorEnvelope {
            kind: format!("{:?}", error.kind),
            code: error.code.clone(),
            status: error.status,
            retryable: error.is_retryable(),
            terminal_reason: error.terminal_reason.code().to_string(),
            raw_body_bytes: error.raw.as_ref().map(|body| body.len()),
            headers: transcript_headers_from_pairs(&error.headers),
            retry_after_ms: error
                .retry_after()
                .map(|duration| duration.as_millis() as u64),
            request_body_snapshot: error.request_body.is_some(),
        }),
    }
}

pub(super) fn transport_exchanges(
    transport: &ScriptedLlmHttpTransport,
) -> Result<Vec<ScriptedLlmHttpExchange>, FixedScriptRunnerError> {
    Ok(transport.exchanges()?)
}

fn provider_transport(transport: &Arc<ScriptedLlmHttpTransport>) -> Arc<dyn LlmHttpTransport> {
    transport.clone()
}

fn failure_classification(failure: &LlmTransportError) -> serde_json::Value {
    json!({
        "kind": format!("{:?}", failure.kind),
        "retryable": failure.is_retryable(),
        "status": failure.status,
        "terminal_reason": failure.terminal_reason.code(),
    })
}

fn transcript_headers_from_pairs(headers: &[(String, String)]) -> Vec<TranscriptHeader> {
    headers
        .iter()
        .map(|(name, value)| TranscriptHeader {
            name: name.clone(),
            value: redacted_header_value(name, value),
        })
        .collect()
}

fn redacted_headers(headers: &[(String, String)]) -> Vec<serde_json::Value> {
    headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": redacted_header_value(name, value) }))
        .collect()
}

pub(super) fn redacted_header_value(name: &str, value: &str) -> String {
    let lower_name = name.to_ascii_lowercase();
    let lower_value = value.to_ascii_lowercase();
    if matches!(
        lower_name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || lower_name.contains("api-key")
        || lower_name.contains("token")
        || lower_value.contains("bearer ")
        || lower_value.contains("sk-")
    {
        "[redacted]".to_string()
    } else {
        value.to_string()
    }
}

pub(super) fn require(
    condition: bool,
    message: impl Into<String>,
) -> Result<(), FixedScriptRunnerError> {
    if condition {
        Ok(())
    } else {
        Err(FixedScriptRunnerError::Assertion(message.into()))
    }
}

fn openai_compatible_provider(
    script: &str,
) -> Result<(OpenAiCompatibleProvider, Arc<ScriptedLlmHttpTransport>), FixedScriptRunnerError> {
    let transport = Arc::new(ScriptedLlmHttpTransport::from_json_str(script)?);
    let provider = OpenAiCompatibleProvider::new("test-key", "https://provider.test")
        .with_transport(provider_transport(&transport));
    Ok((provider, transport))
}

pub(super) fn openai_compatible_request(stream: bool) -> LlmRequest {
    openai_compatible_request_with_events(stream.then(|| LlmEventSender::new(|_event| {})))
}

fn openai_compatible_request_with_events(stream_events: Option<LlmEventSender>) -> LlmRequest {
    LlmRequest {
        model: "openai/gpt-5.4".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "lookup x")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(vec![LlmToolSpec {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" }
                }
            })
            .into(),
            output_schema: json!({}).into(),
        }]),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events,
        provider_trace: None,
    }
}

fn event_collector() -> (Arc<Mutex<Vec<LlmStreamEvent>>>, LlmEventSender) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sender = LlmEventSender::new(move |event| {
        captured.lock_recover().push(event);
    });
    (events, sender)
}

fn openai_responses_request() -> LlmRequest {
    LlmRequest {
        model: "gpt-5.4".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn anthropic_messages_request() -> LlmRequest {
    LlmRequest {
        model: "claude-sonnet-4-20250514".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: Some(LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}

fn google_request(stream: bool) -> LlmRequest {
    LlmRequest {
        model: "gemini-3.1-pro-preview".to_string(),
        messages: vec![LlmMessage::text(LlmRole::User, "answer directly")],
        attachments: Vec::new(),
        resolved_stored: Default::default(),
        tools: Arc::new(Vec::new()),
        tool_choice: LlmToolChoice::Auto,
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        generation: lash_core::GenerationOptions::default(),
        scope: lash_core::LlmRequestScope::new(
            "session-1",
            "session-1:frame:sim",
            "session-1:request:sim",
        ),
        output_spec: None,
        stream_events: stream.then(|| LlmEventSender::new(|_event| {})),
        provider_trace: None,
    }
}
