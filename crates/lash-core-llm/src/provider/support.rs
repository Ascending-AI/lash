pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::Duration;

pub(super) use async_trait::async_trait;
pub(super) use serde::de::{self, Visitor};
pub(super) use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(super) use crate::llm::transport::{
    LlmTransportError, ProviderFailureKind, TransportRetryVerdict,
};
pub(super) use crate::llm::types::{
    AttemptOutcome, AttemptRecord, ChargeSafetyDecision, ChargeSafetyDenialReason,
    ExecutionEvidence, GenerationOptionOutcome, GenerationReceipt, LlmCallId, LlmCallRecord,
    LlmContentBlock, LlmRequest, LlmRequestScope, LlmResponse, LlmTerminalReason, NormalizedError,
    ProtocolPosition, ProviderReplayOriginConflict, ProviderRouteIdentity, RetryClass,
    RetryDecision, RetryDeclineCause, RetryWait,
};
pub(super) use lash_sansio::llm::capability::ReasoningIntent;
pub(super) use lash_sansio::session_model::{FailureCode, Namespace, TurnFailureCode};

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
        timestamp: Default::default(),
        context,
        event,
    }
}
