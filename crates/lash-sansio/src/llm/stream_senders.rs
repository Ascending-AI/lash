//! Callback channels an LLM adapter reports through: streamed response events
//! and raw provider trace events. They carry no serialized shape.

use std::sync::Arc;

use super::types::LlmStreamEvent;

type ScopedEventCallback =
    dyn Fn(LlmStreamEvent, Option<&super::types::LlmRequestScope>) + Send + Sync;

#[derive(Clone)]
pub struct LlmEventSender {
    send: Arc<ScopedEventCallback>,
    scope: Option<Arc<super::types::LlmRequestScope>>,
}

impl LlmEventSender {
    pub fn new<F>(send: F) -> Self
    where
        F: Fn(LlmStreamEvent) + Send + Sync + 'static,
    {
        Self::new_scoped(move |event, _| send(event))
    }

    /// Observe the actual request scope bound by the provider attempt.
    pub fn new_scoped<F>(send: F) -> Self
    where
        F: Fn(LlmStreamEvent, Option<&super::types::LlmRequestScope>) + Send + Sync + 'static,
    {
        Self {
            send: Arc::new(send),
            scope: None,
        }
    }

    /// Bind this copy to one attempt; previously bound copies keep their scope.
    pub fn with_scope(&self, scope: super::types::LlmRequestScope) -> Self {
        Self {
            send: Arc::clone(&self.send),
            scope: Some(Arc::new(scope)),
        }
    }

    /// Transform events without dropping the scope bound by an upstream attempt.
    pub fn map<F>(&self, map: F) -> Self
    where
        F: Fn(LlmStreamEvent) -> LlmStreamEvent + Send + Sync + 'static,
    {
        let send = Arc::clone(&self.send);
        Self {
            send: Arc::new(move |event, scope| send(map(event), scope)),
            scope: self.scope.clone(),
        }
    }

    pub fn send(&self, event: LlmStreamEvent) {
        (self.send)(event, self.scope.as_deref());
    }
}

/// Which way a raw provider observation travelled, with the name its side
/// of the wire gives it.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "direction", rename_all = "snake_case")]
pub enum LlmProviderTraceDirection {
    /// The serialized request Lash sent to `endpoint`.
    Request { endpoint: String },
    /// One event of the provider's response. `event_name` is the vendor's
    /// own spelling, kept as raw evidence and never interpreted.
    Response { event_name: String },
}

#[derive(Clone, Debug)]
pub struct LlmProviderTraceEvent {
    pub provider: &'static str,
    pub direction: LlmProviderTraceDirection,
    pub raw: String,
}

impl LlmProviderTraceEvent {
    pub fn request(provider: &'static str, endpoint: &str, body: String) -> Self {
        Self {
            provider,
            direction: LlmProviderTraceDirection::Request {
                endpoint: endpoint.to_string(),
            },
            raw: body,
        }
    }

    pub fn response(provider: &'static str, event_name: String, raw: String) -> Self {
        Self {
            provider,
            direction: LlmProviderTraceDirection::Response { event_name },
            raw,
        }
    }

    pub fn request_endpoint(&self) -> Option<&str> {
        match &self.direction {
            LlmProviderTraceDirection::Request { endpoint } => Some(endpoint),
            LlmProviderTraceDirection::Response { .. } => None,
        }
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
