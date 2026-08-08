//! Bundled stdio MCP server for the slack-clone bot.

use anyhow::{Context as _, Result};
use rmcp::{ServiceExt as _, transport::stdio};
use slack_clone::bot::slack_api::SlackApi;
use slack_clone::mcp_server::{API_BASE_URL_ENV, BOT_TOKEN_ENV, WorkspaceMcpServer};

#[tokio::main]
async fn main() -> Result<()> {
    let api_base_url = std::env::var(API_BASE_URL_ENV)
        .with_context(|| format!("{API_BASE_URL_ENV} is required"))?;
    let bot_token =
        std::env::var(BOT_TOKEN_ENV).with_context(|| format!("{BOT_TOKEN_ENV} is required"))?;
    let api = SlackApi::new(api_base_url, bot_token)?;
    WorkspaceMcpServer::new(api)
        .serve(stdio())
        .await
        .context("start MCP stdio service")?
        .waiting()
        .await
        .context("serve MCP stdio service")?;
    Ok(())
}
