//! Host policy for server-to-client MCP requests in the workbench.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use lash::LlmProfileMetadata;
use lash::direct::{
    DirectLlmClient, DirectMessage, DirectPart, DirectRequest, DirectRole, LlmTerminalReason,
    NonNegativeFiniteF64,
};
use lash::mcp::{
    McpElicitationHandler, McpElicitationModes, McpElicitationParams, McpElicitationRequest,
    McpElicitationResult, McpHostRefusal, McpRole, McpRoot, McpRootsProvider, McpRootsRequest,
    McpSamplingContent, McpSamplingHandler, McpSamplingRequest, McpSamplingResult, McpStopReason,
};
use lash::provider::{LlmResponse, ProviderHandle};
use serde_json::{Map, Value};

/// The workbench's direct provider-backed implementation of MCP sampling.
pub struct DemoSamplingHandler {
    provider: ProviderHandle,
    model: LlmProfileMetadata,
}

impl DemoSamplingHandler {
    pub fn new(provider: ProviderHandle, model: LlmProfileMetadata) -> Self {
        Self { provider, model }
    }
}

#[async_trait]
impl McpSamplingHandler for DemoSamplingHandler {
    async fn create_message(
        &self,
        request: McpSamplingRequest<'_>,
    ) -> Result<McpSamplingResult, McpHostRefusal> {
        let params = request.params;
        if request.context.server_name() != "workspace_stdio" {
            return Err(McpHostRefusal::invalid_params(
                "the workbench sampling policy only trusts its bundled stdio server",
            ));
        }
        if params.tools.is_some() || params.tool_choice.is_some() {
            return Err(McpHostRefusal::invalid_params(
                "the workbench host exposes basic MCP sampling without tool use",
            ));
        }
        if params.include_context.is_some() {
            return Err(McpHostRefusal::invalid_params(
                "the workbench host does not expose MCP context inclusion",
            ));
        }

        let mut messages = Vec::new();
        for message in &params.messages {
            let role = match message.role {
                McpRole::User => DirectRole::User,
                McpRole::Assistant => DirectRole::Assistant,
            };
            let mut parts = Vec::new();
            for content in &message.content {
                match content {
                    McpSamplingContent::Text { text } => {
                        parts.push(DirectPart::Text(text.clone()));
                    }
                    _ => {
                        return Err(McpHostRefusal::invalid_params(
                            "the workbench sampling demo accepts text messages only",
                        ));
                    }
                }
            }
            messages.push(DirectMessage { role, parts });
        }

        let mut direct = DirectRequest::text("");
        direct.messages = messages;
        direct.generation.output_token_cap = NonZeroUsize::new(params.max_tokens as usize);
        direct.generation.stop_sequences = params.stop_sequences.clone();
        direct.generation.temperature = params
            .temperature
            .map(f64::from)
            .map(NonNegativeFiniteF64::new)
            .transpose()
            .map_err(|error| McpHostRefusal::invalid_params(error.to_string()))?;

        let mut client = DirectLlmClient::new(
            self.provider.clone(),
            lash::LlmProfileConfig::new(lash::RecordedLlmProfile::mint(
                lash::LlmProfileKey::new("mcp-sampling"),
                self.model.clone(),
            )), lash::ExecutionBudgets::recommended()
        )
        // The server's system prompt is this host-owned call's own
        // instruction text.
        .with_instructions(params.system_prompt.as_deref().map(Arc::from));
        let result = tokio::select! {
            result = client.complete(direct) => {
                result.map_err(|error| McpHostRefusal::internal(error.to_string()))?
            }
            () = request.context.cancellation_token().cancelled() => {
                return Err(McpHostRefusal::internal("the MCP sampling request was cancelled"));
            }
        };
        let stop_reason = match result.terminal_reason {
            LlmTerminalReason::OutputLimit => McpStopReason::MaxTokens,
            _ => McpStopReason::EndTurn,
        };
        Ok(McpSamplingResult::text(
            LlmResponse::full_text(&result),
            self.model.wire_model.clone(),
        )
        .with_stop_reason(stop_reason))
    }
}

