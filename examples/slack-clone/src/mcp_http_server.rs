//! Deterministic workspace MCP server served over the streamable-HTTP transport.
//!
//! The bundled [stdio server](crate::mcp_server) covers the transport a desktop
//! MCP client spawns. This one covers the other transport lash supports: a
//! server the host reaches over HTTP, authenticated by a static header the host
//! configures with
//! [`McpServerConfig::with_headers`](lash_plugin_mcp::McpServerConfig::with_headers).
//! It binds loopback only and every tool is a pure function of its own state, so
//! the example never depends on a third-party server or on network egress.
//!
//! The five tools exist to make host-side MCP policy observable:
//!
//! * `workspace_badge` returns a binary blob resource. A host that opts into
//!   [`with_binary_content_attachments`](lash_plugin_mcp::McpServerConfig::with_binary_content_attachments)
//!   persists it as a model attachment; a host that does not receives the same
//!   bytes inline. Same server, same call, two host policies.
//! * `roots_change_report` counts the `notifications/roots/list_changed`
//!   notifications this server received and re-lists the host's roots on each
//!   one, which is what makes
//!   [`notify_roots_changed`](lash_plugin_mcp::McpPluginFactory::notify_roots_changed)
//!   observable from the server's side rather than only from the host's.
//! * `elicit_unknown_prompt` asks a question the host has no standing answer
//!   for, using a field name the host *does* answer elsewhere;
//! * `elicit_pick_count` asks for a form field the host's answer book cannot
//!   satisfy, so the host declines instead of sending content that fails the
//!   server's schema.
//! * `stall` never answers, so a host can prove its call timeout fires and that
//!   its configured [`TimeoutDisconnectPolicy`](lash_plugin_mcp::TimeoutDisconnectPolicy)
//!   decides whether a timeout also drops the connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, Content, CreateElicitationRequestParams, ElicitationSchema, ErrorData,
    RawContent, RawEmbeddedResource, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::{NotificationContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{Json, Peer, ServerHandler, schemars::JsonSchema, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Environment variable naming the address the HTTP MCP server binds.
pub const ADDR_ENV: &str = "SLACK_CLONE_MCP_HTTP_ADDR";
/// Environment variable carrying the bearer token the server requires.
pub const TOKEN_ENV: &str = "SLACK_CLONE_MCP_HTTP_TOKEN";
/// Token used when [`TOKEN_ENV`] is unset. Loopback dev default, not a secret.
pub const DEFAULT_TOKEN: &str = "slack-clone-mcp-http-dev-token";
/// Path the streamable-HTTP MCP endpoint is served at.
pub const MCP_PATH: &str = "/mcp";
/// The MCP server name the bot attaches this server under.
pub const SERVER_NAME: &str = "workspace_http";

/// MCP name of the binary-content tool once the bot has attached the server.
pub const WORKSPACE_BADGE_TOOL: &str = "mcp__workspace_http__workspace_badge";
/// MCP name of the roots-notification report tool.
pub const ROOTS_CHANGE_REPORT_TOOL: &str = "mcp__workspace_http__roots_change_report";
/// MCP name of the unsatisfiable-form elicitation tool.
pub const ELICIT_PICK_COUNT_TOOL: &str = "mcp__workspace_http__elicit_pick_count";
/// MCP name of the tool that asks a question the host's answer book has not read.
pub const ELICIT_UNKNOWN_PROMPT_TOOL: &str = "mcp__workspace_http__elicit_unknown_prompt";
/// MCP name of the tool that never answers.
pub const STALL_TOOL: &str = "mcp__workspace_http__stall";

/// Exact bytes `workspace_badge` returns, before base64 encoding.
pub const BADGE_BYTES: &[u8] = b"slack-clone workspace badge v1\x00\x01\x02\x03";
/// MIME type `workspace_badge` labels its blob with.
pub const BADGE_MEDIA_TYPE: &str = "application/octet-stream";
/// `resource` URI of the badge blob.
pub const BADGE_URI: &str = "slack-clone://workspace/badge.bin";

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RootsChangeReport {
    /// How many `notifications/roots/list_changed` notifications arrived.
    notifications_seen: u64,
    /// Root names from the most recent `roots/list` this server issued.
    roots: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct PickCountResult {
    action: String,
}

/// What the server learned from the host's roots, across every session.
///
/// The streamable-HTTP transport builds one handler per MCP session, so the
/// witness is shared: a report read in one session still counts a notification
/// delivered to another.
#[derive(Default)]
struct RootsWitness {
    notifications_seen: AtomicU64,
    latest_roots: RwLock<Vec<String>>,
}

/// The HTTP-served workspace server.
#[derive(Clone)]
pub struct WorkspaceHttpMcpServer {
    roots: Arc<RootsWitness>,
    tool_router: ToolRouter<Self>,
}

impl WorkspaceHttpMcpServer {
    fn new(roots: Arc<RootsWitness>) -> Self {
        Self {
            roots,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl WorkspaceHttpMcpServer {
    /// Return the workspace badge as a binary blob resource.
    #[tool(
        name = "workspace_badge",
        description = "Return the workspace badge as a binary resource blob"
    )]
    async fn workspace_badge(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
    ) -> Result<CallToolResult, ErrorData> {
        let blob = base64_standard(BADGE_BYTES);
        Ok(CallToolResult::success(vec![
            Content::text("The workspace badge is attached."),
            Content::new(
                RawContent::Resource(RawEmbeddedResource::new(
                    ResourceContents::BlobResourceContents {
                        uri: BADGE_URI.to_string(),
                        mime_type: Some(BADGE_MEDIA_TYPE.to_string()),
                        blob,
                        meta: None,
                    },
                )),
                None,
            ),
        ]))
    }

    /// Report the roots-change notifications this server has received.
    #[tool(
        name = "roots_change_report",
        description = "Report how many host roots-changed notifications this server received and the roots it last read"
    )]
    async fn roots_change_report(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
    ) -> Result<Json<RootsChangeReport>, ErrorData> {
        Ok(Json(RootsChangeReport {
            notifications_seen: self.roots.notifications_seen.load(Ordering::SeqCst),
            roots: self.roots.latest_roots.read().await.clone(),
        }))
    }

    /// Ask the host for a numeric form field its answer book cannot supply.
    #[tool(
        name = "elicit_pick_count",
        description = "Ask the host to fill an integer form field and report the action it chose"
    )]
    async fn elicit_pick_count(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        client: Peer<RoleServer>,
    ) -> Result<Json<PickCountResult>, ErrorData> {
        let requested_schema = ElicitationSchema::builder()
            .required_integer("count", 1, 9)
            .build()
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let elicited = client
            .create_elicitation(CreateElicitationRequestParams::FormElicitationParams {
                meta: None,
                message: "How many workspace badges should the demo render?".to_string(),
                requested_schema,
            })
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(Json(PickCountResult {
            action: serde_json::to_value(elicited.action)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
        }))
    }

    /// Ask a question this host has no standing answer for.
    ///
    /// The field name is one the host does answer elsewhere. Only the prompt
    /// differs, which is the whole point: a host whose consent is keyed by field
    /// name alone would answer this too.
    #[tool(
        name = "elicit_unknown_prompt",
        description = "Ask the host a question it has no standing answer for, and report the action it chose"
    )]
    async fn elicit_unknown_prompt(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        client: Peer<RoleServer>,
    ) -> Result<Json<PickCountResult>, ErrorData> {
        let requested_schema = ElicitationSchema::builder()
            .required_string("answer")
            .build()
            .map_err(|error| ErrorData::internal_error(error, None))?;
        let elicited = client
            .create_elicitation(CreateElicitationRequestParams::FormElicitationParams {
                meta: None,
                message: "May this server post to the workspace on your behalf?".to_string(),
                requested_schema,
            })
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(Json(PickCountResult {
            action: serde_json::to_value(elicited.action)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
        }))
    }

    /// Never answer, so the caller's own call timeout decides the outcome.
    #[tool(
        name = "stall",
        description = "Never return, so the calling host's configured call timeout is what ends the call"
    )]
    async fn stall(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
    ) -> Result<CallToolResult, ErrorData> {
        std::future::pending::<()>().await;
        unreachable!("the stall tool never resolves")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WorkspaceHttpMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Deterministic HTTP-served workspace tools for the slack-clone demo")
    }

    #[allow(
        deprecated,
        reason = "the example targets MCP 2025-11-25 roots notifications"
    )]
    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        // Re-read the host's roots on the notification rather than trusting the
        // list captured at connect: that round trip is the whole point of the
        // notification, and reading it here is what lets `roots_change_report`
        // prove the host's call reached this server.
        let names = match context.peer.list_roots().await {
            Ok(result) => result
                .roots
                .into_iter()
                .map(|root| root.name.unwrap_or(root.uri))
                .collect(),
            Err(error) => {
                eprintln!("slack-clone HTTP MCP server could not re-list roots: {error}");
                Vec::new()
            }
        };
        *self.roots.latest_roots.write().await = names;
        self.roots.notifications_seen.fetch_add(1, Ordering::SeqCst);
    }
}

fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Build the server's HTTP router: bearer auth in front of the MCP endpoint.
///
/// The auth layer is the reason this example can prove
/// [`McpServerConfig::with_headers`](lash_plugin_mcp::McpServerConfig::with_headers)
/// does something: a host that omits or misspells the header never completes the
/// MCP handshake, and its pool reports the server as disconnected with the
/// rejection as `last_error`.
pub fn router(token: String) -> Router {
    let roots = Arc::new(RootsWitness::default());
    let service = StreamableHttpService::new(
        move || Ok(WorkspaceHttpMcpServer::new(Arc::clone(&roots))),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .nest_service(MCP_PATH, service)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(token),
            require_bearer,
        ))
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
        Some(value) if crate::secrets::constant_time_eq(value, expected.as_str()) => {
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            "slack-clone HTTP MCP server requires the configured bearer token",
        )
            .into_response(),
    }
}
