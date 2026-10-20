//! [`PluginFactory`] for MCP integration.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use lash_core::plugin::PluginFactory;
use lash_core::{ToolCall, ToolContract, ToolId, ToolManifest, ToolOutcome, ToolProvider};
use lash_execution_contracts::plugin_error::PluginError;

use crate::config::McpServerConfig;
#[cfg(test)]
use crate::config::{McpStdioTransport, McpTransport};
use crate::error::McpError;
use crate::host::{
    McpElicitationHandler, McpElicitationService, McpHostServices, McpRootsProvider,
    McpSamplingHandler,
};
use crate::pool::McpConnectionPool;

/// Plugin factory for MCP.
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
    /// Bound request correlation independently of publisher admission.
    pub fn route_policy(mut self, policy: crate::McpRoutePolicy) -> Self {
        self.host_services.route_policy = policy;
        self
    }
    pub fn sampling_handler(mut self, handler: Arc<dyn McpSamplingHandler>) -> Self {
        self.host_services.sampling = Some(handler);
        self
    }

    pub fn elicitation_handler(mut self, handler: Arc<dyn McpElicitationHandler>) -> Self {
        self.elicitation_handler = Some(handler);
        self
    }

    /// Supply workspace roots for multi round-trip input requests.
    pub fn roots_provider(mut self, provider: Arc<dyn McpRootsProvider>) -> Self {
        self.host_services.roots = Some(provider);
        self
    }

    pub async fn build(mut self) -> Result<McpPluginFactory, McpError> {
        if let Some(handler) = self.elicitation_handler {
            let capability = handler.modes().capability();
            self.host_services.elicitation = Some(McpElicitationService {
                handler,
                capability,
            });
        }
        let pool =
            McpConnectionPool::connect_with_host_services(self.servers, self.host_services).await?;
        Ok(McpPluginFactory::over(pool))
    }
}

impl McpPluginFactory {
    /// Servers are tried eagerly, but one being down never fails construction — the pool keeps
    /// reconnecting in the background and the server's tools become available once it comes
    /// up.
    /// Only configuration errors fail.
    /// The pool is `Arc`-shared across sessions; cloning the factory and adding it to multiple
    /// `LashCore`s shares the same connections.
    pub async fn new(servers: BTreeMap<String, McpServerConfig>) -> Result<Self, McpError> {
        Self::builder(servers).build().await
    }

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
        Self::over(Arc::new(McpConnectionPool::empty()))
    }

    fn over(pool: Arc<McpConnectionPool>) -> Self {
        Self { pool }
    }

    /// Host-callable event transport. No engine or tools are registered.
    pub fn events(&self) -> crate::events::McpEventClient {
        crate::events::McpEventClient::new(Arc::clone(&self.pool))
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

    /// Typed evidence of the last failed startup, following host capture policy.
    /// The pool clears it when the server connects successfully.
    pub fn startup_failure(&self, server_name: &str) -> Option<crate::error::McpStartupFailure> {
        self.pool.startup_failure(server_name)
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

    /// The `mcp` namespace holds which servers' tools a run is offered:
    /// [`lash_tool_support::SetToolSelection`] changes it sticky, and the run
    /// options under [`MCP_RUN_OPTIONS`] for one run.
    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<
            lash_core::plugin::PluginRecipe,
            lash_execution_contracts::plugin_error::PluginError,
        > = {
            let mut reg = lash_core::plugin::PluginRegistrar::<McpSessionPlugin>::new(
                lash_core::plugin::PluginFactory::id(self.as_ref()),
            );
            McpSessionPlugin::declare(&mut reg)?;
            let id = lash_core::plugin::PluginFactory::id(self.as_ref());
            Ok(lash_core::plugin::PluginRecipe::bound(
                id,
                reg,
                move |_ctx, _state| {
                    Ok(Arc::new(McpSessionPlugin {
                        pool: Arc::clone(&self.pool),
                    }))
                },
            ))
        };
        let mut recipe = recipe?;
        let mut configuration = lash_core::plugin::ConfigRegistrar::new(
            lash_core::plugin::PluginFactory::id(recipe_factory.as_ref()),
        );
        recipe_factory.declare_configuration(&mut configuration)?;
        recipe = recipe.with_configuration(configuration);
        Ok(recipe)
    }
}
impl McpPluginFactory {
    pub fn declare_configuration(
        &self,
        reg: &mut lash_core::plugin::ConfigRegistrar,
    ) -> Result<(), lash_execution_contracts::config_error::ConfigRegistrationError> {
        reg.owner_with_run_options(lash_tool_support::ToolSelectionOwner, MCP_RUN_OPTIONS)?;
        reg.command::<lash_tool_support::SetToolSelection>(|_recorded, command| {
            Ok(lash_core::plugin::OwnerChange {
                recorded: lash_tool_support::ToolSelectionConfig {
                    tools: Some(command.tools),
                },
                output: (),
            })
        })
    }
}

impl McpPluginFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("mcp")
    }
}
impl lash_core::plugin::PluginMetadata for McpPluginFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct McpSessionPlugin {
    pool: Arc<McpConnectionPool>,
}

