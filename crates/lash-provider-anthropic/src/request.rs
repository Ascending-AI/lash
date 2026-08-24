//! Request-body construction: translating an [`LlmRequest`] into the Anthropic
//! Messages wire shape (messages, tools, cache control, thinking config,
//! structured output).

use crate::policy::AnthropicThinkingConfig;
use crate::support::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BreakpointAddress {
    pub(crate) message_index: usize,
    pub(crate) block_index: usize,
}

impl AnthropicProvider {
    fn role_name(role: &LlmRole) -> &'static str {
        match role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "user",
        }
    }

    fn attachment_block_value(req: &LlmRequest, attachment_idx: usize) -> Option<Value> {
        let source = req.attachments.get(attachment_idx)?;
        let media_type = source.media_type()?;
        let block_type = if media_type.is_image() {
            "image"
        } else {
            "document"
        };
        let wire_source = match source {
            AttachmentSource::ExternalUrl { url, .. } => json!({"type": "url", "url": url}),
            AttachmentSource::Inline { .. } | AttachmentSource::Stored { .. } => {
                let bytes = req.attachment_bytes(source)?;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                json!({
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                })
            }
            AttachmentSource::ProviderFile { id, .. } => {
                json!({"type": "file", "file_id": id})
            }
        };
        Some(json!({"type": block_type, "source": wire_source}))
    }

    fn text_block_value(text: &str) -> Value {
        json!({
            "type": "text",
            "text": text,
        })
    }

    /// Translate one `LlmContentBlock` into the Anthropic wire shape.
    /// Returns `None` for blocks that have no valid wire form (e.g. an
    /// empty text block — Anthropic 400s on those).
    fn content_block_value(req: &LlmRequest, block: &LlmContentBlock) -> Option<Value> {
        match block {
            LlmContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    return None;
                }
                Some(Self::text_block_value(text))
            }
            LlmContentBlock::Attachment { attachment_idx } => Some(
                Self::attachment_block_value(req, *attachment_idx)
                    .unwrap_or_else(|| Self::text_block_value("[Attachment]")),
            ),
            LlmContentBlock::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => {
                let input: Value = serde_json::from_str(input_json).unwrap_or_else(|_| json!({}));
                Some(json!({
                    "type": "tool_use",
                    "id": normalize_tool_call_id(call_id),
                    "name": tool_name,
                    "input": input,
                }))
            }
            LlmContentBlock::ToolResult {
                call_id, content, ..
            } => Some(json!({
                "type": "tool_result",
                "tool_use_id": normalize_tool_call_id(call_id),
                "content": content.clone(),
            })),
            LlmContentBlock::Reasoning { text, replay, .. } => {
                // Anthropic requires a signature to replay a thinking
                // block. If we don't have one (e.g. aborted stream, or
                // reasoning captured from a non-Anthropic provider that
                // stored its payload in `encrypted_content` only), fall
                // back to plain text so the turn still validates.
                let Some(sig) = replay.as_ref().and_then(|meta| meta.signature.as_deref()) else {
                    if text.trim().is_empty() {
                        return None;
                    }
                    return Some(Self::text_block_value(text));
                };
                if replay.as_ref().is_some_and(|meta| meta.redacted) {
                    return Some(json!({
                        "type": "redacted_thinking",
                        "data": sig,
                    }));
                }
                if text.trim().is_empty() {
                    return None;
                }
                Some(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": sig,
                }))
            }
        }
    }

    /// Build the `messages` array for Anthropic Messages API. Each lash
    /// `LlmMessage` becomes one wire message; adjacent same-role messages
    /// get merged to match Anthropic's alternation rules.
    pub(crate) fn build_messages(
        &self,
        req: &LlmRequest,
    ) -> (Option<String>, Vec<Value>, Option<BreakpointAddress>) {
        let mut system_prompt: Option<String> = None;
        let mut out: Vec<Value> = Vec::new();
        let mut breakpoint = None;
        let mut first_system_seen = false;

        for msg in &req.messages {
            // First system message is the real system prompt; hoist it
            // into the top-level `system` field. Subsequent system
            // messages (runtime feedback) become user turns so the
            // conversation ends on a user boundary.
            if matches!(msg.role, LlmRole::System) && !first_system_seen {
                first_system_seen = true;
                let text = collect_text(&msg.blocks);
                if !text.is_empty() {
                    system_prompt = Some(text);
                }
                continue;
            }

            let wire_role = Self::role_name(&msg.role);
            let mut blocks: Vec<Value> = Vec::new();
            let mut marked_block_index = None;
            for block in msg.blocks.iter() {
                if let Some(value) = Self::content_block_value(req, block) {
                    if matches!(
                        block,
                        LlmContentBlock::Text {
                            cache_breakpoint: true,
                            ..
                        }
                    ) {
                        marked_block_index = Some(blocks.len());
                    }
                    blocks.push(value);
                }
            }
            if blocks.is_empty() {
                continue;
            }

            // Merge with previous turn if same role — keeps replay valid
            // when a reasoning-only message immediately precedes a text
            // message from the same role.
            let message_count = out.len();
            if let Some(prev) = out.last_mut()
                && prev.get("role").and_then(|v| v.as_str()) == Some(wire_role)
                && let Some(prev_content) = prev.get_mut("content").and_then(|c| c.as_array_mut())
            {
                if let Some(block_index) = marked_block_index {
                    breakpoint = Some(BreakpointAddress {
                        message_index: message_count - 1,
                        block_index: prev_content.len() + block_index,
                    });
                }
                prev_content.extend(blocks);
                continue;
            }

            if let Some(block_index) = marked_block_index {
                breakpoint = Some(BreakpointAddress {
                    message_index: message_count,
                    block_index,
                });
            }
            out.push(json!({
                "role": wire_role,
                "content": blocks,
            }));
        }

        (system_prompt, out, breakpoint)
    }

    fn projection_error(err: SchemaResolutionError) -> LlmTransportError {
        LlmTransportError::new(format!(
            "Anthropic schema projection failed: {}",
            err.first_diagnostic()
        ))
        .with_kind(ProviderFailureKind::Validation)
        .with_raw(
            json!({
                "dialect": err.dialect.map(|dialect| dialect.as_str().to_string()),
                "purpose": format!("{:?}", err.purpose),
                "diagnostics": err.diagnostics,
            })
            .to_string(),
        )
    }

    fn build_tools(&self, req: &LlmRequest) -> Result<Vec<Value>, LlmTransportError> {
        let capabilities = ProviderSchemaCapabilities::anthropic();
        req.tools
            .iter()
            .map(|tool| {
                let input_schema = resolve_schema(
                    &tool.input_schema,
                    SchemaResolutionRequest {
                        provider: "Anthropic",
                        purpose: SchemaPurpose::ToolInput,
                        dialects: capabilities.dialects_for(SchemaPurpose::ToolInput),
                    },
                )
                .map_err(Self::projection_error)?
                .schema;
                Ok(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": input_schema,
                }))
            })
            .collect()
    }

    fn cache_control_value(cache_retention: CacheRetention) -> Option<Value> {
        match cache_retention {
            CacheRetention::None => None,
            CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
            CacheRetention::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
        }
    }

    fn apply_cache_control(
        &self,
        cache_retention: CacheRetention,
        system: &mut Option<Value>,
        messages: &mut [Value],
        tools: &mut [Value],
        breakpoint: Option<BreakpointAddress>,
    ) {
        let Some(ctrl) = Self::cache_control_value(cache_retention) else {
            return;
        };

        if let Some(sys) = system
            && let Some(arr) = sys.as_array_mut()
            && let Some(last) = arr.last_mut()
            && last.is_object()
        {
            last["cache_control"] = ctrl.clone();
        }

        if let Some(address) = breakpoint {
            let block = messages
                .get_mut(address.message_index)
                .and_then(|message| message.get_mut("content"))
                .and_then(Value::as_array_mut)
                .and_then(|content| content.get_mut(address.block_index))
                .expect("breakpoint address points to a surviving content block");
            block["cache_control"] = ctrl.clone();
        }

        if breakpoint.is_none()
            && let Some(last_msg) = messages.last_mut()
            && last_msg.get("role").and_then(|v| v.as_str()) == Some("user")
            && let Some(content) = last_msg.get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(last_block) = content.last_mut()
            && last_block.is_object()
        {
            last_block["cache_control"] = ctrl.clone();
        }

        if let Some(last_tool) = tools.last_mut()
            && last_tool.is_object()
        {
            last_tool["cache_control"] = ctrl;
        }
    }

    /// Derive the Anthropic thinking config from the resolved selection and
    /// host-supplied capability.
    fn thinking_config(req: &LlmRequest) -> Option<AnthropicThinkingConfig> {
        let reasoning = req.model_capability.reasoning.as_ref()?;
        match &req.model_variant {
            ReasoningSelection::ProviderDefault => None,
            ReasoningSelection::Effort(variant) => match &reasoning.encoding {
                ReasoningEncoding::Effort => Some(AnthropicThinkingConfig::Adaptive {
                    effort: variant.clone(),
                }),
                ReasoningEncoding::Budget(budgets) => {
                    budgets
                        .get(variant)
                        .map(|&budget_tokens| AnthropicThinkingConfig::Budget {
                            budget_tokens: budget_tokens as i32,
                        })
                }
            },
            ReasoningSelection::Disabled => match reasoning.disable.as_ref()? {
                ReasoningDisableEncoding::Native | ReasoningDisableEncoding::ToggleFalse => {
                    Some(AnthropicThinkingConfig::Disabled)
                }
                ReasoningDisableEncoding::Omit => None,
                ReasoningDisableEncoding::Effort(effort) => {
                    Some(AnthropicThinkingConfig::Adaptive {
                        effort: effort.clone(),
                    })
                }
                ReasoningDisableEncoding::Budget(budget_tokens) => {
                    Some(AnthropicThinkingConfig::Budget {
                        budget_tokens: *budget_tokens as i32,
                    })
                }
            },
        }
    }

    pub(crate) fn build_request_body(&self, req: &LlmRequest) -> Result<Value, LlmTransportError> {
        let serving_route = self.route_identity(&req.model);
        let safe_request = req.replay_safe_for(&serving_route);
        let req = safe_request.as_ref();
        for source in &req.attachments {
            if matches!(
                source,
                AttachmentSource::ProviderFile {
                    provider_scope,
                    media_type: None,
                    ..
                } if provider_scope.provider.eq_ignore_ascii_case("anthropic")
            ) {
                return Err(LlmTransportError::new(
                    "Anthropic Messages requires the media type for provider file ids in order to choose the image/document modality; supply `media_type` on `ProviderFile`",
                )
                .with_kind(ProviderFailureKind::Validation)
                .with_code("provider_file_media_type_required"));
            }
            let supported = match source {
                AttachmentSource::ProviderFile { provider_scope, .. } => {
                    provider_scope.provider.eq_ignore_ascii_case("anthropic")
                }
                source => source.media_type().is_some_and(|mime| {
                    ANTHROPIC_IMAGE_MIMES.contains(&mime.as_str())
                        || ANTHROPIC_FILE_MIMES.contains(&mime.as_str())
                }),
            };
            if !supported {
                let accepted_by = known_attachment_acceptors(source);
                return Err(unsupported_attachment_capability(
                    "Anthropic Messages",
                    source,
                    &accepted_by,
                ));
            }
            if matches!(source, AttachmentSource::Stored { .. })
                && req.attachment_bytes(source).is_none()
            {
                let mime = source.media_type().expect("stored source MIME");
                return Err(LlmTransportError::new(format!(
                    "Anthropic Messages could not materialize stored attachment MIME `{mime}` because session-guard resolution did not provide its bytes"
                ))
                .with_kind(ProviderFailureKind::Validation)
                .with_code("stored_attachment_not_resolved"));
            }
        }
        let (system_text, mut messages, breakpoint) = self.build_messages(req);
        let mut tools = self.build_tools(req)?;

        let thinking_config = Self::thinking_config(req);
        let policy = resolve_generation_policy(
            &req.generation,
            &self.options,
            DEFAULT_MAX_OUTPUT_TOKENS,
            thinking_config,
        );

        let mut system_value: Option<Value> = system_text.map(|text| {
            json!([{
                "type": "text",
                "text": text,
            }])
        });

        // Cache control: mark system, last user message, and last tool as
        // ephemeral to benefit from prompt caching. Applied before the body
        // is assembled so we only serialize the final state once.
        self.apply_cache_control(
            policy.cache_retention,
            &mut system_value,
            &mut messages,
            &mut tools,
            breakpoint,
        );

        let mut body = json!({
            "model": req.model,
            "max_tokens": policy.max_output_tokens,
            "messages": messages,
        });

        if let Some(system_value) = system_value {
            body["system"] = system_value;
        }
        if !policy.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(policy.stop_sequences);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = match req.tool_choice {
                LlmToolChoice::Auto => json!({ "type": "auto" }),
                LlmToolChoice::None => json!({ "type": "none" }),
                LlmToolChoice::Required => json!({ "type": "any" }),
            };
        }

        // Sampling. Anthropic Messages has no seed field, so a requested seed
        // is not expressible on this wire. Temperature is expressible, but two
        // separate facts can take it away, and both answer with HTTP 400 when
        // ignored: extended thinking pins sampling for the request, and some
        // models pin it outright (everything released after Claude Opus 4.6).
        // The second is host-supplied capability, never inferred from the
        // model name here.
        if let Some(temperature) = &policy.temperature
            && !policy
                .thinking
                .as_ref()
                .is_some_and(AnthropicThinkingConfig::pins_sampling)
            && req.model_capability.allows_caller_temperature()
        {
            body["temperature"] = Value::Number(temperature.clone().into());
        }

        // Extended thinking.
        if let Some(cfg) = policy.thinking {
            let display = if policy.expose_thinking {
                "summarized"
            } else {
                "omitted"
            };
            match cfg {
                AnthropicThinkingConfig::Adaptive { effort } => {
                    // The variant is already validated and alias-normalized by
                    // lash-core against the host-supplied capability, so it is
                    // sent verbatim as the wire effort.
                    body["thinking"] = json!({
                        "type": "adaptive",
                        "display": display,
                    });
                    body["output_config"] = json!({ "effort": effort });
                }
                AnthropicThinkingConfig::Budget { budget_tokens } => {
                    body["thinking"] = json!({
                        "type": "enabled",
                        "budget_tokens": budget_tokens,
                        "display": display,
                    });
                }
                AnthropicThinkingConfig::Disabled => {
                    body["thinking"] = json!({ "type": "disabled" });
                }
            }
        }

        if let Some(output_spec) = &req.output_spec {
            let format = match output_spec {
                LlmOutputSpec::JsonObject => json!({
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "additionalProperties": true,
                    },
                }),
                LlmOutputSpec::JsonSchema(schema) => {
                    let capabilities = ProviderSchemaCapabilities::anthropic();
                    let projected = resolve_schema(
                        &schema.schema,
                        SchemaResolutionRequest {
                            provider: "Anthropic",
                            purpose: SchemaPurpose::StructuredOutput,
                            dialects: capabilities.dialects_for(SchemaPurpose::StructuredOutput),
                        },
                    )
                    .map_err(Self::projection_error)?
                    .schema;
                    json!({
                        "type": "json_schema",
                        "schema": projected,
                    })
                }
            };
            if !body.get("output_config").is_some_and(Value::is_object) {
                body["output_config"] = json!({});
            }
            body["output_config"]["format"] = format;
        }

        body["stream"] = json!(true);
        Ok(body)
    }
}

/// Join all `Text` blocks in a message into a single string, separated by
/// blank lines. Non-text blocks are ignored. Used to collapse a multi-block
/// system message into the top-level `system` field.
fn collect_text(blocks: &[LlmContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let LlmContentBlock::Text { text, .. } = block {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
    }
    out
}

/// Normalize tool call IDs to the Anthropic-allowed character set and length.
fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if sanitized.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        sanitized
    }
}
