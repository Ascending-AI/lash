//! Read-only workspace information served over MCP stdio.
//!
//! This is intentionally a separate process from the bot. It exercises the
//! same MCP boundary a deployment-owned server would, while reading the demo
//! workspace only through its Slack-compatible HTTP API.

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CreateElicitationRequestParams, ElicitationAction, ElicitationResponseNotificationParam,
    ElicitationSchema, ErrorData, ServerCapabilities, ServerInfo,
};
use rmcp::{
    Json, Peer, RoleServer, ServerHandler, schemars::JsonSchema, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::bot::slack_api::SlackApi;
use crate::wire::methods::ChannelObject;

/// The MCP server name the bot registers this stdio server under.
pub const SERVER_NAME: &str = "slack_clone";
/// Environment variable naming the Slack-compatible API origin.
pub const API_BASE_URL_ENV: &str = "SLACK_CLONE_MCP_API_BASE_URL";
/// Environment variable carrying the bot token used for read-only API calls.
pub const BOT_TOKEN_ENV: &str = "SLACK_CLONE_MCP_BOT_TOKEN";
/// MCP name imported by the bot for the channel-summary tool.
pub const LIST_CHANNELS_SUMMARY_TOOL: &str = "mcp__slack_clone__list_channels_summary";
/// MCP name imported by the bot for the workspace-statistics tool.
pub const WORKSPACE_STATS_TOOL: &str = "mcp__slack_clone__workspace_stats";
/// MCP name imported by the bot for provider-backed sampling.
pub const SAMPLE_SUMMARY_TOOL: &str = "mcp__slack_clone__sample_summary";
/// MCP name imported by the bot for form elicitation.
pub const ELICIT_CONFIRMATION_TOOL: &str = "mcp__slack_clone__elicit_confirmation";
/// MCP name imported by the bot for URL elicitation and completion.
pub const URL_ELICITATION_TOOL: &str = "mcp__slack_clone__elicit_via_url";
/// MCP name imported by the bot for roots listing.
pub const LIST_HOST_ROOTS_TOOL: &str = "mcp__slack_clone__list_host_roots";

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ChannelSummary {
    id: String,
    name: String,
    topic: String,
    members: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ChannelSummaries {
    channels: Vec<ChannelSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct WorkspaceStats {
    channels: usize,
    active_members: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct SampleSummaryArguments {
    text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SampleSummaryResult {
    summary: String,
    model: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ElicitationResult {
    action: String,
    answer: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct UrlElicitationResult {
    action: String,
    elicitation_id: String,
    completion_notified: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HostRoot {
    uri: String,
    name: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HostRoots {
    roots: Vec<HostRoot>,
}

/// The bundled server implementation, backed by the platform HTTP API.
#[derive(Clone)]
pub struct WorkspaceMcpServer {
    api: SlackApi,
    tool_router: ToolRouter<Self>,
}

impl WorkspaceMcpServer {
    /// Construct the server for one Slack-compatible API client.
    pub fn new(api: SlackApi) -> Self {
        Self {
            api,
            tool_router: Self::tool_router(),
        }
    }

    async fn all_channels(&self) -> Result<Vec<ChannelObject>, String> {
        let mut channels = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .api
                .conversations_list(cursor.as_deref(), Some(100))
                .await
                .map_err(|error| error.to_string())?;
            channels.extend(page.channels);
            cursor = page
                .response_metadata
                .map(|metadata| metadata.next_cursor)
                .filter(|next| !next.is_empty());
            if cursor.is_none() {
                return Ok(channels);
            }
        }
    }
}

#[tool_router(router = tool_router)]
impl WorkspaceMcpServer {
    /// Return every channel's compact identity and membership summary.
    #[tool(
        name = "list_channels_summary",
        description = "List workspace channels with id, name, topic, and member count"
    )]
    async fn list_channels_summary(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
    ) -> Result<Json<ChannelSummaries>, String> {
        let channels = self
            .all_channels()
            .await?
            .into_iter()
            .map(|channel| ChannelSummary {
                id: channel.id,
                name: channel.name,
                topic: channel.topic.value,
                members: channel.num_members,
            })
            .collect();
        Ok(Json(ChannelSummaries { channels }))
    }

    /// Return aggregate workspace counts from the platform APIs.
    #[tool(
        name = "workspace_stats",
        description = "Count workspace channels and active members"
    )]
    async fn workspace_stats(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
    ) -> Result<Json<WorkspaceStats>, String> {
        let channels = self.all_channels().await?;
        let mut members = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .api
                .users_list(cursor.as_deref())
                .await
                .map_err(|error| error.to_string())?;
            members.extend(page.members.into_iter().filter(|member| !member.deleted));
            cursor = page
                .response_metadata
                .map(|metadata| metadata.next_cursor)
                .filter(|next| !next.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok(Json(WorkspaceStats {
            channels: channels.len(),
            active_members: members.len(),
        }))
    }

    /// Ask the client host to summarize text using its chosen model/provider.
    #[tool(
        name = "sample_summary",
        description = "Ask the Lash host model to summarize text during this MCP tool call"
    )]
    #[allow(deprecated, reason = "the example targets MCP 2025-11-25 sampling")]
    async fn sample_summary(
        &self,
        Parameters(arguments): Parameters<SampleSummaryArguments>,
        client: Peer<RoleServer>,
    ) -> Result<Json<SampleSummaryResult>, ErrorData> {
        let sampled = client
            .create_message(rmcp::model::CreateMessageRequestParams::new(
                vec![rmcp::model::SamplingMessage::user_text(format!(
                    "Summarize this in one short sentence: {}",
                    arguments.text
                ))],
                128,
            ))
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let summary = sampled
            .message
            .content
            .first()
            .and_then(rmcp::model::SamplingMessageContent::as_text)
            .map(|text| text.text.clone())
            .ok_or_else(|| {
                ErrorData::internal_error("host sampling returned non-text content", None)
            })?;
        Ok(Json(SampleSummaryResult {
            summary,
            model: sampled.model,
        }))
    }

    /// Ask the host to answer a structured confirmation form.
    #[tool(
        name = "elicit_confirmation",
        description = "Ask the Lash host a structured yes/no confirmation during this MCP tool call"
    )]
    async fn elicit_confirmation(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        client: Peer<RoleServer>,
    ) -> Result<Json<ElicitationResult>, ErrorData> {
        let requested_schema = ElicitationSchema::builder()
            .required_string("answer")
            .build()
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let elicited = client
            .create_elicitation(CreateElicitationRequestParams::FormElicitationParams {
                meta: None,
                message: "May the Slack-clone MCP demo continue?".to_string(),
                requested_schema,
            })
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let answer = elicited
            .content
            .as_ref()
            .and_then(|content| content.get("answer"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Ok(Json(ElicitationResult {
            action: serde_json::to_value(elicited.action)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
            answer,
        }))
    }

    /// Ask the host to open a URL flow, then notify it when that flow completes.
    #[tool(
        name = "elicit_via_url",
        description = "Exercise URL elicitation and its completion notification through the Lash host"
    )]
    async fn elicit_via_url(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        client: Peer<RoleServer>,
    ) -> Result<Json<UrlElicitationResult>, ErrorData> {
        let elicitation_id = "slack-clone-demo-url-1";
        let elicited = client
            .create_elicitation(CreateElicitationRequestParams::UrlElicitationParams {
                meta: None,
                message: "Approve the Slack-clone MCP demo in the browser".to_string(),
                url: "https://example.invalid/slack-clone/approval".to_string(),
                elicitation_id: elicitation_id.to_string(),
            })
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let completion_notified = if elicited.action == ElicitationAction::Accept {
            client
                .notify_url_elicitation_completed(ElicitationResponseNotificationParam::new(
                    elicitation_id,
                ))
                .await
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            true
        } else {
            false
        };
        Ok(Json(UrlElicitationResult {
            action: serde_json::to_value(elicited.action)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
            elicitation_id: elicitation_id.to_string(),
            completion_notified,
        }))
    }

    /// List the workspace roots supplied by the client host.
    #[tool(
        name = "list_host_roots",
        description = "List workspace roots supplied by the Lash MCP host"
    )]
    #[allow(deprecated, reason = "the example targets MCP 2025-11-25 roots")]
    async fn list_host_roots(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        client: Peer<RoleServer>,
    ) -> Result<Json<HostRoots>, ErrorData> {
        let roots = client
            .list_roots()
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?
            .roots
            .into_iter()
            .map(|root| HostRoot {
                uri: root.uri,
                name: root.name,
            })
            .collect();
        Ok(Json(HostRoots { roots }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WorkspaceMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Read-only workspace information for the slack-clone demo")
    }
}
