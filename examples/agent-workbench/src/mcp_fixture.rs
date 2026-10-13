//! Deterministic workbench MCP peer, usable over stdio or streamable HTTP.
#![allow(
    deprecated,
    reason = "this fixture demonstrates existing sampling and roots capabilities"
)]

use rmcp::handler::server::tool::InputResponses;
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::{NotificationContext, RequestContext, Service};
use rmcp::{
    Json, RoleServer, ServerHandler, schemars::JsonSchema, tool, tool_handler, tool_router,
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
    #[allow(deprecated, reason = "MCP retains deprecated sampling")]
    async fn sample_summary(
        &self,
        Parameters(SampleSummaryArguments { text: _text }): Parameters<SampleSummaryArguments>,
        responses: InputResponses,
    ) -> Result<Json<SampleSummaryResult>, ErrorData> {
        let sampled: rmcp::model::CreateMessageResult = fixture_input(responses)?;
        let summary = sampled
            .message
            .content
            .first()
            .and_then(rmcp::model::SamplingMessageContentBlock::as_text)
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
        responses: InputResponses,
    ) -> Result<Json<ElicitationResult>, ErrorData> {
        let elicited: rmcp::model::ElicitResult = fixture_input(responses)?;
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

    /// Ask the host to complete a URL flow through a multi round-trip request.
    #[tool(
        name = "elicit_via_url",
        description = "Exercise URL elicitation through a multi round-trip request"
    )]
    async fn elicit_via_url(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        responses: InputResponses,
    ) -> Result<Json<UrlElicitationResult>, ErrorData> {
        let elicited: rmcp::model::ElicitResult = fixture_input(responses)?;
        Ok(Json(UrlElicitationResult {
            action: serde_json::to_value(elicited.action)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string()),
        }))
    }

    /// List the workspace roots supplied by the client host.
    #[tool(
        name = "list_host_roots",
        description = "List workspace roots supplied by the Lash MCP host"
    )]
    #[allow(deprecated, reason = "MCP retains deprecated roots")]
    async fn list_host_roots(
        &self,
        Parameters(NoArguments {}): Parameters<NoArguments>,
        responses: InputResponses,
    ) -> Result<Json<HostRoots>, ErrorData> {
        let roots: rmcp::model::ListRootsResult = fixture_input(responses)?;
        let roots = roots
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
            ContentBlock::text("The workspace badge is attached."),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "workbench://workspace/badge.bin".into(),
                mime_type: Some("application/octet-stream".into()),
                blob: base64::engine::general_purpose::STANDARD.encode(BADGE_BYTES),
                meta: None,
            }),
        ]))
    }
}
fn fixture_input<T: serde::de::DeserializeOwned>(
    responses: InputResponses,
) -> Result<T, ErrorData> {
    let value = responses
        .0
        .and_then(|mut responses| responses.remove("host"))
        .ok_or_else(|| ErrorData::invalid_params("missing host input response", None))?;
    serde_json::from_value(value)
        .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
}

/// The SDK custom-result seam is needed for URL MRTR because rmcp 3.5.1's
/// typed input union still requires the removed elicitationId field.
#[derive(Clone)]
struct WorkbenchMcpService(WorkbenchMcpServer);

impl Service<RoleServer> for WorkbenchMcpService {
    fn get_info(&self) -> ServerConfig {
        ServerHandler::get_info(&self.0)
    }
    fn supported_protocol_versions(
        &self,
    ) -> std::borrow::Cow<'static, [rmcp::model::ProtocolVersion]> {
        vec![rmcp::model::ProtocolVersion::V_2026_07_28].into()
    }
    async fn handle_request(
        &self,
        request: rmcp::model::ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ServerResult, ErrorData> {
        if let rmcp::model::ClientRequest::CallToolRequest(call) = &request
            && call.params.input_responses.is_none()
        {
            let input = match call.params.name.as_ref() {
                "sample_summary" => Some(
                    serde_json::json!({ "method":"sampling/createMessage", "params":{
                        "messages":[{"role":"user","content":{"type":"text","text":format!("Summarize this in one short sentence: {}", call.params.arguments.as_ref().and_then(|args| args.get("text")).and_then(serde_json::Value::as_str).unwrap_or_default())}}], "maxTokens":128
                    }}),
                ),
                "elicit_confirmation" => Some(
                    serde_json::json!({ "method":"elicitation/create", "params":{
                        "mode":"form", "message":"May the workbench MCP demo continue?", "requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}
                    }}),
                ),
                "elicit_via_url" => Some(
                    serde_json::json!({ "method":"elicitation/create", "params":{
                        "mode":"url", "message":"Approve the workbench MCP demo in the browser", "url":"https://example.invalid/workbench/approval"
                    }}),
                ),
                "list_host_roots" => Some(serde_json::json!({ "method":"roots/list" })),
                _ => None,
            };
            if let Some(input) = input {
                return Ok(rmcp::model::ServerResult::CustomResult(
                    rmcp::model::CustomResult(serde_json::json!({
                        "resultType":"input_required", "inputRequests":{"host":input}, "requestState":"workbench-host-input"
                    })),
                ));
            }
        }
        Service::handle_request(&self.0, request, context).await
    }
    async fn handle_notification(
        &self,
        notification: rmcp::model::ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Service::handle_notification(&self.0, notification, context).await
    }
}

