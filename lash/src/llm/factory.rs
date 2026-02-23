use crate::llm::adapters::claude_oauth::ClaudeOAuthAdapter;
use crate::llm::adapters::codex_oauth::CodexOAuthAdapter;
use crate::llm::adapters::google_cloudcode::GoogleCloudCodeAdapter;
use crate::llm::adapters::openrouter::OpenRouterAdapter;
use crate::llm::transport::LlmTransport;
use crate::provider::Provider;

pub fn adapter_for(provider: &Provider) -> Box<dyn LlmTransport> {
    match provider {
        Provider::OpenRouter { .. } => Box::new(OpenRouterAdapter::new()),
        Provider::Claude { .. } => Box::new(ClaudeOAuthAdapter::new()),
        Provider::Codex { .. } => Box::new(CodexOAuthAdapter::new()),
        Provider::GoogleOAuth { .. } => Box::new(GoogleCloudCodeAdapter::new()),
    }
}
