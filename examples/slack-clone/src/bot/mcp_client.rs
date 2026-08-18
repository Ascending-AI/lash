//! Host policy for server-to-client MCP requests in the reference bot.

use std::num::NonZeroUsize;

use crate::log_out;
use async_trait::async_trait;
use lash::ModelSpec;
use lash::direct::{
    DirectLlmClient, DirectMessage, DirectPart, DirectRequest, DirectRole, LlmTerminalReason,
    NonNegativeFiniteF64,
};
use lash::provider::ProviderHandle;
use lash_plugin_mcp::{
    CreateElicitationRequestParams, CreateElicitationResult, CreateMessageResult,
    ElicitationAction, ElicitationCapability, FormElicitationCapability, McpElicitationHandler,
    McpElicitationRequest, McpProtocolError, McpRootsProvider, McpRootsRequest, McpSamplingHandler,
    McpSamplingRequest, McpUrlElicitationComplete, Root, SamplingMessage, SamplingMessageContent,
    UrlElicitationCapability,
};
use rmcp::model::Role;
use serde_json::{Map, Value};
use tokio::sync::RwLock;

/// The bot's direct provider-backed implementation of MCP sampling.
pub struct DemoSamplingHandler {
    provider: ProviderHandle,
    model: ModelSpec,
}

impl DemoSamplingHandler {
    pub fn new(provider: ProviderHandle, model: ModelSpec) -> Self {
        Self { provider, model }
    }
}

#[async_trait]
impl McpSamplingHandler for DemoSamplingHandler {
    async fn create_message(
        &self,
        request: McpSamplingRequest<'_>,
    ) -> Result<CreateMessageResult, McpProtocolError> {
        let params = request.params;
        if request.context.server_name() != crate::mcp_server::SERVER_NAME {
            return Err(McpProtocolError::invalid_params(
                "the slack-clone sampling policy only trusts its bundled stdio server",
                None,
            ));
        }
        if params.tools.is_some() || params.tool_choice.is_some() {
            return Err(McpProtocolError::invalid_params(
                "the slack-clone host exposes basic MCP sampling without tool use",
                None,
            ));
        }
        if params.include_context.is_some() {
            return Err(McpProtocolError::invalid_params(
                "the slack-clone host does not expose MCP context inclusion",
                None,
            ));
        }

        let mut messages = Vec::new();
        if let Some(system_prompt) = &params.system_prompt {
            messages.push(DirectMessage {
                role: DirectRole::System,
                parts: vec![DirectPart::Text(system_prompt.clone())],
            });
        }
        for message in &params.messages {
            let role = match message.role {
                Role::User => DirectRole::User,
                Role::Assistant => DirectRole::Assistant,
            };
            let mut parts = Vec::new();
            for content in message.content.iter() {
                match content {
                    SamplingMessageContent::Text(text) => {
                        parts.push(DirectPart::Text(text.text.clone()));
                    }
                    _ => {
                        return Err(McpProtocolError::invalid_params(
                            "the slack-clone sampling demo accepts text messages only",
                            None,
                        ));
                    }
                }
            }
            messages.push(DirectMessage { role, parts });
        }

        let mut direct = DirectRequest::text(&self.model.id, "");
        direct.model_variant = self.model.variant.clone();
        direct.model_capability = self.model.capability.clone();
        direct.messages = messages;
        direct.generation.output_token_cap = NonZeroUsize::new(params.max_tokens as usize);
        direct.generation.stop_sequences = params.stop_sequences.clone().unwrap_or_default();
        direct.generation.temperature = params
            .temperature
            .map(f64::from)
            .map(NonNegativeFiniteF64::new)
            .transpose()
            .map_err(|error| McpProtocolError::invalid_params(error.to_string(), None))?;

        let mut client = DirectLlmClient::new(self.provider.clone());
        let result = tokio::select! {
            result = client.complete(direct) => {
                result.map_err(|error| McpProtocolError::internal_error(error.to_string(), None))?
            }
            () = request.context.cancellation_token().cancelled() => {
                return Err(McpProtocolError::internal_error(
                    "the MCP sampling request was cancelled",
                    None,
                ));
            }
        };
        let stop_reason = match result.terminal_reason {
            LlmTerminalReason::OutputLimit => CreateMessageResult::STOP_REASON_END_MAX_TOKEN,
            _ => CreateMessageResult::STOP_REASON_END_TURN,
        };
        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text(result.full_text.clone()),
            self.model.id.clone(),
        )
        .with_stop_reason(stop_reason))
    }
}

/// MCP servers whose prompts this host is willing to answer at all.
///
/// Elicitation is the server asking the *host* to act, so the trust decision is
/// the host's and it is made by server name, before the prompt is read.
const TRUSTED_SERVERS: [&str; 2] = [
    crate::mcp_server::SERVER_NAME,
    crate::mcp_http_server::SERVER_NAME,
];

