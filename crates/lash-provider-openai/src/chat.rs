use crate::responses_shared as shared;
use crate::support::*;
use std::borrow::Cow;

const PROVIDER: &str = "OpenAI-compatible";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheBreakpointDiagnostics {
    pub(crate) requested: usize,
    pub(crate) emitted: usize,
    pub(crate) dropped: usize,
}

impl OpenAiCompatibleProvider {
    fn chat_cache_control_value(
        cache_retention: CacheRetention,
        extended_ttl: bool,
    ) -> Option<Value> {
        match cache_retention {
            CacheRetention::None => None,
            CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
            CacheRetention::Long if extended_ttl => {
                Some(json!({ "type": "ephemeral", "ttl": "1h" }))
            }
            CacheRetention::Long => Some(json!({ "type": "ephemeral" })),
        }
    }

    fn validate_chat_attachments(req: &LlmRequest) -> Result<(), LlmTransportError> {
        for source in &req.attachments {
            let supported = source
                .media_type()
                .is_some_and(|mime| OPENAI_IMAGE_MIMES.contains(&mime.as_str()))
                && !matches!(source, AttachmentSource::ProviderFile { .. });
            if !supported {
                let accepted_by = known_attachment_acceptors(source);
                return Err(unsupported_attachment_capability(
                    "OpenAI Chat Completions",
                    source,
                    &accepted_by,
                ));
            }
            if matches!(source, AttachmentSource::Stored { .. })
                && req.attachment_bytes(source).is_none()
            {
                return Err(LlmTransportError::new(
                    "OpenAI Chat Completions could not materialize a stored attachment because session-guard resolution did not provide its bytes",
                )
                .with_kind(ProviderFailureKind::Validation)
                .with_code("stored_attachment_not_resolved"));
            }
        }
        Ok(())
    }

    fn chat_attachment_part(req: &LlmRequest, source: &AttachmentSource) -> Value {
        let media_type = source.media_type().expect("validated image source");
        let url = match source {
            AttachmentSource::ExternalUrl { url, .. } => url.clone(),
            AttachmentSource::Inline { .. } | AttachmentSource::Stored { .. } => {
                let bytes = req
                    .attachment_bytes(source)
                    .expect("validated attachment bytes");
                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                format!("data:{media_type};base64,{b64}")
            }
            AttachmentSource::ProviderFile { .. } => unreachable!(),
        };
        json!({
            "type": "image_url",
            "image_url": {
                "url": url,
            },
        })
    }