impl McpSessionPlugin {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        reg.tools().provider(
            lash_core::tool_provider_key!("servers"),
            move |this, _state| {
                Ok(Arc::new(McpToolProvider {
                    pool: Arc::clone(&this.pool),
                }) as Arc<dyn ToolProvider>)
            },
        )?;
        let family = lash_core::plugin::prompt::PromptSectionKey::new(
            crate::pool::guidance::McpGuidanceSections::FAMILY,
        )
        .map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::Registration(error.to_string())
        })?;
        reg.prompt().family(
            lash_core::plugin::prompt::PromptSectionFamilySpec::new(
                family,
                lash_core::plugin::prompt::PromptPlacement::InitialInstructions,
            ),
            move |_this, _state| Ok(Arc::new(crate::pool::guidance::McpGuidanceSections)),
        )?;
        Ok(())
    }
}

/// The `ToolProvider` actually registered with each session's tool catalog.
pub struct McpToolProvider {
    pool: Arc<McpConnectionPool>,
}

impl McpToolProvider {
    pub fn new(pool: Arc<McpConnectionPool>) -> Self {
        Self { pool }
    }

    /// The servers' tools the `mcp` namespace's [`ToolSelection`] admits
    /// under `config`. A selection that does not decode offers nothing.
    ///
    /// [`ToolSelection`]: lash_tool_support::ToolSelection
    fn offered(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash_core::ToolDefinition>, PluginError> {
        Ok({
            let selection =
                lash_tool_support::ToolSelection::of(config, MCP_PLUGIN_ID).map_err(|error| {
                    PluginError::ProviderConfig {
                        owner: MCP_PLUGIN_ID.into(),
                        message: error.to_string(),
                    }
                })?;
            self.pool
                .advertised_tools()
                .into_iter()
                .filter(|tool| selection.admits(&tool.manifest.id))
                .collect()
        })
    }
}

/// The MCP plugin's id, which names its config namespace.
const MCP_PLUGIN_ID: &str = "mcp";

/// The key a send states the `mcp` namespace's run options under: a
/// [`ToolSelection`](lash_tool_support::ToolSelection) for that run alone.
pub const MCP_RUN_OPTIONS: lash_core::RunOptionsKey<lash_tool_support::ToolSelectionConfig> =
    lash_core::RunOptionsKey::new(MCP_PLUGIN_ID);

/// The execution binding format MCP's deferred provider writes and reads:
/// `{"kind": "mcp", "server": <server>, "tool_id": <tool id>}`.
/// version_surface = "coexist"
/// version_guard(items(MCP_BINDING_FORMAT, mcp_binding))
pub const MCP_BINDING_FORMAT: lash_core::FormatVersion = lash_core::FormatVersion::ONE;

/// Non-resident MCP catalog provider for explicit code mode execution grants.
///
/// It advertises no catalog members, but resolves and executes known MCP tools
/// by id for explicit grants. Its attempts complete inline; the grant supplies
/// no durable remote completion source or isolated process implementation.
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
        binding: Option<&lash_core::ExecutionBinding>,
    ) -> Result<(), ToolOutcome> {
        let payload = binding.map_or(&serde_json::Value::Null, |binding| &binding.payload);
        let valid = payload
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind == "mcp")
            && payload
                .get("tool_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|bound_id| bound_id == tool_id.as_str());
        if valid {
            return Ok(());
        }
        Err(
            crate::call_failure::McpCallFailure::InvalidExecutionBinding {
                tool_id: tool_id.to_string(),
                binding: payload.clone(),
            }
            .into(),
        )
    }
}

#[async_trait]
impl ToolProvider for McpToolProvider {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok({
            self.offered(config)?
                .into_iter()
                .map(|tool| tool.manifest())
                .collect()
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.offered(config)
            .ok()?
            .into_iter()
            .find(|tool| tool.manifest.tool() == name)
            .map(|tool| Arc::new(tool.contract()))
    }

    async fn prepare_tool_call(
        &self,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        self.pool.prepare_mcp_call(call)
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.pool.call_admitted_tool(call).await.into()
    }
}

#[async_trait]
impl ToolProvider for McpDeferredToolProvider {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok(Vec::new())
    }

    fn resolve_manifest(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<ToolManifest> {
        None
    }

    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.definition_by_id(id).map(|tool| tool.manifest())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        None
    }

    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.definition_by_id(id)
            .map(|tool| Arc::new(tool.contract()))
    }

    fn readable_binding_formats(&self) -> &'static [lash_core::FormatVersion] {
        &[MCP_BINDING_FORMAT]
    }

    async fn prepare_tool_call(
        &self,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        Self::validate_execution_binding(&call.tool_id, call.context.tool_execution_binding())?;
        self.pool.prepare_mcp_call(call)
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if let Err(result) =
            Self::validate_execution_binding(call.tool_id(), call.context.tool_execution_binding())
        {
            return result.into();
        }
        let binding = match crate::pool::admitted_binding(call.manifest()) {
            Ok(binding) => binding,
            Err(failure) => return failure.into(),
        };
        let payload = call
            .context
            .tool_execution_binding()
            .map_or(&serde_json::Value::Null, |binding| &binding.payload);
        if payload.get("server").and_then(serde_json::Value::as_str)
            != Some(binding.server.as_str())
        {
            return ToolOutcome::from(
                crate::call_failure::McpCallFailure::InvalidExecutionBinding {
                    tool_id: call.tool_id().to_string(),
                    binding: payload.clone(),
                },
            )
            .into();
        }
        self.pool.call_admitted_tool(call).await.into()
    }
}

