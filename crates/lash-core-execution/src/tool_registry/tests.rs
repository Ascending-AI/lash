use super::*;
use crate::SessionId;
use crate::ToolDefinition;
use crate::testing::conformance_support::ToolStateConformanceAccess;
use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

mod grant_support;
mod restore_tests;
use grant_support::{GrantBindingProvider, grant_deferral_registry};

struct MockTool;
struct MixedEnabledTool;
struct ExternalMockSource;
#[derive(Clone)]
struct ExactResolvingSource {
    manifest_resolutions: Arc<AtomicUsize>,
    contract_resolutions: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
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
struct LazyOrchestratingBatchSource {
    definition: crate::facade_support::OrchestratingToolDef,
}
struct TestBatchOrchestratingTool;
struct BlockingLiveTool {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
}

fn test_tool(name: &str, description: &str) -> ToolDefinition {
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

fn host_only_snapshot(generation: u64) -> ToolState {
    let mut tools = BTreeMap::new();
    tools.insert(
        tool_id("host_only"),
        ToolStateEntry::new(test_tool("host_only", "host-only").manifest()),
    );
    ToolState::new(generation, tools)
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
        SessionId::from("registry-test"),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::testing::MockSessionManager::default()),
        Arc::new(crate::UnavailableProcessService),
        crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::NativeRuntimeEffectController::default(),
        )),
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

/// Project a leaf attempt outcome back to a plain result for assertions. The
/// projection asserts the outcome carried no declared intents rather than
/// silently discarding them.
#[track_caller]
fn leaf_outcome(outcome: crate::ToolAttemptOutcome) -> ToolOutcome {
    match outcome {
        crate::ToolAttemptOutcome::Done { result, intents } => {
            assert!(
                intents.is_empty(),
                "test leaf executions declare no intents"
            );
            ToolOutcome::from_output(result.into_output())
        }
        crate::ToolAttemptOutcome::Pending(pending) => ToolOutcome::Pending(Box::new(pending)),
    }
}

async fn execute_leaf_by_id(
    registry: &ToolRegistry,
    tool_id: &crate::ToolId,
    args: &serde_json::Value,
    context: &crate::AttemptContext<'_>,
) -> ToolOutcome {
    let Some(manifest) = registry.resolve_manifest_by_id(tool_id) else {
        return ToolOutcome::err_fmt(format!("Unknown tool id: {tool_id}"));
    };
    leaf_outcome(
        registry
            .execute(ToolCall::new(&manifest, args, context))
            .await,
    )
}

#[tokio::test]
async fn internal_execution_route_refuses_non_internal_activation() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    let tool = test_tool_context();
    let context = crate::InternalProcessContext::__for_testing(&tool);

    let manifest = registry
        .resolve_manifest_by_id(&tool_id("mock_tool"))
        .expect("mock tool manifest resolves");
    let result = registry
        .execute_internal_process_tool(crate::InternalProcessToolCall::new(
            &manifest,
            &serde_json::json!({}),
            &context,
        ))
        .await;

    let Err(result) = result else {
        panic!("an Always-activated tool must not cross the internal route")
    };
    assert!(
        result.as_output().value_for_projection()["message"]
            .as_str()
            .is_some_and(|message| message.contains("not activated for internal execution")),
        "the class-boundary refusal must be explicit: {result:?}"
    );
}

#[async_trait::async_trait]
impl ToolProvider for MockTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        manifests(vec![test_tool("mock_tool", "mock")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        contract_from(vec![test_tool("mock_tool", "mock")], name)
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!("ok")).into()
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!("unreachable")).into()
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!("leaf")).into()
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for LazyOrchestratingBatchSource {
    fn id(&self) -> &str {
        "lazy-orchestrating"
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(Self {
            definition: self.definition.clone(),
        }))
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
        (id == &tool_id("batch")).then(|| test_tool("batch", "lazy orchestrating batch").manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        contract_from(vec![test_tool("batch", "lazy orchestrating batch")], name)
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    fn execution(&self) -> ToolSourceExecution<'_> {
        ToolSourceExecution::Orchestrating(&self.definition)
    }
}