/// MCP servers whose prompts this host is willing to answer at all.
///
/// Elicitation is the server asking the *host* to act, so the trust decision is
/// the host's and it is made by server name, before the prompt is read.
const TRUSTED_SERVERS: [&str; 2] = ["workspace_stdio", "workspace_http"];

/// The answers this host will give an MCP form without a human present.
///
/// Keyed by the exact prompt *and* the field, never by the field alone.
/// Elicitation is a consent primitive: a book keyed only by field name would
/// answer `answer: yes` to any question a trusted server thought to phrase with
/// that field, which is blind consent wearing a policy's clothes. Standing
/// consent is only meaningful for a question the host has actually read.
fn answer_book(prompt: &str, field: &str) -> Option<Value> {
    match (prompt, field) {
        ("May the workbench MCP demo continue?", "answer") => {
            Some(Value::String("yes".to_string()))
        }
        _ => None,
    }
}

/// Deterministic example UI policy for the bundled servers' form and URL prompts.
pub struct DemoElicitationHandler;

#[async_trait]
impl McpElicitationHandler for DemoElicitationHandler {
    fn modes(&self) -> McpElicitationModes {
        McpElicitationModes::FormAndUrl
    }

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<McpElicitationResult, McpHostRefusal> {
        if request.context.cancellation_token().is_cancelled() {
            return Err(McpHostRefusal::internal(
                "the MCP elicitation request was cancelled",
            ));
        }
        if !TRUSTED_SERVERS.contains(&request.context.server_name()) {
            return Ok(McpElicitationResult::Decline);
        }
        match request.params {
            McpElicitationParams::Form {
                message,
                requested_schema,
                ..
            } => {
                // The fixture runs without a human, so the host answers
                // from a fixed book keyed by the prompt and the field. A
                // question the book has not read is declined, never guessed.
                let mut content = Map::new();
                for name in requested_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|properties| properties.keys())
                {
                    let Some(answer) = answer_book(message, name) else {
                        eprintln!(
                            "workbench has no answer on file for MCP form field `{name}` \
                             of prompt {message:?}; declining"
                        );
                        return Ok(McpElicitationResult::Decline);
                    };
                    content.insert(name.clone(), answer);
                }
                // The book is keyed by name, not by type, so its answer can still be the wrong
                // shape for this server's schema.
                match request.accept(Value::Object(content)) {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        eprintln!(
                            "workbench declined an MCP form its answer book cannot satisfy: {}",
                            error.message()
                        );
                        Ok(McpElicitationResult::Decline)
                    }
                }
            }
            McpElicitationParams::Url { message, url, .. } => {
                let approved = message == "Approve the workbench MCP demo in the browser"
                    && url == "https://example.invalid/workbench/approval";
                Ok(if approved {
                    McpElicitationResult::Accept { content: None }
                } else {
                    McpElicitationResult::Decline
                })
            }
            _ => Ok(McpElicitationResult::Decline),
        }
    }
}

/// Workspace roots supplied by the example host.
pub struct DemoRootsProvider {
    roots: Vec<McpRoot>,
}

impl DemoRootsProvider {
    pub fn new(workspace: &std::path::Path) -> Self {
        Self {
            roots: vec![
                McpRoot::new(format!("file://{}", workspace.display())).with_name("workbench"),
            ],
        }
    }
}

#[async_trait]
impl McpRootsProvider for DemoRootsProvider {
    async fn list_roots(
        &self,
        request: McpRootsRequest<'_>,
    ) -> Result<Vec<McpRoot>, McpHostRefusal> {
        if request.context.cancellation_token().is_cancelled() {
            return Err(McpHostRefusal::internal(
                "the MCP roots request was cancelled",
            ));
        }
        Ok(self.roots.clone())
    }
}
