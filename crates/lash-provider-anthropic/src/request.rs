//! Request-body construction: translating an [`LlmRequest`] into the Anthropic
//! Messages wire shape (messages, tools, cache control, thinking config,
//! structured output).

/// version_surface = "coexist"
/// version_guard(items(LASH_ANTHROPIC_TOOL_CALL_WIRE_DOMAIN_VERSION, provider_call_id_map))
const LASH_ANTHROPIC_TOOL_CALL_WIRE_DOMAIN_VERSION: &str = "lash-anthropic-tool-call-wire/v1";

use crate::support::*;
use lash_core::llm::types::LlmMessage;
use lash_sansio::core_support::Blake3DomainHasher;
use std::collections::{BTreeSet, HashMap, HashSet};

/// The wire messages of a request, decided before any is built: each
/// one's source messages, merged in order, and where the request's cache
/// control lands.
pub(crate) struct MessagePlan {
    /// Every unit's members, in order: a source message's index and
    /// whether it is native feedback.
    members: Vec<(usize, bool)>,
    units: Vec<WireUnit>,
    /// Whether some message carries the cache control.
    cache_emitted: bool,
}

/// One wire message of the `messages` array.
struct WireUnit {
    members: std::ops::Range<usize>,
    role: &'static str,
    /// How many content blocks its members emit.
    blocks: usize,
    cache_at: Option<CacheTarget>,
}

/// Where a wire message carries the request's cache control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheTarget {
    /// The block a host marked, by its index before results move first.
    Marked(usize),
    /// The message's last block: the fallback when nothing is marked.
    Last,
}

/// The request-wide wire ids of tool calls and results.
pub(crate) enum ToolIds {
    /// Every id is already legal on the wire and is sent as it is.
    Identity,
    Mapped(HashMap<String, String>),
}

impl ToolIds {
    fn get<'a>(&'a self, id: &'a str) -> Option<&'a str> {
        match self {
            Self::Identity => Some(id),
            Self::Mapped(mapped) => mapped.get(id).map(String::as_str),
        }
    }
}

