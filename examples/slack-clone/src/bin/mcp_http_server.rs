//! Bundled streamable-HTTP MCP server for the slack-clone bot.
//!
//! Loopback only and deterministic: it is started next to the platform and the
//! bot by `scripts/slack-clone-dev.sh`, and the bot attaches it at runtime
//! through its own admin API rather than at boot.

use anyhow::{Context as _, Result};
use slack_clone::log_out;
use slack_clone::mcp_http_server::{ADDR_ENV, DEFAULT_TOKEN, MCP_PATH, TOKEN_ENV, router};

#[tokio::main]
async fn main() -> Result<()> {
    let addr = std::env::var(ADDR_ENV).unwrap_or_else(|_| "127.0.0.1:3042".to_string());
    let token = std::env::var(TOKEN_ENV).unwrap_or_else(|_| DEFAULT_TOKEN.to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let bound = listener.local_addr().context("resolve bound address")?;
    log_out!("slack-clone-mcp-http-server listening on http://{bound}{MCP_PATH}");
    axum::serve(listener, router(token))
        .await
        .context("serve the HTTP MCP endpoint")
}
