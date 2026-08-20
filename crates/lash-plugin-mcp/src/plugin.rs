//! [`PluginFactory`] for MCP integration. Holds a shared connection pool
//! across every session built from the same `LashCore`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};
use lash_core::{
    AttemptContext, ToolCall, ToolContract, ToolId, ToolManifest, ToolOutcome, ToolProvider,
};

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::host::{
    McpElicitationHandler, McpElicitationService, McpHostServices, McpRootsProvider,
    McpSamplingHandler,
};
use crate::pool::McpConnectionPool;

/// Plugin factory for MCP. Add once to `LashCoreBuilder` via
/// `.plugin(Arc::new(factory))`.
pub struct McpPluginFactory {
    pool: Arc<McpConnectionPool>,
}

/// Builder for an MCP plugin factory and its host-owned client handlers.
pub struct McpPluginFactoryBuilder {
    servers: BTreeMap<String, McpServerConfig>,
    host_services: McpHostServices,
    elicitation_handler: Option<Arc<dyn McpElicitationHandler>>,
}

impl McpPluginFactoryBuilder {
    /// Route `sampling/createMessage` through the supplied host handler.
    pub fn sampling_handler(mut self, handler: Arc<dyn McpSamplingHandler>) -> Self {
        self.host_services.sampling = Some(handler);
        self
    }

    /// Route `elicitation/create` through the supplied host handler.
    pub fn elicitation_handler(mut self, handler: Arc<dyn McpElicitationHandler>) -> Self {
        self.elicitation_handler = Some(handler);
        self
    }

    /// Supply workspace roots and enable roots-change notifications.
    pub fn roots_provider(mut self, provider: Arc<dyn McpRootsProvider>) -> Self {
        self.host_services.roots = Some(provider);
        self
    }

    /// Connect the configured servers and build the factory.
    pub async fn build(mut self) -> Result<McpPluginFactory, McpError> {
        if let Some(handler) = self.elicitation_handler {
            let capability = handler.capability();
            if capability.form.is_none() && capability.url.is_none() {
                return Err(McpError::Config(
                    "an MCP elicitation handler must advertise form, URL, or both modes"
                        .to_string(),
                ));
            }
            self.host_services.elicitation = Some(McpElicitationService {
                handler,
                capability,
            });
        }
        let pool =
            McpConnectionPool::connect_with_host_services(self.servers, self.host_services).await?;
        Ok(McpPluginFactory { pool })
    }
}

impl McpPluginFactory {
    /// Connect to every configured server and return a factory whose pool is
    /// ready to use. Servers are tried eagerly, but one being down never
    /// fails construction — the pool keeps reconnecting in the background and
    /// the server's tools become available once it comes up. Only
    /// configuration errors fail. The pool is `Arc`-shared across sessions;
    /// cloning the factory and adding it to multiple `LashCore`s shares the
    /// same connections.
    pub async fn new(servers: BTreeMap<String, McpServerConfig>) -> Result<Self, McpError> {
        Self::builder(servers).build().await
    }

    /// Configure servers together with optional host-owned sampling,
    /// elicitation, and roots handlers.
    pub fn builder(servers: BTreeMap<String, McpServerConfig>) -> McpPluginFactoryBuilder {
        McpPluginFactoryBuilder {
            servers,
            host_services: McpHostServices::default(),
            elicitation_handler: None,
        }
    }

    /// Empty pool — useful when servers are added at runtime via
    /// [`McpPluginFactory::attach_server`].
    pub fn empty() -> Self {
        Self {
            pool: Arc::new(McpConnectionPool::empty()),
        }
    }

    /// Direct access to the underlying pool, in case the embedder wants to
    /// inspect or mutate it directly.
    pub fn pool(&self) -> &Arc<McpConnectionPool> {
        &self.pool
    }