#[async_trait::async_trait]
impl crate::facade_support::OrchestratingToolImplementation for TestBatchOrchestratingTool {
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
    let error = match ToolRegistry::from_tool_registrations(
        vec![(
            "orchestrating:tool:batch".to_string(),
            vec![Arc::new(LeafBatchTool) as Arc<dyn ToolProvider>],
        )],
        Vec::new(),
        vec![test_batch_orchestrating_tool()],
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
fn reconciled_combined_collision_reports_duplicate_name() {
    let manifest = test_tool("combined", "combined").manifest();
    let mut surface = ToolSurface::default();
    surface
        .insert(ToolRegistryEntry::new(
            manifest.clone(),
            ToolSourceKey::Leaf("source".to_string()),
            ToolRegistrationKind::Leaf,
        ))
        .expect("initial tool");

    let error = insert_result_entry(
        &mut surface,
        manifest.id.clone(),
        ToolRegistryEntry::new(
            manifest,
            ToolSourceKey::Leaf("source".to_string()),
            ToolRegistrationKind::Leaf,
        ),
    )
    .expect_err("combined id and name collision");

    // A duplicate name is the more informative diagnosis when both id and
    // name collide because it identifies the model-facing alias as well.
    assert_eq!(
        error.to_string(),
        "validation error: duplicate tool name `combined` for tool ids `tool:combined` and `tool:combined`"
    );
}

#[test]
fn registration_kind_alone_selects_orchestration_dispatch() {
    let leaf = ToolRegistry::from_tool_provider_sources(vec![(
        "subagents".to_string(),
        vec![Arc::new(LeafBatchTool) as Arc<dyn ToolProvider>],
    )])
    .expect("leaf ids and plugin ids have no reserved-name semantics");
    assert!(
        !leaf.is_orchestrating_tool(&tool_id("batch")),
        "an impostor plugin id cannot change a leaf registration's kind"
    );

    let orchestrating = ToolRegistry::from_tool_registrations(
        Vec::new(),
        Vec::new(),
        vec![test_batch_orchestrating_tool()],
    )
    .expect("typed orchestrating registration");
    assert!(orchestrating.is_orchestrating_tool(&tool_id("batch")));
}

#[test]
fn pre_cutover_snapshot_without_registration_kind_is_refused() {
    let source = ToolRegistry::from_tool_registrations(
        Vec::new(),
        Vec::new(),
        vec![test_batch_orchestrating_tool()],
    )
    .expect("source registry");
    let mut legacy_blob = serde_json::to_value(source.export_state()).expect("serialize state");
    let legacy_entry = legacy_blob["tools"]["tool:batch"]
        .as_object_mut()
        .expect("serialized batch entry");
    assert_eq!(
        legacy_entry.remove("registration_kind"),
        Some(json!("orchestrating")),
        "the compatibility probe strips exactly the field the cutover requires"
    );

    let error =
        serde_json::from_value::<ToolState>(legacy_blob).expect_err("deserialize must refuse");
    assert!(
        error.to_string().contains("registration_kind"),
        "the refusal must name the missing field: {error}"
    );
}

#[test]
fn pre_cutover_snapshot_without_orphaned_is_refused() {
    let source = ToolRegistry::from_tool_registrations(
        Vec::new(),
        Vec::new(),
        vec![test_batch_orchestrating_tool()],
    )
    .expect("source registry");
    let mut legacy_blob = serde_json::to_value(source.export_state()).expect("serialize state");
    let legacy_entry = legacy_blob["tools"]["tool:batch"]
        .as_object_mut()
        .expect("serialized batch entry");
    assert_eq!(
        legacy_entry.remove("orphaned"),
        Some(json!(false)),
        "the compatibility probe strips exactly the field the cutover requires"
    );

    let error =
        serde_json::from_value::<ToolState>(legacy_blob).expect_err("deserialize must refuse");
    assert!(
        error.to_string().contains("orphaned"),
        "the refusal must name the missing field: {error}"
    );
}

#[tokio::test]
async fn unadvertised_leaf_cannot_smuggle_an_orchestrating_registration() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(LazyLeafBatchTool))
        .expect("unadvertised leaf source");
    let generation = registry.generation();
    assert!(
        registry.resolve_manifest("batch").is_none(),
        "dispatch lookup cannot admit an unadvertised leaf"
    );
    assert_eq!(registry.generation(), generation);
    assert!(!registry.is_orchestrating_tool(&tool_id("batch")));

