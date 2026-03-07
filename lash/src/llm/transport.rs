use async_trait::async_trait;

use crate::provider::Provider;

use super::types::{LlmRequest, LlmResponse, ModelSelection};

#[derive(Debug, thiserror::Error, Clone)]
#[error("{message}")]
pub struct LlmTransportError {
    pub message: String,
    pub retryable: bool,
    pub raw: Option<String>,
    pub code: Option<String>,
}

impl LlmTransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            raw: None,
            code: None,
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.raw = Some(raw.into());
        self
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

#[async_trait]
pub trait LlmTransport: Send + Sync {
    fn default_root_model(&self) -> &'static str;
    fn default_agent_model(&self, tier: &str) -> Option<ModelSelection>;
    fn requires_streaming(&self) -> bool {
        false
    }
    fn reasoning_effort_for_model(&self, _model: &str) -> Option<&'static str> {
        None
    }
    fn normalize_model(&self, model: &str) -> String;
    fn context_lookup_model(&self, model: &str) -> String;

    async fn ensure_ready(&self, provider: &mut Provider) -> Result<bool, LlmTransportError>;
    async fn complete(
        &self,
        provider: &mut Provider,
        req: LlmRequest,
    ) -> Result<LlmResponse, LlmTransportError>;
}
