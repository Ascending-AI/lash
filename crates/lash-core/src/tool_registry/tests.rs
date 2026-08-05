/// Project every catalog member to a JSON record for host-owned discovery
/// (e.g. the reference `search_tools` example in `lash-cli`). The projection
/// ranges over members and emits no tiered state.
pub(crate) fn project_tool_catalog<I>(entries: I) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = crate::ToolCatalogEntry>,
{
    entries
        .into_iter()
        .map(|entry| {
            let manifest = entry.manifest;
            let mut projected = serde_json::json!({
                "id": manifest.id,
                "name": manifest.name,
                "description": manifest.description,
                "bindings": manifest.bindings,
                "activation": manifest.activation,
            });
            if let Some(contract) = manifest.compact_contract {
                projected
                    .as_object_mut()
                    .expect("projected tool catalog entry is an object")
                    .insert("contract".to_string(), serde_json::json!(contract));
            }
            projected
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDefinition;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockTool;
    struct MixedEnabledTool;
    struct ExternalMockSource;
    struct ExactResolvingSource {
        manifest_resolutions: Arc<AtomicUsize>,
        contract_resolutions: Arc<AtomicUsize>,
        executions: Arc<AtomicUsize>,
        observed_execution_bindings: Option<Arc<std::sync::Mutex<Vec<serde_json::Value>>>>,
    }
    struct NamedExactSource {
        id: &'static str,
    }
    struct DynamicToolProvider {
        names: Arc<std::sync::Mutex<Vec<String>>>,
    }
    struct BlockingLiveTool {
        entered: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    }

    fn test_tool(
        name: &str,
        description: &str,
    ) -> ToolDefinition {
        ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            description,
            ToolDefinition::default_input_schema(),
            json!({ "type": "string" }),
        )
    }

    fn tool_id(name: &str) -> crate::ToolId {
        crate::ToolId::from(format!("tool:{name}"))
    }

    fn manifests(definitions: Vec<ToolDefinition>) -> Vec<ToolManifest> {
        definitions
            .into_iter()
            .map(|tool| tool.manifest())
            .collect()
    }

    fn contract_from(definitions: Vec<ToolDefinition>, name: &str) -> Option<Arc<ToolContract>> {
        definitions
            .into_iter()
            .find(|tool| tool.name() == name)
            .map(|tool| Arc::new(tool.contract()))
    }

    fn dynamic_definition(name: &str) -> ToolDefinition {
        test_tool(name, "dynamic")
    }

    fn test_tool_context() -> crate::ToolContext<'static> {
        crate::ToolContext::builder(
            "registry-test".to_string(),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::testing::MockSessionManager::default()),
            Arc::new(crate::UnavailableProcessService),
            crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(crate::InlineRuntimeEffectController::default())),
            Arc::new(crate::SessionAttachmentStore::in_memory()),
            crate::DirectCompletionClient::unavailable(
                "direct completions are unavailable in this test context",
            ),
        )
        .build()
    }

    #[async_trait::async_trait]
    impl ToolProvider for MockTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool(
                "mock_tool",
                "mock",
            )])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![test_tool(
                    "mock_tool",
                    "mock",
                )],
                name,
            )
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
            ToolResult::ok(serde_json::json!("ok"))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for MixedEnabledTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![
                test_tool(
                    "enabled_tool",
                    "enabled",
                ),
                test_tool(
                    "disabled_tool",
                    "disabled",
                ),
            ])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![
                    test_tool("enabled_tool", "enabled"),
                    test_tool("disabled_tool", "disabled"),
                ],
                name,
            )
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
            ToolResult::ok(serde_json::json!("ok"))
        }
    }

    #[async_trait::async_trait]
    impl ToolSourceExecutor for ExternalMockSource {
        fn id(&self) -> &str {
            "external"
        }

        fn advertised_tools(&self) -> Vec<ToolManifest> {
            manifests(vec![ToolDefinition::raw(
                "tool:mcp__demo__search",
                "mcp__demo__search",
                "search",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
                json!({ "type": "object", "additionalProperties": true }),
            )])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![ToolDefinition::raw(
                    "tool:mcp__demo__search",
                    "mcp__demo__search",
                    "search",
                    json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" }
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    json!({ "type": "object", "additionalProperties": true }),
                )],
                name,
            )
        }

        async fn execute(
            &self,
            tool: &str,
            args: &serde_json::Value,
            _context: &ToolContext<'_>,
        ) -> ToolResult {
            ToolResult::ok(json!({
                "tool": tool,
                "args": args
            }))
        }
    }

    #[async_trait::async_trait]
    impl ToolSourceExecutor for ExactResolvingSource {
        fn id(&self) -> &str {
            "exact"
        }

        fn advertised_tools(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
            self.manifest_resolutions.fetch_add(1, Ordering::SeqCst);
            (name == "host_only").then(|| {
                test_tool(
                    "host_only",
                    "host-only",
                )
                .manifest()
            })
        }

        fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
            self.manifest_resolutions.fetch_add(1, Ordering::SeqCst);
            (id == &tool_id("host_only")).then(|| {
                test_tool(
                    "host_only",
                    "host-only",
                )
                .manifest()
            })
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            self.contract_resolutions.fetch_add(1, Ordering::SeqCst);
            contract_from(
                vec![test_tool(
                    "host_only",
                    "host-only",
                )],
                name,
            )
        }

        fn resolve_contract_by_id(&self, id: &crate::ToolId) -> Option<Arc<ToolContract>> {
            self.contract_resolutions.fetch_add(1, Ordering::SeqCst);
            (id == &tool_id("host_only")).then(|| {
                Arc::new(
                    test_tool(
                        "host_only",
                        "host-only",
                        )
                    .contract(),
                )
            })
        }

        async fn execute(
            &self,
            tool: &str,
            _args: &serde_json::Value,
            context: &ToolContext<'_>,
        ) -> ToolResult {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if let Some(bindings) = &self.observed_execution_bindings {
                bindings
                    .lock()
                    .expect("execution bindings")
                    .push(context.tool_execution_binding().clone());
            }
            ToolResult::ok(json!(tool))
        }
    }

    #[async_trait::async_trait]
    impl ToolSourceExecutor for NamedExactSource {
        fn id(&self) -> &str {
            self.id
        }

        fn advertised_tools(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
            (name == "host_only").then(|| {
                test_tool(
                    "host_only",
                    "host-only",
                )
                .manifest()
            })
        }

        fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
            (id == &tool_id("host_only")).then(|| {
                test_tool(
                    "host_only",
                    "host-only",
                )
                .manifest()
            })
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
            None
        }

        async fn execute(
            &self,
            tool: &str,
            _args: &serde_json::Value,
            _context: &ToolContext<'_>,
        ) -> ToolResult {
            ToolResult::ok(json!(tool))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for DynamicToolProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            self.names
                .lock()
                .expect("dynamic tool names lock")
                .iter()
                .map(|name| dynamic_definition(name).manifest())
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            self.names
                .lock()
                .expect("dynamic tool names lock")
                .iter()
                .any(|tool_name| tool_name == name)
                .then(|| Arc::new(dynamic_definition(name).contract()))
        }

        async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
            ToolResult::ok(json!(call.name))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for BlockingLiveTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool("blocking_live", "blocking live tool")])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![test_tool("blocking_live", "blocking live tool")],
                name,
            )
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("release blocking live tool")
                .forget();
            ToolResult::ok(json!("completed from captured registry"))
        }
    }

    #[test]
    fn registry_makes_advertised_tools_members_by_default() {
        let registry =
            ToolRegistry::from_tool_provider(Arc::new(MixedEnabledTool)).expect("registry");
        let snapshot = registry.export_state();
        assert!(
            snapshot
                .get(&tool_id("enabled_tool"))
                .unwrap()
                .is_member()
        );
        assert!(
            snapshot
                .get(&tool_id("disabled_tool"))
                .unwrap()
                .is_member()
        );
        let members = snapshot
            .tool_manifests()
            .into_iter()
            .map(|manifest| manifest.name)
            .collect::<BTreeSet<_>>();
        assert!(members.contains("enabled_tool"));
        assert!(members.contains("disabled_tool"));
    }

    #[tokio::test]
    async fn removal_hides_source_from_new_session_snapshots_without_revoking_in_flight_snapshot() {
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let root = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("root registry");
        let handle = root
            .add_tool_provider(Arc::new(BlockingLiveTool {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }))
            .expect("register blocking live provider");
        let captured = Arc::new(
            root.compose_session_catalog(true, Vec::new())
                .expect("compose pre-removal session snapshot"),
        );
        let executing = crate::task::spawn({
            let captured = Arc::clone(&captured);
            async move {
                let args = json!({});
                let context = test_tool_context();
                captured
                    .execute(ToolCall {
                        name: "blocking_live",
                        args: &args,
                        context: &context,
                    })
                    .await
            }
        });
        entered
            .acquire()
            .await
            .expect("in-flight execution enters removed provider")
            .forget();

        root.remove_source(&handle)
            .expect("remove provider from root registry");
        let refreshed = root
            .compose_session_catalog(true, Vec::new())
            .expect("compose post-removal session snapshot");
        assert!(
            refreshed.resolve_contract("blocking_live").is_none(),
            "subsequent session composition must miss the removed provider"
        );

        release.add_permits(1);
        let completed = executing.await.expect("join captured-registry execution");
        assert!(completed.is_success());
        assert_eq!(
            completed.value_for_projection(),
            json!("completed from captured registry")
        );
    }

    #[test]
    fn exported_tool_state_is_source_free() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .add_tool_provider(Arc::new(MixedEnabledTool))
            .expect("live provider registered");

        let value = serde_json::to_value(registry.export_state()).expect("serialized tool state");
        let serialized = value.to_string();

        assert!(!serialized.contains("source_id"));
        assert!(!serialized.contains(PLUGIN_TOOL_SOURCE_ID));
        assert!(!serialized.contains("live:"));
    }

    #[test]
    fn apply_state_rebinds_source_free_snapshot_to_current_sources() {
        let source_registry =
            ToolRegistry::from_tool_provider(Arc::new(MixedEnabledTool)).expect("source registry");
        let snapshot = source_registry.export_state();

        let target_registry =
            ToolRegistry::from_tool_provider(Arc::new(MixedEnabledTool)).expect("target registry");
        let next_generation = target_registry
            .apply_state(snapshot.with_generation(target_registry.generation()))
            .expect("state rebound");

        assert_eq!(next_generation, target_registry.generation());
        assert!(target_registry.resolve_contract("enabled_tool").is_some());
    }

    #[test]
    fn apply_state_rejects_tools_not_advertised_by_source() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        let snapshot = registry.export_state();
        let generation = snapshot.generation();
        let mut tools = snapshot.entries().clone();
        tools.insert(
            tool_id("missing"),
            ToolStateEntry::new(
                test_tool(
                    "missing",
                    "missing",
                )
                .manifest(),
            ),
        );
        let snapshot = ToolState::new(generation, tools);
        assert!(matches!(
            registry.apply_state(snapshot),
            Err(ReconfigureError::Validation(_))
        ));
    }

    #[test]
    fn apply_state_rejects_snapshot_when_provider_is_absent() {
        let source_registry =
            ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
        source_registry
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("source registered");
        let snapshot = source_registry.export_state();

        let target_registry =
            ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target registry");
        let err = target_registry
            .apply_state(snapshot.with_generation(target_registry.generation()))
            .expect_err("missing provider should fail");

        assert!(matches!(err, ReconfigureError::Validation(_)));
    }

    #[test]
    fn apply_state_rejects_ambiguous_current_source_binding() {
        let registry = ToolRegistry::empty();
        registry
            .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
            .expect("source a registered");
        registry
            .upsert_source(Arc::new(NamedExactSource { id: "exact-b" }))
            .expect("source b registered");

        let mut tools = BTreeMap::new();
        tools.insert(
            tool_id("host_only"),
            ToolStateEntry::new(
                test_tool(
                    "host_only",
                    "host-only",
                )
                .manifest(),
            ),
        );

        let err = registry
            .apply_state(ToolState::new(registry.generation(), tools))
            .expect_err("ambiguous source binding should fail");

        assert!(matches!(err, ReconfigureError::Validation(_)));
    }

    #[test]
    fn advertised_manifest_resolves_without_exact_host_lookup() {
        let manifest_resolutions = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExactResolvingSource {
                manifest_resolutions: Arc::clone(&manifest_resolutions),
                contract_resolutions: Arc::new(AtomicUsize::new(0)),
                executions: Arc::new(AtomicUsize::new(0)),
                observed_execution_bindings: None,
            }))
            .expect("source registered");

        assert_eq!(
            registry
                .resolve_manifest("mock_tool")
                .map(|manifest| manifest.name),
            Some("mock_tool".to_string())
        );
        assert_eq!(manifest_resolutions.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_sources_re_reads_group_provider_manifests() {
        let names = Arc::new(std::sync::Mutex::new(vec!["dynamic_one".to_string()]));
        let provider: Arc<dyn ToolProvider> = Arc::new(DynamicToolProvider {
            names: Arc::clone(&names),
        });
        let registry = ToolRegistry::from_tool_providers(vec![provider]).expect("registry");

        let tool_names = || {
            registry
                .tool_manifests()
                .into_iter()
                .map(|manifest| manifest.name)
                .collect::<BTreeSet<_>>()
        };

        assert!(tool_names().contains("dynamic_one"));
        assert!(!tool_names().contains("dynamic_two"));

        names
            .lock()
            .expect("dynamic tool names lock")
            .push("dynamic_two".to_string());
        registry.refresh_sources().expect("refresh sources");
        let refreshed = tool_names();
        assert!(refreshed.contains("dynamic_one"));
        assert!(refreshed.contains("dynamic_two"));

        names
            .lock()
            .expect("dynamic tool names lock")
            .retain(|name| name != "dynamic_one");
        registry.refresh_sources().expect("refresh sources");
        let refreshed = tool_names();
        assert!(!refreshed.contains("dynamic_one"));
        assert!(refreshed.contains("dynamic_two"));
    }

    #[tokio::test]
    async fn cold_restore_adds_newly_advertised_tools_and_marks_state_dirty() {
        let names = Arc::new(std::sync::Mutex::new(vec!["dynamic_one".to_string()]));
        let provider: Arc<dyn ToolProvider> = Arc::new(DynamicToolProvider {
            names: Arc::clone(&names),
        });
        let source = ToolRegistry::from_tool_providers(vec![Arc::clone(&provider)])
            .expect("source registry");
        let snapshot = source.export_state();

        names
            .lock()
            .expect("dynamic tool names lock")
            .push("dynamic_two".to_string());
        let resumed =
            ToolRegistry::from_tool_providers(vec![provider]).expect("cold resume registry");
        let report = resumed
            .restore_state(snapshot.clone())
            .expect("restore live surface");

        assert_eq!(report.generation, snapshot.generation() + 1);
        let entry = resumed
            .export_state()
            .get(&tool_id("dynamic_two"))
            .expect("new live tool persisted")
            .clone();
        assert!(entry.is_member());
        let result = resumed
            .execute_by_id(
                &tool_id("dynamic_two"),
                &json!({}),
                &test_tool_context(),
            )
            .await;
        assert!(result.is_success(), "new live tool executes: {result:?}");
    }

    #[tokio::test]
    async fn fork_with_state_adds_newly_advertised_tools() {
        let names = Arc::new(std::sync::Mutex::new(vec!["dynamic_one".to_string()]));
        let provider: Arc<dyn ToolProvider> = Arc::new(DynamicToolProvider {
            names: Arc::clone(&names),
        });
        let registry = ToolRegistry::from_tool_providers(vec![provider]).expect("registry");
        let snapshot = registry.export_state();
        names
            .lock()
            .expect("dynamic tool names lock")
            .push("dynamic_two".to_string());

        let fork = registry.fork_with_state(snapshot).expect("live fork");
        assert!(
            fork.export_state()
                .get(&tool_id("dynamic_two"))
                .is_some_and(ToolStateEntry::is_member)
        );
        let result = fork
            .execute_by_id(
                &tool_id("dynamic_two"),
                &json!({}),
                &test_tool_context(),
            )
            .await;
        assert!(result.is_success(), "forked live tool executes: {result:?}");
    }

    #[tokio::test]
    async fn composed_catalog_adds_newly_advertised_base_tools() {
        let names = Arc::new(std::sync::Mutex::new(vec!["dynamic_one".to_string()]));
        let provider: Arc<dyn ToolProvider> = Arc::new(DynamicToolProvider {
            names: Arc::clone(&names),
        });
        let registry = ToolRegistry::from_tool_providers(vec![provider]).expect("registry");
        names
            .lock()
            .expect("dynamic tool names lock")
            .push("dynamic_two".to_string());

        let composed = registry
            .compose_session_catalog(true, Vec::new())
            .expect("composed live catalog");
        assert!(
            composed
                .export_state()
                .get(&tool_id("dynamic_two"))
                .is_some_and(ToolStateEntry::is_member)
        );
        let result = composed
            .execute_by_id(
                &tool_id("dynamic_two"),
                &json!({}),
                &test_tool_context(),
            )
            .await;
        assert!(
            result.is_success(),
            "composed live tool executes: {result:?}"
        );
    }

    #[tokio::test]
    async fn unknown_manifest_exact_resolves_and_routes_to_owner() {
        let manifest_resolutions = Arc::new(AtomicUsize::new(0));
        let contract_resolutions = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExactResolvingSource {
                manifest_resolutions: Arc::clone(&manifest_resolutions),
                contract_resolutions: Arc::clone(&contract_resolutions),
                executions: Arc::clone(&executions),
                observed_execution_bindings: None,
            }))
            .expect("source registered");

        assert_eq!(
            registry
                .resolve_manifest("host_only")
                .map(|manifest| manifest.name),
            Some("host_only".to_string())
        );
        assert_eq!(manifest_resolutions.load(Ordering::SeqCst), 1);

        let contract = registry.resolve_contract("host_only");
        assert!(contract.is_some());
        assert_eq!(manifest_resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 1);

        let context = test_tool_context();
        let args = json!({});
        let result = registry
            .execute(crate::ToolCall {
                name: "host_only",
                args: &args,
                context: &context,
            })
            .await;
        assert!(result.is_success());
        assert_eq!(result.value_for_projection(), json!("host_only"));
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn execution_grant_routes_without_adding_tool_to_state_or_catalog() {
        let manifest_resolutions = Arc::new(AtomicUsize::new(0));
        let contract_resolutions = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let observed_execution_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExactResolvingSource {
                manifest_resolutions: Arc::clone(&manifest_resolutions),
                contract_resolutions: Arc::clone(&contract_resolutions),
                executions: Arc::clone(&executions),
                observed_execution_bindings: Some(Arc::clone(&observed_execution_bindings)),
            }))
            .expect("source registered");

        assert!(!registry.export_state().contains(&tool_id("host_only")));
        assert!(
            !registry
                .tool_manifests()
                .iter()
                .any(|manifest| manifest.name == "host_only")
        );

        let grant = crate::ToolExecutionGrant::from_definition(test_tool(
            "host_only",
            "host-only",
        ))
        .with_source_id("exact")
        .with_execution_binding(json!({ "kind": "test", "route": "grant" }));
        let prepare_context = crate::ToolPrepareContext::with_execution_binding(
            "registry-test".to_string(),
            Arc::new(crate::testing::MockSessionManager::default()),
            crate::TurnContext::default(),
            Some("grant-call".to_string()),
            grant.execution_binding.clone(),
        );
        let prepared = registry
            .prepare_granted_tool_call(
                &grant,
                crate::ToolPrepareCall {
                    tool_id: grant.manifest.id.clone(),
                    pending: crate::sansio::PendingToolCall {
                        call_id: "grant-call".to_string(),
                        tool_name: grant.manifest.name.clone(),
                        args: json!({}),
                        replay: None,
                    },
                    context: &prepare_context,
                },
            )
            .await
            .expect("grant prepare");
        assert_eq!(prepared.tool_id, grant.manifest.id);

        let context = test_tool_context().with_tool_execution_binding(grant.execution_binding.clone());
        let args = json!({});
        let result = registry.execute_granted(&grant, &args, &context).await;
        assert!(result.is_success());
        assert_eq!(result.value_for_projection(), json!("host_only"));

        assert!(!registry.export_state().contains(&tool_id("host_only")));
        assert!(
            !registry
                .tool_manifests()
                .iter()
                .any(|manifest| manifest.name == "host_only")
        );
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 0);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(
            *observed_execution_bindings
                .lock()
                .expect("execution bindings"),
            vec![json!({ "kind": "test", "route": "grant" })]
        );
    }

    #[tokio::test]
    async fn execution_grant_without_source_does_not_infer_registry_route() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExactResolvingSource {
                manifest_resolutions: Arc::new(AtomicUsize::new(0)),
                contract_resolutions: Arc::new(AtomicUsize::new(0)),
                executions: Arc::new(AtomicUsize::new(0)),
                observed_execution_bindings: None,
            }))
            .expect("source registered");

        let grant = crate::ToolExecutionGrant::from_definition(test_tool(
            "host_only",
            "host-only",
        ));
        let context = test_tool_context();
        let args = json!({});
        let result = registry.execute_granted(&grant, &args, &context).await;

        assert!(!result.is_success());
        assert_eq!(
            result.value_for_projection(),
            json!("Granted tool id `tool:host_only` is missing an explicit tool source")
        );
        assert!(!registry.export_state().contains(&tool_id("host_only")));
    }

    #[tokio::test]
    async fn execution_grant_routes_grouped_source_by_id_not_name() {
        struct HiddenSameNameProvider {
            id: &'static str,
            result: &'static str,
        }

        impl HiddenSameNameProvider {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::raw(
                    self.id,
                    "shared_hidden_name",
                    self.result,
                    ToolDefinition::default_input_schema(),
                    json!({ "type": "string" }),
                )
            }
        }

        #[async_trait::async_trait]
        impl ToolProvider for HiddenSameNameProvider {
            fn tool_manifests(&self) -> Vec<ToolManifest> {
                Vec::new()
            }

            fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
                (name == "shared_hidden_name").then(|| self.definition().manifest())
            }

            fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
                (id.as_str() == self.id).then(|| self.definition().manifest())
            }

            fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
                None
            }

            async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
                ToolResult::ok(json!(self.result))
            }
        }

        let registry = ToolRegistry::from_tool_providers(vec![
            Arc::new(HiddenSameNameProvider {
                id: "tool:hidden_alpha",
                result: "wrong-provider",
            }),
            Arc::new(HiddenSameNameProvider {
                id: "tool:hidden_zeta",
                result: "right-provider",
            }),
        ])
        .expect("registry");
        let grant = crate::ToolExecutionGrant::from_definition(ToolDefinition::raw(
            "tool:hidden_zeta",
            "shared_hidden_name",
            "grant selects the second hidden provider by id",
            ToolDefinition::default_input_schema(),
            json!({ "type": "string" }),
        ))
        .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID);

        let context = test_tool_context();
        let args = json!({});
        let result = registry.execute_granted(&grant, &args, &context).await;

        assert!(result.is_success());
        assert_eq!(result.value_for_projection(), json!("right-provider"));
        assert!(
            registry.export_state().entries().is_empty(),
            "grant execution must not add hidden providers to registry state"
        );
    }

    #[test]
    fn unknown_manifest_without_host_resolver_is_unavailable() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");

        assert!(registry.resolve_manifest("missing").is_none());
        assert!(registry.resolve_contract("missing").is_none());
    }

    #[tokio::test]
    async fn upsert_source_registers_and_executes_external_tools() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("source registered");

        let defs = registry.tool_manifests();
        assert!(defs.iter().any(|def| def.name == "mcp__demo__search"));

        let context = test_tool_context();
        let args = json!({ "query": "hello" });
        let result = registry
            .execute(crate::ToolCall {
                name: "mcp__demo__search",
                args: &args,
                context: &context,
            })
            .await;
        assert!(result.is_success());
        assert_eq!(
            result.value_for_projection()["tool"],
            json!("mcp__demo__search")
        );
        assert_eq!(
            result.value_for_projection()["args"]["query"],
            json!("hello")
        );
    }

    #[test]
    fn upsert_source_preserves_membership_on_refresh() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("source registered");
        let mut snapshot = registry.export_state();
        snapshot
            .set_membership(&tool_id("mcp__demo__search"), false)
            .unwrap();
        registry.apply_state(snapshot).unwrap();
        registry
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("source refreshed");
        let snapshot = registry.export_state();
        assert!(
            !snapshot
                .get(&tool_id("mcp__demo__search"))
                .unwrap()
                .is_member(),
            "a host-removed tool stays a non-member across a source refresh"
        );
    }

    #[test]
    fn restore_state_adopts_generation_at_or_above_three() {
        // Cold rebuild ratchet: a session whose tool catalog advanced to
        // generation >= 3 restores onto a fresh base-1 registry. `restore_state`
        // adopts the snapshot's generation verbatim; `apply_state` (a gen-matched
        // delta) rejects it. This is the exact divergence the durable worker /
        // session resume rebuild relies on `restore_state` to absorb.
        let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
        let snapshot = source.export_state().with_generation(3);

        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target registry");
        assert_eq!(
            target.generation(),
            1,
            "a fresh registry starts at generation 1"
        );
        let restored = target
            .restore_state(snapshot.clone())
            .expect("restore adopts the snapshot generation");
        assert_eq!(
            restored.generation, 3,
            "restore returns the adopted generation"
        );
        assert!(
            restored.orphaned.is_empty(),
            "all tools resolve, so nothing orphans"
        );
        assert_eq!(
            target.generation(),
            3,
            "restore adopts gen 3 onto a base-1 registry without bumping"
        );
        // A re-export round-trips at the same generation (idempotent).
        assert_eq!(target.export_state().generation(), 3);

        // apply_state on the same high-generation snapshot is rejected — proving
        // the rebuild would have failed without restore_state.
        let fresh = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("fresh registry");
        assert!(
            matches!(
                fresh.apply_state(snapshot),
                Err(ReconfigureError::GenerationMismatch {
                    expected: 3,
                    actual: 1
                })
            ),
            "apply_state must reject a gen-3 snapshot on a base-1 registry"
        );
    }

    /// Build a snapshot whose `mcp__demo__search` entry only resolves while
    /// `ExternalMockSource` is registered — restoring it elsewhere orphans it.
    fn snapshot_with_external_tool() -> ToolState {
        let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
        source
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("source registered");
        source.export_state()
    }

    #[tokio::test]
    async fn restore_orphans_unresolved_tools_instead_of_failing() {
        let snapshot = snapshot_with_external_tool();

        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
        let report = target
            .restore_state(snapshot)
            .expect("restore tolerates the missing source");
        assert_eq!(report.orphaned, vec![tool_id("mcp__demo__search")]);

        // Orphans are non-members: excluded from the catalog listing entirely.
        assert!(
            !target
                .tool_manifests()
                .into_iter()
                .any(|manifest| manifest.name == "mcp__demo__search"),
            "orphans are excluded from the catalog"
        );
        let exported = target.export_state();
        assert!(
            !exported
                .tool_manifests()
                .into_iter()
                .any(|manifest| manifest.name == "mcp__demo__search"),
            "exported ToolState also excludes the orphan from the catalog"
        );
        let entry = exported.get(&tool_id("mcp__demo__search")).expect("orphan exported");
        assert!(entry.is_orphaned());
        assert!(!entry.is_member(), "orphans are never catalog members");

        // Execution fails loudly with a precise error.
        let context = test_tool_context();
        let args = json!({ "query": "hello" });
        let result = target
            .execute(crate::ToolCall {
                name: "mcp__demo__search",
                args: &args,
                context: &context,
            })
            .await;
        assert!(!result.is_success());
        assert!(
            format!("{result:?}").contains("unavailable"),
            "orphan execution error names the condition: {result:?}"
        );

        // Bound tools are unaffected.
        assert!(target.resolve_contract("mock_tool").is_some());
    }

    #[tokio::test]
    async fn orphan_rebinds_when_source_is_upserted_again() {
        let snapshot = snapshot_with_external_tool();
        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
        target.restore_state(snapshot).expect("restore");
        let orphaned_generation = target.generation();

        target
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("the returning source must not conflict with its own orphan");
        assert!(
            target.generation() > orphaned_generation,
            "rebinding bumps the generation"
        );

        let exported = target.export_state();
        let entry = exported.get(&tool_id("mcp__demo__search")).expect("entry kept");
        assert!(
            !entry.is_orphaned(),
            "the orphan rebound to the live source"
        );
        assert!(
            entry.is_member(),
            "the rebound tool is a catalog member again"
        );

        let context = test_tool_context();
        let args = json!({ "query": "hello" });
        let result = target
            .execute(crate::ToolCall {
                name: "mcp__demo__search",
                args: &args,
                context: &context,
            })
            .await;
        assert!(result.is_success(), "rebound tool executes: {result:?}");
    }

    #[test]
    fn restore_uses_live_manifest_and_preserves_membership_for_same_id() {
        struct UpdatedMockTool;

        #[async_trait::async_trait]
        impl ToolProvider for UpdatedMockTool {
            fn tool_manifests(&self) -> Vec<ToolManifest> {
                manifests(vec![test_tool("mock_tool", "live manifest")])
            }

            fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
                contract_from(vec![test_tool("mock_tool", "live manifest")], name)
            }

            async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
                ToolResult::ok(json!("updated"))
            }
        }

        let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source");
        let mut snapshot = source.export_state();
        snapshot
            .set_membership(&tool_id("mock_tool"), false)
            .expect("opt out");
        let target =
            ToolRegistry::from_tool_provider(Arc::new(UpdatedMockTool)).expect("target registry");

        target.restore_state(snapshot).expect("restore");
        let exported = target.export_state();
        let entry = exported.get(&tool_id("mock_tool")).expect("same id");
        assert_eq!(entry.manifest().description, "live manifest");
        assert!(!entry.is_member(), "membership remains attached to the id");
    }

    #[tokio::test]
    async fn orphan_rebinds_lazily_via_resolve_manifest() {
        // `NamedExactSource` advertises nothing, so reconcile-on-upsert cannot
        // rebind; only the lazy `resolve_manifest` path can.
        let source_registry = ToolRegistry::empty();
        source_registry
            .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
            .expect("source registered");
        assert!(source_registry.resolve_manifest("host_only").is_some());
        let snapshot = source_registry.export_state();

        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
        let report = target.restore_state(snapshot).expect("restore");
        assert_eq!(report.orphaned, vec![tool_id("host_only")]);

        target
            .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
            .expect("source returns");
        let manifest = target
            .resolve_manifest("host_only")
            .expect("resolves after the source returned");
        assert_eq!(manifest.name, "host_only");
        let entry = target.export_state();
        let entry = entry.get(&tool_id("host_only")).expect("entry kept");
        assert!(!entry.is_orphaned(), "lazy rebind clears the orphan flag");
        assert!(entry.is_member(), "the rebound tool is a catalog member");
    }

    #[test]
    fn restore_binds_snapshot_id_from_source_that_advertises_nothing() {
        let source_registry = ToolRegistry::empty();
        source_registry
            .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
            .expect("source registered");
        assert!(source_registry.resolve_manifest("host_only").is_some());
        let snapshot = source_registry.export_state();

        let target = ToolRegistry::empty();
        target
            .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
            .expect("lazy source registered before restore");
        let report = target.restore_state(snapshot).expect("lazy id binds");

        assert!(report.orphaned.is_empty());
        let exported = target.export_state();
        let entry = exported
            .get(&tool_id("host_only"))
            .expect("snapshot-only id retained");
        assert!(!entry.is_orphaned());
        assert!(entry.is_member());
    }

    #[tokio::test]
    async fn hidden_lazy_resolved_tool_is_not_executable_by_id() {
        let target = ToolRegistry::empty_with_hidden_tools(
            ["host_only".to_string()].into_iter().collect(),
        );
        target
            .upsert_source(Arc::new(NamedExactSource { id: "exact-a" }))
            .expect("lazy source registered");

        let result = target
            .execute_by_id(
                &tool_id("host_only"),
                &json!({}),
                &test_tool_context(),
            )
            .await;

        assert!(!result.is_success(), "hidden lazy id must not execute");
        assert!(
            !target
                .export_state()
                .get(&tool_id("host_only"))
                .expect("lazy tool recorded")
                .is_member()
        );
    }

    #[test]
    fn restore_drops_superseded_orphan_and_does_not_transfer_opt_out() {
        struct ReplacedSearchTool;
        #[async_trait::async_trait]
        impl ToolProvider for ReplacedSearchTool {
            fn tool_manifests(&self) -> Vec<ToolManifest> {
                manifests(vec![ToolDefinition::raw(
                    "tool:replaced",
                    "mcp__demo__search",
                    "a different implementation under the same name",
                    ToolDefinition::default_input_schema(),
                    json!({}),
                )])
            }
            fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
                None
            }
            async fn execute(&self, _call: ToolCall<'_>) -> ToolResult {
                ToolResult::ok(json!("ok"))
            }
        }

        let mut snapshot = snapshot_with_external_tool();
        snapshot
            .set_membership(&tool_id("mcp__demo__search"), false)
            .expect("opt out old id");
        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
        target
            .add_tool_provider(Arc::new(ReplacedSearchTool))
            .expect("replacement registered");
        let report = target
            .restore_state(snapshot)
            .expect("same name with a different id supersedes the old orphan");
        assert!(report.orphaned.is_empty());

        let exported = target.export_state();
        assert!(
            !exported.contains(&tool_id("mcp__demo__search")),
            "the old unresolved grant is superseded by the live name"
        );
        assert!(
            exported
                .get(&crate::ToolId::from("tool:replaced"))
                .is_some_and(ToolStateEntry::is_member),
            "membership policy is per id, so the replacement defaults to member"
        );
    }

    #[test]
    fn apply_state_round_trips_while_orphans_exist() {
        // `export_state` → edit → `apply_state` must work with an orphan in
        // the snapshot: the exported orphan flag exempts it from strictness.
        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
        target
            .restore_state(snapshot_with_external_tool())
            .expect("restore");

        let mut edited = target.export_state();
        edited
            .set_membership(&tool_id("mock_tool"), false)
            .expect("edit bound tool");
        target
            .apply_state(edited)
            .expect("apply accepts the snapshot it exported");
        let exported = target.export_state();
        assert!(exported.get(&tool_id("mcp__demo__search")).unwrap().is_orphaned());
        assert!(
            !exported.get(&tool_id("mock_tool")).unwrap().is_member(),
            "the host-removed bound tool stays a non-member through the round-trip"
        );

        // But a snapshot that does NOT mark the tool orphaned still fails —
        // strictness is preserved for entries that were bound at export.
        let strict = snapshot_with_external_tool().with_generation(target.generation());
        assert!(matches!(
            target.apply_state(strict),
            Err(ReconfigureError::Validation(_))
        ));
    }

    #[test]
    fn orphan_flag_serializes_and_legacy_snapshots_deserialize_as_bound() {
        let target = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("target");
        target
            .restore_state(snapshot_with_external_tool())
            .expect("restore");
        let value = serde_json::to_value(target.export_state()).expect("serializes");
        assert_eq!(
            value["tools"]["tool:mcp__demo__search"]["orphaned"],
            json!(true)
        );
        assert!(
            value["tools"]["tool:mock_tool"].get("orphaned").is_none(),
            "bound entries omit the flag, keeping old and new snapshots byte-compatible"
        );

        let legacy: ToolStateEntry = serde_json::from_value(json!({
            "manifest": value["tools"]["tool:mock_tool"]["manifest"]
        }))
        .expect("legacy entry without the flag deserializes");
        assert!(!legacy.is_orphaned());
    }

    #[test]
    fn remove_source_removes_all_source_tools() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        registry
            .upsert_source(Arc::new(ExternalMockSource))
            .expect("source registered");
        registry
            .remove_source_id("external")
            .expect("source removed");
        let defs = registry.tool_manifests();
        assert!(!defs.iter().any(|def| def.name == "mcp__demo__search"));
    }

    #[test]
    fn project_tool_catalog_projects_all_members_with_catalog_metadata() {
        fn member_fixture(name: &str) -> crate::ToolDefinition {
            crate::ToolDefinition::raw(
                format!("tool:{name}"),
                name,
                format!("desc for {name}"),
                crate::ToolDefinition::default_input_schema(),
                serde_json::json!({}),
            )
        }
        let catalog = project_tool_catalog([
            crate::ToolCatalogEntry {
                manifest: member_fixture("read_file").manifest(),
            },
            crate::ToolCatalogEntry {
                manifest: member_fixture("search_tools").manifest(),
            },
        ]);
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0]["name"], serde_json::json!("read_file"));
        assert_eq!(
            catalog[0]["contract"]["signature"],
            serde_json::json!("read_file({})")
        );
        // Membership is the execution gate; the projection emits no tier.
        assert!(catalog[0].get("availability").is_none());
        assert!(catalog[0].get("showcased").is_none());
        assert!(catalog[0].get("callable").is_none());
        assert!(catalog[0].get("searchable").is_none());
        assert_eq!(catalog[1]["name"], serde_json::json!("search_tools"));
    }

    #[test]
    fn project_tool_catalog_preserves_dynamic_output_contracts() {
        fn member_fixture(name: &str) -> crate::ToolDefinition {
            crate::ToolDefinition::raw(
                format!("tool:{name}"),
                name,
                format!("desc for {name}"),
                crate::ToolDefinition::default_input_schema(),
                serde_json::json!({}),
            )
        }
        let catalog = project_tool_catalog([crate::ToolCatalogEntry {
            manifest: member_fixture("llm_query")
                .with_output_from_input_schema(
                    "output",
                    Some(serde_json::json!({ "type": "string" })),
                )
                .manifest(),
        }]);

        assert_eq!(
            catalog[0]["contract"]["signature"],
            serde_json::json!("llm_query<T = str>({})")
        );
        assert_eq!(catalog[0]["contract"]["returns"], serde_json::json!("T"));
    }
}