    fn build_chat_messages(req: &LlmRequest) -> Vec<Value> {
        let mut messages = Vec::new();
        for msg in &req.messages {
            let role = role_name(&msg.role);
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();
            let mut reasoning_details = Vec::new();

            for block in msg.blocks.iter() {
                match block {
                    LlmContentBlock::Text { text, .. } if !text.is_empty() => {
                        let mut part = json!({
                            "type": "text",
                            "text": text,
                        });
                        if let LlmContentBlock::Text {
                            cache_breakpoint: true,
                            ..
                        } = block
                        {
                            part["__lash_cache_breakpoint"] = json!(true);
                        }
                        text_parts.push(part);
                    }
                    LlmContentBlock::Attachment { attachment_idx }
                        if matches!(msg.role, LlmRole::User) =>
                    {
                        if let Some(att) = req.attachments.get(*attachment_idx) {
                            text_parts.push(Self::chat_attachment_part(req, att));
                        }
                    }
                    LlmContentBlock::ToolCall {
                        call_id,
                        tool_name,
                        input_json,
                        replay,
                        ..
                    } if matches!(msg.role, LlmRole::Assistant) => {
                        tool_calls.push(json!({
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": tool_name,
                                "arguments": input_json,
                            },
                        }));
                        if let Some(opaque) =
                            replay.as_ref().and_then(|meta| meta.opaque.as_deref())
                            && let Ok(detail) = serde_json::from_str::<Value>(opaque)
                            && detail.get("type").and_then(Value::as_str)
                                == Some("reasoning.encrypted")
                        {
                            reasoning_details.push(detail);
                        }
                    }
                    LlmContentBlock::ToolResult {
                        call_id,
                        content,
                        tool_name,
                    } => {
                        let mut tool_message = json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": content,
                        });
                        if let Some(name) = tool_name.as_deref()
                            && !name.is_empty()
                        {
                            tool_message["name"] = json!(name);
                        }
                        messages.push(tool_message);
                    }
                    LlmContentBlock::Reasoning { .. } | LlmContentBlock::Attachment { .. } => {}
                    LlmContentBlock::Text { .. } | LlmContentBlock::ToolCall { .. } => {}
                }
            }

            if matches!(
                msg.role,
                LlmRole::System | LlmRole::User | LlmRole::Assistant
            ) && (!text_parts.is_empty() || !tool_calls.is_empty())
            {
                let mut wire_message = json!({ "role": role });
                if !text_parts.is_empty() {
                    wire_message["content"] = Value::Array(text_parts);
                } else if matches!(msg.role, LlmRole::Assistant) {
                    wire_message["content"] = Value::Null;
                }
                if !tool_calls.is_empty() {
                    wire_message["tool_calls"] = Value::Array(tool_calls);
                }
                if !reasoning_details.is_empty() {
                    wire_message["reasoning_details"] = Value::Array(reasoning_details);
                }
                messages.push(wire_message);
            }
        }
        messages
    }

    fn build_chat_tools(
        req: &LlmRequest,
        strict_tools: bool,
        capabilities: &ProviderSchemaCapabilities,
    ) -> Result<Vec<Value>, LlmTransportError> {
        req.tools
            .iter()
            .map(|tool| {
                let parameters = shared::projected_schema(
                    PROVIDER,
                    &tool.input_schema,
                    capabilities,
                    SchemaPurpose::ToolInput,
                )?;
                Ok(json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": parameters,
                        "strict": strict_tools,
                    },
                }))
            })
            .collect()
    }

    fn add_cache_control_to_text_content(message: &mut Value, cache_control: &Value) -> bool {
        let Some(content) = message.get_mut("content") else {
            return false;
        };
        let Some(parts) = content.as_array_mut() else {
            return false;
        };
        for part in parts.iter_mut().rev() {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                part["cache_control"] = cache_control.clone();
                return true;
            }
        }
        false
    }

    fn add_cache_control_to_marked_text_content(
        message: &mut Value,
        cache_control: &Value,
    ) -> bool {
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            return false;
        };
        for part in parts.iter_mut().rev() {
            let is_marked = part
                .get("__lash_cache_breakpoint")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_marked && part.get("type").and_then(Value::as_str) == Some("text") {
                part["cache_control"] = cache_control.clone();
                part.as_object_mut()
                    .map(|obj| obj.remove("__lash_cache_breakpoint"));
                return true;
            }
        }
        false
    }

    fn strip_internal_cache_markers(messages: &mut [Value]) {
        for message in messages {
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    part.as_object_mut()
                        .map(|obj| obj.remove("__lash_cache_breakpoint"));
                }
            }
        }
    }

    fn apply_chat_cache_control(
        req: &LlmRequest,
        cache_retention: CacheRetention,
        messages: &mut [Value],
        tools: &mut [Value],
    ) -> CacheBreakpointDiagnostics {
        let requested = req
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter(|block| {
                matches!(
                    block,
                    LlmContentBlock::Text {
                        cache_breakpoint: true,
                        ..
                    }
                )
            })
            .count();
        let Some(dialect) = req.model_capability.cache_control else {
            Self::strip_internal_cache_markers(messages);
            return CacheBreakpointDiagnostics {
                requested,
                emitted: 0,
                dropped: requested,
            };
        };
        let extended_ttl = matches!(dialect, CacheControlDialect::Anthropic);
        let Some(cache_control) = Self::chat_cache_control_value(cache_retention, extended_ttl)
        else {
            Self::strip_internal_cache_markers(messages);
            return CacheBreakpointDiagnostics {
                requested,
                emitted: 0,
                dropped: requested,
            };
        };

        if matches!(dialect, CacheControlDialect::Gemini) {
            let applied_explicit_breakpoint = messages.iter_mut().rev().any(|message| {
                Self::add_cache_control_to_marked_text_content(message, &cache_control)
            });
            if !applied_explicit_breakpoint {
                for message in messages.iter_mut().rev() {
                    if matches!(
                        message.get("role").and_then(Value::as_str),
                        Some("user" | "assistant")
                    ) && Self::add_cache_control_to_text_content(message, &cache_control)
                    {
                        break;
                    }
                }
            }
            Self::strip_internal_cache_markers(messages);
            let emitted = usize::from(applied_explicit_breakpoint);
            return CacheBreakpointDiagnostics {
                requested,
                emitted,
                dropped: requested.saturating_sub(emitted),
            };
        }

        for message in messages.iter_mut() {
            if matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            ) {
                Self::add_cache_control_to_text_content(message, &cache_control);
                break;
            }
        }
        if let Some(last_tool) = tools.last_mut() {
            last_tool["cache_control"] = cache_control.clone();
        }
        let mut applied_explicit_breakpoint = false;
        for message in messages.iter_mut().rev() {
            if matches!(
                message.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) && Self::add_cache_control_to_marked_text_content(message, &cache_control)
            {
                applied_explicit_breakpoint = true;
                break;
            }
        }
        if !applied_explicit_breakpoint {
            for message in messages.iter_mut().rev() {
                if matches!(
                    message.get("role").and_then(Value::as_str),
                    Some("user" | "assistant")
                ) && Self::add_cache_control_to_text_content(message, &cache_control)
                {
                    break;
                }
            }
        }
        Self::strip_internal_cache_markers(messages);
        let emitted = usize::from(applied_explicit_breakpoint);
        CacheBreakpointDiagnostics {
            requested,
            emitted,
            dropped: requested.saturating_sub(emitted),
        }
    }

    pub(crate) fn build_chat_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        self.build_chat_request_body_with_diagnostics(req, stream)
            .map(|(body, _)| body)
    }

    pub(crate) fn build_chat_request_body_with_diagnostics(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<(Value, CacheBreakpointDiagnostics), LlmTransportError> {
        Self::validate_chat_attachments(req)?;
        let compat = self.resolved_compat(CompletionEndpoint::ChatCompletions);
        let mut messages = Self::build_chat_messages(req);
        let mut tools =
            Self::build_chat_tools(req, compat.strict_tools, &compat.schema_capabilities)?;
        let policy = resolve_generation_policy(
            &req.generation,
            &self.options,
            DEFAULT_MAX_OUTPUT_TOKENS,
            (),
        );
        let cache_diagnostics =
            Self::apply_chat_cache_control(req, policy.cache_retention, &mut messages, &mut tools);
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": stream,
        });
        if let Some(provider_routing) = compat.provider_routing {
            body["provider"] = json!(provider_routing);
        }
        apply_max_tokens_field(&mut body, compat.max_tokens_field, policy.max_output_tokens);
        // Chat Completions is the one dialect that carries both sampling
        // controls; each is omitted entirely when the caller set nothing.
        if let Some(temperature) = &policy.temperature {
            body["temperature"] = Value::Number(temperature.clone().into());
        }
        if let Some(seed) = policy.seed {
            body["seed"] = json!(seed);
        }
        if !policy.stop_sequences.is_empty() {
            body["stop"] = json!(policy.stop_sequences);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!(tool_choice_value(&req.tool_choice));
            if compat.request_fields {
                body["parallel_tool_calls"] = json!(true);
            }
        }
        if stream && compat.streaming_usage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(intent) = reasoning_intent(req) {
            compat
                .reasoning_format
                .encode(CompletionEndpoint::ChatCompletions, &intent, &mut body)
                .map_err(|error| {
                    reasoning_encode_transport_error(
                        CompletionEndpoint::ChatCompletions,
                        &intent,
                        error,
                    )
                })?;
        }
        if let Some(output_spec) = &req.output_spec {
            body["response_format"] = match output_spec {
                LlmOutputSpec::JsonObject => json!({ "type": "json_object" }),
                LlmOutputSpec::JsonSchema(schema) => {
                    let projected = shared::projected_schema(
                        PROVIDER,
                        &schema.schema,
                        &compat.schema_capabilities,
                        SchemaPurpose::StructuredOutput,
                    )?;
                    json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": schema.name,
                            "schema": projected,
                            "strict": schema.strict,
                        },
                    })
                }
            };
        }
        Ok((body, cache_diagnostics))
    }

    fn chat_message_text(message: &Value) -> String {
        let content = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .collect::<String>(),
            _ => String::new(),
        };
        if content.is_empty() {
            message
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        } else {
            content
        }
    }

    pub(crate) fn chat_response_parts_from_value(value: &Value) -> Vec<LlmOutputPart> {
        let mut parts = Vec::new();
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return parts;
        };
        let Some(message) = choice.get("message") else {
            return parts;
        };
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(reasoning) = message.get(field).and_then(Value::as_str)
                && !reasoning.trim().is_empty()
            {
                parts.push(LlmOutputPart::Reasoning {
                    text: reasoning.to_string(),
                    replay: None,
                });
                break;
            }
        }
        let text = Self::chat_message_text(message);
        if !text.is_empty() {
            parts.push(LlmOutputPart::Text {
                text,
                response_meta: None,
            });
        }
        let reasoning_details = message
            .get("reasoning_details")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|detail| {
                detail.get("type").and_then(Value::as_str) == Some("reasoning.encrypted")
                    && detail.get("id").and_then(Value::as_str).is_some()
                    && detail.get("data").and_then(Value::as_str).is_some()
            })
            .map(|detail| {
                (
                    detail
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    detail.to_string(),
                )
            })
            .collect::<HashMap<_, _>>();
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let Some(function) = tool_call.get("function") else {
                    continue;
                };
                let Some(name) = function.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let arguments = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string();
                parts.push(LlmOutputPart::ToolCall {
                    call_id: tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    tool_name: name.to_string(),
                    input_json: arguments,
                    replay: tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| reasoning_details.get(id).cloned())
                        .map(|opaque| ProviderReplayMeta {
                            item_id: None,
                            opaque: Some(opaque),
                        }),
                });
            }
        }
        parts
    }

    pub(crate) fn process_chat_sse_event(
        raw: &str,
        state: &mut ChatStreamState,
    ) -> Result<(), LlmTransportError> {
        let raw = raw.trim();
        if raw.is_empty() || raw == "[DONE]" {
            return Ok(());
        }
        let event: ChatSseEvent<'_> = serde_json::from_str(raw).map_err(|e| {
            LlmTransportError::new(format!("Invalid Chat Completions SSE payload: {e}"))
                .with_raw(raw)
        })?;
        if let Some(error) = event.error.as_ref() {
            let retryable = responses_error_is_retryable(error);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI-compatible chat stream error");
            return Err(LlmTransportError::new(message)
                .retryable(retryable)
                .with_raw(raw));
        }
        if let Some(usage) = event.usage.as_ref()
            && !usage.is_null()
        {
            state.provider_usage = Some(usage.clone());
            merge_usage(&mut state.usage, &usage_from_usage_value(usage));
            state.capture_reasoning_tokens(usage);
        }
        state.capture_identity(event.id, event.model);
        state.final_response_raw = Some(raw.to_string());
        for choice in event.choices {
            if let Some(usage) = choice.usage.as_ref()
                && !usage.is_null()
            {
                state.provider_usage = Some(usage.clone());
                merge_usage(&mut state.usage, &usage_from_usage_value(usage));
                state.capture_reasoning_tokens(usage);
            }
            if let Some(finish_reason) = choice.finish_reason {
                if state.provider_finish_reason.is_none() {
                    state.provider_finish_reason = choice
                        .native_finish_reason
                        .filter(|reason| !reason.is_empty())
                        .or_else(|| (!finish_reason.is_empty()).then_some(finish_reason))
                        .map(str::to_string);
                }
                state.terminal_reason =
                    terminal_reason_from_chat_finish_reason(finish_reason, state.terminal_reason);
            }
            let Some(delta) = choice.delta else {
                continue;
            };
            if let Some(content) = delta.content.as_ref() {
                state.push_text_delta(content);
            }
            if let Some(reasoning) = delta
                .reasoning_content
                .as_ref()
                .or(delta.reasoning.as_ref())
                .or(delta.reasoning_text.as_ref())
                && !reasoning.is_empty()
            {
                state.push_reasoning_delta(reasoning);
            }
            if let Some(tool_calls) = delta.tool_calls.as_ref() {
                for tool_call in tool_calls {
                    state.update_tool_call_delta(tool_call);
                }
            }
            if let Some(details) = delta.reasoning_details.as_ref() {
                state.apply_reasoning_details(details);
            }
        }
        Ok(())
    }

    pub(crate) fn parse_chat_sse_payload(
        payload: &str,
        state: &mut ChatStreamState,
    ) -> Result<(), LlmTransportError> {
        lash_llm_transport::frame_sse_payload(payload, |raw| {
            Self::process_chat_sse_event(raw, state)
        })
    }
}

