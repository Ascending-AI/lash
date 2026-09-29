//! Request-body construction: translating an [`LlmRequest`] into the Anthropic
//! Messages wire shape (messages, tools, cache control, thinking config,
//! structured output).

use crate::support::*;
use lash_core::llm::types::LlmMessage;
use lash_sansio::core_support::Blake3DomainHasher;
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BreakpointAddress {
    pub(crate) message_index: usize,
    pub(crate) block_index: usize,
}

type BuiltMessages = (Option<String>, Vec<Value>, Option<BreakpointAddress>);

impl AnthropicProvider {
    fn role_name(role: &LlmRole) -> &'static str {
        match role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "user",
        }
    }

    fn attachment_block_value(req: &LlmRequest, source: &AttachmentSource) -> Option<Value> {
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
    fn content_block_value(
        req: &LlmRequest,
        block: &LlmContentBlock,
        tool_ids: &HashMap<String, String>,
    ) -> Result<Option<Value>, LlmTransportError> {
        match block {
            LlmContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    return Ok(None);
                }
                Ok(Some(Self::text_block_value(text)))
            }
            LlmContentBlock::Attachment { source } => Ok(Self::attachment_block_value(req, source)),
            LlmContentBlock::ToolCall {
                call_id,
                tool_name,
                input_json,
                ..
            } => Ok(Some(json!({
                "type": "tool_use",
                "id": mapped_tool_call_id(call_id, tool_ids)?,
                "name": tool_name,
                "input": tool_call_input_replay_value(input_json),
            }))),
            LlmContentBlock::ToolResult {
                call_id, content, ..
            } => {
                let mut result = json!({
                    "type": "tool_result",
                    "tool_use_id": mapped_tool_call_id(call_id, tool_ids)?,
                });
                // One result per call: a lone text block is the plain string
                // form; anything else is the ordered text/image/document array.
                match content.as_slice() {
                    [] => {}
                    [ModelToolReturnPart::Text { text }] => result["content"] = json!(text),
                    [ModelToolReturnPart::Retained(retained)] => {
                        result["content"] = json!(retained.witness)
                    }
                    blocks => {
                        result["content"] = Value::Array(
                            blocks
                                .iter()
                                .filter_map(|block| match block {
                                    ModelToolReturnPart::Text { text }
                                        if text.trim().is_empty() =>
                                    {
                                        None
                                    }
                                    ModelToolReturnPart::Text { text } => {
                                        Some(Self::text_block_value(text))
                                    }
                                    // Retained output is sent as its witness;
                                    // its reference is never materialized.
                                    ModelToolReturnPart::Retained(retained) => {
                                        Some(Self::text_block_value(&retained.witness))
                                    }
                                    ModelToolReturnPart::Attachment(source) => {
                                        Self::attachment_block_value(req, source)
                                    }
                                })
                                .collect(),
                        );
                    }
                }
                Ok(Some(result))
            }
            LlmContentBlock::Reasoning { text, replay, .. } => {
                // Anthropic requires a signature to replay a thinking
                // block. If we don't have one (e.g. aborted stream, or
                // reasoning captured from a non-Anthropic provider that
                // stored its payload in `encrypted_content` only), fall
                // back to plain text so the turn still validates.
                let Some(sig) = replay.as_ref().and_then(|meta| meta.signature.as_deref()) else {
                    if text.trim().is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(Self::text_block_value(text)));
                };
                if replay.as_ref().is_some_and(|meta| meta.redacted) {
                    return Ok(Some(json!({
                        "type": "redacted_thinking",
                        "data": sig,
                    })));
                }
                if text.trim().is_empty() {
                    return Ok(None);
                }
                Ok(Some(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": sig,
                })))
            }
        }
    }

    fn message_has_content(msg: &LlmMessage) -> bool {
        msg.blocks.iter().any(|block| match block {
            LlmContentBlock::Text { text, .. } => !text.trim().is_empty(),
            LlmContentBlock::Attachment { source } => source.media_type().is_some(),
            LlmContentBlock::ToolCall { .. } | LlmContentBlock::ToolResult { .. } => true,
            LlmContentBlock::Reasoning { text, replay, .. } => {
                !text.trim().is_empty()
                    || replay
                        .as_ref()
                        .is_some_and(|meta| meta.redacted && meta.signature.is_some())
            }
        })
    }

    fn native_feedback_content(msg: &LlmMessage) -> bool {
        matches!(msg.role, LlmRole::System)
            && msg
                .blocks
                .iter()
                .all(|block| matches!(block, LlmContentBlock::Text { text, .. } if !text.trim().is_empty()))
            && !msg.blocks.is_empty()
    }

    // Judge the emitted neighbors: a tagged fallback is a user block, not
    // part of a native section. Lash has no server-tool-result variant.
    fn native_feedback_position(req: &LlmRequest, index: usize, out: &[Value]) -> bool {
        let before = out.last().and_then(|message| message["role"].as_str());
        let after = req.messages[index + 1..].iter().find(|msg| {
            !Self::native_feedback_content(msg)
                && (matches!(msg.role, LlmRole::System) || Self::message_has_content(msg))
        });
        matches!(before, Some("user" | "system"))
            && after.is_none_or(|msg| matches!(msg.role, LlmRole::Assistant))
    }

    /// Build the `messages` array for Anthropic Messages API. Each lash
    /// `LlmMessage` becomes one wire message; adjacent same-role messages
    /// get merged to match Anthropic's alternation rules.
    pub(crate) fn build_messages(
        &self,
        req: &LlmRequest,
    ) -> Result<BuiltMessages, LlmTransportError> {
        let system_prompt = req.instructions.as_deref().map(str::to_owned);
        let tool_ids = provider_call_id_map(req)?;
        let mut out: Vec<Value> = Vec::new();
        let mut breakpoint = None;
        for (index, msg) in req.messages.iter().enumerate() {
            let feedback = matches!(msg.role, LlmRole::System);
            let native = Self::native_feedback_content(msg)
                && req.model_capability.native_mid_conversation_system
                && Self::native_feedback_position(req, index, &out);
            let wire_role = if native {
                "system"
            } else {
                Self::role_name(&msg.role)
            };
            let mut blocks: Vec<Value> = Vec::new();
            let mut marked_block_index = None;
            let tagged;
            let source_blocks = if feedback && !native {
                let mut fallback = vec![LlmContentBlock::Text {
                    text: format!(
                        "<runtime_feedback>{}</runtime_feedback>",
                        collect_text(&msg.blocks)
                    )
                    .into(),
                    response_meta: None,
                    cache_breakpoint: msg.blocks.iter().any(|block| {
                        matches!(
                            block,
                            LlmContentBlock::Text {
                                cache_breakpoint: true,
                                ..
                            }
                        )
                    }),
                }];
                fallback.extend(
                    msg.blocks
                        .iter()
                        .filter(|block| !matches!(block, LlmContentBlock::Text { .. }))
                        .cloned(),
                );
                tagged = fallback;
                tagged.as_slice()
            } else {
                msg.blocks.as_slice()
            };
            for block in source_blocks {
                if let Some(value) = Self::content_block_value(req, block, &tool_ids)? {
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

        // A coalesced user turn may start with feedback injected between a
        // tool call and its results. Anthropic requires every result first.
        for (message_index, message) in out.iter_mut().enumerate() {
            if message["role"] != "user" {
                continue;
            }
            #[expect(
                clippy::expect_used,
                reason = "every message this builder emits carries a `content` array"
            )]
            let blocks = message["content"].as_array_mut().expect("content blocks");
            let is_result = |block: &Value| block["type"] == "tool_result";
            if let Some(address) = breakpoint.as_mut()
                && address.message_index == message_index
            {
                let old = address.block_index;
                address.block_index = if is_result(&blocks[old]) {
                    blocks[..old].iter().filter(|b| is_result(b)).count()
                } else {
                    blocks.iter().filter(|b| is_result(b)).count()
                        + blocks[..old].iter().filter(|b| !is_result(b)).count()
                };
            }
            blocks.sort_by_key(|block| !is_result(block));
        }
        Ok((system_prompt, out, breakpoint))
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
    ) -> bool {
        let Some(ctrl) = Self::cache_control_value(cache_retention) else {
            return false;
        };
        let mut cache_control_emitted = false;

        if let Some(sys) = system
            && let Some(arr) = sys.as_array_mut()
            && let Some(last) = arr.last_mut()
            && last.is_object()
        {
            last["cache_control"] = ctrl.clone();
            cache_control_emitted = true;
        }

        #[expect(
            clippy::expect_used,
            reason = "the address was recorded from `messages` earlier in this call and nothing removes blocks in between"
        )]
        if let Some(address) = breakpoint {
            let block = messages
                .get_mut(address.message_index)
                .and_then(|message| message.get_mut("content"))
                .and_then(Value::as_array_mut)
                .and_then(|content| content.get_mut(address.block_index))
                .expect("breakpoint address points to a surviving content block");
            block["cache_control"] = ctrl.clone();
            cache_control_emitted = true;
        }

        if breakpoint.is_none()
            && let Some(last_msg) = messages.last_mut()
            && matches!(
                last_msg.get("role").and_then(|v| v.as_str()),
                Some("user" | "system")
            )
            && let Some(content) = last_msg.get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(last_block) = content.last_mut()
            && last_block.is_object()
        {
            last_block["cache_control"] = ctrl.clone();
            cache_control_emitted = true;
        }

        if let Some(last_tool) = tools.last_mut()
            && last_tool.is_object()
        {
            last_tool["cache_control"] = ctrl;
            cache_control_emitted = true;
        }
        cache_control_emitted
    }

    /// What Anthropic Messages can carry for this request. It requires a
    /// cap, has no seed field, and its active thinking pins sampling and is
    /// the only place a reasoning summary can be requested.
    fn generation_wire(req: &LlmRequest) -> GenerationWire {
        GenerationWire {
            label: "Anthropic Messages",
            output_token_cap: OutputCapWire::Required,
            temperature: true,
            seed: false,
            stop_sequences: true,
            // `disable_parallel_tool_use` lives on `tool_choice`, which is
            // only sent with tools.
            parallel_tool_calls: !req.tools.is_empty(),
            thinking_summary: ThinkingSummaryWire::WithActiveThinking,
            active_thinking_pins_sampling: true,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_request_body(&self, req: &LlmRequest) -> Result<Value, LlmTransportError> {
        self.build_request(req).map(|(body, _)| body)
    }

    /// The request body and the receipt of the host settings it carries.
    /// Every refusal happens here, before the caller does any I/O.
    pub(crate) fn build_request(
        &self,
        req: &LlmRequest,
    ) -> Result<(Value, GenerationReceipt), LlmTransportError> {
        let serving_route = self.route_identity(&req.model);
        let safe_request = req
            .reasoning_retention_safe_for(
                &serving_route,
                "Anthropic Messages",
                ProviderReasoningRetentionSupport::AnthropicClearThinking,
            )
            .map_err(|error: ReasoningRetentionValidationError| {
                LlmTransportError::new(error.message)
                    .with_kind(ProviderFailureKind::Unsupported)
                    .with_lash_code(TurnFailureCode::UnsupportedReasoningRetention)
                    .with_retry_verdict(TransportRetryVerdict::Forbidden)
            })?;
        let req = safe_request.as_ref();
        for (message_index, message) in req.messages.iter().enumerate() {
            for source in message
                .blocks
                .iter()
                .flat_map(LlmContentBlock::attachment_sources)
            {
                let validation = (|| {
                    if matches!(
                        source,
                        AttachmentSource::ProviderFile {
                            media_type: None,
                            ..
                        }
                    ) {
                        return Err(LlmTransportError::new(
                    "Anthropic Messages requires the media type for provider file ids in order to choose the image/document modality; supply `media_type` on `ProviderFile`",
                )
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::ProviderFileMediaTypeRequired));
                    }
                    let supported = req
                        .model_capability
                        .attachment_acceptance
                        .accepts("Anthropic Messages", source);
                    if !supported {
                        let accepted_by = known_attachment_acceptors(
                            &req.model_capability.attachment_acceptance,
                            source,
                        );
                        return Err(unsupported_attachment_capability(
                            "Anthropic Messages",
                            source,
                            &accepted_by,
                        ));
                    }
                    if let AttachmentSource::Stored { attachment_ref } = source
                        && req.attachment_bytes(source).is_none()
                    {
                        let mime = &attachment_ref.media_type;
                        return Err(LlmTransportError::new(format!(
                    "Anthropic Messages could not materialize stored attachment MIME `{mime}` because session-guard resolution did not provide its bytes"
                ))
                .with_kind(ProviderFailureKind::Validation)
                .with_lash_code(TurnFailureCode::StoredAttachmentNotResolved));
                    }

                    Ok(())
                })();
                validation.map_err(|mut error: LlmTransportError| {
                    error.message = format!("message index {message_index}: {}", error.message);
                    error
                })?;
            }
        }
        let policy = resolve_generation_policy(
            req,
            &self.options,
            self.kind(),
            &Self::generation_wire(req),
        )?;
        // Resolution refuses a call with no effective cap on this wire.
        let max_tokens = policy.max_output_tokens.ok_or_else(|| {
            LlmTransportError::new("Anthropic Messages requires an output-token cap.")
                .with_lash_code(TurnFailureCode::OutputTokenCapRequired)
                .with_retry_verdict(TransportRetryVerdict::Forbidden)
        })?;
        let mut emission = GenerationEmission {
            output_token_cap: true,
            ..GenerationEmission::default()
        };
        let mut thinking_body = json!({});
        if let Some(intent) = &policy.reasoning {
            apply_thinking(
                intent,
                policy.request_thinking_summary,
                max_tokens,
                &mut thinking_body,
            )?;
            emission.reasoning = true;
            // `display` exists only inside active thinking, which is exactly
            // when resolution asks for the summary.
            emission.thinking_summary = policy.request_thinking_summary;
        }
        let (system_text, mut messages, breakpoint) = self.build_messages(req)?;
        let mut tools = self.build_tools(req)?;

        let mut system_value: Option<Value> = system_text.map(|text| {
            json!([{
                "type": "text",
                "text": text,
            }])
        });

        // Cache control: mark system, last user message, and last tool as
        // ephemeral to benefit from prompt caching. Applied before the body
        // is assembled so we only serialize the final state once.
        emission.cache = self.apply_cache_control(
            policy.cache_retention,
            &mut system_value,
            &mut messages,
            &mut tools,
            breakpoint,
        );

        let mut body = json!({
            "model": req.model,
            "max_tokens": max_tokens,
            "messages": messages,
        });

        if let ReasoningRetentionSelection::AnthropicClearThinking { keep } =
            req.model_capability.reasoning_retention.selection
        {
            let keep = match keep {
                AnthropicThinkingRetention::All => json!("all"),
                AnthropicThinkingRetention::Turns(turns) => {
                    json!({ "type": "thinking_turns", "value": turns.get() })
                }
            };
            body["context_management"] = json!({
                "edits": [{
                    "type": "clear_thinking_20251015",
                    "keep": keep,
                }],
            });
            emission.reasoning_retention = true;
        }

        if let Some(system_value) = system_value {
            body["system"] = system_value;
        }
        if !policy.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(policy.stop_sequences);
            emission.stop_sequences = true;
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = match req.tool_choice {
                LlmToolChoice::Auto => json!({ "type": "auto" }),
                LlmToolChoice::None => json!({ "type": "none" }),
                LlmToolChoice::Required => json!({ "type": "any" }),
            };
            if let Some(parallel_tool_calls) = policy.parallel_tool_calls {
                body["tool_choice"]["disable_parallel_tool_use"] = json!(!parallel_tool_calls);
                emission.parallel_tool_calls = true;
            }
        }

        // Resolution refused a temperature that the model's capability or
        // active thinking pins, so one that survives is sent.
        if let Some(temperature) = &policy.temperature {
            body["temperature"] = Value::Number(temperature.clone().into());
            emission.temperature = true;
        }
        if let Value::Object(fields) = thinking_body {
            for (key, value) in fields {
                body[key] = value;
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
        let passthrough = merge_extra_body(
            &mut body,
            &req.extra_body,
            &reserved_generation_paths(req, "/stop_sequences", "/temperature"),
        )?;
        let mut receipt = policy.receipt(req, &emission);
        receipt.passthrough = if !self.extra_headers.is_empty() {
            lash_core::GenerationOptionOutcome::Applied
        } else {
            passthrough
        };
        Ok((body, receipt))
    }
}

/// Concatenate feedback text without changing any text bytes. Non-text
/// blocks remain separate content blocks in the fallback user message.
fn collect_text(blocks: &[LlmContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let LlmContentBlock::Text { text, .. } = block {
            out.push_str(text);
        }
    }
    out
}

/// Look up the request-wide wire ID shared by a tool call and its result.
fn mapped_tool_call_id<'a>(
    id: &str,
    tool_ids: &'a HashMap<String, String>,
) -> Result<&'a str, LlmTransportError> {
    tool_ids.get(id).map(String::as_str).ok_or_else(|| {
        LlmTransportError::new("Anthropic tool identity must not be empty")
            .with_kind(ProviderFailureKind::Validation)
            .with_retry_verdict(TransportRetryVerdict::NotRetryable)
    })
}