    /// Attach a new server at runtime. Startup outages leave the server
    /// registered and reconnecting in the background, matching factory
    /// construction. The new tools become visible to any session created
    /// after discovery succeeds; existing sessions will see them after their
    /// next tool-catalog refresh.
    pub async fn attach_server(
        &self,
        server_name: String,
        config: McpServerConfig,
    ) -> Result<(), McpError> {
        self.pool.attach(server_name, config).await
    }

    /// Detach a server at runtime.
    pub async fn detach_server(&self, server_name: &str) -> Result<(), McpError> {
        self.pool.detach(server_name).await
    }

    /// Connection status of every configured server, including the last
    /// connection error for servers currently reconnecting in the background.
    pub fn server_statuses(&self) -> Vec<crate::pool::McpServerStatus> {
        self.pool.server_statuses()
    }

    /// Notify connected servers that the host's current roots changed.
    pub async fn notify_roots_changed(&self) -> Result<(), McpError> {
        self.pool.notify_roots_changed().await
    }
}

#[async_trait]
impl PluginFactory for McpPluginFactory {
    fn id(&self) -> &'static str {
        "mcp"
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        self.pool.shutdown_all().await;
        Ok(())
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(McpSessionPlugin {
            pool: Arc::clone(&self.pool),
        }))
    }
}

struct McpSessionPlugin {
    pool: Arc<McpConnectionPool>,
}

impl SessionPlugin for McpSessionPlugin {
    fn id(&self) -> &'static str {
        "mcp"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.tools().provider(Arc::new(McpToolProvider {
            pool: Arc::clone(&self.pool),
        }) as Arc<dyn ToolProvider>)
    }
}

/// The `ToolProvider` actually registered with each session's tool catalog.
/// Delegates definitions and execution to the shared pool.
pub struct McpToolProvider {
    pool: Arc<McpConnectionPool>,
}

impl McpToolProvider {
    pub fn new(pool: Arc<McpConnectionPool>) -> Self {
        Self { pool }
    }
}

/// Non-resident MCP provider for RLM deferred execution.
///
/// It advertises no catalog members, but resolves and executes known MCP tools
/// by id for explicit deferred grants.
pub struct McpDeferredToolProvider {
    pool: Arc<McpConnectionPool>,
}

impl McpDeferredToolProvider {
    pub fn new(pool: Arc<McpConnectionPool>) -> Self {
        Self { pool }
    }

    fn definition_by_id(&self, id: &ToolId) -> Option<lash_core::ToolDefinition> {
        self.pool
            .advertised_tools()
            .into_iter()
            .find(|tool| tool.manifest.id == *id)
    }

    fn validate_execution_binding(
        tool_id: &ToolId,
        binding: &serde_json::Value,
    ) -> Result<(), ToolOutcome> {
        let valid = binding
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind == "mcp")
            && binding
                .get("tool_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|bound_id| bound_id == tool_id.as_str());
        if valid {
            return Ok(());
        }
        Err(ToolOutcome::err_fmt(format_args!(
            "MCP deferred execution for tool id `{tool_id}` requires an execution binding with kind `mcp` and matching `tool_id`"
        )))
    }
}

