use super::*;
use crate::{SessionId, ToolDefinition};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

struct ToggleExactProvider {
    active: Arc<AtomicBool>,
    definition: ToolDefinition,
    route: &'static str,
}

struct HiddenOrchestratingSource;

struct DefaultHiddenProvider {
    definition: ToolDefinition,
}

#[repr(transparent)]
struct FilteringProvider {
    inner: DefaultHiddenProvider,
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

fn tool_id(name: &str) -> ToolId {
    ToolId::from(format!("tool:{name}"))
}

fn test_attempt_context() -> crate::AttemptContext<'static> {
    let tool_context = crate::ToolContext::builder(
        SessionId::from("registry-pinning-test"),
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
    .build();
    crate::testing::mock_attempt_context_from(&tool_context)
}

#[async_trait::async_trait]
impl ToolProvider for ToggleExactProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        (self.active.load(Ordering::SeqCst) && id == self.definition.id())
            .then(|| self.definition.manifest())
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        (self.active.load(Ordering::SeqCst) && id == self.definition.id())
            .then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(json!(self.route))
    }
}

#[async_trait::async_trait]
impl ToolProvider for DefaultHiddenProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![self.definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(json!("delegated-default"))
    }
}

#[async_trait::async_trait]
impl ToolProvider for FilteringProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.inner.resolve_manifest_by_id(id)
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        self.inner.resolve_contract(name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        self.inner.execute(call).await
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for HiddenOrchestratingSource {
    fn id(&self) -> &str {
        "hidden-orchestrating"
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(Self))
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
        (id == &tool_id("batch")).then(|| test_tool("batch", "hidden orchestrating").manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        (name == "batch").then(|| Arc::new(test_tool("batch", "hidden orchestrating").contract()))
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

#[test]
fn request_pin_detects_hidden_cross_lane_known_id_collision() {
    let leaf_active = Arc::new(AtomicBool::new(false));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(HiddenOrchestratingSource))
        .expect("hidden orchestrating source registered");
    registry
        .upsert_source(Arc::new(ToolProviderSource::new(
            "hidden-leaf",
            vec![Arc::new(ToggleExactProvider {
                active: Arc::clone(&leaf_active),
                definition: test_tool("batch", "hidden leaf batch"),
                route: "leaf",
            })],
        )))
        .expect("inactive hidden leaf source registered");
    let mut entries = BTreeMap::new();
    entries.insert(
        tool_id("batch"),
        ToolStateEntry::new(test_tool("batch", "persisted batch").manifest()),
    );
    registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("the hidden orchestrating route initially binds the resident");

    leaf_active.store(true, Ordering::SeqCst);
    let error = registry
        .compose_session_catalog(true, Vec::new())
        .err()
        .expect("the new hidden leaf route collides during request pinning");
    assert!(
        matches!(
            error,
            ReconfigureError::CrossLaneToolIdCollision {
                ref tool_id,
                ref leaf_source_id,
            } if tool_id.as_str() == "tool:batch" && leaf_source_id == "hidden-leaf"
        ),
        "unexpected collision error: {error:?}"
    );
}

#[tokio::test]
async fn request_pin_preserves_transparent_wrapper_delegated_default_resolution() {
    let definition = test_tool("wrapped_hidden", "delegated default resolver");
    let provider = FilteringProvider {
        inner: DefaultHiddenProvider {
            definition: definition.clone(),
        },
    };
    assert!(
        provider.resolve_manifest_by_id(definition.id()).is_some(),
        "the wrapper delegates exact-id resolution to the inner default"
    );
    let registry =
        ToolRegistry::from_tool_provider(Arc::new(provider)).expect("filtering wrapper registry");
    let mut entries = BTreeMap::new();
    entries.insert(
        definition.id().clone(),
        ToolStateEntry::new(definition.manifest()),
    );
    registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("the delegated default initially binds the known resident");

    let pinned = registry
        .compose_session_catalog(true, Vec::new())
        .expect("two-phase capture preserves the delegated default result");
    let entry = pinned
        .export_state()
        .get(definition.id())
        .expect("wrapped resident remains in state")
        .clone();
    assert!(entry.is_member());
    assert!(!entry.is_orphaned());
    let result = pinned
        .execute_by_id(definition.id(), &json!({}), &test_attempt_context())
        .await;
    assert_eq!(result.value_for_projection(), json!("delegated-default"));
}

#[tokio::test]
async fn pinned_source_rebinds_nonadvertised_orphan_when_its_provider_returns() {
    let available = Arc::new(AtomicBool::new(false));
    let definition = test_tool("returning_resident", "returning nonadvertised resident");
    let registry = ToolRegistry::from_tool_provider(Arc::new(ToggleExactProvider {
        active: Arc::clone(&available),
        definition: definition.clone(),
        route: "returning-resident",
    }))
    .expect("returning provider registry");
    let mut entries = BTreeMap::new();
    entries.insert(
        tool_id("returning_resident"),
        ToolStateEntry::new(definition.manifest()),
    );
    let report = registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("unavailable exact-id route restores as an orphan");
    assert_eq!(report.orphaned, vec![tool_id("returning_resident")]);

    available.store(true, Ordering::SeqCst);
    let pinned = registry
        .compose_session_catalog(true, Vec::new())
        .expect("returning exact-id route rebinds during request refresh");
    let entry = pinned
        .export_state()
        .get(&tool_id("returning_resident"))
        .expect("returning resident remains in state")
        .clone();
    assert!(entry.is_member(), "returning resident is admitted again");
    assert!(!entry.is_orphaned(), "returning resident is rebound");

    let result = pinned
        .execute_by_id(
            &tool_id("returning_resident"),
            &json!({}),
            &test_attempt_context(),
        )
        .await;
    assert_eq!(result.value_for_projection(), json!("returning-resident"));
}

#[tokio::test]
async fn pinned_source_rebinds_known_id_to_another_nonadvertising_source() {
    let a_active = Arc::new(AtomicBool::new(true));
    let b_active = Arc::new(AtomicBool::new(false));
    let definition = test_tool("moving_resident", "moving nonadvertised resident");
    let registry = ToolRegistry::from_tool_provider_sources(vec![
        (
            "moving-a".to_string(),
            vec![Arc::new(ToggleExactProvider {
                active: Arc::clone(&a_active),
                definition: definition.clone(),
                route: "route-a",
            }) as Arc<dyn ToolProvider>],
        ),
        (
            "moving-b".to_string(),
            vec![Arc::new(ToggleExactProvider {
                active: Arc::clone(&b_active),
                definition: definition.clone(),
                route: "route-b",
            }) as Arc<dyn ToolProvider>],
        ),
    ])
    .expect("moving provider registry");
    let mut entries = BTreeMap::new();
    entries.insert(
        tool_id("moving_resident"),
        ToolStateEntry::new(definition.manifest()),
    );
    registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("source A initially resolves the resident");

    a_active.store(false, Ordering::SeqCst);
    b_active.store(true, Ordering::SeqCst);
    let pinned = registry
        .compose_session_catalog(true, Vec::new())
        .expect("source B rebinds the known resident");
    let result = pinned
        .execute_by_id(
            &tool_id("moving_resident"),
            &json!({}),
            &test_attempt_context(),
        )
        .await;
    assert_eq!(result.value_for_projection(), json!("route-b"));
}
