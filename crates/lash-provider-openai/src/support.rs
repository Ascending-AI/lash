pub(crate) use async_trait::async_trait;
pub(crate) use base64::Engine;
pub(crate) use serde::Deserialize;
pub(crate) use serde_json::{Value, json};
pub(crate) use std::collections::HashMap;

pub(crate) use lash_core::llm::transport::{
    LlmTransportError, OPENAI_FILE_MIMES, OPENAI_IMAGE_MIMES, ProviderFailureKind,
    TransportRetryVerdict, known_attachment_acceptors, unsupported_attachment_capability,
};
pub(crate) use lash_core::llm::types::{
    AttachmentSource, ExecutionEvidence, LlmContentBlock, LlmEventSender, LlmOutputPart,
    LlmOutputSpec, LlmProviderTraceSender, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent,
    LlmStreamEvidence, LlmTerminalReason, LlmUsage, ProviderReplayMeta, ProviderRouteIdentity,
};
pub(crate) use lash_core::{
    facade_support::ProviderSchemaCapabilities, facade_support::SchemaPurpose,
};
// `ResponseTextMeta` is only referenced by the crate's `#[cfg(test)]`
// assertions (the request/response shapes that exercise the shared Responses
// input builder), so gate the re-export to test builds to keep the non-test
// lib free of unused-import warnings.
pub(crate) use crate::schema::{classify_openai_error, responses_error_retry_verdict};
#[cfg(test)]
pub(crate) use lash_core::llm::types::{LlmRequestScope, ResponseTextMeta};
pub(crate) use lash_core::provider::{
    CacheControlDialect, CacheRetention, Provider, ProviderComponents, ProviderOptions,
    StreamTermination, resolve_generation_policy,
};
pub(crate) use lash_llm_transport::streaming::{
    SseStreamBounds, drive_sse_response, emit_stream_progress,
};
pub(crate) use lash_llm_transport::timeouts::response_start_timeout;
pub(crate) use lash_llm_transport::util::{emit_provider_request_trace, emit_provider_trace};
pub(crate) use lash_llm_transport::{
    LlmHttpBody, LlmHttpMethod, LlmHttpRequest, LlmHttpTransport, ResponseMetadataCapture,
    first_header_value, header_contains, http_error_envelope, read_http_body_text,
};

pub(crate) use crate::chat::*;
pub(crate) use crate::common::*;
pub(crate) use crate::config::*;
pub(crate) use crate::driver::*;

pub(crate) use crate::reasoning::*;
pub(crate) use crate::responses_shared::{ResponsesStreamState, role_name, tool_choice_value};
