use super::*;
use crate::{SessionId, ToolDefinition};
use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};

struct ToggleExactProvider {
    active: Arc<AtomicBool>,
    definition: ToolDefinition,
    route: &'static str,
}

struct HiddenOrchestratingSource {
    definition: crate::facade_support::OrchestratingToolDef,
}

struct HiddenOrchestratingTool;

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
            crate::testing::UnavailableEffectController,
        )),
        Arc::new(crate::SessionAttachmentStore::unavailable()),
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(self.route)).into()
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

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!("delegated-default")).into()
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

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.inner.execute(call).await
    }
}

#[async_trait::async_trait]
impl crate::facade_support::OrchestratingToolImplementation for HiddenOrchestratingTool {
    fn manifest(&self) -> ToolManifest {
        test_tool("batch", "hidden orchestrating").manifest()
    }

    fn contract(&self) -> Arc<ToolContract> {
        Arc::new(test_tool("batch", "hidden orchestrating").contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        _context: &crate::facade_support::OrchestrationContext<'_>,
    ) -> ToolOutcome {
        ToolOutcome::err_fmt("hidden orchestrating probe is never executed")
    }
}

fn hidden_orchestrating_source() -> HiddenOrchestratingSource {
    HiddenOrchestratingSource {
        definition: crate::facade_support::OrchestratingToolDef::new(Arc::new(
            HiddenOrchestratingTool,
        )),
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
        Ok(Arc::new(hidden_orchestrating_source()))
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

/// The projection asserts the outcome carries no declared intents before unwrapping the
/// completed result.
async fn execute_leaf_by_id(
    registry: &ToolRegistry,
    tool_id: &ToolId,
    args: &serde_json::Value,
    context: &crate::AttemptContext<'_>,
) -> ToolOutcome {
    let Some(manifest) = registry.resolve_manifest_by_id(tool_id) else {
        return ToolOutcome::err_fmt(format!("Unknown tool id: {tool_id}"));
    };
    match registry
        .execute(crate::ToolCall::new(&manifest, args, context))
        .await
    {
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

#[test]
fn request_pin_detects_hidden_cross_lane_known_id_collision() {
    let leaf_active = Arc::new(AtomicBool::new(false));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(hidden_orchestrating_source()))
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
        .compose_session_catalog(Vec::new())
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
        .compose_session_catalog(Vec::new())
        .expect("two-phase capture preserves the delegated default result");
    let entry = pinned
        .export_state()
        .get(definition.id())
        .expect("wrapped resident remains in state")
        .clone();
    assert!(entry.is_member());
    assert!(!entry.is_orphaned());
    let result = execute_leaf_by_id(
        &pinned,
        definition.id(),
        &json!({}),
        &test_attempt_context(),
    )
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
    assert_eq!(report.lost_members, vec![tool_id("returning_resident")]);

    available.store(true, Ordering::SeqCst);
    let pinned = registry
        .compose_session_catalog(Vec::new())
        .expect("returning exact-id route rebinds during request refresh");
    let entry = pinned
        .export_state()
        .get(&tool_id("returning_resident"))
        .expect("returning resident remains in state")
        .clone();
    assert!(entry.is_member(), "returning resident is admitted again");
    assert!(!entry.is_orphaned(), "returning resident is rebound");

    let result = execute_leaf_by_id(
        &pinned,
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
        .compose_session_catalog(Vec::new())
        .expect("source B rebinds the known resident");
    let result = execute_leaf_by_id(
        &pinned,
        &tool_id("moving_resident"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert_eq!(result.value_for_projection(), json!("route-b"));
}

/// A curated snapshot may rename the model-facing name on a known tool id; the
/// pinned resident route must still hand the provider the manifest it
/// advertised for that id, and a provider swap must hand the new provider its
/// own manifest — exact id and provider-facing name, never the alias.
#[tokio::test]
async fn pinned_source_executes_with_the_provider_manifest_under_alias_drift_and_provider_swap() {
    struct NameRecordingExactProvider {
        active: Arc<AtomicBool>,
        definition: ToolDefinition,
        seen: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl ToolProvider for NameRecordingExactProvider {
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

        async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
            self.seen
                .lock_recover()
                .push((call.tool_id().to_string(), call.name().to_string()));
            ToolOutcome::ok(json!({ "provider": call.name() })).into()
        }
    }

    fn drifted_provider_definition(name: &str) -> ToolDefinition {
        ToolDefinition::raw(
            "tool:drifted_resident",
            name,
            "provider-facing manifest for the drifted resident",
            ToolDefinition::default_input_schema(),
            json!({ "type": "string" }),
        )
    }

    let a_active = Arc::new(AtomicBool::new(true));
    let b_active = Arc::new(AtomicBool::new(false));
    let a_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let b_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let registry = ToolRegistry::from_tool_provider_sources(vec![
        (
            "drifted-a".to_string(),
            vec![Arc::new(NameRecordingExactProvider {
                active: Arc::clone(&a_active),
                definition: drifted_provider_definition("provider_alpha"),
                seen: Arc::clone(&a_seen),
            }) as Arc<dyn ToolProvider>],
        ),
        (
            "drifted-b".to_string(),
            vec![Arc::new(NameRecordingExactProvider {
                active: Arc::clone(&b_active),
                definition: drifted_provider_definition("provider_beta"),
                seen: Arc::clone(&b_seen),
            }) as Arc<dyn ToolProvider>],
        ),
    ])
    .expect("two-source resident registry");

    // The snapshot pins the same tool id under a curated model-facing alias.
    let mut entries = BTreeMap::new();
    entries.insert(
        tool_id("drifted_resident"),
        ToolStateEntry::new(
            ToolDefinition::raw(
                "tool:drifted_resident",
                "curated_alias",
                "snapshot-curated alias for the drifted resident",
                ToolDefinition::default_input_schema(),
                json!({ "type": "string" }),
            )
            .manifest(),
        ),
    );
    registry
        .restore_state(ToolState::new(registry.generation(), entries))
        .expect("the exact-id resident route rebinds under a curated alias");

    let pinned = registry
        .compose_session_catalog(Vec::new())
        .expect("source A pins the drifted resident");
    // A deferred grant pins the curated model-facing manifest at grant time;
    // the provider must still see its own advertised manifest.
    let curated = ToolDefinition::raw(
        "tool:drifted_resident",
        "curated_alias",
        "grant-pinned alias for the drifted resident",
        ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    )
    .manifest();
    let attempt = test_attempt_context();
    let outcome = pinned
        .execute(ToolCall::new(&curated, &json!({}), &attempt))
        .await;
    let result = match outcome {
        crate::ToolAttemptOutcome::Done { result, intents } => {
            assert!(
                intents.is_empty(),
                "the drifted resident declares no intents"
            );
            ToolOutcome::from_output(result.into_output())
        }
        crate::ToolAttemptOutcome::Pending(pending) => ToolOutcome::Pending(Box::new(pending)),
    };
    assert_eq!(
        result.value_for_projection(),
        json!({ "provider": "provider_alpha" })
    );
    assert_eq!(
        a_seen.lock_recover().as_slice(),
        &[(
            "tool:drifted_resident".to_string(),
            "provider_alpha".to_string()
        )],
        "provider A must see its own manifest's id and name, not the alias",
    );

    a_active.store(false, Ordering::SeqCst);
    b_active.store(true, Ordering::SeqCst);
    let pinned = registry
        .compose_session_catalog(Vec::new())
        .expect("source B rebinds the drifted resident");
    let outcome = pinned
        .execute(ToolCall::new(&curated, &json!({}), &attempt))
        .await;
    let result = match outcome {
        crate::ToolAttemptOutcome::Done { result, intents } => {
            assert!(
                intents.is_empty(),
                "the drifted resident declares no intents"
            );
            ToolOutcome::from_output(result.into_output())
        }
        crate::ToolAttemptOutcome::Pending(pending) => ToolOutcome::Pending(Box::new(pending)),
    };
    assert_eq!(
        result.value_for_projection(),
        json!({ "provider": "provider_beta" })
    );
    assert_eq!(
        b_seen.lock_recover().as_slice(),
        &[(
            "tool:drifted_resident".to_string(),
            "provider_beta".to_string()
        )],
        "provider B must see its own manifest's id and name after the swap",
    );
}