    registry
        .upsert_source(Arc::new(OrchestratingToolSource::new(
            test_batch_orchestrating_tool(),
        )))
        .expect("the advertised typed registration establishes the live lane");
    assert!(
        registry.is_orchestrating_tool(&tool_id("batch")),
        "only the live typed source can establish the orchestrating lane"
    );

    let leaf_route = execute_leaf_by_id(
        &registry,
        &tool_id("batch"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert!(
        !leaf_route.is_success(),
        "the unadvertised leaf body cannot execute after the typed source is admitted"
    );
    assert!(format!("{leaf_route:?}").contains("is an orchestrating tool"));

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
            test_tool("enabled_tool", "enabled"),
            test_tool("disabled_tool", "disabled"),
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!("ok")).into()
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!("ok")).into()
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!("ok")).into()
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for ExternalMockSource {
    fn id(&self) -> &str {
        "external"
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(Self))
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

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    fn execution(&self) -> ToolSourceExecution<'_> {
        ToolSourceExecution::Leaf(self)
    }
}

#[async_trait::async_trait]
impl LeafToolSourceExecutor for ExternalMockSource {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!({
            "tool": call.name(),
            "args": call.args
        }))
        .into()
    }

    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for ExactResolvingSource {
    fn id(&self) -> &str {
        "exact"
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(self.clone()))
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
        self.manifest_resolutions.fetch_add(1, Ordering::SeqCst);
        (id == &tool_id("host_only")).then(|| test_tool("host_only", "host-only").manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.contract_resolutions.fetch_add(1, Ordering::SeqCst);
        contract_from(vec![test_tool("host_only", "host-only")], name)
    }

    fn resolve_contract_by_id(&self, id: &crate::ToolId) -> Option<Arc<ToolContract>> {
        self.contract_resolutions.fetch_add(1, Ordering::SeqCst);
        (id == &tool_id("host_only"))
            .then(|| Arc::new(test_tool("host_only", "host-only").contract()))
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    fn execution(&self) -> ToolSourceExecution<'_> {
        ToolSourceExecution::Leaf(self)
    }
}

#[async_trait::async_trait]
impl LeafToolSourceExecutor for ExactResolvingSource {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolOutcome::ok(json!(call.name())).into()
    }

    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for NamedExactSource {
    fn id(&self) -> &str {
        self.id
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(Self { id: self.id }))
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
        (id == &tool_id("host_only")).then(|| test_tool("host_only", "host-only").manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<PreparedToolCall, ToolOutcome> {
        Ok(PreparedToolCall::identity(call.tool_id, call.pending))
    }

    fn execution(&self) -> ToolSourceExecution<'_> {
        ToolSourceExecution::Leaf(self)
    }
}

#[async_trait::async_trait]
impl LeafToolSourceExecutor for NamedExactSource {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(call.name())).into()
    }

    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
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

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(call.name())).into()
    }
}

#[async_trait::async_trait]
impl ToolProvider for BlockingLiveTool {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        manifests(vec![test_tool("blocking_live", "blocking live tool")])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        contract_from(vec![test_tool("blocking_live", "blocking live tool")], name)
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("release blocking live tool")
            .forget();
        ToolOutcome::ok(json!("completed from captured registry")).into()
    }
}

