//! Crate-internal prelude. Submodules `use crate::support::*` to share the
//! common imports without repeating the list, mirroring the OpenAI crate's
//! layout.

pub(crate) use async_trait::async_trait;
pub(crate) use base64::Engine;
pub(crate) use serde_json::{Value, json};

pub(crate) use lash_core::llm::transport::{
    LlmTransportError, ProviderFailureKind, TransportRetryVerdict, TurnFailureCode,
};
pub(crate) use lash_core::llm::types::{
    AnthropicThinkingRetention, AttachmentSlot, ExecutionEvidence, GenerationReceipt,
    LiveRequestBody, LlmContentBlock, LlmEventSender, LlmOutputPart, LlmOutputSpec, LlmRequest,
    LlmResponse, LlmRole, LlmStreamEvent, LlmStreamEvidence, LlmTerminalReason, LlmToolChoice,
    LlmUsage, ProviderReasoningReplay, ProviderReasoningRetentionSupport, ProviderRouteIdentity,
    ReasoningRetentionSelection, ReasoningRetentionValidationError, RecordedRequestTemplate,
    ResponseContext, StreamBlockIdentity, TransientJson, tool_call_input_replay_value,
};
pub(crate) use lash_core::provider::{
    CacheRetention, GenerationEmission, GenerationWire, OutputCapWire, Provider,
    ProviderComponents, ProviderOptions, ProviderToken, StreamTermination, ThinkingSummaryWire,
    TokenRequestReason, TokenSource, resolve_generation_policy,
};
pub(crate) use lash_core::{
    facade_support::ProviderSchemaCapabilities, facade_support::SchemaPurpose,
    facade_support::SchemaResolutionError, facade_support::SchemaResolutionRequest,
    facade_support::resolve_schema,
};
pub(crate) use lash_llm_transport::normalize::{
    http_error_envelope, merge_usage, serialize_options_tail, terminal_reason_from_parts,
};
pub(crate) use lash_llm_transport::streaming::{SseStreamBounds, drive_sse_response};
pub(crate) use lash_llm_transport::timeouts::response_start_timeout;
pub(crate) use lash_llm_transport::util::{emit_provider_request_trace, emit_provider_trace};
pub(crate) use lash_llm_transport::{
    LlmHttpRequest, LlmHttpTransport, ReqwestLlmHttpTransport, ResponseMetadataCapture, TokenGate,
    first_header_value, merge_extra_body, merge_extra_headers, read_http_body_text,
    rejected_before_output, reserved_generation_paths, validate_extra_headers,
};
pub(crate) use lash_sansio::ModelToolReturnPart;
pub(crate) use std::sync::Arc;

pub(crate) use crate::config::*;
pub(crate) use crate::policy::*;

pub(crate) use lash_core::provider::attachment_wire::{
    attachment_operand, check_slot, lower_attachment_json, template_error,
};
pub(crate) use lash_sansio::AttachmentRef;
pub(crate) use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, ProviderAccepts, ProviderFileScope,
};
