//! Crate-internal prelude. Submodules `use crate::support::*` to share the
//! common imports without repeating the list, mirroring the OpenAI crate's
//! layout.

pub(crate) use async_trait::async_trait;
pub(crate) use base64::Engine;
pub(crate) use serde::Deserialize;
pub(crate) use serde_json::{Value, json};

pub(crate) use lash_core::llm::transport::{
    GOOGLE_FILE_MIMES, GOOGLE_IMAGE_MIMES, GOOGLE_MEDIA_FAMILIES, LlmTransportError,
    ProviderFailureKind, known_attachment_acceptors, unsupported_attachment_capability,
};
pub(crate) use lash_core::llm::types::{
    AttachmentSource, ExecutionEvidence, GenerationDisposition, GenerationOptionDisposition,
    LlmContentBlock, LlmOutputPart, LlmOutputSpec, LlmRequest, LlmResponse, LlmRole,
    LlmStreamEvent, LlmStreamEvidence, LlmTerminalReason, LlmToolChoice, LlmUsage,
    ProviderReasoningReplay, ProviderReplayMeta, ProviderRouteIdentity, ResponseTextMeta,
};
pub(crate) use lash_core::provider::{
    Provider, ProviderComponents, ProviderFactory, ProviderOptions, ReasoningDisableEncoding,
    ReasoningEncoding, ReasoningSelection, StreamTermination, resolve_generation_policy,
};
pub(crate) use lash_llm_transport::normalize::{
    http_error_envelope, serialize_options_tail, terminal_reason_from_parts,
};
pub(crate) use lash_llm_transport::streaming::{SseStreamBounds, drive_sse_response};
pub(crate) use lash_llm_transport::timeouts::response_start_timeout;
pub(crate) use lash_llm_transport::util::{
    emit_provider_request_trace, emit_provider_trace, parse_i64,
};
pub(crate) use lash_llm_transport::{
    LlmHttpRequest, LlmHttpTransport, ReqwestLlmHttpTransport, ResponseMetadataCapture,
    first_header_value, read_http_body_text,
};
pub(crate) use lash_provider_auth::{
    CredentialCallError, CredentialError, CredentialErrorKind, CredentialExecuteError, Lease,
};

pub(crate) use crate::config::*;

/// Mutable accumulators a single Cloud Code SSE event folds into: the running
/// full text, the per-event visible/reasoning deltas, the usage snapshot
/// (normalized plus the raw `usageMetadata` sidecar), optional tool-call and
/// structured output-part sinks, and the last finish-bearing event.
pub(crate) struct SseTextPartSink<'a> {
    pub full: &'a mut String,
    pub text_deltas: &'a mut Vec<String>,
    pub reasoning_deltas: &'a mut Vec<String>,
    pub usage: &'a mut LlmUsage,
    pub provider_usage: &'a mut Option<Value>,
    pub execution_evidence: &'a mut Option<ExecutionEvidence>,
    pub tool_call_parts: Option<&'a mut Vec<LlmOutputPart>>,
    pub output_parts: Option<&'a mut Vec<LlmOutputPart>>,
    pub finish_event: &'a mut Option<Value>,
}
