//! Deterministic workbench MCP peer, usable over stdio or streamable HTTP.
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, Content, CreateElicitationRequestParams, ElicitationAction,
    ElicitationResponseNotificationParam, ElicitationSchema, ErrorData, RawContent,
    RawEmbeddedResource, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::{
    Json, Peer, RoleServer, ServerHandler, schemars::JsonSchema, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

pub const BADGE_BYTES: &[u8] = b"workbench workspace badge v1\x00\x01\x02\x03";
pub const TOKEN: &str = "workbench-mcp-fixture-token";
#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
struct NoArguments {}

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

#[derive(Clone)]
pub struct WorkbenchMcpServer {
    tool_router: ToolRouter<Self>,
}
impl WorkbenchMcpServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}
#[tool_router(router = tool_router)]
impl WorkbenchMcpServer {
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
                message: "May the workbench MCP demo continue?".to_string(),
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
        let elicitation_id = "workbench-demo-url-1";
        let elicited = client
            .create_elicitation(CreateElicitationRequestParams::UrlElicitationParams {
                meta: None,
                message: "Approve the workbench MCP demo in the browser".to_string(),
                url: "https://example.invalid/workbench/approval".to_string(),
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
    #[tool(
        name = "workspace_badge",
        description = "Return the workspace badge as a binary resource blob"
    )]
    async fn workspace_badge(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(root) = std::env::var_os("AGENT_WORKBENCH_MCP_GATE_DIR") {
            use std::io::Write as _;
            let root = std::path::PathBuf::from(root);
            std::fs::create_dir_all(&root).map_err(internal)?;
            // Every execution of the body, for a caller counting them.
            let mut calls = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join("badge-calls"))
                .map_err(internal)?;
            writeln!(calls, "{}", std::process::id()).map_err(internal)?;
            calls.sync_all().map_err(internal)?;
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(root.join("badge-claimed"))
            {
                Ok(claim) => {
                    claim.sync_all().map_err(internal)?;
                    // Publish a complete PID barrier; an empty file is not body-entry proof.
                    let mut file = std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(root.join("badge-entered.tmp"))
                        .map_err(internal)?;
                    writeln!(file, "{}", std::process::id()).map_err(internal)?;
                    file.sync_all().map_err(internal)?;
                    std::fs::rename(root.join("badge-entered.tmp"), root.join("badge-entered"))
                        .map_err(internal)?;
                    std::fs::File::open(&root)
                        .and_then(|directory| directory.sync_all())
                        .map_err(internal)?;
                    std::future::pending::<()>().await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(internal(error)),
            }
        }
        use base64::Engine as _;
        Ok(CallToolResult::success(vec![
            Content::text("The workspace badge is attached."),
            Content::new(
                RawContent::Resource(RawEmbeddedResource::new(
                    ResourceContents::BlobResourceContents {
                        uri: "workbench://workspace/badge.bin".into(),
                        mime_type: Some("application/octet-stream".into()),
                        blob: base64::engine::general_purpose::STANDARD.encode(BADGE_BYTES),
                        meta: None,
                    },
                )),
                None,
            ),
        ]))
    }
}
fn internal(error: std::io::Error) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for WorkbenchMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Deterministic workbench fixture tools")
    }
}

use anyhow::{Context as _, Result};
use rmcp::ServiceExt as _;
pub async fn serve() -> Result<()> {
    if std::env::args().nth(2).as_deref() == Some("http") {
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };
        let address = std::env::var("AGENT_WORKBENCH_MCP_ADDR")?;
        let listener = tokio::net::TcpListener::bind(&address).await?;
        let service = StreamableHttpService::new(
            || Ok(WorkbenchMcpServer::new()),
            std::sync::Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let app = axum::Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(auth));
        axum::serve(listener, app)
            .await
            .context("serve workbench MCP HTTP fixture")
    } else {
        if let Ok(path) = std::env::var("AGENT_WORKBENCH_MCP_STDIO_PID") {
            std::fs::write(path, std::process::id().to_string())?;
        }
        WorkbenchMcpServer::new()
            .serve(rmcp::transport::stdio())
            .await?
            .waiting()
            .await
            .context("serve workbench MCP stdio fixture")?;
        Ok(())
    }
}
async fn auth(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;
    if request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {}", TOKEN))
    {
        next.run(request).await
    } else {
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}