fn internal(error: std::io::Error) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}
#[tool_handler(router = self.tool_router)]
impl ServerHandler for WorkbenchMcpServer {
    fn supported_protocol_versions(
        &self,
    ) -> std::borrow::Cow<'static, [rmcp::model::ProtocolVersion]> {
        vec![rmcp::model::ProtocolVersion::V_2026_07_28].into()
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Deterministic workbench fixture tools")
    }
}

async fn fixture_http_request(
    body: axum::body::Bytes,
) -> Result<axum::Json<serde_json::Value>, axum::http::StatusCode> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    // The fixture's HTTP endpoint is stateless. Each POST carries the client
    // context, and MRTR supplies the state on the next independent POST.
    let (server_io, mut client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        if let Ok(running) = WorkbenchMcpService(WorkbenchMcpServer::new())
            .serve(server_io)
            .await
        {
            let _ = running.waiting().await;
        }
    });
    let result = async {
        client_io
            .write_all(&body)
            .await
            .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
        client_io
            .write_all(b"\n")
            .await
            .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
        let mut response = String::new();
        tokio::io::BufReader::new(client_io)
            .read_line(&mut response)
            .await
            .map_err(|_| axum::http::StatusCode::BAD_GATEWAY)?;
        serde_json::from_str(&response)
            .map(axum::Json)
            .map_err(|_| axum::http::StatusCode::BAD_GATEWAY)
    }
    .await;
    server.abort();
    let _ = server.await;
    result
}

use anyhow::{Context as _, Result};
use rmcp::ServiceExt as _;
pub async fn serve() -> Result<()> {
    if std::env::args().nth(2).as_deref() == Some("http") {
        let address = std::env::var("AGENT_WORKBENCH_MCP_ADDR")?;
        let listener = tokio::net::TcpListener::bind(&address).await?;
        let app = axum::Router::new()
            .route("/mcp", axum::routing::post(fixture_http_request))
            .layer(axum::middleware::from_fn(auth));
        axum::serve(listener, app)
            .await
            .context("serve workbench MCP HTTP fixture")
    } else {
        if let Ok(path) = std::env::var("AGENT_WORKBENCH_MCP_STDIO_PID") {
            std::fs::write(path, std::process::id().to_string())?;
        }
        WorkbenchMcpService(WorkbenchMcpServer::new())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// MCP-2026-MRTR-FIXTURE: every host-input tool resumes on an independent
    /// HTTP request; URL elicitation carries no removed completion identifier.
    #[tokio::test]
    async fn mrtr_fixture_round_trips_host_inputs_over_stateless_http() {
        let sampled = rmcp::model::CreateMessageResult::new(
            rmcp::model::SamplingMessage::assistant_text("fixture summary"),
            "fixture-model".into(),
        );
        let cases = [
            (
                "sample_summary",
                json!({"text":"source"}),
                serde_json::to_value(sampled).unwrap(),
                json!({"summary":"fixture summary","model":"fixture-model"}),
            ),
            (
                "elicit_confirmation",
                json!({}),
                json!({"action":"accept","content":{"answer":"yes"}}),
                json!({"action":"accept","answer":"yes"}),
            ),
            (
                "elicit_via_url",
                json!({}),
                json!({"action":"accept"}),
                json!({"action":"accept"}),
            ),
            (
                "list_host_roots",
                json!({}),
                json!({"roots":[{"uri":"file:///fixture"}]}),
                json!({"roots":[{"uri":"file:///fixture","name":null}]}),
            ),
        ];
        for (name, arguments, input, expected) in cases {
            let mut request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":name,"arguments":arguments,"_meta":{
                    "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{"sampling":{},"roots":{},"elicitation":{"form":{},"url":{}}},
                    "io.modelcontextprotocol/clientInfo":{"name":"fixture-law","version":"1"}
                }
            }});
            let response = fixture_http_request(serde_json::to_vec(&request).unwrap().into())
                .await
                .unwrap()
                .0;
            assert_eq!(
                response["result"]["resultType"], "input_required",
                "{name}: {response}"
            );
            assert!(
                response["result"]["inputRequests"]["host"]["params"]
                    .get("elicitationId")
                    .is_none()
            );
            request["id"] = json!(2);
            request["params"]["requestState"] = response["result"]["requestState"].clone();
            request["params"]["inputResponses"] = json!({"host":input});
            let response = fixture_http_request(serde_json::to_vec(&request).unwrap().into())
                .await
                .unwrap()
                .0;
            assert_eq!(
                response["result"]["resultType"], "complete",
                "{name}: {response}"
            );
            assert_eq!(
                response["result"]["structuredContent"], expected,
                "{name}: {response}"
            );
        }
    }
}