#[test]
fn indexed_contract_lookup_reuses_the_indexed_manifest() {
    let manifest_reads = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::from_tool_providers(vec![Arc::new(CountingManifestProvider {
        manifest_reads: Arc::clone(&manifest_reads),
    })])
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
            (id == Self::definition().id()).then(|| Arc::new(Self::definition().contract()))
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("ok")).into()
        }
    }

    let registry =
        ToolRegistry::from_tool_providers(vec![Arc::new(ByIdOnlyProvider)]).expect("registry");
    let actual = registry
        .resolve_contract("deferred_contract")
        .expect("by-id-only contract should resolve through the source index");

    assert_eq!(
        serde_json::to_value(actual.as_ref()).expect("serialize actual contract"),
        serde_json::to_value(ByIdOnlyProvider::definition().contract())
            .expect("serialize expected contract")
    );
}

/// Pinned behaviour: after a provider renames an id (`search` → `find`) and
/// hands the old name to a different id, a contract lookup routed by the
/// stable id must observe the id's current contract — never the contract of
/// whatever now answers to the stale name. The provider's live name
/// resolution is authoritative for the name→contract hop, and
/// `resolve_contract_for_manifest` is the single enforcement point that
/// rejects a name-resolved contract whose identity no longer matches the
/// manifest the id lookup established.
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

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("ok")).into()
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
    *definitions.lock_recover() = vec![reassigned_id.clone(), reused_name.clone()];

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
    let registry = ToolRegistry::from_tool_provider(Arc::new(MixedEnabledTool)).expect("registry");
    let snapshot = registry.export_state();
    assert!(snapshot.get(&tool_id("enabled_tool")).unwrap().is_member());
    assert!(snapshot.get(&tool_id("disabled_tool")).unwrap().is_member());
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
        root.compose_session_catalog(Vec::new())
            .expect("compose pre-removal session snapshot"),
    );
    let executing = crate::task::spawn({
        let captured = Arc::clone(&captured);
        async move {
            let args = json!({});
            let context = test_attempt_context();
            let manifest = captured
                .resolve_manifest("blocking_live")
                .expect("captured registry resolves blocking_live");
            captured
                .execute(ToolCall::new(&manifest, &args, &context))
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
        .compose_session_catalog(Vec::new())
        .expect("compose post-removal session snapshot");
    assert!(
        refreshed.resolve_contract("blocking_live").is_none(),
        "subsequent session composition must miss the removed provider"
    );

    release.add_permits(1);
    let completed = leaf_outcome(executing.await.expect("join captured-registry execution"));
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
        .apply_state(snapshot.with_generation_for_conformance(target_registry.generation()))
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
        ToolStateEntry::new(test_tool("missing", "missing").manifest()),
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
        .apply_state(snapshot.with_generation_for_conformance(target_registry.generation()))
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
        ToolStateEntry::new(test_tool("host_only", "host-only").manifest()),
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
        SessionId::from("registry-test"),
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
        .upsert_source(Arc::new(LazyOrchestratingBatchSource {
            definition: test_batch_orchestrating_tool(),
        }))
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

    names.lock_recover().push("dynamic_two".to_string());
    registry.refresh_sources().expect("refresh sources");
    let refreshed = tool_names();
    assert!(refreshed.contains("dynamic_one"));
    assert!(refreshed.contains("dynamic_two"));

    names.lock_recover().retain(|name| name != "dynamic_one");
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
    let source =
        ToolRegistry::from_tool_providers(vec![Arc::clone(&provider)]).expect("source registry");
    let snapshot = source.export_state();

    names.lock_recover().push("dynamic_two".to_string());
    let resumed = ToolRegistry::from_tool_providers(vec![provider]).expect("cold resume registry");
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
    let result = execute_leaf_by_id(
        &resumed,
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
    names.lock_recover().push("dynamic_two".to_string());

    let fork = registry.fork_with_state(snapshot).expect("live fork");
    assert!(
        fork.export_state()
            .get(&tool_id("dynamic_two"))
            .is_some_and(ToolStateEntry::is_member)
    );
    let result = execute_leaf_by_id(
        &fork,
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
    names.lock_recover().push("dynamic_two".to_string());

    let composed = registry
        .compose_session_catalog(Vec::new())
        .expect("composed live catalog");
    assert!(
        composed
            .export_state()
            .get(&tool_id("dynamic_two"))
            .is_some_and(ToolStateEntry::is_member)
    );
    let result = execute_leaf_by_id(
        &composed,
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
async fn dispatch_manifest_lookup_does_not_mutate_registry_generation() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ExactResolvingSource {
            manifest_resolutions: Arc::new(AtomicUsize::new(0)),
            contract_resolutions: Arc::new(AtomicUsize::new(0)),
            executions: Arc::new(AtomicUsize::new(0)),
        }))
        .expect("source registered");
    let generation_before_dispatch = registry.generation();

    let args = json!({});
    let attempt = test_attempt_context();
    let manifest = test_tool("host_only", "host-only").manifest();
    let result = leaf_outcome(
        registry
            .execute(crate::ToolCall::new(&manifest, &args, &attempt))
            .await,
    );

    assert!(!result.is_success());
    assert_eq!(
        registry.generation(),
        generation_before_dispatch,
        "dispatch-path manifest lookup must not mutate registry state"
    );
}

#[tokio::test]
async fn unadmitted_exact_manifest_is_not_dispatchable() {
    let manifest_resolutions = Arc::new(AtomicUsize::new(0));
    let contract_resolutions = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ExactResolvingSource {
            manifest_resolutions: Arc::clone(&manifest_resolutions),
            contract_resolutions: Arc::clone(&contract_resolutions),
            executions: Arc::clone(&executions),
        }))
        .expect("source registered");

    assert_eq!(
        registry
            .resolve_manifest("host_only")
            .map(|manifest| manifest.name),
        None
    );
    assert_eq!(manifest_resolutions.load(Ordering::SeqCst), 0);

    let contract = registry.resolve_contract("host_only");
    assert!(contract.is_none());
    assert_eq!(manifest_resolutions.load(Ordering::SeqCst), 0);
    assert_eq!(contract_resolutions.load(Ordering::SeqCst), 0);

    let context = test_attempt_context();
    let args = json!({});
    let manifest = test_tool("host_only", "host-only").manifest();
    let result = leaf_outcome(
        registry
            .execute(crate::ToolCall::new(&manifest, &args, &context))
            .await,
    );
    assert!(!result.is_success());
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn execution_grant_routes_through_ordinary_provider_contexts_without_catalog_membership() {
    let prepared_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executed_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ToolProviderSource::new(
            "exact",
            vec![Arc::new(GrantBindingProvider {
                prepared_bindings: Arc::clone(&prepared_bindings),
                executed_bindings: Arc::clone(&executed_bindings),
            })],
        )))
        .expect("source registered");
    let registry = registry
        .compose_session_catalog(Vec::new())
        .expect("resident catalog with live grant sources");

    assert!(!registry.export_state().contains(&tool_id("host_only")));
    assert!(
        !registry
            .tool_manifests()
            .iter()
            .any(|manifest| manifest.name == "host_only")
    );

    let grant = crate::ToolExecutionGrant::from_definition(test_tool("host_only", "host-only"))
        .with_source_id("exact")
        .with_execution_binding(json!({ "kind": "test", "route": "grant" }));
    let prepare_context = crate::ToolPrepareContext::with_execution_binding(
        SessionId::from("registry-test"),
        Arc::new(crate::testing::MockSessionManager::default()),
        crate::TurnContext::default(),
        Some("grant-call".to_string()),
        grant.execution_binding.clone(),
    )
    .with_granted_source_id(grant.source_id.clone());
    let prepared = registry
        .prepare_tool_call(crate::ToolPrepareCall {
            tool_id: grant.manifest().id.clone(),
            pending: crate::sansio::PendingToolCall {
                call_id: "grant-call".to_string(),
                tool_name: grant.manifest().name.clone(),
                args: json!({}),
                replay: None,
            },
            context: &prepare_context,
        })
        .await
        .expect("grant prepare");
    assert_eq!(prepared.tool_id, grant.manifest().id);

    let context = crate::testing::mock_attempt_context_from(
        &test_tool_context()
            .with_tool_execution_binding(grant.execution_binding.clone())
            .with_granted_source_id(grant.source_id.clone()),
    );
    let args = json!({});
    let result = leaf_outcome(
        registry
            .execute(ToolCall::new(grant.manifest(), &args, &context))
            .await,
    );
    assert!(result.is_success());
    assert_eq!(result.value_for_projection(), json!("host_only"));

    assert!(!registry.export_state().contains(&tool_id("host_only")));
    assert!(
        !registry
            .tool_manifests()
            .iter()
            .any(|manifest| manifest.name == "host_only")
    );
    assert_eq!(
        *prepared_bindings.lock_recover(),
        vec![json!({ "kind": "test", "route": "grant" })]
    );
    assert_eq!(
        *executed_bindings.lock_recover(),
        vec![json!({ "kind": "test", "route": "grant" })]
    );
}

