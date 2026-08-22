/// Project every catalog member to a JSON record for host-owned discovery
/// (e.g. the production `tools.search` path in agent-workbench). The projection
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
    use lash_sansio::sync::MutexExt;
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
    struct CountingManifestProvider {
        manifest_reads: Arc<AtomicUsize>,
    }
    struct CountingPrepareProvider {
        prepares: Arc<AtomicUsize>,
        defer_queries: Arc<AtomicUsize>,
    }
    struct LeafBatchTool;
    struct LazyLeafBatchTool;
    struct LazyOrchestratingBatchSource;
    struct TestBatchOrchestratingTool;
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

    fn test_attempt_context() -> crate::AttemptContext<'static> {
        crate::testing::mock_attempt_context_from(&test_tool_context())
    }

    #[tokio::test]
    async fn internal_execution_route_refuses_non_internal_activation() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
        let tool = test_tool_context();
        let context = crate::InternalProcessContext::__for_testing(&tool);

        let result = registry
            .execute_internal_by_id(
                &tool_id("mock_tool"),
                &serde_json::json!({}),
                &context,
            )
            .await;

        assert!(
            !result.as_output().is_success(),
            "an Always-activated tool must not cross the internal route"
        );
        assert!(
            result
                .as_output()
                .value_for_projection()["message"]
                .as_str()
                .is_some_and(|message| message.contains("not activated for internal execution")),
            "the class-boundary refusal must be explicit: {result:?}"
        );
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

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(serde_json::json!("ok"))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for LeafBatchTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool("batch", "leaf batch")])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(vec![test_tool("batch", "leaf batch")], name)
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(serde_json::json!("unreachable"))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for LazyLeafBatchTool {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest(&self, name: &str) -> Option<ToolManifest> {
            (name == "batch").then(|| test_tool("batch", "lazy leaf batch").manifest())
        }

        fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
            (id == &tool_id("batch")).then(|| test_tool("batch", "lazy leaf batch").manifest())
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(vec![test_tool("batch", "lazy leaf batch")], name)
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(json!("leaf"))
        }
    }

    #[async_trait::async_trait]
    impl ToolSourceExecutor for LazyOrchestratingBatchSource {
        fn id(&self) -> &str {
            "lazy-orchestrating"
        }

        fn source_key(&self) -> ToolSourceKey {
            ToolSourceKey::Orchestrating(tool_id("batch"))
        }

        fn registration_kind(&self) -> ToolRegistrationKind {
            ToolRegistrationKind::Orchestrating
        }

        fn advertised_tools(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
            (id == &tool_id("batch"))
                .then(|| test_tool("batch", "lazy orchestrating batch").manifest())
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![test_tool("batch", "lazy orchestrating batch")],
                name,
            )
        }

        async fn execute(
            &self,
            _tool: &str,
            _args: &serde_json::Value,
            _context: &crate::AttemptContext<'_>,
        ) -> ToolOutcome {
            ToolOutcome::err_fmt("orchestrating source cannot execute through the leaf route")
        }
    }

    #[async_trait::async_trait]
    impl crate::facade_support::OrchestratingToolImplementation
        for TestBatchOrchestratingTool
    {
        fn manifest(&self) -> ToolManifest {
            test_tool("batch", "orchestrating batch").manifest()
        }

        fn contract(&self) -> Arc<ToolContract> {
            Arc::new(test_tool("batch", "orchestrating batch").contract())
        }

        async fn execute(
            &self,
            _args: &serde_json::Value,
            context: &crate::facade_support::OrchestrationContext<'_>,
        ) -> ToolOutcome {
            ToolOutcome::ok(json!({ "session_id": context.session_id() }))
        }
    }

    fn test_batch_orchestrating_tool() -> crate::facade_support::OrchestratingToolDef {
        crate::facade_support::OrchestratingToolDef::new(Arc::new(TestBatchOrchestratingTool))
    }

    #[test]
    fn leaf_and_orchestrating_tool_id_collision_is_typed() {
        let error = match ToolRegistry::from_tool_registrations_with_hidden_tools(
            vec![(
                "orchestrating:tool:batch".to_string(),
                vec![Arc::new(LeafBatchTool) as Arc<dyn ToolProvider>],
            )],
            vec![test_batch_orchestrating_tool()],
            BTreeSet::new(),
        ) {
            Ok(_) => panic!("cross-lane tool ids must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ReconfigureError::CrossLaneToolIdCollision {
                ref tool_id,
                ref leaf_source_id,
            } if tool_id.as_str() == "tool:batch"
                && leaf_source_id == "orchestrating:tool:batch"
        ));
    }

    #[test]
    fn registration_kind_alone_selects_orchestration_dispatch() {
        let leaf = ToolRegistry::from_tool_provider_sources_with_hidden_tools(
            vec![(
                "subagents".to_string(),
                vec![Arc::new(LeafBatchTool) as Arc<dyn ToolProvider>],
            )],
            BTreeSet::new(),
        )
        .expect("leaf ids and plugin ids have no reserved-name semantics");
        assert!(
            !leaf.is_orchestrating_tool(&tool_id("batch")),
            "an impostor plugin id cannot change a leaf registration's kind"
        );

        let orchestrating = ToolRegistry::from_tool_registrations_with_hidden_tools(
            Vec::new(),
            vec![test_batch_orchestrating_tool()],
            BTreeSet::new(),
        )
        .expect("typed orchestrating registration");
        assert!(orchestrating.is_orchestrating_tool(&tool_id("batch")));
    }

    #[tokio::test]
    async fn pre_cutover_batch_snapshot_restores_and_dispatches_as_orchestration() {
        let source = ToolRegistry::from_tool_registrations_with_hidden_tools(
            Vec::new(),
            vec![test_batch_orchestrating_tool()],
            BTreeSet::new(),
        )
        .expect("source registry");
        let mut legacy_blob = serde_json::to_value(source.export_state()).expect("serialize state");
        let legacy_entry = legacy_blob["tools"]["tool:batch"]
            .as_object_mut()
            .expect("serialized batch entry");
        assert_eq!(
            legacy_entry.remove("registration_kind"),
            Some(json!("orchestrating")),
            "the compatibility probe strips exactly the field introduced by the cutover"
        );
        assert_eq!(
            legacy_entry.keys().collect::<Vec<_>>(),
            vec!["manifest"],
            "the remaining entry is exactly the pre-cutover writer shape"
        );
        let legacy_snapshot: ToolState =
            serde_json::from_value(legacy_blob).expect("deserialize pre-cutover state");

        let target = ToolRegistry::from_tool_registrations_with_hidden_tools(
            Vec::new(),
            vec![test_batch_orchestrating_tool()],
            BTreeSet::new(),
        )
        .expect("target registry");
        target
            .restore_state(legacy_snapshot)
            .expect("the live surface re-derives the registration lane");
        assert!(
            target.is_orchestrating_tool(&tool_id("batch")),
            "the restored registration is effectively routed through the orchestrating lane"
        );

        let context = crate::facade_support::OrchestrationContext::new(test_tool_context());
        let result = target
            .execute_orchestrating_by_id(&tool_id("batch"), &json!({}), &context)
            .await;
        assert!(result.is_success(), "batch takes the orchestrating route");
        assert_eq!(
            result.value_for_projection(),
            json!({ "session_id": "registry-test" })
        );
    }

    #[tokio::test]
    async fn lazy_leaf_resolution_cannot_smuggle_an_orchestrating_registration() {
        let registry = ToolRegistry::from_tool_provider(Arc::new(LazyLeafBatchTool))
            .expect("lazy leaf source");
        assert!(
            registry.resolve_manifest("batch").is_some(),
            "the leaf is learned through the lazy path"
        );
        assert!(!registry.is_orchestrating_tool(&tool_id("batch")));

        registry
            .upsert_source(Arc::new(OrchestratingToolSource::new(
                test_batch_orchestrating_tool(),
            )))
            .expect("the live typed registration supersedes stale snapshot lane state");
        assert!(
            registry.is_orchestrating_tool(&tool_id("batch")),
            "only the live typed source can establish the orchestrating lane"
        );

        let leaf_route = registry
            .execute_by_id(&tool_id("batch"), &json!({}), &test_attempt_context())
            .await;
        assert!(
            !leaf_route.is_success(),
            "the earlier lazy leaf body cannot execute after the typed source wins"
        );
        assert!(
            format!("{leaf_route:?}")
                .contains("orchestrating tools require direct OrchestrationContext dispatch")
        );

        let context = crate::facade_support::OrchestrationContext::new(test_tool_context());
        let orchestrating_route = registry
            .execute_orchestrating_by_id(&tool_id("batch"), &json!({}), &context)
            .await;
        assert!(orchestrating_route.is_success());
        assert_eq!(
            orchestrating_route.value_for_projection(),
            json!({ "session_id": "registry-test" })
        );
        assert_eq!(
            registry
                .resolve_manifest("batch")
                .expect("the typed source remains bound")
                .description,
            "orchestrating batch"
        );
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

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(serde_json::json!("ok"))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for CountingManifestProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            self.manifest_reads.fetch_add(1, Ordering::SeqCst);
            manifests(vec![
                test_tool("indexed_alpha", "alpha contract"),
                test_tool("indexed_beta", "beta contract"),
            ])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(
                vec![
                    test_tool("indexed_alpha", "alpha contract"),
                    test_tool("indexed_beta", "beta contract"),
                ],
                name,
            )
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(serde_json::json!("ok"))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for CountingPrepareProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool("advertised", "advertised tool")])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(vec![test_tool("advertised", "advertised tool")], name)
        }

        async fn prepare_tool_call(
            &self,
            call: crate::ToolPrepareCall<'_>,
        ) -> Result<PreparedToolCall, ToolOutcome> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            Ok(PreparedToolCall::identity(call.tool_id, call.pending))
        }

        fn attempt_may_defer(&self, _tool_id: &crate::ToolId) -> bool {
            self.defer_queries.fetch_add(1, Ordering::SeqCst);
            true
        }

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(json!("ok"))
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
            _context: &crate::AttemptContext<'_>,
        ) -> ToolOutcome {
            ToolOutcome::ok(json!({
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
            context: &crate::AttemptContext<'_>,
        ) -> ToolOutcome {
            self.executions.fetch_add(1, Ordering::SeqCst);
            if let Some(bindings) = &self.observed_execution_bindings {
                bindings
                    .lock_recover()
                    .push(context.tool_execution_binding().clone());
            }
            ToolOutcome::ok(json!(tool))
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
            _context: &crate::AttemptContext<'_>,
        ) -> ToolOutcome {
            ToolOutcome::ok(json!(tool))
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for DynamicToolProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            self.names
                .lock_recover()
                .iter()
                .map(|name| dynamic_definition(name).manifest())
                .collect()
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            self.names
                .lock_recover()
                .iter()
                .any(|tool_name| tool_name == name)
                .then(|| Arc::new(dynamic_definition(name).contract()))
        }

        async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
            ToolOutcome::ok(json!(call.name))
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

        async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("release blocking live tool")
                .forget();
            ToolOutcome::ok(json!("completed from captured registry"))
        }
    }

    #[test]
    fn indexed_contract_lookup_reuses_the_indexed_manifest() {
        let manifest_reads = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::from_tool_providers(vec![Arc::new(
            CountingManifestProvider {
                manifest_reads: Arc::clone(&manifest_reads),
            },
        )])
        .expect("registry");
        let reads_after_registration = manifest_reads.load(Ordering::SeqCst);

        for (name, description) in [
            ("indexed_alpha", "alpha contract"),
            ("indexed_beta", "beta contract"),
        ] {
            let actual = registry
                .resolve_contract(name)
                .expect("indexed provider contract should resolve");
            assert_eq!(
                serde_json::to_value(actual.as_ref()).expect("serialize actual contract"),
                serde_json::to_value(test_tool(name, description).contract())
                    .expect("serialize expected contract"),
                "indexed contract must match the old by-id path for {name}"
            );
        }

        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            reads_after_registration,
            "contract routing must not rematerialize the provider manifest catalog"
        );
    }

    #[test]
    fn single_provider_contract_lookup_reuses_the_indexed_manifest() {
        let manifest_reads = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::from_tool_provider(Arc::new(CountingManifestProvider {
            manifest_reads: Arc::clone(&manifest_reads),
        }))
        .expect("registry");
        let reads_after_registration = manifest_reads.load(Ordering::SeqCst);

        let actual = registry
            .resolve_contract("indexed_beta")
            .expect("indexed provider contract should resolve");
        assert_eq!(
            serde_json::to_value(actual.as_ref()).expect("serialize actual contract"),
            serde_json::to_value(test_tool("indexed_beta", "beta contract").contract())
                .expect("serialize expected contract")
        );
        assert_eq!(
            manifest_reads.load(Ordering::SeqCst),
            reads_after_registration,
            "single-provider routing must not rematerialize the provider manifest catalog"
        );
    }

    #[test]
    fn indexed_contract_lookup_falls_back_to_by_id_resolution() {
        struct ByIdOnlyProvider;

        impl ByIdOnlyProvider {
            fn definition() -> ToolDefinition {
                test_tool("deferred_contract", "resolved only by id")
            }
        }

        #[async_trait::async_trait]
        impl ToolProvider for ByIdOnlyProvider {
            fn tool_manifests(&self) -> Vec<ToolManifest> {
                manifests(vec![Self::definition()])
            }

            fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
                None
            }

            fn resolve_contract_by_id(&self, id: &crate::ToolId) -> Option<Arc<ToolContract>> {
                (id == Self::definition().id())
                    .then(|| Arc::new(Self::definition().contract()))
            }

            async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
                ToolOutcome::ok(json!("ok"))
            }
        }

        let registry = ToolRegistry::from_tool_providers(vec![Arc::new(ByIdOnlyProvider)])
            .expect("registry");
        let actual = registry
            .resolve_contract("deferred_contract")
            .expect("by-id-only contract should resolve through the source index");

        assert_eq!(
            serde_json::to_value(actual.as_ref()).expect("serialize actual contract"),
            serde_json::to_value(ByIdOnlyProvider::definition().contract())
                .expect("serialize expected contract")
        );
    }

    #[test]
    fn indexed_contract_lookup_does_not_cross_identity_after_name_drift() {
        struct DriftingProvider {
            definitions: Arc<std::sync::Mutex<Vec<ToolDefinition>>>,
        }

        #[async_trait::async_trait]
        impl ToolProvider for DriftingProvider {
            fn tool_manifests(&self) -> Vec<ToolManifest> {
                self.definitions
                    .lock_recover()
                    .iter()
                    .map(ToolDefinition::manifest)
                    .collect()
            }

            fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
                self.definitions
                    .lock_recover()
                    .iter()
                    .find(|definition| definition.name() == name)
                    .map(|definition| Arc::new(definition.contract()))
            }

            async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
                ToolOutcome::ok(json!("ok"))
            }
        }

        let definitions = Arc::new(std::sync::Mutex::new(vec![ToolDefinition::raw(
            "tool:stable-id",
            "search",
            "original search",
            json!({ "type": "object", "properties": { "query": { "type": "string" } } }),
            json!({ "type": "string" }),
        )]));
        let registry = ToolRegistry::from_tool_providers(vec![Arc::new(DriftingProvider {
            definitions: Arc::clone(&definitions),
        })])
        .expect("registry");

        let reassigned_id = ToolDefinition::raw(
            "tool:stable-id",
            "find",
            "same id with a new name",
            json!({ "type": "object", "properties": { "needle": { "type": "integer" } } }),
            json!({ "type": "integer" }),
        );
        let reused_name = ToolDefinition::raw(
            "tool:different-id",
            "search",
            "old name reassigned to another id",
            json!({ "type": "object", "properties": { "query": { "type": "boolean" } } }),
            json!({ "type": "boolean" }),
        );
        *definitions.lock_recover() =
            vec![reassigned_id.clone(), reused_name.clone()];

        let actual = registry
            .resolve_contract("search")
            .expect("the old by-id path still resolves the stable id");
        let actual = serde_json::to_value(actual.as_ref()).expect("serialize actual contract");
        assert_eq!(
            actual,
            serde_json::to_value(reassigned_id.contract()).expect("serialize by-id contract"),
            "a stale indexed name must fall back to the provider's by-id outcome"
        );
        assert_ne!(
            actual,
            serde_json::to_value(reused_name.contract()).expect("serialize reused-name contract"),
            "a stale indexed name must not return another id's contract"
        );
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
                let context = test_attempt_context();
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

    #[tokio::test]
    async fn single_provider_source_refuses_unknown_id_without_calling_the_provider() {
        let prepares = Arc::new(AtomicUsize::new(0));
        let defer_queries = Arc::new(AtomicUsize::new(0));
        let source = ToolProviderSource::new(
            "single",
            vec![Arc::new(CountingPrepareProvider {
                prepares: Arc::clone(&prepares),
                defer_queries: Arc::clone(&defer_queries),
            }) as Arc<dyn ToolProvider>],
        );

        let prepare_context = crate::ToolPrepareContext::with_execution_binding(
            "registry-test".to_string(),
            Arc::new(crate::testing::MockSessionManager::default()),
            crate::TurnContext::default(),
            Some("unknown-call".to_string()),
            json!({}),
        );
        let refusal = source
            .prepare_tool_call(crate::ToolPrepareCall {
                tool_id: tool_id("unadvertised"),
                pending: crate::sansio::PendingToolCall {
                    call_id: "unknown-call".to_string(),
                    tool_name: "unadvertised".to_string(),
                    args: json!({}),
                    replay: None,
                },
                context: &prepare_context,
            })
            .await
            .expect_err("an unadvertised id is refused before the provider prepare hook runs");
        assert!(format!("{refusal:?}").contains("Unknown tool id"));

        assert!(
            !source.attempt_may_defer(&tool_id("unadvertised")),
            "an unadvertised id never reserves a deferred completion key"
        );

        assert_eq!(
            prepares.load(Ordering::SeqCst),
            0,
            "the provider prepare hook is not invoked for an unadvertised id"
        );
        assert_eq!(
            defer_queries.load(Ordering::SeqCst),
            0,
            "the provider defer capability is not queried for an unadvertised id"
        );

        assert!(
            source.attempt_may_defer(&tool_id("advertised")),
            "an advertised id still reaches the provider"
        );
        assert_eq!(defer_queries.load(Ordering::SeqCst), 1);

        source
            .prepare_tool_call(crate::ToolPrepareCall {
                tool_id: tool_id("advertised"),
                pending: crate::sansio::PendingToolCall {
                    call_id: "advertised-call".to_string(),
                    tool_name: "advertised".to_string(),
                    args: json!({}),
                    replay: None,
                },
                context: &prepare_context,
            })
            .await
            .expect("an advertised id still reaches the provider prepare hook");
        assert_eq!(
            prepares.load(Ordering::SeqCst),
            1,
            "the zero-count assertion above is a real refusal, not a prepare route that refuses everything"
        );
    }

    #[test]
    fn snapshot_resolution_rejects_lazy_live_sources_from_both_lanes() {
        let registry = ToolRegistry::empty();
        registry
            .upsert_source(Arc::new(ToolProviderSource::new(
                "lazy-leaf",
                vec![Arc::new(LazyLeafBatchTool)],
            )))
            .expect("lazy leaf source registered");
        registry
            .upsert_source(Arc::new(LazyOrchestratingBatchSource))
            .expect("lazy orchestrating source registered");

        let mut tools = BTreeMap::new();
        tools.insert(
            tool_id("batch"),
            ToolStateEntry::new(test_tool("batch", "snapshot batch").manifest()),
        );
        let error = registry
            .apply_state(ToolState::new(registry.generation(), tools))
            .expect_err("two live registration lanes resolving one id must collide");

        assert!(matches!(
            error,
            ReconfigureError::CrossLaneToolIdCollision {
                ref tool_id,
                ref leaf_source_id,
            } if tool_id.as_str() == "tool:batch" && leaf_source_id == "lazy-leaf"
        ));
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
    fn refresh_sources_re_reads_multi_provider_manifests() {
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
            .lock_recover()
            .push("dynamic_two".to_string());
        registry.refresh_sources().expect("refresh sources");
        let refreshed = tool_names();
        assert!(refreshed.contains("dynamic_one"));
        assert!(refreshed.contains("dynamic_two"));

        names
            .lock_recover()
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
            .lock_recover()
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
                &test_attempt_context(),
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
            .lock_recover()
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
                &test_attempt_context(),
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
            .lock_recover()
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
                &test_attempt_context(),
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

        let context = test_attempt_context();
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

        let context = crate::testing::mock_attempt_context_from(
            &test_tool_context().with_tool_execution_binding(grant.execution_binding.clone()),
        );
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
                .lock_recover(),
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
        let context = test_attempt_context();
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
    async fn execution_grant_routes_multi_provider_source_by_id_not_name() {
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

            async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
                ToolOutcome::ok(json!(self.result))
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

        let context = test_attempt_context();
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

        let context = test_attempt_context();
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
        let context = test_attempt_context();
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
    async fn crafted_orchestrating_orphan_cannot_block_a_legitimate_leaf_registration() {
        let source = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("source registry");
        let mut crafted_blob =
            serde_json::to_value(source.export_state()).expect("serialize leaf state");
        crafted_blob["tools"]["tool:mock_tool"]["registration_kind"] =
            json!("orchestrating");
        let crafted_snapshot: ToolState =
            serde_json::from_value(crafted_blob).expect("deserialize crafted state");

        let target = ToolRegistry::empty();
        let report = target
            .restore_state(crafted_snapshot)
            .expect("an unresolved crafted entry remains an orphan");
        assert_eq!(report.orphaned, vec![tool_id("mock_tool")]);
        assert!(target.is_orchestrating_tool(&tool_id("mock_tool")));

        let orphan_result = target
            .execute_by_id(&tool_id("mock_tool"), &json!({}), &test_attempt_context())
            .await;
        assert!(
            !orphan_result.is_success(),
            "a claimed lane never makes an orphan executable"
        );
        assert!(format!("{orphan_result:?}").contains("unavailable"));

        target
            .upsert_source(Arc::new(ToolProviderSource::new(
                "legitimate-leaf",
                vec![Arc::new(MockTool)],
            )))
            .expect("the live leaf lane supersedes the stored claim");
        assert!(
            !target.is_orchestrating_tool(&tool_id("mock_tool")),
            "the rebound kind comes from the legitimate live source"
        );
        let rebound = target
            .execute_by_id(&tool_id("mock_tool"), &json!({}), &test_attempt_context())
            .await;
        assert!(rebound.is_success(), "the legitimate leaf executes");
        assert_eq!(rebound.value_for_projection(), json!("ok"));
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

        let context = test_attempt_context();
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

            async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
                ToolOutcome::ok(json!("updated"))
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
                &test_attempt_context(),
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
            async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
                ToolOutcome::ok(json!("ok"))
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