fn provider_call_id_map(req: &LlmRequest) -> Result<HashMap<String, String>, LlmTransportError> {
    let ids: BTreeSet<&str> = req
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::ToolCall { call_id, .. }
            | LlmContentBlock::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    if ids.contains("") {
        return Err(
            LlmTransportError::new("Anthropic tool identity must not be empty")
                .with_kind(ProviderFailureKind::Validation)
                .with_retry_verdict(TransportRetryVerdict::NotRetryable),
        );
    }

    let mut mapped = HashMap::with_capacity(ids.len());
    let mut used = HashSet::with_capacity(ids.len());
    for id in ids.iter().copied().filter(|id| legal_tool_call_id(id)) {
        used.insert(id.to_string());
        mapped.insert(id.to_string(), id.to_string());
    }
    for id in ids.into_iter().filter(|id| !legal_tool_call_id(id)) {
        let prefix: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(47)
            .collect();
        for attempt in 0u64.. {
            let mut hash = Blake3DomainHasher::new("lash-anthropic-tool-call-wire/v1");
            hash.update(id.as_bytes());
            hash.update(attempt.to_le_bytes());
            let digest = hash.finalize_hex();
            let candidate = format!("{prefix}_{}", &digest[..16]);
            if used.insert(candidate.clone()) {
                mapped.insert(id.to_string(), candidate);
                break;
            }
        }
    }
    Ok(mapped)
}

fn legal_tool_call_id(id: &str) -> bool {
    id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