/// `run_tool_granted` stands up the same granted route dispatch builds for a
/// grant-admitted call, so a host can exercise its granted branch without a
/// live turn (FIG-3436).
#[tokio::test]
async fn run_tool_granted_honors_the_granted_source_binding() {
    let executed_bindings = Arc::new(std::sync::Mutex::new(Vec::new()));
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ToolProviderSource::new(
            "exact",
            vec![Arc::new(GrantBindingProvider {
                prepared_bindings: Arc::new(std::sync::Mutex::new(Vec::new())),
                executed_bindings: Arc::clone(&executed_bindings),
            })],
        )))
        .expect("source registered");
    let registry = registry
        .compose_session_catalog(Vec::new())
        .expect("resident catalog with live grant sources");

    let grant = crate::ToolExecutionGrant::from_definition(test_tool("host_only", "host-only"))
        .with_source_id("exact")
        .with_execution_binding(json!({ "kind": "test", "route": "grant" }));
    let args = json!({});

    // The catalog route cannot admit the tool: the grant is the authority,
    // and `run_tool` keeps building the ungranted route.
    let ungranted = leaf_outcome(crate::testing::run_tool(&registry, "host_only", &args).await);
    assert!(!ungranted.is_success());

    let result = leaf_outcome(crate::testing::run_tool_granted(&registry, &grant, &args).await);
    assert!(result.is_success());
    assert_eq!(result.value_for_projection(), json!("host_only"));
    assert_eq!(
        *executed_bindings.lock_recover(),
        vec![json!({ "kind": "test", "route": "grant" })]
    );
}