/// The answers this host will give an MCP form without a human present.
///
/// Keyed by the exact prompt *and* the field, never by the field alone.
/// Elicitation is a consent primitive: a book keyed only by field name would
/// answer `answer: yes` to any question a trusted server thought to phrase with
/// that field, which is blind consent wearing a policy's clothes. Standing
/// consent is only meaningful for a question the host has actually read.
fn answer_book(prompt: &str, field: &str) -> Option<Value> {
    match (prompt, field) {
        ("May the Slack-clone MCP demo continue?", "answer") => {
            Some(Value::String("yes".to_string()))
        }
        ("How many workspace badges should the demo render?", "count") => {
            Some(Value::String("one".to_string()))
        }
        _ => None,
    }
}

/// Deterministic example UI policy for the bundled servers' form and URL prompts.
pub struct DemoElicitationHandler;

#[async_trait]
impl McpElicitationHandler for DemoElicitationHandler {
    fn capability(&self) -> ElicitationCapability {
        ElicitationCapability {
            form: Some(FormElicitationCapability::default()),
            url: Some(UrlElicitationCapability::default()),
        }
    }

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<CreateElicitationResult, McpProtocolError> {
        if request.context.cancellation_token().is_cancelled() {
            return Err(McpProtocolError::internal_error(
                "the MCP elicitation request was cancelled",
                None,
            ));
        }
        if !TRUSTED_SERVERS.contains(&request.context.server_name()) {
            return Ok(CreateElicitationResult::new(ElicitationAction::Decline));
        }
        match request.params {
            CreateElicitationRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                // No human is watching a bot's MCP call, so the host answers
                // from a fixed book keyed by the prompt and the field. A
                // question the book has not read is declined, never guessed.
                let mut content = Map::new();
                for name in requested_schema.properties.keys() {
                    let Some(answer) = answer_book(message, name) else {
                        log_out!(
                            "slack-clone-bot has no answer on file for MCP form field `{name}` \
                             of prompt {message:?}; declining"
                        );
                        return Ok(CreateElicitationResult::new(ElicitationAction::Decline));
                    };
                    content.insert(name.clone(), answer);
                }
                // The book is keyed by name, not by type, so its answer can
                // still be the wrong shape for this server's schema. Validate
                // before sending: a decline is a legitimate MCP answer, while
                // content that fails the schema the server just published is a
                // protocol violation the host would be committing knowingly.
                match request.accept(Value::Object(content)) {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        log_out!(
                            "slack-clone-bot declined an MCP form its answer book cannot satisfy: {}",
                            error.message()
                        );
                        Ok(CreateElicitationResult::new(ElicitationAction::Decline))
                    }
                }
            }
            CreateElicitationRequestParams::UrlElicitationParams {
                message,
                url,
                elicitation_id,
                ..
            } => {
                let approved = message == "Approve the Slack-clone MCP demo in the browser"
                    && url == "https://example.invalid/slack-clone/approval"
                    && !elicitation_id.is_empty();
                Ok(CreateElicitationResult::new(if approved {
                    ElicitationAction::Accept
                } else {
                    ElicitationAction::Decline
                }))
            }
        }
    }

    async fn url_elicitation_complete(&self, notification: McpUrlElicitationComplete<'_>) {
        log_out!(
            "slack-clone-bot MCP URL elicitation completed: server={}, elicitation_id={}",
            notification.context.server_name(),
            notification.elicitation_id
        );
    }
}

/// Workspace roots supplied by the example host.
///
/// The list is mutable because roots are a live host fact, not a boot-time
/// constant: an operator can publish another root while the bot runs, and the
/// host then tells connected servers to re-read the list with
/// [`McpPluginFactory::notify_roots_changed`](lash_plugin_mcp::McpPluginFactory::notify_roots_changed).
/// The provider is the single source both the notification and the servers'
/// subsequent `roots/list` calls read.
pub struct DemoRootsProvider {
    roots: RwLock<Vec<Root>>,
}

impl DemoRootsProvider {
    pub fn new(workspace: &std::path::Path) -> Self {
        Self {
            roots: RwLock::new(vec![
                Root::new(format!("file://{}", workspace.display())).with_name("slack-clone"),
            ]),
        }
    }

    /// Publish another root, replacing any root already at the same URI.
    ///
    /// Returns the number of roots the host now publishes.
    pub async fn publish(&self, uri: String, name: Option<String>) -> usize {
        let mut roots = self.roots.write().await;
        roots.retain(|root| root.uri != uri);
        let mut root = Root::new(uri);
        if let Some(name) = name {
            root = root.with_name(name);
        }
        roots.push(root);
        roots.len()
    }

    /// The roots this host currently publishes.
    pub async fn published(&self) -> Vec<Root> {
        self.roots.read().await.clone()
    }
}

#[async_trait]
impl McpRootsProvider for DemoRootsProvider {
    async fn list_roots(
        &self,
        request: McpRootsRequest<'_>,
    ) -> Result<Vec<Root>, McpProtocolError> {
        if request.context.cancellation_token().is_cancelled() {
            return Err(McpProtocolError::internal_error(
                "the MCP roots request was cancelled",
                None,
            ));
        }
        Ok(self.published().await)
    }
}
