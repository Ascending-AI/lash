//! Host policy for server-to-client MCP requests in the reference bot.

use std::num::NonZeroUsize;

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
use rmcp::model::{PrimitiveSchema, Role};
use serde_json::{Map, Value};

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
        if request.context.server_name() != "slack_clone" {
            return Err(McpProtocolError::invalid_params(
                "the slack-clone sampling policy only trusts its bundled server",
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

/// Deterministic example UI policy for the bundled server's form and URL prompts.
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
        if request.context.server_name() != "slack_clone" {
            return Ok(CreateElicitationResult::new(ElicitationAction::Decline));
        }
        match request.params {
            CreateElicitationRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                let answer_is_required = requested_schema
                    .required
                    .as_ref()
                    .is_some_and(|required| required.len() == 1 && required[0] == "answer");
                if message != "May the Slack-clone MCP demo continue?"
                    || requested_schema.properties.len() != 1
                    || !answer_is_required
                {
                    return Ok(CreateElicitationResult::new(ElicitationAction::Decline));
                }
                let mut content = Map::new();
                for (name, property) in &requested_schema.properties {
                    match property {
                        PrimitiveSchema::String(_) if name == "answer" => {
                            content.insert(name.clone(), Value::String("yes".to_string()));
                        }
                        _ => {
                            return Err(McpProtocolError::invalid_params(
                                "the slack-clone form policy requires one string field named `answer`",
                                None,
                            ));
                        }
                    }
                }
                request
                    .accept(Value::Object(content))
                    .map_err(|error| McpProtocolError::invalid_params(error.to_string(), None))
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
        println!(
            "slack-clone-bot MCP URL elicitation completed: server={}, elicitation_id={}",
            notification.context.server_name(),
            notification.elicitation_id
        );
    }
}

/// Static workspace root supplied by the example host.
pub struct DemoRootsProvider {
    root: Root,
}

impl DemoRootsProvider {
    pub fn new(workspace: &std::path::Path) -> Self {
        Self {
            root: Root::new(format!("file://{}", workspace.display())).with_name("slack-clone"),
        }
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
        Ok(vec![self.root.clone()])
    }
}
