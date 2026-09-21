pub(crate) use async_trait::async_trait;
#[cfg(test)]
pub(crate) use base64::Engine;
pub(crate) use serde::Deserialize;
pub(crate) use serde_json::{Value, json};
pub(crate) use std::collections::HashMap;

pub(crate) use lash_core::llm::transport::{
    LlmTransportError, ProviderFailureKind, TransportRetryVerdict, TurnFailureCode,
    known_attachment_acceptors, unsupported_attachment_capability,
};
pub(crate) use lash_core::llm::types::{
    AttachmentSource, ExecutionEvidence, LlmContentBlock, LlmEventSender, LlmOutputPart,
    LlmOutputSpec, LlmProviderTraceSender, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent,
    LlmStreamEvidence, LlmTerminalReason, LlmUsage, ProviderReasoningRetentionSupport,
    ProviderReplayMeta, ProviderRouteIdentity, ReasoningRetentionSelection, StreamBlockIdentity,
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
pub(crate) use lash_core::llm::types::{LlmRequestScope, ResponsePhase, ResponseTextMeta};
pub(crate) use lash_core::provider::{
    CacheControlDialect, CacheRetention, GenerationRetryGuarantee, Provider, ProviderComponents,
    ProviderOptions, StreamTermination, resolve_generation_policy,
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
pub(crate) use lash_sansio::Redacted;

pub(crate) use crate::chat::*;
pub(crate) use crate::common::*;
pub(crate) use crate::config::*;
pub(crate) use crate::driver::*;

pub(crate) use crate::reasoning::*;
pub(crate) use crate::responses_shared::{ResponsesStreamState, role_name, tool_choice_value};

/// `expose_thinking` gates the reasoning lane's block events the same way it
/// previously gated bare reasoning deltas.
pub(crate) fn is_reasoning_block_event(event: &LlmStreamEvent) -> bool {
    matches!(
        event,
        LlmStreamEvent::ReasoningBlockStart { .. }
            | LlmStreamEvent::ReasoningDelta { .. }
            | LlmStreamEvent::ReasoningBlockEnd { .. }
    )
}

/// The per-block texts of a completed reasoning item: one block per `summary`
/// entry when the item carries the server's summary parts (ids
/// `{item_id}:summary:{index}`, matching the live-streamed minting), else one
/// block for the item's whole text. Buffered completions emit these so their
/// stream carries the same block identities a live stream would have.
pub(crate) fn reasoning_part_block_texts(
    part: &LlmOutputPart,
    next_ordinal: &mut u64,
) -> Vec<(StreamBlockIdentity, String)> {
    let LlmOutputPart::Reasoning { text, replay } = part else {
        return Vec::new();
    };
    let item_id = replay
        .as_ref()
        .and_then(|meta| meta.item_id.as_deref())
        .filter(|id| !id.is_empty());
    let summary = replay
        .as_ref()
        .map(|meta| meta.summary.as_slice())
        .unwrap_or_default();
    let mut minted = Vec::new();
    let mut mint = |id: String, item_id: Option<&str>, text: &str, next_ordinal: &mut u64| {
        let block =
            StreamBlockIdentity::new(id, *next_ordinal).with_item_id(item_id.map(str::to_string));
        *next_ordinal += 1;
        minted.push((block, text.to_string()));
    };
    if summary.is_empty() {
        mint(
            item_id
                .map(str::to_string)
                .unwrap_or_else(|| format!("reasoning:{next_ordinal}")),
            item_id,
            text,
            next_ordinal,
        );
    } else {
        for (index, entry) in summary.iter().enumerate() {
            let id = item_id
                .map(|item_id| format!("{item_id}:summary:{index}"))
                .unwrap_or_else(|| format!("reasoning:{next_ordinal}"));
            mint(id, item_id, entry, next_ordinal);
        }
    }
    minted
}