#[test]
fn granted_deferred_source_reports_attempt_may_defer() {
    let registry = grant_deferral_registry(true);

    assert!(registry.attempt_may_defer_for_grant(&tool_id("host_only"), Some("grant-source")));
}

#[test]
fn granted_non_deferred_source_reports_attempt_cannot_defer() {
    let registry = grant_deferral_registry(false);

    assert!(!registry.attempt_may_defer_for_grant(&tool_id("host_only"), Some("grant-source")));
}

#[tokio::test]
async fn execution_grant_without_source_does_not_infer_registry_route() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(MockTool)).expect("registry");
    registry
        .upsert_source(Arc::new(ExactResolvingSource {
            manifest_resolutions: Arc::new(AtomicUsize::new(0)),
            contract_resolutions: Arc::new(AtomicUsize::new(0)),
            executions: Arc::new(AtomicUsize::new(0)),
        }))
        .expect("source registered");

    let grant = crate::ToolExecutionGrant::from_definition(test_tool("host_only", "host-only"));
    let context = crate::testing::mock_attempt_context_from(
        &test_tool_context().with_granted_source_id(grant.source_id.clone()),
    );
    let args = json!({});
    let result = leaf_outcome(
        registry
            .execute(ToolCall::new(grant.manifest(), &args, &context))
            .await,
    );

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

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!(self.result)).into()
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
    let registry = registry
        .compose_session_catalog(Vec::new())
        .expect("resident snapshot keeps hidden providers out of its admitted source");
    let grant = crate::ToolExecutionGrant::from_definition(ToolDefinition::raw(
        "tool:hidden_zeta",
        "shared_hidden_name",
        "grant selects the second hidden provider by id",
        ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    ))
    .with_source_id(crate::PLUGIN_TOOL_SOURCE_ID);

    let context = crate::testing::mock_attempt_context_from(
        &test_tool_context().with_granted_source_id(grant.source_id.clone()),
    );
    let args = json!({});
    let result = leaf_outcome(
        registry
            .execute(ToolCall::new(grant.manifest(), &args, &context))
            .await,
    );

    assert!(result.is_success());
    assert_eq!(result.value_for_projection(), json!("right-provider"));
    assert!(
        registry.export_state().entries().is_empty(),
        "grant execution must not add hidden providers to registry state"
    );
}

