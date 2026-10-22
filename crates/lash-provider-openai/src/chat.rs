use crate::responses_shared as shared;
use crate::support::*;
use lash_core::facade_support::{ModelToolReturnPart, SchemaResolutionRequest, resolve_schema};
use lash_core::llm::types::{ExecutionEvidenceMergeError, LlmMessage};
use lash_llm_transport::tool_result_text;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use std::borrow::Cow;

const PROVIDER: &str = "OpenAI-compatible";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheBreakpointDiagnostics {
    pub(crate) requested: usize,
    pub(crate) emitted: usize,
    pub(crate) dropped: usize,
    pub(crate) cache_control_emitted: bool,
}

/// Where a Chat Completions body carries the request's cache control.
#[derive(Default)]
struct ChatCachePlan {
    ctrl: Option<Value>,
    /// Whether the instruction message carries it.
    instructions: bool,
    target: Option<ChatCacheTarget>,
}

/// The message part that carries the cache control besides the
/// instruction message and the last tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatCacheTarget {
    /// The last marked text part of the source message at this index.
    Marked(usize),
    /// The last text part of the source message at this index.
    LastText(usize),
    /// The instruction message's text: no history message has text.
    Instructions,
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

    fn chat_attachment_part(
        reference: &AttachmentRef,
        position: AttachmentPosition,
    ) -> TemplateJson {
        TemplateJson::object([
            ("type", json!("image_url").into()),
            (
                "image_url",
                TemplateJson::object([("url", TemplateJson::attachment(reference, position))]),
            ),
        ])
    }

    /// Build the `messages` array: the instruction message, then the
    /// history, each group of messages written by `units`.
    ///
    /// A group starts at an assistant message, which ends the feedback a
    /// tool message is placed before, so a group's wire messages are the
    /// same alone as within the whole array, and a group lowered before,
    /// with the same cache control, is reused whole.
    fn build_chat_messages(
        req: &LlmRequest,
        cache: &ChatCachePlan,
        units: &mut UnitWriter<'_>,
    ) -> Result<Vec<TemplateJson>, LlmTransportError> {
        let mut messages: Vec<TemplateJson> = Vec::new();
        if let Some(instructions) = &req.instructions {
            let mut message: TemplateJson = json!({"role": req.model.metadata().capability.instruction_role.as_str(), "content": [{"type": "text", "text": instructions}]}).into();
            if let Some(ctrl) = &cache.ctrl
                && (cache.instructions || cache.target == Some(ChatCacheTarget::Instructions))
            {
                Self::add_cache_control_to_text_content(&mut message, ctrl);
            }
            messages.push(message);
        }
        let mut start = 0;
        for end in 1..=req.messages.len() {
            if end < req.messages.len() && !matches!(req.messages[end].role, LlmRole::Assistant) {
                continue;
            }
            let group = start..end;
            let target = match cache.target {
                Some(ChatCacheTarget::Marked(index)) if group.contains(&index) => {
                    Some((index, true))
                }
                Some(ChatCacheTarget::LastText(index)) if group.contains(&index) => {
                    Some((index, false))
                }
                _ => None,
            };
            messages.extend(units.unit_at(
                group.clone(),
                |key| {
                    let key = req.messages[group.clone()]
                        .iter()
                        .fold(key, |key, msg| key.message(msg));
                    match (target, &cache.ctrl) {
                        (Some((index, marked)), Some(ctrl)) => key
                            .u64((index - group.start) as u64)
                            .u64(u64::from(marked))
                            .str(&ctrl.to_string()),
                        _ => key.u64(u64::MAX),
                    }
                },
                || {
                    Ok(Self::build_chat_group(
                        req,
                        group.clone(),
                        target,
                        cache.ctrl.as_ref(),
                    ))
                },
            )?);
            start = end;
        }
        Ok(messages)
    }

    /// The wire messages of the source messages in `group`, the one at
    /// `target` carrying `ctrl`: on its last marked text part, or on its
    /// last text part.
    fn build_chat_group(
        req: &LlmRequest,
        group: std::ops::Range<usize>,
        target: Option<(usize, bool)>,
        ctrl: Option<&Value>,
    ) -> Vec<TemplateJson> {
        let mut messages: Vec<TemplateJson> = Vec::new();
        let mut feedback_start = None;
        for (index, msg) in req
            .messages
            .iter()
            .enumerate()
            .skip(group.start)
            .take(group.len())
        {
            let target = target
                .filter(|(at, _)| *at == index)
                .and_then(|(_, marked)| Some((marked, ctrl?)));
            shared::feedback_boundary(msg, messages.len(), &mut feedback_start);
            let fallback = shared::attachment_feedback(msg);
            let msg = fallback.as_ref().unwrap_or(msg);
            let role = if matches!(msg.role, LlmRole::System) {
                req.model.metadata().capability.instruction_role.as_str()
            } else {
                role_name(&msg.role)
            };
            let mut text_parts: Vec<TemplateJson> = Vec::new();
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
                            && matches!(target, Some((true, _)))
                        {
                            part["__lash_cache_breakpoint"] = json!(true);
                        }
                        text_parts.push(part.into());
                    }
                    LlmContentBlock::Attachment { reference }
                        if matches!(msg.role, LlmRole::User) =>
                    {
                        text_parts.push(Self::chat_attachment_part(
                            reference,
                            AttachmentPosition::Message,
                        ));
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
                                "arguments": tool_call_input_replay_string(input_json),
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
                        status,
                    } => {
                        // A `tool` message carries text only: the result's
                        // text keeps `[Attachment N]` markers where its
                        // attachments sat, and the attachments follow, in
                        // marker order, in the user message after the tool
                        // messages.
                        let mut attachments = content
                            .iter()
                            .filter_map(ModelToolReturnPart::attachment)
                            .peekable();
                        if attachments.peek().is_some() {
                            text_parts.push(
                                json!({
                                    "type": "text",
                                    "text": format!("Attachments from tool result {call_id}:"),
                                })
                                .into(),
                            );
                            text_parts.extend(attachments.map(|reference| {
                                Self::chat_attachment_part(
                                    reference,
                                    AttachmentPosition::ToolResult,
                                )
                            }));
                        }
                        let mut tool_message = json!({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": if status.is_error() {
                                format!("Error: {}", tool_result_text(content))
                            } else {
                                tool_result_text(content).into_owned()
                            },
                        });
                        if let Some(name) = tool_name.as_deref()
                            && !name.is_empty()
                        {
                            tool_message["name"] = json!(name);
                        }
                        shared::push_tool_output(
                            &mut messages,
                            tool_message.into(),
                            &mut feedback_start,
                        );
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
                if text_parts.is_empty() && matches!(msg.role, LlmRole::Assistant) {
                    wire_message["content"] = Value::Null;
                }
                if !tool_calls.is_empty() {
                    wire_message["tool_calls"] = Value::Array(tool_calls);
                }
                if !reasoning_details.is_empty() {
                    wire_message["reasoning_details"] = Value::Array(reasoning_details);
                }
                let mut wire_message = TemplateJson::from(wire_message);
                if !text_parts.is_empty() {
                    wire_message.set("content", text_parts);
                }
                match target {
                    Some((true, ctrl)) => {
                        Self::add_cache_control_to_marked_text_content(&mut wire_message, ctrl);
                        // Only the last marked part carries the control.
                        if let Some(parts) = Self::content_parts(&mut wire_message) {
                            for part in parts {
                                part.remove("__lash_cache_breakpoint");
                            }
                        }
                    }
                    Some((false, ctrl)) => {
                        Self::add_cache_control_to_text_content(&mut wire_message, ctrl);
                    }
                    None => {}
                }
                messages.push(wire_message);
            }
        }
        messages
    }

    fn build_chat_tools(
        req: &LlmRequest,
        capabilities: &ProviderSchemaCapabilities,
    ) -> Result<Vec<Value>, LlmTransportError> {
        req.tools
            .iter()
            .map(|tool| {
                let resolved = resolve_schema(
                    &tool.input_schema,
                    SchemaResolutionRequest {
                        provider: PROVIDER,
                        purpose: SchemaPurpose::ToolInput,
                        dialects: capabilities.dialects_for(SchemaPurpose::ToolInput),
                    },
                ).map_err(|error| shared::projection_error(PROVIDER, error))?;
                Ok(json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": resolved.schema,
                        "strict": resolved.dialect == lash_vocabulary::schema_contract::SchemaDialect::OpenaiStrictToolParameters,
                    },
                }))
            })
            .collect()
    }

    /// The content parts of a wire message, when its content is an array.
    fn content_parts(message: &mut TemplateJson) -> Option<&mut Vec<TemplateJson>> {
        if !message
            .json_field("content")
            .is_none_or(serde_json::Value::is_array)
        {
            return None;
        }
        message.field_mut("content")?.as_array_mut()
    }

    fn add_cache_control_to_text_content(
        message: &mut TemplateJson,
        cache_control: &Value,
    ) -> bool {
        let Some(parts) = Self::content_parts(message) else {
            return false;
        };
        for part in parts.iter_mut().rev() {
            if part.str_field("type") == Some("text") {
                part.set("cache_control", cache_control.clone());
                return true;
            }
        }
        false
    }

    fn add_cache_control_to_marked_text_content(
        message: &mut TemplateJson,
        cache_control: &Value,
    ) -> bool {
        let Some(parts) = Self::content_parts(message) else {
            return false;
        };
        for part in parts.iter_mut().rev() {
            let is_marked = part
                .json_field("__lash_cache_breakpoint")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_marked && part.str_field("type") == Some("text") {
                part.set("cache_control", cache_control.clone());
                part.remove("__lash_cache_breakpoint");
                return true;
            }
        }
        false
    }

    /// Whether `msg`'s wire message has a marked text part, and whether it
    /// has a text part at all. Feedback carrying attachments is one tagged
    /// text part, marked when any of its text was.
    fn chat_text_parts(msg: &LlmMessage) -> (bool, bool) {
        let marked = |block: &LlmContentBlock| {
            matches!(
                block,
                LlmContentBlock::Text {
                    cache_breakpoint: true,
                    ..
                }
            )
        };
        if matches!(msg.role, LlmRole::System)
            && msg
                .blocks
                .iter()
                .any(|block| matches!(block, LlmContentBlock::Attachment { .. }))
        {
            return (msg.blocks.iter().any(marked), true);
        }
        let text = |block: &LlmContentBlock| matches!(block, LlmContentBlock::Text { text, .. } if !text.is_empty());
        (
            msg.blocks.iter().any(|block| text(block) && marked(block)),
            msg.blocks.iter().any(|block| {
                text(block)
                    || matches!(block, LlmContentBlock::ToolResult { content, .. }
                        if content.iter().any(|part| part.attachment().is_some()))
            }),
        )
    }

    /// Where the request's cache control goes, decided before any message
    /// is built: the last marked text part, else the last text part of a
    /// message, the instruction message's included; and, except in the
    /// Gemini dialect, the instruction message and the last tool too.
    fn plan_chat_cache_control(
        req: &LlmRequest,
        cache_retention: CacheRetention,
        tools: &mut [Value],
    ) -> (ChatCachePlan, CacheBreakpointDiagnostics) {
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
        let unplanned = (
            ChatCachePlan::default(),
            CacheBreakpointDiagnostics {
                requested,
                emitted: 0,
                dropped: requested,
                cache_control_emitted: false,
            },
        );
        let Some(dialect) = req.model.metadata().capability.cache_control else {
            return unplanned;
        };
        let extended_ttl = matches!(dialect, CacheControlDialect::Anthropic);
        let Some(cache_control) = Self::chat_cache_control_value(cache_retention, extended_ttl)
        else {
            return unplanned;
        };
        let gemini = matches!(dialect, CacheControlDialect::Gemini);
        let marked = req
            .messages
            .iter()
            .rposition(|message| Self::chat_text_parts(message).0);
        let target = match marked {
            Some(index) => Some(ChatCacheTarget::Marked(index)),
            None => req
                .messages
                .iter()
                .rposition(|message| Self::chat_text_parts(message).1)
                .map(ChatCacheTarget::LastText)
                .or(req
                    .instructions
                    .is_some()
                    .then_some(ChatCacheTarget::Instructions)),
        };
        let instructions = !gemini && req.instructions.is_some();
        let mut cache_control_emitted = target.is_some() || instructions;
        if !gemini && let Some(last_tool) = tools.last_mut() {
            last_tool["cache_control"] = cache_control.clone();
            cache_control_emitted = true;
        }
        let emitted = usize::from(marked.is_some());
        (
            ChatCachePlan {
                ctrl: Some(cache_control),
                instructions,
                target,
            },
            CacheBreakpointDiagnostics {
                requested,
                emitted,
                dropped: requested.saturating_sub(emitted),
                cache_control_emitted,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn build_chat_request_body(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<Value, LlmTransportError> {
        self.build_chat_request_body_with_diagnostics(req, stream)
            .map(|(built, _)| built.body.redacted())
    }

    /// [`Self::build_chat_request`] with every message built as JSON.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_chat_request_body_with_diagnostics(
        &self,
        req: &LlmRequest,
        stream: bool,
    ) -> Result<(BuiltRequest<TemplateJson>, CacheBreakpointDiagnostics), LlmTransportError> {
        self.build_chat_request(req, stream, &mut UnitWriter::Plain)
    }

    /// What Chat Completions, as this endpoint's compat configures it, can
    /// carry for this request.
    fn chat_generation_wire(compat: &OpenAiResolvedCompat, req: &LlmRequest) -> GenerationWire {
        GenerationWire {
            label: "OpenAI-compatible Chat Completions",
            output_token_cap: output_cap_wire(compat.max_tokens_field),
            temperature: true,
            seed: true,
            stop_sequences: true,
            // Chat Completions rejects `parallel_tool_calls` without tools,
            // and `request_fields: false` endpoints take none of these fields.
            parallel_tool_calls: compat.request_fields && !req.tools.is_empty(),
            thinking_summary: ThinkingSummaryWire::NoField,
            active_thinking_pins_sampling: false,
        }
    }

    /// The Chat Completions body and the diagnostics of its cache
    /// breakpoints, each group of history messages written by `units`.
    pub(crate) fn build_chat_request(
        &self,
        req: &LlmRequest,
        stream: bool,
        units: &mut UnitWriter<'_>,
    ) -> Result<(BuiltRequest<TemplateJson>, CacheBreakpointDiagnostics), LlmTransportError> {
        let serving_route = self.route_identity(req.model.wire_model());
        let safe_request = req
            .reasoning_retention_safe_for(
                &serving_route,
                "OpenAI Chat Completions",
                ProviderReasoningRetentionSupport::ClientSideUserSegments,
            )
            .map_err(reasoning_retention_transport_error)?;
        let req = safe_request.as_ref();
        let compat = self.resolved_compat(CompletionEndpoint::ChatCompletions);
        let policy =
            resolve_generation_policy(req, self.kind(), &Self::chat_generation_wire(&compat, req))?;
        let mut emission = GenerationEmission {
            reasoning_retention: matches!(
                req.model
                    .metadata()
                    .capability
                    .reasoning_retention
                    .selection,
                ReasoningRetentionSelection::ClientSideUserSegments { .. }
            ),
            ..GenerationEmission::default()
        };
        let mut reasoning_body = json!({});
        if let Some(intent) = &policy.reasoning {
            apply_reasoning(
                CompletionEndpoint::ChatCompletions,
                compat.reasoning,
                intent,
                &mut reasoning_body,
            )?;
            emission.reasoning = true;
        }
        let mut tools = Self::build_chat_tools(req, &compat.schema_capabilities)?;
        let (cache_plan, cache_diagnostics) =
            Self::plan_chat_cache_control(req, policy.cache_retention, &mut tools);
        let messages = Self::build_chat_messages(req, &cache_plan, units)?;
        emission.cache = cache_diagnostics.cache_control_emitted;
        let mut body = json!({
            "model": req.model.wire_model(),
            // Set last: the messages hold the request's attachment slots.
            "messages": [],
            "stream": stream,
        });
        if let Some(provider_routing) = compat.provider_routing {
            body["provider"] = json!(provider_routing);
        }
        emission.output_token_cap =
            apply_max_tokens_field(&mut body, compat.max_tokens_field, policy.max_output_tokens);
        if let Some(temperature) = &policy.temperature {
            body["temperature"] = Value::Number(temperature.clone().into());
            emission.temperature = true;
        }
        if let Some(seed) = policy.seed {
            body["seed"] = json!(seed);
            emission.seed = true;
        }
        if !policy.stop_sequences.is_empty() {
            body["stop"] = json!(policy.stop_sequences);
            emission.stop_sequences = true;
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!(tool_choice_value(&req.tool_choice));
        }
        if let Some(parallel_tool_calls) = policy.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel_tool_calls);
            emission.parallel_tool_calls = true;
        }
        if stream && compat.streaming_usage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Value::Object(fields) = reasoning_body {
            for (key, value) in fields {
                body[key] = value;
            }
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
        let mut reserved = reserved_generation_paths(req, "/stop", "/temperature");
        if compat.cache_session_affinity {
            reserved.push("/session_id");
        }
        let passthrough = merge_extra_body(&mut body, &req.model.metadata().extra_body, &reserved)?;
        let mut receipt = policy.receipt(req, &emission);
        receipt.passthrough = if !self.wire.extra_headers.is_empty() {
            lash_core::GenerationOptionOutcome::Applied
        } else {
            passthrough
        };
        let mut body = TemplateJson::from(body);
        body.set("messages", messages);
        Ok((BuiltRequest { body, receipt }, cache_diagnostics))
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

    pub(crate) fn chat_response_parts_from_value_with_decoder(
        value: &Value,
        tool_argument_decoder: &shared::ToolArgumentDecoder,
    ) -> Vec<LlmOutputPart> {
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
                        .unwrap_or_default(),
                    tool_name: name.to_string(),
                    input_json: tool_argument_decoder.decode(name, arguments),
                    replay: tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| reasoning_details.get(id).cloned())
                        .map(|opaque| ProviderReplayMeta {
                            item_id: None,
                            opaque: Some(opaque),
                            ..ProviderReplayMeta::default()
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
                .with_raw(state.diagnostic_limits.body_excerpt(raw))
                .with_retry_verdict(TransportRetryVerdict::NotRetryable)
        })?;
        if let Some(error) = event.error.as_ref() {
            let retry_verdict = responses_error_retry_verdict(error);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("OpenAI-compatible chat stream error");
            return Err(LlmTransportError::new(
                state.diagnostic_limits.diagnostic_message(message),
            )
            .with_retry_verdict(retry_verdict)
            .with_raw(state.diagnostic_limits.body_excerpt(raw)));
        }
        let mut event_evidence = None;
        merge_execution_evidence(
            &mut event_evidence,
            ExecutionEvidence {
                served_model: event
                    .model
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                provider_response_id: event
                    .id
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                reasoning_output_tokens: event.usage.as_ref().and_then(reasoning_output_tokens),
                ..ExecutionEvidence::default()
            },
        )?;
        for choice in &event.choices {
            merge_execution_evidence(
                &mut event_evidence,
                ExecutionEvidence {
                    reasoning_output_tokens: choice
                        .usage
                        .as_ref()
                        .and_then(reasoning_output_tokens),
                    provider_finish_reason: choice.finish_reason.and_then(|finish_reason| {
                        choice
                            .native_finish_reason
                            .filter(|reason| !reason.is_empty())
                            .or_else(|| (!finish_reason.is_empty()).then_some(finish_reason))
                            .map(str::to_string)
                    }),
                    ..ExecutionEvidence::default()
                },
            )?;
        }
        // Identity is the monotonic boundary for every field carried by the
        // event evidence. Reject drift before usage, terminal facts, or
        // content from the conflicting route can enter the retained state.
        ExecutionEvidence::merge_optional(&mut state.execution_evidence, event_evidence)
            .map_err(|error| execution_evidence_error("Chat Completions stream", error))?;
        if let Some(usage) = event.usage.as_ref()
            && !usage.is_null()
        {
            state.provider_usage = Some(usage.clone());
            merge_usage(&mut state.usage, &usage_from_usage_value(usage));
        }
        state.final_response_raw = Some(raw.to_string());
        for choice in event.choices {
            if let Some(usage) = choice.usage.as_ref()
                && !usage.is_null()
            {
                state.provider_usage = Some(usage.clone());
                merge_usage(&mut state.usage, &usage_from_usage_value(usage));
            }
            if let Some(finish_reason) = choice.finish_reason {
                state.normal_stop_seen |= finish_reason == "stop";
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

fn execution_evidence_error(
    context: &str,
    error: ExecutionEvidenceMergeError,
) -> LlmTransportError {
    LlmTransportError::new(format!("{context} {error}"))
        .with_kind(ProviderFailureKind::Stream)
        .with_lash_code(TurnFailureCode::from_wire(error.code()))
}

fn merge_execution_evidence(
    accumulated: &mut Option<ExecutionEvidence>,
    next: ExecutionEvidence,
) -> Result<(), LlmTransportError> {
    ExecutionEvidence::merge_optional(accumulated, Some(next))
        .map_err(|error| execution_evidence_error("Chat Completions stream", error))
}

fn reasoning_output_tokens(usage: &Value) -> Option<u64> {
    usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ChatStreamState {
    pub(crate) diagnostic_limits: crate::RequestDiagnosticLimits,
    pub(crate) full_text: String,
    pub(crate) reasoning_text: String,
    /// Block-boundary stream events minted at the provider edge, drained by
    /// the driver after each SSE event. Chat Completions has no native block
    /// notion, so blocks get deterministic per-response ordinals.
    pub(crate) block_events: Vec<LlmStreamEvent>,
    next_block_ordinal: u64,
    /// Open reasoning/text blocks and the text accumulated under each so the
    /// block end can carry authoritative text.
    reasoning_block: Option<(StreamBlockIdentity, String)>,
    text_block: Option<StreamBlockIdentity>,
    pub(crate) usage: LlmUsage,
    pub(crate) provider_usage: Option<Value>,
    pub(crate) tool_calls: HashMap<usize, ChatStreamingToolCall>,
    emitted_tool_call_indices: std::collections::HashSet<usize>,
    pub(crate) final_response_raw: Option<String>,
    pub(crate) terminal_reason: LlmTerminalReason,
    /// True only when the Chat wire carries the normal successful terminal
    /// status. Native provider evidence is retained separately and cannot
    /// stand in for a missing `finish_reason`.
    pub(crate) normal_stop_seen: bool,
    pub(crate) execution_evidence: Option<ExecutionEvidence>,
    pub(crate) tool_argument_decoder: shared::ToolArgumentDecoder,
    /// Stamped from the request's recorded `expose_thinking` default at state construction
    /// so the assembled `LlmResponse` carries the visibility policy forward
    /// for the runtime's reasoning republication gate.
    pub(crate) expose_thinking: bool,
}

impl ChatStreamState {
    pub(crate) fn with_tool_argument_decoder(
        tool_argument_decoder: shared::ToolArgumentDecoder,
    ) -> Self {
        Self {
            tool_argument_decoder,
            ..Self::default()
        }
    }

    pub(crate) fn capture_response_value(
        &mut self,
        value: &Value,
    ) -> Result<(), LlmTransportError> {
        self.normal_stop_seen |= value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
            == Some("stop");
        let provider_finish_reason = value
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
        ExecutionEvidence::merge_optional(
            &mut self.execution_evidence,
            Some(ExecutionEvidence {
                served_model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                provider_response_id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                reasoning_output_tokens: value.get("usage").and_then(reasoning_output_tokens),
                provider_finish_reason,
                ..ExecutionEvidence::default()
            }),
        )
        .map_err(|error| execution_evidence_error("Chat Completions response", error))
    }
    fn mint_block(&mut self, prefix: &str) -> StreamBlockIdentity {
        let ordinal = self.next_block_ordinal;
        self.next_block_ordinal += 1;
        StreamBlockIdentity::new(format!("{prefix}:{ordinal}"), ordinal)
    }

    /// Chat Completions has no explicit reasoning boundary; reasoning text
    /// precedes assistant text on the wire, so the first prose delta seals
    /// the open reasoning block with its accumulated text.
    fn close_reasoning_block(&mut self) {
        if let Some((block, text)) = self.reasoning_block.take() {
            self.block_events
                .push(LlmStreamEvent::Block(StreamBlockEvent::Completed {
                    kind: StreamBlockKind::Reasoning,
                    block,
                    text,
                }));
        }
    }

    pub(crate) fn push_text_delta(&mut self, piece: &str) {
        if piece.is_empty() {
            return;
        }
        self.close_reasoning_block();
        if self.text_block.is_none() {
            let block = self.mint_block("text");
            self.block_events
                .push(LlmStreamEvent::Block(StreamBlockEvent::Started {
                    kind: StreamBlockKind::AssistantText,
                    block: block.clone(),
                }));
            self.text_block = Some(block);
        }
        let Some(block) = self.text_block.clone() else {
            return;
        };
        self.full_text.push_str(piece);
        self.block_events
            .push(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                block,
                text: piece.to_string(),
            }));
    }

    pub(crate) fn push_reasoning_delta(&mut self, piece: &str) {
        if piece.is_empty() {
            return;
        }
        if self.reasoning_block.is_none() {
            let block = self.mint_block("reasoning");
            self.block_events
                .push(LlmStreamEvent::Block(StreamBlockEvent::Started {
                    kind: StreamBlockKind::Reasoning,
                    block: block.clone(),
                }));
            self.reasoning_block = Some((block, String::new()));
        }
        let Some((block, block_text)) = self.reasoning_block.as_mut() else {
            return;
        };
        self.reasoning_text.push_str(piece);
        block_text.push_str(piece);
        self.block_events
            .push(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::Reasoning,
                block: block.clone(),
                text: piece.to_string(),
            }));
    }

    /// Seals any blocks still open when the stream ends, carrying the
    /// authoritative text accumulated under each.
    pub(crate) fn finish_blocks(&mut self) -> Vec<LlmStreamEvent> {
        self.close_reasoning_block();
        if let Some(block) = self.text_block.take() {
            self.block_events
                .push(LlmStreamEvent::Block(StreamBlockEvent::Completed {
                    kind: StreamBlockKind::AssistantText,
                    block,
                    text: self.full_text.clone(),
                }));
        }
        self.take_block_events()
    }

    pub(crate) fn take_block_events(&mut self) -> Vec<LlmStreamEvent> {
        std::mem::take(&mut self.block_events)
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
            let tool_name = tool_call.tool_name.clone();
            let input_json = if tool_call.input_json.is_empty() {
                "{}".to_string()
            } else {
                tool_call.input_json.clone()
            };
            parts.push(LlmOutputPart::ToolCall {
                call_id: tool_call.call_id.clone(),
                input_json: self.tool_argument_decoder.decode(&tool_name, input_json),
                tool_name,
                replay: tool_call
                    .signature
                    .clone()
                    .map(|opaque| ProviderReplayMeta {
                        item_id: None,
                        opaque: Some(opaque),
                        ..ProviderReplayMeta::default()
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
            let tool_name = tool_call.tool_name.clone();
            let input_json = if tool_call.input_json.is_empty() {
                "{}".to_string()
            } else {
                tool_call.input_json.clone()
            };
            parts.push(LlmOutputPart::ToolCall {
                call_id: tool_call.call_id.clone(),
                input_json: self.tool_argument_decoder.decode(&tool_name, input_json),
                tool_name,
                replay: tool_call
                    .signature
                    .clone()
                    .map(|opaque| ProviderReplayMeta {
                        item_id: None,
                        opaque: Some(opaque),
                        ..ProviderReplayMeta::default()
                    }),
            });
        }
        if parts.is_empty()
            && let Some(final_response) = self
                .final_response_raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        {
            parts = OpenAiCompatibleProvider::chat_response_parts_from_value_with_decoder(
                &final_response,
                &self.tool_argument_decoder,
            );
        }
        parts
    }
}
