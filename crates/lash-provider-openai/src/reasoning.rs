use crate::driver::CompletionEndpoint;
use lash_core::llm::transport::{LlmTransportError, TransportRetryVerdict};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;

/// The dialect-independent, already-validated reasoning intent resolved from
/// `ReasoningSelection` × `ReasoningCapability`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReasoningWireIntent {
    Effort(String),
    Budget(u32),
    ToggleFalse,
}

/// Error returned when a reasoning dialect cannot represent an intent.
/// Callers must never silently omit an intent after receiving this error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReasoningEncodeError {
    pub dialect: String,
    pub detail: String,
}

/// A reasoning wire dialect that maps an intent onto an outgoing request body.
pub trait ReasoningWireEncoder: fmt::Debug + Send + Sync {
    /// Stable serialized-config, equality, and error-message identity.
    fn name(&self) -> &str;

    fn encode(
        &self,
        endpoint: CompletionEndpoint,
        intent: &ReasoningWireIntent,
        body: &mut Value,
    ) -> Result<(), ReasoningEncodeError>;
}

/// Config-facing reasoning wire dialect handle.
///
/// A format's name is its identity. Custom encoders must therefore use names
/// that are unique among all dialects a host attaches.
#[derive(Clone)]
pub struct ReasoningWireFormat(Arc<dyn ReasoningWireEncoder>);

impl ReasoningWireFormat {
    pub fn none() -> Self {
        Self(Arc::new(NoneReasoningWireEncoder))
    }

    pub fn openai() -> Self {
        Self(Arc::new(OpenAiReasoningWireEncoder))
    }

    pub fn openrouter() -> Self {
        Self(Arc::new(OpenRouterReasoningWireEncoder))
    }

    pub fn custom(encoder: Arc<dyn ReasoningWireEncoder>) -> Self {
        Self(encoder)
    }

    pub fn name(&self) -> &str {
        self.0.name()
    }

    pub fn encode(
        &self,
        endpoint: CompletionEndpoint,
        intent: &ReasoningWireIntent,
        body: &mut Value,
    ) -> Result<(), ReasoningEncodeError> {
        self.0.encode(endpoint, intent, body)
    }
}

impl fmt::Debug for ReasoningWireFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl PartialEq for ReasoningWireFormat {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for ReasoningWireFormat {}

impl Serialize for ReasoningWireFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for ReasoningWireFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "none" => Ok(Self::none()),
            "openai" => Ok(Self::openai()),
            "openrouter" => Ok(Self::openrouter()),
            _ => Err(serde::de::Error::custom(format!(
                "unknown reasoning format `{name}`; built-ins are none/openai/openrouter; custom dialects must be attached programmatically"
            ))),
        }
    }
}

/// Reasoning selections are not transmitted on endpoints using this dialect.
/// Hosts must only declare reasoning vocabularies on endpoints with a verified
/// dialect.
#[derive(Debug)]
struct NoneReasoningWireEncoder;

impl ReasoningWireEncoder for NoneReasoningWireEncoder {
    fn name(&self) -> &str {
        "none"
    }

    fn encode(
        &self,
        _endpoint: CompletionEndpoint,
        _intent: &ReasoningWireIntent,
        _body: &mut Value,
    ) -> Result<(), ReasoningEncodeError> {
        Ok(())
    }
}

#[derive(Debug)]
struct OpenAiReasoningWireEncoder;

impl ReasoningWireEncoder for OpenAiReasoningWireEncoder {
    fn name(&self) -> &str {
        "openai"
    }

    fn encode(
        &self,
        endpoint: CompletionEndpoint,
        intent: &ReasoningWireIntent,
        body: &mut Value,
    ) -> Result<(), ReasoningEncodeError> {
        match (endpoint, intent) {
            (CompletionEndpoint::ChatCompletions, ReasoningWireIntent::Effort(effort)) => {
                body["reasoning_effort"] = json!(effort);
                Ok(())
            }
            (CompletionEndpoint::Responses, ReasoningWireIntent::Effort(effort)) => {
                body["reasoning"] = json!({ "effort": effort });
                Ok(())
            }
            (CompletionEndpoint::ChatCompletions, ReasoningWireIntent::Budget(_)) => {
                Err(self.unrepresentable(
                    "top-level reasoning_effort cannot express a token budget",
                ))
            }
            (CompletionEndpoint::ChatCompletions, ReasoningWireIntent::ToggleFalse) => {
                Err(self.unrepresentable(
                    "top-level reasoning_effort cannot express enabled:false; disabling on OpenAI must use an effort name from the capability's disable encoding",
                ))
            }
            (CompletionEndpoint::Responses, ReasoningWireIntent::Budget(_)) => Err(self
                .unrepresentable("OpenAI Responses has no reasoning token-budget field")),
            (CompletionEndpoint::Responses, ReasoningWireIntent::ToggleFalse) => Err(self
                .unrepresentable("OpenAI Responses has no reasoning enabled:false field")),
        }
    }
}

impl OpenAiReasoningWireEncoder {
    fn unrepresentable(&self, detail: impl Into<String>) -> ReasoningEncodeError {
        ReasoningEncodeError {
            dialect: self.name().to_string(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug)]
struct OpenRouterReasoningWireEncoder;

impl ReasoningWireEncoder for OpenRouterReasoningWireEncoder {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn encode(
        &self,
        _endpoint: CompletionEndpoint,
        intent: &ReasoningWireIntent,
        body: &mut Value,
    ) -> Result<(), ReasoningEncodeError> {
        body["reasoning"] = match intent {
            ReasoningWireIntent::Effort(effort) => json!({ "effort": effort }),
            ReasoningWireIntent::Budget(max_tokens) => json!({ "max_tokens": max_tokens }),
            ReasoningWireIntent::ToggleFalse => json!({ "enabled": false }),
        };
        Ok(())
    }
}

pub(crate) fn reasoning_encode_transport_error(
    endpoint: CompletionEndpoint,
    intent: &ReasoningWireIntent,
    error: ReasoningEncodeError,
) -> LlmTransportError {
    LlmTransportError::new(format!(
        "reasoning dialect `{}` cannot encode {intent:?} for {endpoint:?}: {}",
        error.dialect, error.detail
    ))
    .with_code("reasoning_encoding_unrepresentable")
    .with_retry_verdict(TransportRetryVerdict::Forbidden)
}