#[tokio::test]
async fn pinned_source_preserves_provider_execute_result_and_intents() {
    struct IntentProvider;

    impl IntentProvider {
        fn definition() -> ToolDefinition {
            test_tool("intent_route", "ordered intent witness")
        }

        fn intent() -> crate::ToolIntent {
            crate::ToolIntent::EmitProcessEvent(crate::EmitProcessEventIntent {
                session_id: SessionId::from("registry-test"),
                process_id: crate::ProcessId::from("pinned-process"),
                event_type: "pinned.intent".to_string(),
                payload: json!({ "route": "id" }),
            })
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for IntentProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![Self::definition()])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            (name == Self::definition().name()).then(|| Arc::new(Self::definition().contract()))
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolAttemptOutcome::done(
                crate::ToolOutcomeDone::ok(json!("id-route")),
                crate::ToolIntents::v3(vec![Self::intent()]),
            )
        }
    }

    let registry = ToolRegistry::from_tool_provider(Arc::new(IntentProvider))
        .expect("intent provider registry")
        .compose_session_catalog(Vec::new())
        .expect("pinned intent provider registry");
    let id = tool_id("intent_route");
    let args = json!({});
    let attempt = test_attempt_context();

    let manifest = registry
        .resolve_manifest_by_id(&id)
        .expect("intent manifest");
    let outcome = registry
        .execute(ToolCall::new(&manifest, &args, &attempt))
        .await;
    let crate::ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("the single execute route completes")
    };
    assert_eq!(
        result.into_output().value_for_projection(),
        json!("id-route")
    );
    let [intent] = intents.intents.as_slice() else {
        panic!("the single execute route returns its declared intents")
    };
    let crate::ToolIntent::EmitProcessEvent(intent) = intent else {
        panic!("the declared intent reaches the caller verbatim")
    };
    assert_eq!(intent.event_type, "pinned.intent");
    assert_eq!(intent.process_id, crate::ProcessId::from("pinned-process"));
    assert_eq!(intent.session_id, SessionId::from("registry-test"));
    assert_eq!(intent.payload, json!({ "route": "id" }));
}

