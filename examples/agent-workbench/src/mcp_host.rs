//! Operator-owned MCP integrations, beside the workbench search peer.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use lash::mcp::{McpPluginFactory, McpServerConfig, McpStdioTransport, McpStreamableHttpTransport};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub async fn factory(
    url: &str,
    provider: lash::provider::ProviderHandle,
    model: String,
    workspace: &std::path::Path,
) -> anyhow::Result<Arc<McpPluginFactory>> {
    let mut servers = BTreeMap::from([(
        crate::WORKBENCH_SEARCH_MCP_SERVER.into(),
        McpServerConfig::streamable_http(McpStreamableHttpTransport::new(url)),
    )]);
    // Streamable HTTP servers the deployment attaches at boot, configured
    // as the attach route's requests: a restarted host keeps its operator's
    // integrations only through its configuration.
    if let Ok(configured) = std::env::var("AGENT_WORKBENCH_MCP_SERVERS") {
        let configured: Vec<AttachRequest> = serde_json::from_str(&configured)
            .map_err(|error| anyhow::anyhow!("invalid AGENT_WORKBENCH_MCP_SERVERS: {error}"))?;
        for server in configured {
            servers.insert(server.name.clone(), http_config(&server));
        }
    }
    if let Ok(command) = std::env::var("AGENT_WORKBENCH_MCP_FIXTURE_BIN") {
        servers.insert(
            "workspace_stdio".into(),
            McpServerConfig::stdio(McpStdioTransport::new(
                command,
                vec!["mcp-fixture".into(), "stdio".into()],
            )),
        );
    }
    Ok(Arc::new(
        McpPluginFactory::builder(servers)
            .sampling_handler(Arc::new(crate::mcp_policy::DemoSamplingHandler::new(
                provider,
                lash::LlmProfileMetadata::new(
                    model,
                    std::num::NonZeroUsize::MIN.saturating_add(200_000 - 1),
                ),
            )))
            .elicitation_handler(Arc::new(crate::mcp_policy::DemoElicitationHandler))
            .roots_provider(Arc::new(crate::mcp_policy::DemoRootsProvider::new(
                workspace,
            )))
            .build()
            .await?,
    ))
}

pub fn router(factory: Arc<McpPluginFactory>) -> Router {
    Router::new()
        .route("/api/mcp/servers", get(list).post(attach))
        .route("/api/mcp/servers/{name}", axum::routing::delete(detach))
        .with_state(factory)
}
fn views(factory: &McpPluginFactory) -> Vec<Value> {
    factory.server_statuses().into_iter().map(|status| json!({
        "name": status.server_name, "connected": status.health.is_connected(),
        "tools": factory.pool().advertised_tools_for_server(&status.server_name).into_iter().map(|tool| tool.name().to_owned()).collect::<Vec<_>>()
    })).collect()
}
async fn list(State(factory): State<Arc<McpPluginFactory>>) -> Json<Value> {
    Json(json!({"servers": views(&factory)}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachRequest {
    name: String,
    url: String,
    token: String,
}
async fn attach(
    State(factory): State<Arc<McpPluginFactory>>,
    Json(request): Json<AttachRequest>,
) -> Response {
    match factory
        .attach_server(request.name.clone(), http_config(&request))
        .await
    {
        Ok(()) => Json(
            views(&factory)
                .into_iter()
                .find(|view| view["name"] == request.name)
                .unwrap_or(Value::Null),
        )
        .into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}
/// An operator's streamable HTTP server, behind its bearer token.
fn http_config(request: &AttachRequest) -> McpServerConfig {
    let mut config = McpServerConfig::streamable_http(
        McpStreamableHttpTransport::new(request.url.clone()).with_headers(BTreeMap::from([(
            "Authorization",
            format!("Bearer {}", request.token),
        )])),
    );
    config.call_policy.call_timeout_ms = 5_000;
    config.call_policy.call_max_total_timeout_ms = 10_000;
    config
}
async fn detach(
    State(factory): State<Arc<McpPluginFactory>>,
    Path(name): Path<String>,
) -> Response {
    match factory.detach_server(&name).await {
        Ok(()) => Json(json!({"detached": name})).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}