/// `payload` as the execution binding MCP's deferred provider reads.
#[cfg(test)]
pub(crate) fn mcp_binding(payload: serde_json::Value) -> lash_core::ExecutionBinding {
    lash_core::ExecutionBinding {
        codec: lash_core::plugin::ToolProviderHandle::new(
            lash_core::plugin::RegistryKind::Session,
            MCP_PLUGIN_ID,
            lash_core::tool_provider_key!("deferred"),
        ),
        format: MCP_BINDING_FORMAT,
        payload,
    }
}

/// Admit the same payload a Run's prepare phase records for an MCP call.
#[cfg(test)]
pub(crate) async fn prepared_test_call(
    provider: &McpToolProvider,
    manifest: &ToolManifest,
) -> lash_core::testing::ToolCallFixture<'static> {
    let context = lash_core::ToolPrepareContext::for_testing(
        lash_core::RuntimeOwner::Session("mcp-admitted-test".into()),
        Arc::new(lash_core::testing::MockSessionManager::default()),
        None,
    );
    let prepared = provider
        .prepare_tool_call(lash_core::ToolPrepareCall {
            tool_id: manifest.id.clone(),
            pending: lash_core::sansio::PendingToolCall {
                call_id: context.call_id().clone(),
                provider_call_id: None,
                tool_name: manifest.tool(),
                args: serde_json::json!({}),
                replay: None,
            },
            context: &context,
        })
        .await
        .expect("admit the discovered MCP binding");
    lash_core::testing::ToolCallFixture::mock().prepared_call(&prepared)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn plugin_shutdown_kills_stdio_child_without_drop_and_is_idempotent() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let pid_file = scratch.path().join("mcp.pid");
        let discover = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "supportedVersions": ["2026-07-28"], "resultType":"complete", "ttlMs":0,"cacheScope":"private",
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
            read -r _; printf '%s\\n' \"$RESP2\"; \
            cat >/dev/null"
            .to_string();
        let servers = BTreeMap::from([(
            "lifecycle".to_string(),
            McpServerConfig {
                events: Default::default(),
                diagnostics: Default::default(),
                data_limits: Default::default(),
                startup_timeout_ms: 5_000,
                call_policy: crate::McpCallPolicy {
                    call_timeout_ms: 5_000,
                    ..Default::default()
                },
                shutdown_policy: Default::default(),
                transport: McpTransport::Stdio(McpStdioTransport {
                    command: "sh".to_string(),
                    args: vec!["-c".to_string(), script],
                    env: BTreeMap::from([
                        ("PID_FILE".to_string(), pid_file.display().to_string()),
                        ("RESP1".to_string(), discover.to_string()),
                        ("RESP2".to_string(), list.to_string()),
                    ]),
                    cwd: None,
                }),
                trace_propagation: Default::default(),
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
    #[tokio::test]
    async fn mcp_law_binding_and_catalog_name_keep_request_causes() {
        let id = lash_core::ToolId::new("mcp:fixture/work");
        let binding = McpDeferredToolProvider::validate_execution_binding(&id, None).unwrap_err();
        let context = lash_core::testing::mock_attempt_context();
        let missing = McpConnectionPool::empty()
            .call_tool("missing", &json!({}), &context)
            .await;
        for (result, code, kind) in [
            (
                binding,
                "mcp_invalid_execution_binding",
                "invalid_execution_binding",
            ),
            (missing, "mcp_unknown_tool", "unknown_tool"),
        ] {
            let lash_core::ToolCallOutcome::Failure(error) =
                &result.as_done_output().unwrap().outcome
            else {
                panic!("expected failure");
            };
            assert_eq!(error.class, lash_core::ToolFailureClass::InvalidRequest);
            assert_eq!(error.code, code);
            assert_eq!(error.source, lash_core::ToolFailureSource::Plugin);
            assert_eq!(error.raw.as_ref().unwrap().to_json_value()["kind"], kind);
        }
    }
}

#[cfg(test)]
mod certification {
    use super::*;
    /// FIG-6093: no server fixture means explicit partial certification.
    #[tokio::test]
    async fn mcp_passes_certification_with_explicit_unavailable_server_scenarios() {
        let report = lash::testing::certify_plugin(Arc::new(McpPluginFactory::empty()))
            .await
            .unwrap();
        let failures: Vec<_> = report.failures().map(ToString::to_string).collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert_eq!(
            report.verdict(),
            lash::testing::PluginCertificationVerdict::Partial
        );
        assert!(report.reports.iter().any(|report| matches!(
            report.result,
            lash::testing::PluginLawResult::Unavailable {
                reason: lash::testing::CertificationUnavailable::NoScenario
            }
        )));
    }
}
