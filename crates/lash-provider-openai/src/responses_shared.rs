//! Provider-agnostic OpenAI Responses API machinery.
//!
//! The OpenAI Responses streaming protocol (`response.output_item.*`,
//! `response.output_text.delta`, `response.reasoning_summary_*`,
//! `response.function_call_arguments.*`, …) is spoken verbatim by both the
//! direct OpenAI provider and the Codex OAuth provider. This module owns the
//! single implementation of:
//!
//! * [`ResponsesStreamState`] — the incremental stream accumulator, including
//!   message-by-id reconciliation, reasoning-part assembly, tool-call
//!   buffering, and final-response merging.
//! * [`process_sse_event`] / [`parse_sse_payload`] — the SSE event state
//!   machine.
//! * Request-building primitives shared by both providers: schema projection
//!   ([`build_tools`], [`projected_schema`]), tool-choice mapping, role names,
//!   image parts, and final-response parsing ([`response_parts_from_value`]).
//!
//! Each provider keeps only its genuine specifics: the OpenAI provider owns
//! `build_responses_request_body` (surrogate sanitisation, OpenRouter/local
//! field gating, assistant-message id flushing); Codex owns its request body
//! (ordered runtime feedback and tool-result image folding), its
//! endpoint/headers, and its failure classification.

use serde_json::{Value, json};
use std::collections::HashMap;

use crate::schema::{classify_openai_error, responses_error_retry_verdict};
use lash_core::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_core::llm::types::{
    AttachmentSource, ExecutionEvidence, LlmContentBlock, LlmMessage, LlmOutputPart, LlmRequest,
    LlmResponse, LlmRole, LlmStreamEvent, LlmToolChoice, LlmUsage, ProviderReasoningReplay,
    ProviderReplayMeta, ResponsePhase, ResponseTextMeta, StreamBlockIdentity,
};
use lash_core::{
    SchemaContract, TurnFailureCode, facade_support::ProviderSchemaCapabilities,
    facade_support::SchemaPurpose, facade_support::SchemaResolutionError,
    facade_support::SchemaResolutionRequest, facade_support::resolve_schema,
};
use lash_llm_transport::{
    frame_sse_payload, merge_usage,
    openai_terminal_reason_from_response_value as terminal_reason_from_response_value,
    openai_usage_from_response_value as usage_from_response_value, terminal_reason_from_parts,
};

mod input;
pub use input::{ResponsesInputOptions, build_responses_input};
pub(crate) use input::{attachment_feedback, feedback_boundary, push_tool_output};
mod tool_argument_decoder;
pub use tool_argument_decoder::ToolArgumentDecoder;

// ---------------------------------------------------------------------------
// Request-building primitives
// ---------------------------------------------------------------------------

pub fn role_name(role: &LlmRole) -> &'static str {
    match role {
        LlmRole::User => "user",
        LlmRole::Assistant => "assistant",
        LlmRole::System => "system",
    }
}

pub fn validate_responses_attachments(
    req: &LlmRequest,
    provider: &str,
) -> Result<(), LlmTransportError> {
    for (message_index, message) in req.messages.iter().enumerate() {
        for source in message.blocks.iter().filter_map(|block| match block {
            LlmContentBlock::Attachment { source } => Some(source.as_ref()),
            _ => None,
        }) {
            let validation = (|| {
                if !req
                    .model_capability
                    .attachment_acceptance
                    .accepts("OpenAI Responses", source)
                {
                    let accepted = req.model_capability.attachment_acceptance.acceptors(source);
                    return Err(
                        lash_core::llm::transport::unsupported_attachment_capability(
                            provider, source, &accepted,
                        ),
                    );
                }
                if let AttachmentSource::Stored { attachment_ref } = source
                    && req.attachment_bytes(source).is_none()
                {
                    let mime = &attachment_ref.media_type;
                    return Err(LlmTransportError::new(format!("{provider} could not materialize stored attachment MIME `{mime}` because session-guard resolution did not provide its bytes"))
                .with_kind(ProviderFailureKind::Validation).with_adapter_code(TurnFailureCode::StoredAttachmentNotResolved));
                }

                Ok(())
            })();
            validation.map_err(|mut error: LlmTransportError| {
                error.message = format!("message index {message_index}: {}", error.message);
                error
            })?;
        }
    }

    Ok(())
}

/// `validate_responses_attachments` runs over the same request first and
/// refuses every source without a media type or resolved bytes.
#[expect(clippy::expect_used, reason = "the validator refused these")]
pub fn input_attachment_part(req: &LlmRequest, source: &AttachmentSource) -> Value {
    if let AttachmentSource::ProviderFile { id, .. } = source {
        return json!({"type": "input_file", "file_id": id});
    }
    let media_type = source.media_type().expect("validated MIME-bearing source");
    if media_type.is_image() {
        let image_url = match source {
            AttachmentSource::ExternalUrl { url, .. } => url.clone(),
            AttachmentSource::Inline { .. } | AttachmentSource::Stored { .. } => {
                let bytes = req
                    .attachment_bytes(source)
                    .expect("validated attachment bytes");
                crate::request_work::attachment_data_url(media_type.as_str(), bytes)
            }
            AttachmentSource::ProviderFile { .. } => unreachable!(),
        };
        let mut part = json!({"type": "input_image"});
        part["image_url"] = Value::String(image_url);
        return part;
    }
    match source {
        AttachmentSource::ExternalUrl { url, .. } => {
            json!({"type": "input_file", "file_url": url})
        }
        AttachmentSource::Inline { .. } | AttachmentSource::Stored { .. } => {
            let bytes = req
                .attachment_bytes(source)
                .expect("validated attachment bytes");
            let mut part = json!({"type": "input_file"});
            part["file_data"] = Value::String(crate::request_work::attachment_data_url(
                media_type.as_str(),
                bytes,
            ));
            part
        }
        AttachmentSource::ProviderFile { .. } => unreachable!(),
    }
}

