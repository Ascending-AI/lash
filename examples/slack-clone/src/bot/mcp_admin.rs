//! The bot's operator surface for MCP integrations.
//!
//! MCP servers are attached and detached **by the operator, never by the
//! model**: the routes here live on the bot's own HTTP surface, behind an
//! operator credential of their own (`SLACK_CLONE_ADMIN_TOKEN`), and nothing in
//! the tool catalog can reach them. The credential is separate from the
//! platform's verification token on purpose — that one authenticates event
//! envelopes the platform sends the bot, so it says *who is calling*, not *what
//! they may do*. A host that let a turn attach its own tool source would have handed the
//! model its own permission system.
//!
//! What this surface is for, beyond the demo:
//!
//! * an integration can be turned on and off while the bot is serving, which is
//!   what [`McpPluginFactory::attach_server`] and
//!   [`McpPluginFactory::detach_server`] exist for. Sessions opened after the
//!   change see the new catalog; the bot opens one per delivered event;
//! * an operator page needs both halves of "is this integration healthy" — the
//!   connection status from [`McpPluginFactory::server_statuses`] and the tools
//!   it actually advertises, which come from the pool
//!   ([`McpConnectionPool::advertised_tools_for_server`]);
//! * publishing a workspace root is a host fact the servers have to be told
//!   about, which is [`McpPluginFactory::notify_roots_changed`].

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lash_plugin_mcp::{McpConnectionPool, McpError, McpPluginFactory, McpServerStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::mcp_client::DemoRootsProvider;
use super::runtime::{BotRuntime, http_mcp_server_config};
use crate::secrets::constant_time_eq;

/// Collection route for attached MCP servers.
pub const SERVERS_PATH: &str = "/admin/mcp/servers";
/// Member route for one attached MCP server.
pub const SERVER_PATH: &str = "/admin/mcp/servers/{name}";
/// Route that publishes a workspace root and notifies connected servers.
pub const ROOTS_PATH: &str = "/admin/mcp/roots";

/// Operator handle over the bot's MCP plugin factory and published roots.
#[derive(Clone)]
pub struct McpAdmin {
    factory: Arc<McpPluginFactory>,
    roots: Arc<DemoRootsProvider>,
    token: Arc<String>,
}

impl McpAdmin {
    /// Build the operator handle from a built runtime and the bot's shared secret.
    pub fn new(runtime: &BotRuntime, token: impl Into<String>) -> Self {
        Self {
            factory: Arc::clone(&runtime.mcp),
            roots: Arc::clone(&runtime.roots),
            token: Arc::new(token.into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AttachRequest {
    /// MCP server name; its normalized form becomes the advertised tool prefix.
    name: String,
    /// Streamable-HTTP endpoint of the server.
    url: String,
    /// Bearer token installed as a static header on every request.
    token: String,
}

#[derive(Debug, Deserialize)]
struct PublishRootRequest {
    uri: String,
    #[serde(default)]
    name: Option<String>,
}

/// One server as an operator sees it: health plus what it actually offers.
#[derive(Debug, Serialize)]
struct ServerView {
    name: String,
    connected: bool,
    tool_count: usize,
    reconnect_exhausted: bool,
    last_error: Option<String>,
    /// Prefixed tool names this server currently advertises.
    tools: Vec<String>,
}

impl ServerView {
    fn new(status: McpServerStatus, tools: Vec<String>) -> Self {
        Self {
            name: status.server_name,
            connected: status.connected,
            tool_count: status.tool_count,
            reconnect_exhausted: status.reconnect_exhausted,
            last_error: status.last_error,
            tools,
        }
    }
}

/// Build the operator router. Merged into the bot's server next to the webhook.
pub fn router(admin: McpAdmin) -> Router {
    let token = Arc::clone(&admin.token);
    Router::new()
        .route(SERVERS_PATH, get(list_servers).post(attach_server))
        .route(SERVER_PATH, axum::routing::delete(detach_server))
        .route(ROOTS_PATH, post(publish_root))
        .layer(axum::middleware::from_fn_with_state(token, require_bearer))
        .with_state(admin)
}

async fn require_bearer(
    State(expected): State<Arc<String>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(value) if constant_time_eq(value, expected.as_str()) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            "the slack-clone-bot MCP admin API requires the bot's shared secret",
        )
            .into_response(),
    }
}

async fn list_servers(State(admin): State<McpAdmin>) -> Json<serde_json::Value> {
    Json(json!({ "servers": server_views(&admin) }))
}

async fn attach_server(
    State(admin): State<McpAdmin>,
    Json(request): Json<AttachRequest>,
) -> Response {
    let config = http_mcp_server_config(&request.url, &request.token);
    if let Err(error) = admin
        .factory
        .attach_server(request.name.clone(), config)
        .await
    {
        return mcp_error_response(&error);
    }
    // Attach returns as soon as the eager connect attempt settles, and a server
    // that refused the credential is *registered but disconnected* rather than
    // an error — the pool keeps retrying it. Returning the status row is what
    // tells the operator which of those two outcomes they got.
    let view = server_views(&admin)
        .into_iter()
        .find(|view| view.name == request.name);
    match view {
        Some(view) => (StatusCode::OK, Json(view)).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the attached MCP server is missing from the pool",
        )
            .into_response(),
    }
}

async fn detach_server(State(admin): State<McpAdmin>, Path(name): Path<String>) -> Response {
    match admin.factory.detach_server(&name).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "detached": name }))).into_response(),
        Err(error) => mcp_error_response(&error),
    }
}

async fn publish_root(
    State(admin): State<McpAdmin>,
    Json(request): Json<PublishRootRequest>,
) -> Response {
    let published = admin.roots.publish(request.uri, request.name).await;
    // The provider is updated first: a server that reacts to the notification by
    // re-reading `roots/list` must not be able to observe the old list.
    match admin.factory.notify_roots_changed().await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "roots": published, "notified": true })),
        )
            .into_response(),
        Err(error) => mcp_error_response(&error),
    }
}

fn server_views(admin: &McpAdmin) -> Vec<ServerView> {
    let pool: &Arc<McpConnectionPool> = admin.factory.pool();
    admin
        .factory
        .server_statuses()
        .into_iter()
        .map(|status| {
            let tools = pool
                .advertised_tools_for_server(&status.server_name)
                .into_iter()
                .map(|tool| tool.manifest.name)
                .collect();
            ServerView::new(status, tools)
        })
        .collect()
}

/// Map a typed MCP failure onto the HTTP status an operator should act on.
///
/// A configuration error is the operator's own request to fix — a name with the
/// reserved `__` separator in it, a URL that is empty — so it is a 400. Anything
/// else happened between this host and a server it does not own, which is a
/// gateway failure and not something re-sending the same request will repair.
fn mcp_error_response(error: &McpError) -> Response {
    let status = match error {
        McpError::Config(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": error.to_string() }))).into_response()
}
