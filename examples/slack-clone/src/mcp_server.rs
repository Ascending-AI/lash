//! Read-only workspace information served over MCP stdio.
//!
//! This is intentionally a separate process from the bot. It exercises the
//! same MCP boundary a deployment-owned server would, while reading the demo
//! workspace only through its Slack-compatible HTTP API.

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{Json, ServerHandler, schemars::JsonSchema, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::bot::slack_api::SlackApi;
use crate::wire::methods::ChannelObject;

/// Environment variable naming the Slack-compatible API origin.
pub const API_BASE_URL_ENV: &str = "SLACK_CLONE_MCP_API_BASE_URL";
/// Environment variable carrying the bot token used for read-only API calls.
pub const BOT_TOKEN_ENV: &str = "SLACK_CLONE_MCP_BOT_TOKEN";
/// MCP name imported by the bot for the channel-summary tool.
pub const LIST_CHANNELS_SUMMARY_TOOL: &str = "mcp__slack_clone__list_channels_summary";
/// MCP name imported by the bot for the workspace-statistics tool.
pub const WORKSPACE_STATS_TOOL: &str = "mcp__slack_clone__workspace_stats";

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
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WorkspaceMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Read-only workspace information for the slack-clone demo")
    }
}
