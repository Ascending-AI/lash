//! Callback channels an LLM adapter reports through: streamed response events
//! and raw provider trace events. They carry no serialized shape.

use std::sync::Arc;

use super::types::LlmStreamEvent;

#[derive(Clone)]
pub struct LlmEventSender(Arc<dyn Fn(LlmStreamEvent) + Send + Sync>);

impl LlmEventSender {
    pub fn new<F>(send: F) -> Self
    where
        F: Fn(LlmStreamEvent) + Send + Sync + 'static,
    {
        Self(Arc::new(send))
    }

    pub fn send(&self, event: LlmStreamEvent) {
        (self.0)(event);
    }
}

#[derive(Clone, Debug)]
pub struct LlmProviderTraceEvent {
    pub provider: &'static str,
    pub event_name: String,
    pub raw: String,
}

const PROVIDER_REQUEST_EVENT_PREFIX: &str = "\0lash.provider_request:";

impl LlmProviderTraceEvent {
    /// Request traces share the provider trace channel with response events,
    /// while the reserved event-name prefix lets the runtime persist them as
    /// a distinct durable trace event without wrapping or changing `raw`.
    pub fn request(provider: &'static str, endpoint: &str, body: String) -> Self {
        Self {
            provider,
            event_name: format!("{PROVIDER_REQUEST_EVENT_PREFIX}{endpoint}"),
            raw: body,
        }
    }

    pub fn request_endpoint(&self) -> Option<&str> {
        self.event_name.strip_prefix(PROVIDER_REQUEST_EVENT_PREFIX)
    }
}

#[derive(Clone)]
pub struct LlmProviderTraceSender(Arc<dyn Fn(LlmProviderTraceEvent) + Send + Sync>);

impl LlmProviderTraceSender {
    pub fn new<F>(send: F) -> Self
    where
        F: Fn(LlmProviderTraceEvent) + Send + Sync + 'static,
    {
        Self(Arc::new(send))
    }

    pub fn send(&self, event: LlmProviderTraceEvent) {
        (self.0)(event);
    }
}

impl std::fmt::Debug for LlmProviderTraceSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProviderTraceSender")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for LlmEventSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmEventSender").finish_non_exhaustive()
    }
}
