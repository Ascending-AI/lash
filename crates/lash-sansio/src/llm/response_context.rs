//! What reading a model call's response needs beside the body it sent.
//!
//! A provider's `send` takes the admitted body and a [`ResponseContext`],
//! and no request: the body is the only statement of what the call asks, so
//! a resend cannot disagree with its first attempt. The context's
//! [`ResponseContract`] and scope are recorded with the call's admission; its
//! senders are live and made afresh for every send.

use std::sync::Arc;

use super::types::{
    LlmEventSender, LlmOutputSpec, LlmProviderTraceSender, LlmRequest, LlmRequestScope,
    RecordedRequestTemplate,
};
use crate::SchemaContract;
use crate::llm_profile::LlmProfileConfig;

/// A tool whose calls the response may carry: its name and the input schema
/// its arguments are decoded against.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallContract {
    pub name: String,
    pub input_schema: SchemaContract,
}

/// The recorded facts a response is decoded under: the model of the pinned
/// route with its metadata, the output the call asked for, and the tools it
/// offered. It is fixed when the call is lowered and recorded at its
/// admission; nothing in it says what the call asks.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseContract {
    pub model: LlmProfileConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_spec: Option<LlmOutputSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolCallContract>,
}

impl ResponseContract {
    /// The contract of the call `request` lowers to.
    pub fn of_request(request: &LlmRequest) -> Self {
        Self {
            model: request.model.clone(),
            output_spec: request.output_spec.clone(),
            tools: request
                .tools
                .iter()
                .map(|tool| ToolCallContract {
                    name: tool.name.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
        }
    }
}

/// Everything a provider's `send` has beside the body: who the call is made
/// for, the recorded contract its response is decoded under, and the live
/// senders this send reports through.
#[derive(Clone)]
pub struct ResponseContext {
    /// The call's correlation scope, with the attempt this send is.
    pub scope: LlmRequestScope,
    pub contract: Arc<ResponseContract>,
    pub stream_events: Option<LlmEventSender>,
    pub provider_trace: Option<LlmProviderTraceSender>,
}

impl ResponseContext {
    /// The context an admission records for `request`, with no senders.
    pub fn recorded(scope: LlmRequestScope, contract: Arc<ResponseContract>) -> Self {
        Self {
            scope,
            contract,
            stream_events: None,
            provider_trace: None,
        }
    }

    /// The context of the call `request` lowers to, reporting through the
    /// request's own senders.
    pub fn of_request(request: &LlmRequest) -> Self {
        Self {
            scope: request.scope.clone(),
            contract: Arc::new(ResponseContract::of_request(request)),
            stream_events: request.stream_events.clone(),
            provider_trace: request.provider_trace.clone(),
        }
    }

    /// This context reporting through `stream_events` and `provider_trace`.
    #[must_use]
    pub fn with_senders(
        mut self,
        stream_events: Option<LlmEventSender>,
        provider_trace: Option<LlmProviderTraceSender>,
    ) -> Self {
        self.stream_events = stream_events;
        self.provider_trace = provider_trace;
        self
    }

    /// The model of the pinned route.
    pub fn model(&self) -> &LlmProfileConfig {
        &self.contract.model
    }

    /// See [`LlmRequestScope::continuation_key`].
    pub fn continuation_key(&self) -> String {
        self.scope.continuation_key()
    }
}

/// An admitted call as every send of it takes it: the request template its
/// admission recorded, and the response context recorded with it. A first
/// send and a resend are handed the same one.
#[derive(Clone)]
pub struct AdmittedSend {
    pub template: Arc<RecordedRequestTemplate>,
    /// The recorded context, with no senders: each send adds its own.
    pub response: ResponseContext,
}

impl AdmittedSend {
    /// The admission of the call `request` lowered to `template`.
    pub fn of_request(request: &LlmRequest, template: Arc<RecordedRequestTemplate>) -> Self {
        Self {
            template,
            response: ResponseContext::of_request(request).with_senders(None, None),
        }
    }
}

impl std::fmt::Debug for AdmittedSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedSend")
            .field("template", &self.template.redacted())
            .field("response", &self.response)
            .finish()
    }
}

impl std::fmt::Debug for ResponseContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseContext")
            .field("scope", &self.scope)
            .field("contract", &self.contract)
            .field("stream_events", &self.stream_events.is_some())
            .field("provider_trace", &self.provider_trace.is_some())
            .finish()
    }
}
