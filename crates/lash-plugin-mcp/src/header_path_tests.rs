//! MCP-HEADER-PATH: only annotations reached through properties mirror exact argument paths.
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;

use crate::{McpConnectionPool, McpServerConfig, McpStreamableHttpTransport};

#[derive(Clone)]
struct HeaderServer;

fn tools() -> Vec<Tool> {
    [
        ("nested", json!({"type":"object","properties":{"location":{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}}}})),
        ("array", json!({"type":"object","properties":{"locations":{"type":"array","items":{"type":"string","x-mcp-header":"Region"}}}})),
    ].into_iter().map(|(name, schema)| Tool::new_with_raw(name, None, schema.as_object().unwrap().clone())).collect()
}

impl ServerHandler for HeaderServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|tool| tool.name == name)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        assert_eq!(request.arguments.unwrap()["location"]["region"], "eu");
        let parts = context.extensions.get::<http::request::Parts>().unwrap();
        let header = parts
            .headers
            .get("Mcp-Param-Region")
            .expect("mirrored region");
        assert_eq!(header, "eu");
        Ok(CallToolResult::success(vec![ContentBlock::text(header.to_str().unwrap())]).into())
    }
}

/// FIG-6213: nested properties survive discovery and reach the HTTP server as headers;
/// the same catalog excludes an annotation beneath items.
#[tokio::test]
async fn http_pool_lists_nested_header_tool_and_mirrors_its_exact_path() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = StreamableHttpService::new(
            || Ok(HeaderServer),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().with_json_response(true),
        );
        let app = axum::Router::new().nest_service("/mcp", service);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = McpServerConfig::streamable_http(McpStreamableHttpTransport::new(format!(
            "http://{address}/mcp"
        )));
        let pool = McpConnectionPool::connect(BTreeMap::from([("headers".into(), config)]))
            .await
            .unwrap();
        let advertised = pool.advertised_tools();
        assert_eq!(
            advertised.len(),
            1,
            "only the properties-chain tool is valid"
        );
        assert_eq!(
            advertised[0].name(),
            crate::mcp_tool_names("headers", &["nested"])["nested"]
        );
        let result = pool
            .call_tool(
                advertised[0].name(),
                &json!({"location":{"region":"eu"}}),
                &lash_core::testing::mock_attempt_context(),
            )
            .await;
        assert!(result.is_success(), "nested header call: {result:?}");
        pool.shutdown_all().await;
        server.abort();
        let _ = server.await;
    })
    .await
    .expect("bounded HTTP header law");
}