#[derive(Debug, Deserialize)]
struct ChatSseEvent<'a> {
    #[serde(default, borrow)]
    id: Option<&'a str>,
    #[serde(default, borrow)]
    model: Option<&'a str>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default, borrow)]
    choices: Vec<ChatSseChoice<'a>>,
}

#[derive(Debug, Deserialize)]
struct ChatSseChoice<'a> {
    #[serde(default, borrow)]
    delta: Option<ChatSseDelta<'a>>,
    #[serde(default, borrow)]
    finish_reason: Option<&'a str>,
    #[serde(default, borrow)]
    native_finish_reason: Option<&'a str>,
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatSseDelta<'a> {
    #[serde(default, borrow)]
    content: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    reasoning_content: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    reasoning: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    reasoning_text: Option<Cow<'a, str>>,
    #[serde(default)]
    tool_calls: Option<Vec<Value>>,
    #[serde(default)]
    reasoning_details: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ChatStreamingToolCall {
    pub(crate) call_id: String,
    pub(crate) tool_name: String,
    pub(crate) input_json: String,
    pub(crate) signature: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ChatStreamState {
    pub(crate) full_text: String,
    pub(crate) pending_text_deltas: Vec<String>,
    pub(crate) reasoning_text: String,
    pub(crate) reasoning_deltas: Vec<String>,
    pub(crate) usage: LlmUsage,
    pub(crate) provider_usage: Option<Value>,
    pub(crate) tool_calls: HashMap<usize, ChatStreamingToolCall>,
    emitted_tool_call_indices: std::collections::HashSet<usize>,
    pub(crate) final_response_raw: Option<String>,
    pub(crate) terminal_reason: LlmTerminalReason,
    pub(crate) provider_response_id: Option<String>,
    pub(crate) served_model: Option<String>,
    pub(crate) reasoning_output_tokens: Option<u64>,
    pub(crate) provider_finish_reason: Option<String>,
}

impl ChatStreamState {
    pub(crate) fn capture_response_value(&mut self, value: &Value) {
        self.capture_identity(
            value.get("id").and_then(Value::as_str),
            value.get("model").and_then(Value::as_str),
        );
        if let Some(usage) = value.get("usage") {
            self.capture_reasoning_tokens(usage);
        }
        self.provider_finish_reason = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| {
                choice
                    .get("native_finish_reason")
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.is_empty())
                    .or_else(|| {
                        choice
                            .get("finish_reason")
                            .and_then(Value::as_str)
                            .filter(|reason| !reason.is_empty())
                    })
            })
            .map(str::to_string);
    }

    pub(crate) fn capture_identity(&mut self, id: Option<&str>, model: Option<&str>) {
        if self.provider_response_id.is_none()
            && let Some(id) = id.filter(|value| !value.is_empty())
        {
            self.provider_response_id = Some(id.to_string());
        }
        if self.served_model.is_none()
            && let Some(model) = model.filter(|value| !value.is_empty())
        {
            self.served_model = Some(model.to_string());
        }
    }

    pub(crate) fn capture_reasoning_tokens(&mut self, usage: &Value) {
        if let Some(tokens) = usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
        {
            self.reasoning_output_tokens = Some(tokens);
        }
    }

    pub(crate) fn execution_evidence(&self) -> Option<ExecutionEvidence> {
        let evidence = ExecutionEvidence {
            served_model: self.served_model.clone(),
            provider_response_id: self.provider_response_id.clone(),
            provider_request_id: None,
            reasoning_output_tokens: self.reasoning_output_tokens,
            provider_finish_reason: self.provider_finish_reason.clone(),
            collection_interruption: None,
        };
        (evidence != ExecutionEvidence::default()).then_some(evidence)
    }
    pub(crate) fn push_text_delta(&mut self, piece: &str) {
        if piece.is_empty() {
            return;
        }
        self.full_text.push_str(piece);
        self.pending_text_deltas.push(piece.to_string());
    }

    pub(crate) fn push_reasoning_delta(&mut self, piece: &str) {
        if piece.is_empty() {
            return;
        }
        self.reasoning_text.push_str(piece);
        self.reasoning_deltas.push(piece.to_string());
    }

    pub(crate) fn update_tool_call_delta(&mut self, value: &Value) {
        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let tool_call = self.tool_calls.entry(index).or_default();
        if let Some(id) = value.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            tool_call.call_id = id.to_string();
        }
        if let Some(function) = value.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str)
                && !name.is_empty()
            {
                tool_call.tool_name = name.to_string();
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                && !arguments.is_empty()
            {
                tool_call.input_json.push_str(arguments);
            }
        }
    }

    pub(crate) fn apply_reasoning_details(&mut self, details: &Value) {
        let Some(details) = details.as_array() else {
            return;
        };
        for detail in details {
            if detail.get("type").and_then(Value::as_str) != Some("reasoning.encrypted") {
                continue;
            }
            let Some(id) = detail.get("id").and_then(Value::as_str) else {
                continue;
            };
            if detail.get("data").and_then(Value::as_str).is_none() {
                continue;
            }
            for tool_call in self.tool_calls.values_mut() {
                if tool_call.call_id == id {
                    tool_call.signature = Some(detail.to_string());
                    break;
                }
            }
        }
    }

    pub(crate) fn take_reasoning_deltas(&mut self) -> Vec<String> {
        std::mem::take(&mut self.reasoning_deltas)
    }

    pub(crate) fn take_text_deltas(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_text_deltas)
    }

    fn take_tool_call_parts(&mut self, require_complete_json: bool) -> Vec<LlmOutputPart> {
        let mut indices = self.tool_calls.keys().copied().collect::<Vec<_>>();
        indices.sort_unstable();
        let mut parts = Vec::new();
        for index in indices {
            if self.emitted_tool_call_indices.contains(&index) {
                continue;
            }
            let Some(tool_call) = self.tool_calls.get_mut(&index) else {
                continue;
            };
            if tool_call.tool_name.is_empty()
                || (require_complete_json
                    && (tool_call.input_json.is_empty()
                        || !matches!(
                            serde_json::from_str::<Value>(&tool_call.input_json),
                            Ok(Value::Object(_))
                        )))
            {
                continue;
            }
            if tool_call.call_id.is_empty() {
                tool_call.call_id = uuid::Uuid::new_v4().to_string();
            }
            parts.push(LlmOutputPart::ToolCall {
                call_id: tool_call.call_id.clone(),
                tool_name: tool_call.tool_name.clone(),
                input_json: if tool_call.input_json.is_empty() {
                    "{}".to_string()
                } else {
                    tool_call.input_json.clone()
                },
                replay: tool_call
                    .signature
                    .clone()
                    .map(|opaque| ProviderReplayMeta {
                        item_id: None,
                        opaque: Some(opaque),
                    }),
            });
            self.emitted_tool_call_indices.insert(index);
        }
        parts
    }

    pub(crate) fn take_completed_tool_call_parts(&mut self) -> Vec<LlmOutputPart> {
        self.take_tool_call_parts(true)
    }

    pub(crate) fn take_remaining_tool_call_parts(&mut self) -> Vec<LlmOutputPart> {
        self.take_tool_call_parts(false)
    }

    pub(crate) fn parts(&self) -> Vec<LlmOutputPart> {
        let mut parts = Vec::new();
        if !self.reasoning_text.trim().is_empty() {
            parts.push(LlmOutputPart::Reasoning {
                text: self.reasoning_text.clone(),
                replay: None,
            });
        }
        if !self.full_text.is_empty() {
            parts.push(LlmOutputPart::Text {
                text: self.full_text.clone(),
                response_meta: None,
            });
        }
        let mut tool_calls = self.tool_calls.iter().collect::<Vec<_>>();
        tool_calls.sort_by_key(|(index, _)| **index);
        for (_, tool_call) in tool_calls {
            if tool_call.tool_name.is_empty() {
                continue;
            }
            parts.push(LlmOutputPart::ToolCall {
                call_id: if tool_call.call_id.is_empty() {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    tool_call.call_id.clone()
                },
                tool_name: tool_call.tool_name.clone(),
                input_json: if tool_call.input_json.is_empty() {
                    "{}".to_string()
                } else {
                    tool_call.input_json.clone()
                },
                replay: tool_call
                    .signature
                    .clone()
                    .map(|opaque| ProviderReplayMeta {
                        item_id: None,
                        opaque: Some(opaque),
                    }),
            });
        }
        if parts.is_empty()
            && let Some(final_response) = self
                .final_response_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        {
            parts = OpenAiCompatibleProvider::chat_response_parts_from_value(&final_response);
        }
        parts
    }
}
