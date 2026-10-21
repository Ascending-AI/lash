pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::Duration;

pub(super) use async_trait::async_trait;
pub(super) use serde::{Deserialize, Serialize};

pub(super) use crate::llm::transport::ProviderFailureKind;
pub(super) use crate::llm::types::{
    AttachmentSlot, AttemptOutcome, AttemptRecord, ChargeSafetyDecision, ChargeSafetyDenialReason,
    ExecutionEvidence, GenerationOptionOutcome, GenerationReceipt, LiveRequestBody, LlmCallId,
    LlmCallRecord, LlmContentBlock, LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalReason,
    NormalizedError, ProtocolPosition, ProviderReplayOriginConflict, ProviderRouteIdentity,
    RecordedRequestTemplate, ResponseContext, RetryClass, RetryDecision, RetryDeclineCause,
    RetryWait, SegmentCache, TransientJson,
};
pub(super) use lash_protocol_types::llm::capability::ReasoningIntent;
pub(super) use lash_protocol_types::{
    session_model::FailureCode, session_model::Namespace, session_model::TurnFailureCode,
};

pub(super) use lash_transport_failure::{LlmTransportError, TransportRetryVerdict};

#[cfg(test)]
pub(super) use super::handle::*;
pub(super) use super::options::*;
pub(super) use super::rate_limit::*;
pub(super) use super::traits::*;

#[cfg(test)]
pub(super) fn fixture_record(
    context: lash_trace::TraceContext,
    event: lash_trace::TraceEvent,
) -> lash_trace::TraceRecord {
    lash_trace::TraceRecord {
        schema_version: lash_trace::TRACE_SCHEMA_VERSION,
        id: "fixture-record".into(),
        timestamp: lash_trace::TraceTimestamp::from_unix_ms(0),
        content: lash_trace::ContentPolicy::captured().into(),
        context,
        position: None,
        omissions: Default::default(),
        event,
    }
}