#[async_trait]
impl ToolProvider for McpToolProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        self.pool
            .advertised_tools()
            .into_iter()
            .map(|tool| tool.manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.pool
            .advertised_tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .map(|tool| Arc::new(tool.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.pool
            .call_tool(call.name, call.args, call.context)
            .await
    }
}

#[async_trait]
impl ToolProvider for McpDeferredToolProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest(&self, _name: &str) -> Option<ToolManifest> {
        None
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.definition_by_id(id).map(|tool| tool.manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        self.definition_by_id(id)
            .map(|tool| Arc::new(tool.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::err_fmt(format_args!(
            "MCP tool `{}` is not a resident catalog member; execute it through a deferred grant",
            call.name
        ))
    }

    async fn execute_by_id(
        &self,
        tool_id: &ToolId,
        args: &serde_json::Value,
        context: &AttemptContext<'_>,
    ) -> ToolOutcome {
        if let Err(result) =
            Self::validate_execution_binding(tool_id, context.tool_execution_binding())
        {
            return result;
        }
        let Some(definition) = self.definition_by_id(tool_id) else {
            return ToolOutcome::err_fmt(format_args!("Unknown MCP tool id: {tool_id}"));
        };
        self.pool
            .call_tool(&definition.manifest.name, args, context)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::ToolDefinition;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    /// Pure unit test ported from `crates/lash/src/mcp.rs`. Verifies that a
    /// `ToolDefinition::raw` constructed from an MCP-advertised input schema
    /// keeps the schema verbatim — this is the canonical input contract the
    /// model sees, so any drift here is user-visible.
    #[test]
    fn mcp_definition_preserves_server_schema_as_canonical_input_contract() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                },
                "filters": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "strict": {
                    "type": ["boolean", "null"],
                    "default": false
                }
            },
            "required": ["query", "filters"]
        });
        let definition = ToolDefinition::raw(
            "mcp:demo/search",
            "mcp__demo__search",
            "Search",
            schema.clone(),
            json!({}),
        );
        assert_eq!(definition.contract.input_schema.canonical, schema);
        assert_eq!(definition.parameter_metadata().len(), 3);
    }

    /// Full stdio integration test: spin up a tiny `sh` mock that emits three
    /// pre-canned JSON-RPC responses (initialize, tools/list, tools/call)
    /// matching rmcp's request-id sequence (0, 1, 2), then verify the pool
    /// imports the advertised tool with the right discovery metadata and
    /// executes it end-to-end.
    #[tokio::test]
    async fn adapter_imports_and_executes_stdio_tools() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "demo", "version": "1.0.0" }
            }
        });
        let list = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "search-docs",
                    "description": "Search docs",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    },
                    "outputSchema": {
                        "type": "object",
                        "properties": {
                            "matches": { "type": "array" }
                        },
                        "required": ["matches"]
                    }
                }]
            }
        });
        let call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "structuredContent": {
                    "matches": ["matched"]
                },
                "content": [{
                    "type": "text",
                    "text": "{\n  \"matches\": [\"matched\"]\n}"
                }]
            }
        });

        // Read each request line before emitting the matching response —
        // rmcp drops responses that arrive before their request is in flight,
        // so a "dump all responses upfront" mock races against the event
        // loop and the third response never gets matched. Reading one line
        // per request keeps the sequence deterministic.
        // Lines:
        //   1. initialize          → respond with RESP1
        //   2. notifications/initialized (no response)
        //   3. tools/list          → respond with RESP2
        //   4. tools/call          → respond with RESP3
        let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; printf '%s\\n' \"$RESP3\"; \
            cat >/dev/null"
            .to_string();

        let mut env = BTreeMap::new();
        env.insert("RESP1".to_string(), initialize.to_string());
        env.insert("RESP2".to_string(), list.to_string());
        env.insert("RESP3".to_string(), call.to_string());

        let mut servers = BTreeMap::new();
        servers.insert(
            "docs".to_string(),
            McpServerConfig::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env,
                cwd: None,
                startup_timeout_ms: 10_000,
                call_policy: crate::McpCallPolicy {
                    call_timeout_ms: 10_000,
                    ..Default::default()
                },
                binary_content_attachments: false,
            },
        );

        let factory = McpPluginFactory::new(servers)
            .await
            .expect("factory connects to stdio mock");

        let defs = factory.pool().advertised_tools();
        assert_eq!(defs.len(), 1, "expected one imported tool, got {defs:?}");
        assert_eq!(defs[0].name(), "mcp__docs__search_docs");
        #[cfg(feature = "lashlang")]
        {
            let binding =
                lash_lashlang_runtime::ToolManifestBindingExt::tool_binding(&defs[0].manifest)
                    .expect("valid lashlang binding")
                    .expect("mcp tool has lashlang binding");
            assert_eq!(binding.module_path, vec!["docs".to_string()]);
            assert_eq!(binding.operation.as_deref(), Some("search_docs"));
            assert_eq!(binding.aliases, vec!["search-docs".to_string()]);
        }
        #[cfg(not(feature = "lashlang"))]
        assert!(defs[0].manifest.bindings.is_empty());
        assert_eq!(
            defs[0]
                .contract
                .input_schema
                .canonical
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|props| props.get("query"))
                .and_then(|query| query.get("type"))
                .cloned(),
            Some(json!("string"))
        );
        assert_eq!(
            defs[0].contract.output_schema.canonical,
            json!({
                "type": "object",
                "properties": {
                    "matches": { "type": "array" }
                },
                "required": ["matches"]
            })
        );

        let deferred = McpDeferredToolProvider::new(Arc::clone(factory.pool()));
        let tool_id = defs[0].manifest.id.clone();
        let args = json!({ "query": "lash" });
        let missing_binding = deferred
            .execute_by_id(&tool_id, &args, &lash_core::testing::mock_attempt_context())
            .await;
        assert!(!missing_binding.is_success());
        assert!(
            missing_binding
                .value_for_projection()
                .as_str()
                .expect("string error")
                .contains("requires an execution binding"),
            "{missing_binding:?}"
        );

        let wrong_binding_context = lash_core::testing::mock_attempt_context_with_execution_binding(
            json!({ "kind": "mcp", "server": "docs", "tool_id": "mcp:docs/other" }),
        );
        let wrong_binding = deferred
            .execute_by_id(&tool_id, &args, &wrong_binding_context)
            .await;
        assert!(!wrong_binding.is_success());

        let valid_binding_context = lash_core::testing::mock_attempt_context_with_execution_binding(
            json!({ "kind": "mcp", "server": "docs", "tool_id": tool_id.to_string() }),
        );
        let deferred_result = deferred
            .execute_by_id(&tool_id, &args, &valid_binding_context)
            .await;
        assert!(deferred_result.is_success(), "{deferred_result:?}");
        assert_eq!(
            deferred_result.value_for_projection(),
            json!({ "matches": ["matched"] })
        );

        factory.shutdown().await.expect("shut down MCP factory");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_shutdown_kills_stdio_child_without_drop_and_is_idempotent() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let pid_file = scratch.path().join("mcp.pid");
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "lifecycle", "version": "1.0.0" }
            }
        });
        let list = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "tools": [] }
        });
        let script = "\
            printf '%s\\n' \"$$\" > \"$PID_FILE\"; \
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            cat >/dev/null"
            .to_string();
        let servers = BTreeMap::from([(
            "lifecycle".to_string(),
            McpServerConfig::Stdio {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env: BTreeMap::from([
                    ("PID_FILE".to_string(), pid_file.display().to_string()),
                    ("RESP1".to_string(), initialize.to_string()),
                    ("RESP2".to_string(), list.to_string()),
                ]),
                cwd: None,
                startup_timeout_ms: 5_000,
                call_policy: crate::McpCallPolicy {
                    call_timeout_ms: 5_000,
                    ..Default::default()
                },
                binary_content_attachments: false,
            },
        )]);
        let factory = McpPluginFactory::new(servers)
            .await
            .expect("connect lifecycle server");
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("read child pid")
            .trim()
            .parse()
            .expect("parse child pid");
        assert!(
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .expect("probe live child")
                .success(),
            "MCP child must be live before explicit shutdown"
        );

        factory.shutdown().await.expect("shut down MCP factory");

        assert!(factory.server_statuses().is_empty());
        assert!(
            !std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .expect("probe stopped child")
                .success(),
            "explicit plugin shutdown must reap the MCP child before factory drop"
        );
        factory.shutdown().await.expect("repeat MCP shutdown");
        assert!(
            factory.server_statuses().is_empty(),
            "a second plugin shutdown is a no-op"
        );
    }
}