#[tokio::test]
async fn pinned_source_retains_exactly_known_nonadvertised_resident_id() {
    struct KnownResidentProvider;

    impl KnownResidentProvider {
        fn definition() -> ToolDefinition {
            test_tool("known_resident", "known but not advertised")
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for KnownResidentProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
            (id == Self::definition().id()).then(|| Self::definition().manifest())
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
            None
        }

        fn resolve_contract_by_id(&self, id: &crate::ToolId) -> Option<Arc<ToolContract>> {
            (id == Self::definition().id()).then(|| Arc::new(Self::definition().contract()))
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("known-resident")).into()
        }
    }

    let registry = ToolRegistry::from_tool_provider(Arc::new(KnownResidentProvider))
        .expect("known resident provider registry");
    let mut entries = BTreeMap::new();
    entries.insert(
        tool_id("known_resident"),
        ToolStateEntry::new(KnownResidentProvider::definition().manifest()),
    );
    registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("the exact-id resolver restores the resident binding");

    let pinned = registry
        .compose_session_catalog(Vec::new())
        .expect("known resident survives request refresh");
    let entry = pinned
        .export_state()
        .get(&tool_id("known_resident"))
        .expect("known resident remains in state")
        .clone();
    assert!(entry.is_member(), "resident curation remains admitted");
    assert!(!entry.is_orphaned(), "the exact live route remains bound");

    let result = execute_leaf_by_id(
        &pinned,
        &tool_id("known_resident"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert_eq!(result.value_for_projection(), json!("known-resident"));
}

#[tokio::test]
async fn resident_snapshot_refuses_mismatched_known_id_without_overwriting_advertised_route() {
    struct AdvertisedProvider;

    #[async_trait::async_trait]
    impl ToolProvider for AdvertisedProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool("advertised", "advertised route")])
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
            contract_from(vec![test_tool("advertised", "advertised route")], name)
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("advertised-route")).into()
        }
    }

    struct KnownIdProvider {
        mismatched: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl ToolProvider for KnownIdProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            Vec::new()
        }

        fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
            (id == &tool_id("known")).then(|| {
                if self.mismatched.load(Ordering::SeqCst) {
                    test_tool("advertised", "malformed known-id route").manifest()
                } else {
                    test_tool("known", "valid known-id route").manifest()
                }
            })
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
            None
        }

        fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
            (id == &tool_id("known"))
                .then(|| Arc::new(test_tool("known", "valid known-id route").contract()))
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("malformed-route")).into()
        }
    }

    let mismatched = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let registry = ToolRegistry::from_tool_providers(vec![
        Arc::new(AdvertisedProvider),
        Arc::new(KnownIdProvider {
            mismatched: Arc::clone(&mismatched),
        }),
    ])
    .expect("grouped provider registry");
    let mut restored = BTreeMap::new();
    restored.insert(
        tool_id("known"),
        ToolStateEntry::new(test_tool("known", "persisted known resident").manifest()),
    );
    registry
        .restore_state(ToolState::new(registry.generation(), restored))
        .expect("valid exact-id route restores the known resident");
    let before = serde_json::to_value(registry.export_state()).expect("serialize state");

    mismatched.store(true, Ordering::SeqCst);
    let pin = registry.compose_session_catalog(Vec::new());
    let error = pin.err().map(|error| error.to_string());
    let after = serde_json::to_value(registry.export_state()).expect("serialize state");
    let advertised = execute_leaf_by_id(
        &registry,
        &tool_id("advertised"),
        &json!({}),
        &test_attempt_context(),
    )
    .await
    .value_for_projection();

    assert!(
        error.is_some() && before == after && advertised == json!("advertised-route"),
        "mismatched known-id pin must refuse without changing state or the advertised route: \
         error={error:?}, state_unchanged={}, advertised={advertised}",
        before == after,
    );
    assert_eq!(
        error.as_deref(),
        Some(
            "validation error: source `plugins` resolved tool id `tool:known` with mismatched \
             manifest id `tool:advertised`"
        )
    );
}

#[test]
fn unadmitted_alias_lookup_does_not_fall_through_to_source() {
    struct IdOccupiedProvider;

    #[async_trait::async_trait]
    impl ToolProvider for IdOccupiedProvider {
        fn tool_manifests(&self) -> Vec<ToolManifest> {
            manifests(vec![test_tool("occupied", "advertised manifest")])
        }

        fn resolve_manifest_by_id(&self, id: &crate::ToolId) -> Option<ToolManifest> {
            (id == &tool_id("alias")).then(|| {
                ToolDefinition::raw(
                    "tool:occupied",
                    "resolved_alias",
                    "lazy alias manifest",
                    ToolDefinition::default_input_schema(),
                    json!({ "type": "string" }),
                )
                .manifest()
            })
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
            None
        }

        async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            ToolOutcome::ok(json!("unreachable")).into()
        }
    }

    let registry =
        ToolRegistry::from_tool_provider(Arc::new(IdOccupiedProvider)).expect("registry");

    assert!(registry.resolve_manifest_by_id(&tool_id("alias")).is_none());
    assert_eq!(
        registry
            .resolve_manifest_by_id(&tool_id("occupied"))
            .expect("advertised id remains indexed")
            .description,
        "advertised manifest"
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
    let manifest = registry
        .resolve_manifest("mcp__demo__search")
        .expect("registered external tool resolves");
    let result = leaf_outcome(
        registry
            .execute(crate::ToolCall::new(&manifest, &args, &context))
            .await,
    );
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