impl AnthropicProvider {
    fn role_name(role: &LlmRole) -> &'static str {
        match role {
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::System => "user",
        }
    }

    fn attachment_block_value(
        reference: &AttachmentRef,
        position: AttachmentPosition,
    ) -> TemplateJson {
        let block_type = if reference.media_type.is_image() {
            "image"
        } else {
            "document"
        };
        TemplateJson::object([
            ("type", json!(block_type).into()),
            ("source", TemplateJson::attachment(reference, position)),
        ])
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
        block: &LlmContentBlock,
        tool_ids: &ToolIds,
    ) -> Result<Option<TemplateJson>, LlmTransportError> {
        Ok(match block {
            LlmContentBlock::Attachment { reference } => Some(Self::attachment_block_value(
                reference,
                AttachmentPosition::Message,
            )),
            LlmContentBlock::ToolResult {
                call_id, content, ..
            } => {
                let mut result = TemplateJson::object([
                    ("type", json!("tool_result").into()),
                    (
                        "tool_use_id",
                        json!(mapped_tool_call_id(call_id, tool_ids)?).into(),
                    ),
                ]);
                // One result per call: a lone text block is the plain string
                // form; anything else is the ordered text/image/document
                // array, each attachment a slot where its block sits.
                match content.as_slice() {
                    [] => {}
                    [ModelToolReturnPart::Text { text }] => {
                        result.set("content", json!(text));
                    }
                    [ModelToolReturnPart::Retained(retained)] => {
                        result.set("content", json!(retained.witness));
                    }
                    blocks => {
                        let blocks = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ModelToolReturnPart::Text { text } if text.trim().is_empty() => {
                                    None
                                }
                                ModelToolReturnPart::Text { text } => {
                                    Some(Self::text_block_value(text).into())
                                }
                                // Retained output is sent as its witness;
                                // its reference is never materialized.
                                ModelToolReturnPart::Retained(retained) => {
                                    Some(Self::text_block_value(&retained.witness).into())
                                }
                                ModelToolReturnPart::Attachment(source) => {
                                    Some(Self::attachment_block_value(
                                        source,
                                        AttachmentPosition::ToolResult,
                                    ))
                                }
                            })
                            .collect::<Vec<TemplateJson>>();
                        result.set("content", blocks);
                    }
                }
                Some(result)
            }
            other => Self::plain_block_value(other, tool_ids)?.map(TemplateJson::from),
        })
    }

    /// The wire shape of a block that can hold no attachment.
    fn plain_block_value(
        block: &LlmContentBlock,
        tool_ids: &ToolIds,
    ) -> Result<Option<Value>, LlmTransportError> {
        match block {
            LlmContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    return Ok(None);
                }
                Ok(Some(Self::text_block_value(text)))
            }
            // Placed by `content_block_value`: they can hold attachments.
            LlmContentBlock::Attachment { .. } | LlmContentBlock::ToolResult { .. } => Ok(None),
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

    /// Whether `block` has a wire form (see [`Self::content_block_value`]).
    fn block_emits(block: &LlmContentBlock) -> bool {
        match block {
            LlmContentBlock::Text { text, .. } => !text.trim().is_empty(),
            LlmContentBlock::Attachment { .. }
            | LlmContentBlock::ToolCall { .. }
            | LlmContentBlock::ToolResult { .. } => true,
            LlmContentBlock::Reasoning { text, replay, .. } => {
                !text.trim().is_empty()
                    || replay
                        .as_ref()
                        .is_some_and(|meta| meta.redacted && meta.signature.is_some())
            }
        }
    }

    fn message_has_content(msg: &LlmMessage) -> bool {
        msg.blocks.iter().any(Self::block_emits)
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
    fn native_feedback_position(req: &LlmRequest, index: usize, before: Option<&str>) -> bool {
        let after = req.messages[index + 1..].iter().find(|msg| {
            !Self::native_feedback_content(msg)
                && (matches!(msg.role, LlmRole::System) || Self::message_has_content(msg))
        });
        matches!(before, Some("user" | "system"))
            && after.is_none_or(|msg| matches!(msg.role, LlmRole::Assistant))
    }

    /// Whether each block `msg` emits is marked for the cache, in order.
    /// Feedback sent as user content is one tagged text block, marked when
    /// any of its text was, then its other blocks.
    fn emitted_marks(msg: &LlmMessage, native: bool) -> impl Iterator<Item = bool> + '_ {
        let marked = |block: &LlmContentBlock| {
            matches!(
                block,
                LlmContentBlock::Text {
                    cache_breakpoint: true,
                    ..
                }
            )
        };
        let tagged = matches!(msg.role, LlmRole::System) && !native;
        tagged
            .then(|| msg.blocks.iter().any(marked))
            .into_iter()
            .chain(
                msg.blocks
                    .iter()
                    .filter(move |block| !tagged || !matches!(block, LlmContentBlock::Text { .. }))
                    .filter(|block| Self::block_emits(block))
                    .map(marked),
            )
    }

    /// Plan the `messages` array. Each lash `LlmMessage` becomes one wire
    /// message; adjacent same-role messages merge to match Anthropic's
    /// alternation rules (which keeps replay valid when a reasoning-only
    /// message precedes a text message from the same role). The cache
    /// control goes to the last marked block, else to a last user or
    /// system message's last block.
    fn plan_messages(req: &LlmRequest, cache_control: bool) -> MessagePlan {
        let native_capable = req
            .model
            .metadata()
            .capability
            .native_mid_conversation_system;
        let mut members = Vec::with_capacity(req.messages.len());
        let mut units: Vec<WireUnit> = Vec::new();
        let mut breakpoint = None;
        for (index, msg) in req.messages.iter().enumerate() {
            let native = native_capable
                && Self::native_feedback_content(msg)
                && Self::native_feedback_position(req, index, units.last().map(|unit| unit.role));
            let role = if native {
                "system"
            } else {
                Self::role_name(&msg.role)
            };
            let mut count = 0;
            let mut marked = None;
            for is_marked in Self::emitted_marks(msg, native) {
                if is_marked {
                    marked = Some(count);
                }
                count += 1;
            }
            if count == 0 {
                continue;
            }
            if units.last().is_none_or(|unit| unit.role != role) {
                units.push(WireUnit {
                    members: members.len()..members.len(),
                    role,
                    blocks: 0,
                    cache_at: None,
                });
            }
            let unit_index = units.len() - 1;
            let unit = &mut units[unit_index];
            if let Some(marked) = marked {
                breakpoint = Some((unit_index, unit.blocks + marked));
            }
            members.push((index, native));
            unit.members.end = members.len();
            unit.blocks += count;
        }
        let mut cache_emitted = false;
        if cache_control {
            if let Some((unit, block)) = breakpoint {
                units[unit].cache_at = Some(CacheTarget::Marked(block));
                cache_emitted = true;
            } else if let Some(last) = units.last_mut()
                && matches!(last.role, "user" | "system")
            {
                last.cache_at = Some(CacheTarget::Last);
                cache_emitted = true;
            }
        }
        MessagePlan {
            members,
            units,
            cache_emitted,
        }
    }

    /// The wire message of `unit`: its members' blocks, merged, a user
    /// message's tool results first, and `ctrl` where the plan put it.
    fn unit_message(
        req: &LlmRequest,
        members: &[(usize, bool)],
        unit: &WireUnit,
        tool_ids: &ToolIds,
        ctrl: Option<&Value>,
    ) -> Result<TemplateJson, LlmTransportError> {
        let mut blocks: Vec<TemplateJson> = Vec::with_capacity(unit.blocks);
        for &(index, native) in members {
            let msg = &req.messages[index];
            let tagged;
            let source_blocks = if matches!(msg.role, LlmRole::System) && !native {
                let mut fallback = vec![LlmContentBlock::Text {
                    text: format!(
                        "<runtime_feedback>{}</runtime_feedback>",
                        collect_text(&msg.blocks)
                    )
                    .into(),
                    response_meta: None,
                    cache_breakpoint: false,
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
                if let Some(value) = Self::content_block_value(block, tool_ids)? {
                    blocks.push(value);
                }
            }
        }
        let mut target = unit.cache_at;
        // A coalesced user turn may start with feedback injected between a
        // tool call and its results. Anthropic requires every result first.
        if unit.role == "user" {
            let is_result = |block: &TemplateJson| block.str_field("type") == Some("tool_result");
            if let Some(CacheTarget::Marked(old)) = target {
                target = Some(CacheTarget::Marked(if is_result(&blocks[old]) {
                    blocks[..old].iter().filter(|b| is_result(b)).count()
                } else {
                    blocks.iter().filter(|b| is_result(b)).count()
                        + blocks[..old].iter().filter(|b| !is_result(b)).count()
                }));
            }
            blocks.sort_by_key(|block| !is_result(block));
        }
        if let (Some(target), Some(ctrl)) = (target, ctrl) {
            #[expect(
                clippy::expect_used,
                reason = "the plan counted the blocks this message emits"
            )]
            let block = match target {
                CacheTarget::Marked(index) => blocks.get_mut(index),
                CacheTarget::Last => blocks.last_mut(),
            }
            .expect("the planned cache target is a surviving content block");
            block.set("cache_control", ctrl.clone());
        }
        Ok(TemplateJson::object([
            ("role", json!(unit.role).into()),
            ("content", blocks.into()),
        ]))
    }

    /// Build the `messages` array, each wire message written by `units`:
    /// reused whole when its members, their native placement, its cache
    /// control and the wire ids it names are those of a message lowered
    /// before. Returns whether a message carries `ctrl`.
    pub(crate) fn build_messages(
        req: &LlmRequest,
        ctrl: Option<&Value>,
        units: &mut UnitWriter<'_>,
    ) -> Result<(Vec<TemplateJson>, bool), LlmTransportError> {
        let tool_ids = provider_call_id_map(req)?;
        let plan = Self::plan_messages(req, ctrl.is_some());
        let mut messages = Vec::with_capacity(plan.units.len());
        for unit in &plan.units {
            let members = &plan.members[unit.members.clone()];
            messages.extend(units.unit(
                |mut key| {
                    for &(index, native) in members {
                        let msg = &req.messages[index];
                        key = key.message(msg).u64(u64::from(native));
                        if let ToolIds::Mapped(_) = tool_ids {
                            for block in msg.blocks.iter() {
                                if let LlmContentBlock::ToolCall { call_id, .. }
                                | LlmContentBlock::ToolResult { call_id, .. } = block
                                {
                                    key = key.str(tool_ids.get(call_id).unwrap_or_default());
                                }
                            }
                        }
                    }
                    key = key.str(unit.role);
                    match (unit.cache_at, ctrl) {
                        (Some(CacheTarget::Marked(index)), Some(ctrl)) => {
                            key.u64(1).u64(index as u64).str(&ctrl.to_string())
                        }
                        (Some(CacheTarget::Last), Some(ctrl)) => key.u64(2).str(&ctrl.to_string()),
                        _ => key.u64(0),
                    }
                },
                || {
                    Self::unit_message(req, members, unit, &tool_ids, ctrl)
                        .map(|message| vec![message])
                },
            )?);
        }
        Ok((messages, plan.cache_emitted))
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

    /// Mark the system prompt and the last tool with `ctrl`. Returns
    /// whether either carries it.
    fn apply_cache_control(ctrl: &Value, system: &mut Option<Value>, tools: &mut [Value]) -> bool {
        let mut cache_control_emitted = false;
        if let Some(sys) = system
            && let Some(arr) = sys.as_array_mut()
            && let Some(last) = arr.last_mut()
            && last.is_object()
        {
            last["cache_control"] = ctrl.clone();
            cache_control_emitted = true;
        }
        if let Some(last_tool) = tools.last_mut()
            && last_tool.is_object()
        {
            last_tool["cache_control"] = ctrl.clone();
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

    /// [`Self::build_request_tree`] as plain JSON, each attachment shown by
    /// its redacted marker.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn build_request(
        &self,
        req: &LlmRequest,
    ) -> Result<(Value, GenerationReceipt), LlmTransportError> {
        self.build_request_tree(req, &mut UnitWriter::Plain)
            .map(|(body, receipt)| (body.redacted(), receipt))
    }

    /// The request body and the receipt of the host settings it carries,
    /// each wire message written by `units`. Every refusal happens here,
    /// before the caller does any I/O.
    pub(crate) fn build_request_tree(
        &self,
        req: &LlmRequest,
        units: &mut UnitWriter<'_>,
    ) -> Result<(TemplateJson, GenerationReceipt), LlmTransportError> {
        let serving_route = self.route_identity(req.model.wire_model());
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
        let policy = resolve_generation_policy(req, self.kind(), &Self::generation_wire(req))?;
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
        // Cache control: mark system, the marked or last user message, and
        // last tool as ephemeral to benefit from prompt caching.
        let ctrl = Self::cache_control_value(policy.cache_retention);
        let (messages, messages_cached) = Self::build_messages(req, ctrl.as_ref(), units)?;
        let mut tools = self.build_tools(req)?;
        let mut system_value: Option<Value> = req.instructions.as_deref().map(|text| {
            json!([{
                "type": "text",
                "text": text,
            }])
        });
        emission.cache = ctrl.as_ref().is_some_and(|ctrl| {
            Self::apply_cache_control(ctrl, &mut system_value, &mut tools) | messages_cached
        });

        let mut body = json!({
            "model": req.model.wire_model(),
            "max_tokens": max_tokens,
            // Set last: the messages hold the request's attachment slots.
            "messages": [],
        });

        if let ReasoningRetentionSelection::AnthropicClearThinking { keep } = req
            .model
            .metadata()
            .capability
            .reasoning_retention
            .selection
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
            &req.model.metadata().extra_body,
            &reserved_generation_paths(req, "/stop_sequences", "/temperature"),
        )?;
        let mut receipt = policy.receipt(req, &emission);
        receipt.passthrough = if !self.extra_headers.is_empty() {
            lash_core::GenerationOptionOutcome::Applied
        } else {
            passthrough
        };
        let mut body = TemplateJson::from(body);
        body.set("messages", messages);
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
    id: &'a str,
    tool_ids: &'a ToolIds,
) -> Result<&'a str, LlmTransportError> {
    tool_ids.get(id).ok_or_else(|| {
        LlmTransportError::new("Anthropic tool identity must not be empty")
            .with_kind(ProviderFailureKind::Validation)
            .with_retry_verdict(TransportRetryVerdict::NotRetryable)
    })
}

fn provider_call_id_map(req: &LlmRequest) -> Result<ToolIds, LlmTransportError> {
    let call_ids = || {
        req.messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::ToolCall { call_id, .. }
                | LlmContentBlock::ToolResult { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
    };
    // Legal ids are sent as they are; only an illegal one needs the map,
    // which a mapped id's collision avoidance reads whole.
    if call_ids().all(legal_tool_call_id) {
        return Ok(ToolIds::Identity);
    }
    let ids: BTreeSet<&str> = call_ids().collect();
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
            let mut hash = Blake3DomainHasher::new(LASH_ANTHROPIC_TOOL_CALL_WIRE_DOMAIN_VERSION);
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
    Ok(ToolIds::Mapped(mapped))
}

fn legal_tool_call_id(id: &str) -> bool {
    id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
