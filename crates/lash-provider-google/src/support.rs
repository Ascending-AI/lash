//! Crate-internal prelude. Submodules `use crate::support::*` to share the
//! common imports without repeating the list, mirroring the OpenAI crate's
//! layout.

pub(crate) use async_trait::async_trait;
pub(crate) use base64::Engine;
pub(crate) use serde::Deserialize;
pub(crate) use serde_json::{Value, json};

pub(crate) use lash_core::llm::transport::{
    LlmTransportError, ProviderFailureKind, TransportRetryVerdict, TurnFailureCode,
    known_attachment_acceptors, unsupported_attachment_capability,
};
pub(crate) use lash_core::llm::types::{
    AttachmentSource, ExecutionEvidence, GenerationOptionOutcome, GenerationReceipt,
    LlmContentBlock, LlmOutputPart, LlmOutputSpec, LlmRequest, LlmResponse, LlmRole,
    LlmStreamEvent, LlmStreamEvidence, LlmTerminalReason, LlmToolChoice, LlmUsage,
    ProviderReasoningReplay, ProviderReasoningRetentionSupport, ProviderReplayMeta,
    ProviderRouteIdentity, ReasoningRetentionValidationError, ResponseTextMeta,
    StreamBlockIdentity,
};
pub(crate) use lash_core::provider::{
    Provider, ProviderComponents, ProviderOptions, ReasoningDisableEncoding, ReasoningEncoding,
    ReasoningSelection, StreamTermination, resolve_generation_policy,
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
    CredentialCallError, CredentialError, CredentialExecuteError, Lease,
};
pub(crate) use lash_sansio::Redacted;

pub(crate) use crate::config::*;

pub(crate) use crate::stream::GoogleStreamState;