pub fn tool_choice_value(choice: &LlmToolChoice) -> &'static str {
    match choice {
        LlmToolChoice::Auto => "auto",
        LlmToolChoice::None => "none",
        LlmToolChoice::Required => "required",
    }
}

pub fn projection_error(provider: &str, err: SchemaResolutionError) -> LlmTransportError {
    LlmTransportError::new(format!(
        "{provider} schema projection failed: {}",
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

pub fn projected_schema(
    provider: &str,
    contract: &SchemaContract,
    capabilities: &ProviderSchemaCapabilities,
    purpose: SchemaPurpose,
) -> Result<Value, LlmTransportError> {
    resolve_schema(
        contract,
        SchemaResolutionRequest {
            provider,
            purpose,
            dialects: capabilities.dialects_for(purpose),
        },
    )
    .map(|projection| projection.schema)
    .map_err(|err| projection_error(provider, err))
}

pub fn build_tools(
    provider: &str,
    req: &lash_core::llm::types::LlmRequest,
) -> Result<Vec<Value>, LlmTransportError> {
    build_tools_with_strict(provider, req, false)
}

pub fn build_tools_with_strict(
    provider: &str,
    req: &lash_core::llm::types::LlmRequest,
    strict_tools: bool,
) -> Result<Vec<Value>, LlmTransportError> {
    let capabilities = ProviderSchemaCapabilities::openai(strict_tools);
    build_tools_with_capabilities(provider, req, strict_tools, &capabilities)
}

pub fn build_tools_with_capabilities(
    provider: &str,
    req: &lash_core::llm::types::LlmRequest,
    strict_tools: bool,
    capabilities: &ProviderSchemaCapabilities,
) -> Result<Vec<Value>, LlmTransportError> {
    req.tools
        .iter()
        .map(|tool| {
            let parameters = projected_schema(
                provider,
                &tool.input_schema,
                capabilities,
                SchemaPurpose::ToolInput,
            )?;
            Ok(json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": parameters,
                "strict": strict_tools,
            }))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Terminal reason + response assembly
// ---------------------------------------------------------------------------

/// Collapse a finished [`ResponsesStreamState`] into an [`LlmResponse`]. Used
/// by Codex; the direct OpenAI driver inlines an equivalent assembly with its
/// own streaming plumbing.
pub fn response_from_stream_state(
    state: ResponsesStreamState,
    request_body: Option<String>,
    http_summary: String,
) -> LlmResponse {
    let parts = state.response_parts();
    let terminal_reason = match &state.final_response {
        Some(final_response) => terminal_reason_from_response_value(final_response, &parts),
        None => terminal_reason_from_parts(&parts),
    };
    LlmResponse {
        parts,
        usage: state.usage,
        terminal_reason,
        terminal_diagnostic: None,
        provider_usage: state.provider_usage,
        request_body,
        http_summary: Some(http_summary),
        execution_evidence: state.execution_evidence,
        generation_disposition: None,
        response_metadata: Default::default(),
        expose_thinking: Some(state.expose_thinking),
    }
}

// ---------------------------------------------------------------------------
// Final-response parsing
// ---------------------------------------------------------------------------

pub fn response_text_meta_from_message_item(item: &Value) -> ResponseTextMeta {
    ResponseTextMeta {
        id: item.get("id").and_then(|v| v.as_str()).map(str::to_string),
        status: item
            .get("status")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| Some("completed".to_string())),
        phase: item
            .get("phase")
            .and_then(|v| v.as_str())
            .and_then(ResponsePhase::from_provider_wire),
        ..ResponseTextMeta::default()
    }
}

pub fn message_text_from_item(item: &Value) -> String {
    item.get("content")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|part| match part.get("type").and_then(|v| v.as_str()) {
            Some("output_text") => part.get("text").and_then(|v| v.as_str()),
            Some("refusal") => part
                .get("refusal")
                .and_then(|v| v.as_str())
                .or_else(|| part.get("text").and_then(|v| v.as_str())),
            _ => None,
        })
        .collect::<String>()
}

pub fn extract_text(value: &Value) -> String {
    if let Some(output) = value.get("output").and_then(|v| v.as_array())
        && output.iter().any(|item| {
            item.get("type").and_then(|v| v.as_str()) == Some("message")
                && item
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .and_then(ResponsePhase::from_provider_wire)
                    .is_some_and(|phase| phase == ResponsePhase::FinalAnswer)
                && !message_text_from_item(item).is_empty()
        })
    {
        return lash_core::facade_support::visible_response_text_from_parts(
            &response_parts_from_value(value),
        );
    }
    if let Some(s) = value.get("output_text").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    value
        .get("output")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(message_text_from_item)
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub fn has_structured_message_text(value: &Value) -> bool {
    value
        .get("output")
        .and_then(|v| v.as_array())
        .is_some_and(|output| {
            output.iter().any(|item| {
                item.get("type").and_then(|v| v.as_str()) == Some("message")
                    && !message_text_from_item(item).is_empty()
            })
        })
}

pub fn response_parts_from_value(value: &Value) -> Vec<LlmOutputPart> {
    response_parts_from_value_with_decoder(value, &ToolArgumentDecoder::default())
}

/// The assistant-text block identity for one visible message item:
/// `message:{item_id}` when the server named the item, else a deterministic
/// per-response ordinal. Shared by the live SSE mint and the
/// buffered/plain-JSON replay of a Responses payload so both lanes mint the
/// same identity for the same item.
pub fn text_part_block_identity(
    item_id: Option<&str>,
    next_ordinal: &mut u64,
) -> StreamBlockIdentity {
    let ordinal = *next_ordinal;
    *next_ordinal += 1;
    let item_id = item_id.filter(|id| !id.is_empty());
    StreamBlockIdentity::new(
        item_id
            .map(|id| format!("message:{id}"))
            .unwrap_or_else(|| format!("text:{ordinal}")),
        ordinal,
    )
    .with_item_id(item_id.map(str::to_string))
}

pub fn response_parts_from_value_with_decoder(
    value: &Value,
    tool_argument_decoder: &ToolArgumentDecoder,
) -> Vec<LlmOutputPart> {
    let mut parts = Vec::new();
    if let Some(output) = value.get("output").and_then(|v| v.as_array()) {
        for item in output {
            match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "reasoning" => {
                    let summary = item
                        .get("summary")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|entry| {
                                    entry.get("text").and_then(|v| v.as_str()).map(String::from)
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let text = summary.join("\n\n");
                    parts.push(LlmOutputPart::Reasoning {
                        text,
                        replay: Some(ProviderReasoningReplay {
                            item_id: item.get("id").and_then(|v| v.as_str()).map(str::to_string),
                            encrypted_content: item
                                .get("encrypted_content")
                                .and_then(|v| v.as_str())
                                .map(str::to_string),
                            signature: None,
                            redacted: false,
                            summary,
                            ..ProviderReasoningReplay::default()
                        }),
                    });
                }
                "message" => {
                    let text = message_text_from_item(item);
                    if !text.is_empty() {
                        parts.push(LlmOutputPart::Text {
                            text,
                            response_meta: Some(response_text_meta_from_message_item(item)),
                        });
                    }
                }
                "function_call" => {
                    let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let arguments = item
                        .get("arguments")
                        .map(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .unwrap_or_else(|| "{}".to_string());
                    parts.push(LlmOutputPart::ToolCall {
                        call_id: item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        tool_name: name.to_string(),
                        input_json: tool_argument_decoder.decode(name, arguments),
                        replay: item.get("id").and_then(|v| v.as_str()).map(|id| {
                            ProviderReplayMeta {
                                item_id: Some(id.to_string()),
                                opaque: None,
                                ..ProviderReplayMeta::default()
                            }
                        }),
                    });
                }
                _ => {}
            }
        }
    }
    if !parts
        .iter()
        .any(|part| matches!(part, LlmOutputPart::Text { text, .. } if !text.is_empty()))
        && let Some(text) = value.get("output_text").and_then(|v| v.as_str())
        && !text.is_empty()
    {
        parts.push(LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        });
    }
    parts
}

// ---------------------------------------------------------------------------
// Stream state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct ResponsesStreamingToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub input_json: String,
    /// Responses API item-id (e.g. `fc_...`). Preserved so we can re-emit it on
    /// the next request body alongside `call_id`; the server uses it to pair a
    /// function_call with its sibling reasoning item.
    pub item_id: String,
}

mod slots;
use slots::{
    ResponsesPartKind, ResponsesPartSlot, ResponsesPartSlotAllocation, ResponsesPartSlotIdentity,
    ResponsesPartSlotKey,
};

#[derive(Clone, Debug, Default)]
pub struct ResponsesStreamState {
    pub parts: Vec<LlmOutputPart>,
    pub usage: LlmUsage,
    pub provider_usage: Option<Value>,
    pub execution_evidence: Option<ExecutionEvidence>,
    pub(crate) tool_argument_decoder: ToolArgumentDecoder,
    pub final_response: Option<Value>,
    /// Set only by a terminal Responses event, never merely by an event that
    /// happens to carry a `response` snapshot.
    pub terminal_event_seen: bool,
    /// True only when a terminal Responses payload carries the normal
    /// successful `completed` status.
    pub completed_status_seen: bool,
    pub(crate) current_text_slot: Option<usize>,
    /// Owner of the reasoning item currently receiving summary-part deltas.
    /// The server groups one reasoning item into multiple summary parts;
    /// the item keeps one part slot while each summary part is its own
    /// stream block (see `current_reasoning_block`).
    pub(crate) current_reasoning_slot: Option<usize>,
    pub(crate) last_reasoning_slot: Option<usize>,
    /// Block-boundary stream events minted at the provider edge, drained by
    /// the driver after each SSE event.
    pub(crate) block_events: Vec<LlmStreamEvent>,
    /// Per-response block ordinal; every minted block gets the next value so
    /// ordering never depends on parsing `id`.
    pub(crate) next_block_ordinal: u64,
    /// Message-slot owner → the assistant-text block minted for that item.
    pub(crate) text_blocks: HashMap<usize, StreamBlockIdentity>,
    /// Owners whose text block already sealed at `response.output_item.done`.
    /// `finish_blocks` seals the rest at the terminal event.
    pub(crate) sealed_text_owners: std::collections::HashSet<usize>,
    /// The reasoning summary part currently open for deltas, its
    /// `(item_id, summary_index)` key, and its accumulated text for the
    /// authoritative block end.
    pub(crate) current_reasoning_block: Option<(StreamBlockIdentity, Option<u64>, String)>,
    /// Both Responses identifier forms resolve through this table to one
    /// canonical owner per part kind. Aliases never carry payloads.
    pub(crate) part_slots: HashMap<ResponsesPartSlotKey, usize>,
    pub(crate) slot_owners: Vec<ResponsesPartSlot>,
    /// Set once streamed output evidence has arrived. Allocating an empty
    /// message, reasoning, or tool-call slot does not set this flag. The terminal
    /// `response.completed.response.output` is authoritative for status/usage
    /// but is only parsed into parts when the stream did not deliver items.
    pub streamed_item_content_received: bool,
    /// Set when the stream contains an event type this adapter does not
    /// recognise. Unknown events are possible output by default: teaching the
    /// parser about a new event may make that classification more precise, but
    /// schema drift must never make a second generation look charge-safe.
    pub unrecognized_event_observed: bool,
    /// Stamped from `ProviderOptions::expose_thinking` at state construction
    /// so the assembled `LlmResponse` carries the visibility policy forward
    /// for the runtime's reasoning republication gate.
    pub expose_thinking: bool,
}

impl ResponsesStreamState {
    pub fn with_tool_argument_decoder(tool_argument_decoder: ToolArgumentDecoder) -> Self {
        Self {
            tool_argument_decoder,
            ..Self::default()
        }
    }

    fn slot_has_kind(&self, owner: usize, kind: ResponsesPartKind) -> bool {
        self.slot_owners
            .get(owner)
            .is_some_and(|slot| slot.kind() == kind)
    }

    fn allocate_part_slot(&mut self, kind: ResponsesPartKind) -> usize {
        let slot = match kind {
            ResponsesPartKind::Message => {
                let part_index = self.parts.len();
                self.parts.push(LlmOutputPart::Text {
                    text: String::new(),
                    response_meta: None,
                });
                ResponsesPartSlot::Message(part_index)
            }
            ResponsesPartKind::Reasoning => {
                let part_index = self.parts.len();
                self.parts.push(LlmOutputPart::Reasoning {
                    text: String::new(),
                    replay: None,
                });
                ResponsesPartSlot::Reasoning(part_index)
            }
            ResponsesPartKind::ToolCall => {
                ResponsesPartSlot::ToolCall(ResponsesStreamingToolCall::default())
            }
        };
        let owner = self.slot_owners.len();
        self.slot_owners.push(slot);
        owner
    }

    fn part_slot_index(&self, owner: usize, kind: ResponsesPartKind) -> Option<usize> {
        match (self.slot_owners.get(owner)?, kind) {
            (ResponsesPartSlot::Message(index), ResponsesPartKind::Message)
            | (ResponsesPartSlot::Reasoning(index), ResponsesPartKind::Reasoning) => Some(*index),
            _ => None,
        }
    }

    fn bind_part_slot(&mut self, key: ResponsesPartSlotKey, owner: usize) {
        self.part_slots.insert(key, owner);
    }

    /// Resolve both Responses identifier forms to one canonical payload owner.
    /// `output_index` wins an existing disagreement because server output order
    /// remains stable even when item ids change between snapshots.
    fn allocate_or_find_part_slot(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        kind: ResponsesPartKind,
        current: Option<usize>,
        allocation: ResponsesPartSlotAllocation,
    ) -> Option<usize> {
        let output_key = output_index.map(|output_index| ResponsesPartSlotKey {
            kind,
            identity: ResponsesPartSlotIdentity::OutputIndex(output_index),
        });
        let item_key = item_id
            .filter(|id| !id.is_empty())
            .map(|id| ResponsesPartSlotKey {
                kind,
                identity: ResponsesPartSlotIdentity::ItemId(id.to_string()),
            });
        let output_owner = (allocation != ResponsesPartSlotAllocation::Fresh)
            .then(|| {
                output_key
                    .as_ref()
                    .and_then(|key| self.part_slots.get(key).copied())
            })
            .flatten()
            .filter(|owner| self.slot_has_kind(*owner, kind));
        let item_owner = (allocation != ResponsesPartSlotAllocation::Fresh)
            .then(|| {
                item_key
                    .as_ref()
                    .and_then(|key| self.part_slots.get(key).copied())
            })
            .flatten()
            .filter(|owner| self.slot_has_kind(*owner, kind));
        let has_key = output_key.is_some() || item_key.is_some();
        let owner = output_owner
            .or(item_owner)
            .or_else(|| {
                (!has_key || allocation == ResponsesPartSlotAllocation::ReuseCurrent)
                    .then_some(current)
                    .flatten()
                    .filter(|_| allocation != ResponsesPartSlotAllocation::Fresh)
                    .filter(|owner| self.slot_has_kind(*owner, kind))
            })
            .or_else(|| {
                (kind != ResponsesPartKind::ToolCall || has_key)
                    .then(|| self.allocate_part_slot(kind))
            })?;

        if let Some(key) = output_key {
            self.bind_part_slot(key, owner);
        }
        if let Some(key) = item_key {
            self.bind_part_slot(key, owner);
        }
        Some(owner)
    }

    pub(crate) fn pending_tool_call_has_output_evidence(&self) -> bool {
        self.slot_owners.iter().any(|slot| {
            matches!(
                slot,
                ResponsesPartSlot::ToolCall(tool_call) if !tool_call.input_json.is_empty()
            )
        })
    }

    fn tool_call_mut(&mut self, owner: usize) -> Option<&mut ResponsesStreamingToolCall> {
        match self.slot_owners.get_mut(owner) {
            Some(ResponsesPartSlot::ToolCall(tool_call)) => Some(tool_call),
            _ => None,
        }
    }

    pub fn capture_execution_evidence(
        &mut self,
        response: &Value,
        terminal_event: bool,
    ) -> Result<(), LlmTransportError> {
        let usage = response.get("usage").unwrap_or(&Value::Null);
        let provider_finish_reason = terminal_event
            .then(|| {
                response
                    .get("incomplete_details")
                    .or_else(|| response.get("incompleteDetails"))
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .or_else(|| response.get("status").and_then(Value::as_str))
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            })
            .flatten();
        let next = ExecutionEvidence {
            served_model: response
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            provider_response_id: response
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            provider_request_id: None,
            reasoning_output_tokens: usage
                .get("output_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
                .and_then(Value::as_u64),
            provider_finish_reason,
            collection_interruption: None,
        };
        if next == ExecutionEvidence::default() {
            return Ok(());
        }
        ExecutionEvidence::merge_optional(&mut self.execution_evidence, Some(next)).map_err(
            |error| {
                LlmTransportError::new(format!("Responses stream {error}"))
                    .with_kind(ProviderFailureKind::Stream)
                    .with_adapter_code(TurnFailureCode::from_wire(error.code()))
            },
        )
    }

    /// The assistant-text block for one message item: `message:{item_id}` when
    /// the server named the item, else a deterministic per-response ordinal.
    /// Minted once per slot owner; the first use emits `TextBlockStart`.
    fn text_block(&mut self, owner: usize, item_id: Option<&str>) -> StreamBlockIdentity {
        if let Some(block) = self.text_blocks.get(&owner) {
            return block.clone();
        }
        let ordinal = self.next_block_ordinal;
        self.next_block_ordinal += 1;
        let item_id = item_id.filter(|id| !id.is_empty());
        let block = StreamBlockIdentity::new(
            item_id
                .map(|id| format!("message:{id}"))
                .unwrap_or_else(|| format!("text:{ordinal}")),
            ordinal,
        )
        .with_item_id(item_id.map(str::to_string));
        self.text_blocks.insert(owner, block.clone());
        self.block_events.push(LlmStreamEvent::TextBlockStart {
            block: block.clone(),
        });
        block
    }

    /// Opens the reasoning block for one summary part: `"{item_id}:{index}"`
    /// when the server named both, else a deterministic per-response ordinal.
    fn open_reasoning_block(&mut self, item_id: Option<&str>, summary_index: Option<u64>) {
        let ordinal = self.next_block_ordinal;
        self.next_block_ordinal += 1;
        let item_id = item_id.filter(|id| !id.is_empty());
        let block = StreamBlockIdentity::new(
            match (item_id, summary_index) {
                (Some(item_id), Some(summary_index)) => {
                    format!("{item_id}:summary:{summary_index}")
                }
                _ => format!("reasoning:{ordinal}"),
            },
            ordinal,
        )
        .with_item_id(item_id.map(str::to_string));
        self.block_events.push(LlmStreamEvent::ReasoningBlockStart {
            block: block.clone(),
        });
        self.current_reasoning_block = Some((block, summary_index, String::new()));
    }

    /// Closes the open reasoning block with its authoritative accumulated
    /// text, matching the `trim_end` applied to the item part.
    fn close_reasoning_block(&mut self) {
        let Some((block, _, text)) = self.current_reasoning_block.take() else {
            return;
        };
        self.block_events.push(LlmStreamEvent::ReasoningBlockEnd {
            block,
            text: text.trim_end().to_string(),
        });
    }

    pub fn begin_message(&mut self, item: Option<&Value>, output_index: Option<usize>) {
        let item_id = item
            .and_then(|item| item.get("id").and_then(|v| v.as_str()))
            .map(str::to_string);
        let meta = item.map(response_text_meta_from_message_item);
        let (owner, _) = self.message_part_index(output_index, item_id.as_deref(), meta);
        self.current_text_slot = Some(owner);
        self.text_block(owner, item_id.as_deref());
    }

    pub fn finish_message(
        &mut self,
        item: Option<&Value>,
        output_index: Option<usize>,
    ) -> Option<LlmOutputPart> {
        let mut finalized = None;
        if let Some(item) = item {
            let text = message_text_from_item(item);
            let meta = response_text_meta_from_message_item(item);
            let item_id = meta.id.clone();
            let (owner, index) =
                self.message_part_index(output_index, item_id.as_deref(), Some(meta));
            if !text.is_empty() {
                self.reconcile_text_part(index, &text);
                self.streamed_item_content_received = true;
            }
            finalized = self.parts.get(index).cloned();
            if let Some(block) = self.text_blocks.get(&owner).cloned() {
                let authoritative = finalized
                    .as_ref()
                    .and_then(|part| match part {
                        LlmOutputPart::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                self.block_events.push(LlmStreamEvent::TextBlockEnd {
                    block,
                    text: authoritative,
                });
                self.sealed_text_owners.insert(owner);
            }
        }
        self.current_text_slot = None;
        finalized
    }

    pub fn push_text_delta(
        &mut self,
        piece: &str,
        output_index: Option<usize>,
        item_id: Option<&str>,
    ) {
        if piece.is_empty() {
            return;
        }
        let part_index = self.ensure_text_part_index(output_index, item_id);
        let block = self
            .current_text_slot
            .map(|owner| self.text_block(owner, item_id));
        self.append_text_delta_to_part(part_index, piece);
        if let Some(block) = block {
            self.block_events.push(LlmStreamEvent::Delta {
                block,
                text: piece.to_string(),
            });
        }
    }

    pub fn reconcile_text_event(
        &mut self,
        text: &str,
        output_index: Option<usize>,
        item_id: Option<&str>,
    ) {
        if text.is_empty() {
            return;
        }
        let part_index = self.ensure_text_part_index(output_index, item_id);
        self.reconcile_text_part(part_index, text);
        self.streamed_item_content_received = true;
    }

    fn reconcile_text_part(&mut self, part_index: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        let existing = self
            .parts
            .get(part_index)
            .and_then(|part| match part {
                LlmOutputPart::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if text == existing {
            return;
        }
        if let Some(suffix) = text.strip_prefix(existing.as_str()) {
            let owner = self.slot_owners.iter().position(
                |slot| matches!(slot, ResponsesPartSlot::Message(index) if *index == part_index),
            );
            let block = owner.map(|owner| self.text_block(owner, None));
            self.append_text_delta_to_part(part_index, suffix);
            if let Some(block) = block {
                self.block_events.push(LlmStreamEvent::Delta {
                    block,
                    text: suffix.to_string(),
                });
            }
            return;
        }
        self.set_text_part(part_index, text.to_string());
    }

    /// Azure can omit `encrypted_content` from `response.output_item.done` and
    /// provide it only in the terminal `response.output`. Backfill the replay
    /// blob onto streamed reasoning parts so store:false multi-turn replay
    /// stays stateless even when item.done was incomplete.
    fn backfill_reasoning_replay(&mut self, response: &Value) {
        let Some(items) = response.get("output").and_then(|v| v.as_array()) else {
            return;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(item_id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(blob) = item.get("encrypted_content").and_then(Value::as_str) else {
                continue;
            };
            if let Some(existing) = self.parts.iter_mut().find(|part| {
                matches!(part, LlmOutputPart::Reasoning { replay, .. } if replay.as_ref().and_then(|meta| meta.item_id.as_deref()) == Some(item_id))
            }) && let LlmOutputPart::Reasoning { replay, .. } = existing
                && let Some(meta) = replay
                && meta.encrypted_content.is_none()
            {
                meta.encrypted_content = Some(blob.to_string());
            }
        }
    }

    pub fn merge_final_response(&mut self, response: &Value) {
        if self.streamed_item_content_received {
            self.backfill_reasoning_replay(response);
            return;
        }
        let structured_message_text = has_structured_message_text(response);
        for part in response_parts_from_value_with_decoder(response, &self.tool_argument_decoder) {
            match part {
                LlmOutputPart::Text {
                    text,
                    response_meta,
                } => {
                    let item_id = response_meta.as_ref().and_then(|meta| meta.id.clone());
                    if item_id.is_none()
                        && !structured_message_text
                        && self.parts.iter().any(|part| {
                            matches!(part, LlmOutputPart::Text { text, .. } if !text.is_empty())
                        })
                    {
                        continue;
                    }
                    let (_, index) =
                        self.message_part_index(None, item_id.as_deref(), response_meta);
                    self.reconcile_text_part(index, &text);
                }
                part @ LlmOutputPart::Reasoning { .. } => {
                    let part_item_id = match &part {
                        LlmOutputPart::Reasoning { replay, .. } => {
                            replay.as_ref().and_then(|meta| meta.item_id.as_deref())
                        }
                        _ => None,
                    };
                    if let Some(id) = part_item_id
                        && let Some(existing) = self.parts.iter_mut().find(|existing| {
                            matches!(existing, LlmOutputPart::Reasoning { replay, .. } if replay.as_ref().and_then(|meta| meta.item_id.as_deref()) == Some(id))
                        })
                    {
                        *existing = part;
                        continue;
                    }
                    if !self.parts.iter().any(|existing| existing == &part) {
                        self.parts.push(part);
                    }
                }
                part @ LlmOutputPart::ToolCall { .. } => {
                    let (part_item_id, part_call_id) = match &part {
                        LlmOutputPart::ToolCall {
                            replay, call_id, ..
                        } => (
                            replay.as_ref().and_then(|meta| meta.item_id.as_deref()),
                            call_id.as_str(),
                        ),
                        _ => (None, ""),
                    };
                    let duplicate = self.parts.iter().any(|existing| match existing {
                        LlmOutputPart::ToolCall {
                            replay: existing_replay,
                            call_id: existing_call_id,
                            ..
                        } => {
                            part_item_id
                                .zip(
                                    existing_replay
                                        .as_ref()
                                        .and_then(|meta| meta.item_id.as_deref()),
                                )
                                .is_some_and(|(a, b)| a == b)
                                || (!part_call_id.is_empty() && part_call_id == existing_call_id)
                        }
                        _ => false,
                    });
                    if !duplicate {
                        self.parts.push(part);
                    }
                }
            }
        }
    }

    /// Allocate or find a slot of a kind that is keyed by output index, and
    /// resolve the part index it owns. `allocate_or_find_part_slot` declines
    /// only when a slot needs a provider key it was not given, which the
    /// message and reasoning kinds never do.
    #[expect(clippy::expect_used, reason = "no provider key needed")]
    fn keyless_part_slot(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        kind: ResponsesPartKind,
        current: Option<usize>,
        allocation: ResponsesPartSlotAllocation,
    ) -> (usize, usize) {
        let owner = self
            .allocate_or_find_part_slot(output_index, item_id, kind, current, allocation)
            .expect("slots of this kind are allocated without a provider key");
        let index = self
            .part_slot_index(owner, kind)
            .expect("the slot owns its part");
        (owner, index)
    }

    pub fn ensure_text_part_index(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
    ) -> usize {
        let (owner, index) = self.keyless_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Message,
            self.current_text_slot,
            ResponsesPartSlotAllocation::ReuseCurrent,
        );
        self.current_text_slot = Some(owner);
        index
    }

    fn message_part_index(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        response_meta: Option<ResponseTextMeta>,
    ) -> (usize, usize) {
        let (owner, index) = self.keyless_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Message,
            self.current_text_slot,
            ResponsesPartSlotAllocation::Resolve,
        );

        if let Some(response_meta) = response_meta
            && let Some(LlmOutputPart::Text {
                response_meta: existing_meta,
                ..
            }) = self.parts.get_mut(index)
        {
            *existing_meta = Some(response_meta);
        }
        (owner, index)
    }

    fn set_text_part(&mut self, part_index: usize, text: String) {
        if let Some(LlmOutputPart::Text { text: existing, .. }) = self.parts.get_mut(part_index) {
            *existing = text;
        }
    }

    fn append_text_delta_to_part(&mut self, part_index: usize, piece: &str) {
        if piece.is_empty() {
            return;
        }
        if let Some(LlmOutputPart::Text { text, .. }) = self.parts.get_mut(part_index) {
            text.push_str(piece);
        }
        self.streamed_item_content_received = true;
    }

    pub fn full_text(&self) -> String {
        lash_core::facade_support::visible_response_text_from_parts(&self.parts)
    }

    pub fn begin_reasoning_item(&mut self, output_index: Option<usize>, item_id: Option<&str>) {
        self.current_reasoning_slot = self.allocate_or_find_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Reasoning,
            None,
            ResponsesPartSlotAllocation::Resolve,
        );
    }

    pub fn begin_reasoning_part(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        summary_index: Option<u64>,
    ) {
        self.close_reasoning_block();
        let allocation = if output_index.is_some() {
            ResponsesPartSlotAllocation::Resolve
        } else {
            ResponsesPartSlotAllocation::Fresh
        };
        self.current_reasoning_slot = self.allocate_or_find_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Reasoning,
            None,
            allocation,
        );
        self.open_reasoning_block(item_id, summary_index);
    }

    pub fn push_reasoning_delta(
        &mut self,
        delta: &str,
        output_index: Option<usize>,
        item_id: Option<&str>,
        summary_index: Option<u64>,
    ) {
        if delta.is_empty() {
            return;
        }
        let (owner, index) = self.keyless_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Reasoning,
            self.current_reasoning_slot,
            ResponsesPartSlotAllocation::Resolve,
        );
        self.current_reasoning_slot = Some(owner);
        if let Some(LlmOutputPart::Reasoning { text, .. }) = self.parts.get_mut(index) {
            text.push_str(delta);
        }
        self.streamed_item_content_received = true;
        // A delta can arrive without its `summary_part.added` (or under a new
        // summary_index mid-part): close the open block and mint a fresh one
        // keyed by the delta's own identity facts.
        let key_matches =
            self.current_reasoning_block
                .as_ref()
                .is_some_and(|(block, block_summary_index, _)| {
                    block.item_id.as_deref() == item_id.filter(|id| !id.is_empty())
                        && *block_summary_index == summary_index
                });
        if !key_matches {
            self.close_reasoning_block();
            self.open_reasoning_block(item_id, summary_index);
        }
        if let Some((block, _, block_text)) = self.current_reasoning_block.as_mut() {
            block_text.push_str(delta);
            self.block_events.push(LlmStreamEvent::ReasoningDelta {
                block: block.clone(),
                text: delta.to_string(),
            });
        }
    }

    pub fn reconcile_reasoning_event(
        &mut self,
        text: &str,
        output_index: Option<usize>,
        item_id: Option<&str>,
        summary_index: Option<u64>,
    ) {
        let Some(owner) = self.allocate_or_find_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Reasoning,
            self.current_reasoning_slot,
            ResponsesPartSlotAllocation::Resolve,
        ) else {
            return;
        };
        self.current_reasoning_slot = Some(owner);
        let Some(index) = self.part_slot_index(owner, ResponsesPartKind::Reasoning) else {
            return;
        };
        let existing = self
            .parts
            .get(index)
            .and_then(|part| match part {
                LlmOutputPart::Reasoning { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if text != existing
            && let Some(suffix) = text.strip_prefix(existing.as_str())
        {
            self.push_reasoning_delta(suffix, output_index, item_id, summary_index);
        }
    }

    pub fn finish_reasoning_part(&mut self) {
        self.close_reasoning_block();
        // Drop the cursor; the next `part.added` opens a fresh slot. Trim
        // trailing whitespace so concatenated paragraphs don't carry blanks.
        if let Some(owner) = self.current_reasoning_slot.take()
            && let Some(index) = self.part_slot_index(owner, ResponsesPartKind::Reasoning)
            && let Some(LlmOutputPart::Reasoning { text, .. }) = self.parts.get_mut(index)
        {
            self.last_reasoning_slot = Some(owner);
            let trimmed = text.trim_end();
            if trimmed.len() != text.len() {
                *text = trimmed.to_string();
            }
        }
    }

    /// Populate the most recent reasoning part with the authoritative payload
    /// from `response.output_item.done`: the `rs_...` id, the `summary[*].text`
    /// entries, and the `encrypted_content` blob replayed on the next turn.
    pub fn finalize_reasoning_item(
        &mut self,
        item: &Value,
        output_index: Option<usize>,
    ) -> Option<LlmOutputPart> {
        self.streamed_item_content_received |=
            crate::responses_output_evidence::reasoning_item_has_output_evidence(item);
        let item_id = item.get("id").and_then(Value::as_str);
        let owner = self.allocate_or_find_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::Reasoning,
            self.current_reasoning_slot.or(self.last_reasoning_slot),
            ResponsesPartSlotAllocation::ReuseCurrent,
        )?;
        self.current_reasoning_slot = Some(owner);
        let index = self.part_slot_index(owner, ResponsesPartKind::Reasoning)?;
        let part = self.parts.get_mut(index)?;
        let LlmOutputPart::Reasoning { replay, .. } = part else {
            return None;
        };
        let meta = replay.get_or_insert_with(ProviderReasoningReplay::default);
        if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
            meta.item_id = Some(id.to_string());
        }
        if let Some(blob) = item.get("encrypted_content").and_then(|v| v.as_str()) {
            meta.encrypted_content = Some(blob.to_string());
        }
        if let Some(arr) = item.get("summary").and_then(|v| v.as_array()) {
            let texts: Vec<String> = arr
                .iter()
                .filter_map(|entry| entry.get("text").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if !texts.is_empty() {
                meta.summary = texts;
            }
        }
        Some(part.clone())
    }

    /// Drains the block-boundary events (`TextBlockStart`/`Delta`/
    /// `TextBlockEnd`, `ReasoningBlockStart`/`ReasoningDelta`/
    /// `ReasoningBlockEnd`) minted while folding the last SSE event.
    pub fn take_block_events(&mut self) -> Vec<LlmStreamEvent> {
        std::mem::take(&mut self.block_events)
    }

    /// Seal every block still open at a terminal boundary — normal
    /// completion, `response.incomplete`, failure, or an abort. Every
    /// `BlockStart` pairs with a `BlockEnd` carrying the text accumulated for
    /// that block.
    pub fn finish_blocks(&mut self) -> Vec<LlmStreamEvent> {
        self.close_reasoning_block();
        let mut open: Vec<(usize, StreamBlockIdentity)> = self
            .text_blocks
            .iter()
            .filter(|(owner, _)| !self.sealed_text_owners.contains(owner))
            .map(|(owner, block)| (*owner, block.clone()))
            .collect();
        open.sort_by_key(|(_, block)| block.ordinal);
        for (owner, block) in open {
            let text = self
                .slot_owners
                .get(owner)
                .and_then(|slot| match slot {
                    ResponsesPartSlot::Message(index) => self.parts.get(*index),
                    _ => None,
                })
                .and_then(|part| match part {
                    LlmOutputPart::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            self.block_events
                .push(LlmStreamEvent::TextBlockEnd { block, text });
        }
        self.sealed_text_owners
            .extend(self.text_blocks.keys().copied());
        self.take_block_events()
    }

    fn tool_call_slot(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
    ) -> Option<usize> {
        self.allocate_or_find_part_slot(
            output_index,
            item_id,
            ResponsesPartKind::ToolCall,
            None,
            ResponsesPartSlotAllocation::Resolve,
        )
    }

    pub fn update_tool_call_from_item(
        &mut self,
        item: &Value,
        output_index: Option<usize>,
    ) -> Option<usize> {
        let item_id = item.get("id").and_then(|v| v.as_str());
        let owner = self.tool_call_slot(output_index, item_id)?;
        let mut content_received = false;
        if let Some(tool_call) = self.tool_call_mut(owner) {
            if tool_call.item_id.is_empty()
                && let Some(item_id) = item_id
            {
                tool_call.item_id = item_id.to_string();
            }
            if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
                tool_call.call_id = call_id.to_string();
            }
            if let Some(tool_name) = item.get("name").and_then(|v| v.as_str()) {
                tool_call.tool_name = tool_name.to_string();
                content_received |= !tool_name.is_empty();
            }
            if let Some(arguments) = item.get("arguments").and_then(|v| v.as_str())
                && !arguments.is_empty()
            {
                tool_call.input_json = arguments.to_string();
                content_received = true;
            }
        }
        self.streamed_item_content_received |= content_received;
        Some(owner)
    }

    pub fn push_tool_call_delta(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        delta: &str,
    ) {
        if delta.is_empty() {
            return;
        }
        let Some(owner) = self.tool_call_slot(output_index, item_id) else {
            return;
        };
        self.streamed_item_content_received = true;
        if let Some(tool_call) = self.tool_call_mut(owner) {
            tool_call.input_json.push_str(delta);
        }
    }

    pub fn set_tool_call_arguments(
        &mut self,
        output_index: Option<usize>,
        item_id: Option<&str>,
        arguments: &str,
    ) {
        let Some(owner) = self.tool_call_slot(output_index, item_id) else {
            return;
        };
        if arguments.is_empty() {
            return;
        }
        self.streamed_item_content_received = true;
        if let Some(tool_call) = self.tool_call_mut(owner) {
            tool_call.input_json = arguments.to_string();
        }
    }

    pub fn finish_tool_call(
        &mut self,
        item: &Value,
        output_index: Option<usize>,
    ) -> Option<LlmOutputPart> {
        let owner = self.update_tool_call_from_item(item, output_index)?;
        let tool_call = {
            let tool_call = self.tool_call_mut(owner)?;
            if tool_call.call_id.is_empty() {
                tool_call.call_id = uuid::Uuid::new_v4().to_string();
            }
            if tool_call.tool_name.is_empty() {
                return None;
            }
            if tool_call.input_json.is_empty() {
                tool_call.input_json = "{}".to_string();
            }
            tool_call.clone()
        };
        let tool_name = tool_call.tool_name;
        let part = LlmOutputPart::ToolCall {
            call_id: tool_call.call_id,
            input_json: self
                .tool_argument_decoder
                .decode(&tool_name, tool_call.input_json),
            tool_name,
            replay: (!tool_call.item_id.is_empty()).then_some(ProviderReplayMeta {
                item_id: Some(tool_call.item_id),
                opaque: None,
                ..ProviderReplayMeta::default()
            }),
        };
        if !self.parts.iter().any(|existing| existing == &part) {
            self.parts.push(part.clone());
            return Some(part);
        }
        None
    }

    /// Non-empty parts collected so far, falling back to the final response's
    /// parsed parts / text when nothing streamed.
    pub fn response_parts(&self) -> Vec<LlmOutputPart> {
        let parts = self
            .parts
            .iter()
            .filter_map(|part| match part {
                LlmOutputPart::Text { text, .. } if text.is_empty() => None,
                LlmOutputPart::Reasoning { text, .. } if text.trim().is_empty() => None,
                other => Some(other.clone()),
            })
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            return parts;
        }
        if let Some(final_response) = &self.final_response {
            let parts =
                response_parts_from_value_with_decoder(final_response, &self.tool_argument_decoder);
            if !parts.is_empty() {
                return parts;
            }
            let text = extract_text(final_response);
            if !text.is_empty() {
                return vec![LlmOutputPart::Text {
                    text,
                    response_meta: None,
                }];
            }
        }
        Vec::new()
    }
}

mod sse;

pub use sse::{parse_sse_payload, process_sse_event};
