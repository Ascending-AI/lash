use super::super::*;
use lash_sansio::sync::MutexExt;
use serde_json::json;
use std::sync::{
    Arc, Barrier, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

struct MutableAdmissionSource {
    id: &'static str,
    names: Arc<Mutex<Vec<String>>>,
    block_once: AtomicBool,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl MutableAdmissionSource {
    fn ungated(id: &'static str, names: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            id,
            names,
            block_once: AtomicBool::new(false),
            entered: Arc::new(Barrier::new(1)),
            release: Arc::new(Barrier::new(1)),
        }
    }

    fn gated(
        id: &'static str,
        names: Arc<Mutex<Vec<String>>>,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    ) -> Self {
        Self {
            id,
            names,
            block_once: AtomicBool::new(true),
            entered,
            release,
        }
    }

    fn unarmed_gate(
        id: &'static str,
        names: Arc<Mutex<Vec<String>>>,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    ) -> Self {
        Self {
            id,
            names,
            block_once: AtomicBool::new(false),
            entered,
            release,
        }
    }

    fn arm(&self) {
        assert!(
            !self.block_once.swap(true, Ordering::SeqCst),
            "source gate was already armed"
        );
    }

    fn manifest(name: &str) -> ToolManifest {
        crate::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("{name} test tool"),
            crate::ToolDefinition::default_input_schema(),
            json!({}),
        )
        .manifest()
    }
}

struct AdmissionSourceSnapshot {
    id: String,
    manifests: Vec<ToolManifest>,
    result: Option<&'static str>,
}

#[async_trait::async_trait]
impl ToolSourceExecutor for AdmissionSourceSnapshot {
    fn id(&self) -> &str {
        &self.id
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(Self {
            id: self.id.clone(),
            manifests: self.manifests.clone(),
            result: self.result,
        }))
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        self.manifests.clone()
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
impl LeafToolSourceExecutor for AdmissionSourceSnapshot {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(self.result.unwrap_or(call.name()))).into()
    }

    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for MutableAdmissionSource {
    fn id(&self) -> &str {
        self.id
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(AdmissionSourceSnapshot {
            id: self.id.to_string(),
            manifests: self.advertised_tools(),
            result: None,
        }))
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        if self.block_once.swap(false, Ordering::SeqCst) {
            self.entered.wait();
            self.release.wait();
        }
        self.names
            .lock_recover()
            .iter()
            .map(|name| Self::manifest(name))
            .collect()
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
impl LeafToolSourceExecutor for MutableAdmissionSource {
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(call.name())).into()
    }

    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
}

struct ReentrantDropProvider {
    registry: Option<Arc<Mutex<Option<ToolRegistry>>>>,
    read_completed: Arc<AtomicBool>,
}

struct RoutedAdmissionSource {
    id: &'static str,
    enabled: Arc<AtomicBool>,
    result: &'static str,
    block_once: AtomicBool,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl RoutedAdmissionSource {
    fn ungated(id: &'static str, enabled: Arc<AtomicBool>, result: &'static str) -> Self {
        Self {
            id,
            enabled,
            result,
            block_once: AtomicBool::new(false),
            entered: Arc::new(Barrier::new(1)),
            release: Arc::new(Barrier::new(1)),
        }
    }

    fn unarmed_gate(
        id: &'static str,
        enabled: Arc<AtomicBool>,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    ) -> Self {
        Self {
            id,
            enabled,
            result: "unused",
            block_once: AtomicBool::new(false),
            entered,
            release,
        }
    }

    fn arm(&self) {
        assert!(
            !self.block_once.swap(true, Ordering::SeqCst),
            "source gate was already armed"
        );
    }
}

#[async_trait::async_trait]
impl ToolSourceExecutor for RoutedAdmissionSource {
    fn id(&self) -> &str {
        self.id
    }

    fn snapshot_execution_source(
        &self,
        _known_resident_ids: &BTreeSet<ToolId>,
    ) -> Result<Arc<dyn ToolSourceExecutor>, ReconfigureError> {
        Ok(Arc::new(AdmissionSourceSnapshot {
            id: self.id.to_string(),
            manifests: self.advertised_tools(),
            result: Some(self.result),
        }))
    }

    fn advertised_tools(&self) -> Vec<ToolManifest> {
        let manifests = self
            .enabled
            .load(Ordering::SeqCst)
            .then(|| MutableAdmissionSource::manifest("alpha"))
            .into_iter()
            .collect();
        if self.block_once.swap(false, Ordering::SeqCst) {
            self.entered.wait();
            self.release.wait();
        }
        manifests
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
impl LeafToolSourceExecutor for RoutedAdmissionSource {
    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(self.result)).into()
    }

    fn attempt_may_defer(&self, _tool_id: &ToolId) -> bool {
        false
    }
}

struct RoutedProvider {
    result: &'static str,
}

#[async_trait::async_trait]
impl ToolProvider for RoutedProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![MutableAdmissionSource::manifest("alpha")]
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        ToolOutcome::ok(json!(self.result)).into()
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
        .execute(ToolCall::new(&manifest, args, context))
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

fn test_attempt_context() -> crate::AttemptContext<'static> {
    let tool = crate::ToolContext::builder(
        crate::SessionId::from("registry-admission-test"),
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
    crate::testing::mock_attempt_context_from(&tool)
}

#[async_trait::async_trait]
impl ToolProvider for ReentrantDropProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        unreachable!("drop probe is never executed")
    }
}

impl Drop for ReentrantDropProvider {
    fn drop(&mut self) {
        let Some(registry) = &self.registry else {
            return;
        };
        let registry = registry
            .lock_recover()
            .clone()
            .expect("drop probe registry installed");
        let _ = registry.tool_manifests();
        self.read_completed.store(true, Ordering::SeqCst);
    }
}

#[test]
fn lookup_between_export_and_apply_is_a_pure_read() {
    let names = Arc::new(Mutex::new(vec!["alpha".to_string()]));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated("source", names)))
        .expect("source admission");
    let mut snapshot = registry.export_state();
    snapshot
        .set_membership(&ToolId::from("tool:alpha"), false)
        .expect("edit exported curation");
    let generation = snapshot.generation();

    assert!(registry.resolve_manifest("missing").is_none());
    assert!(
        registry
            .resolve_manifest_by_id(&ToolId::from("tool:missing"))
            .is_none()
    );
    assert_eq!(registry.generation(), generation);

    registry
        .apply_state(snapshot)
        .expect("pure lookup cannot fence an exported snapshot");
    assert!(
        !registry
            .export_state()
            .get(&ToolId::from("tool:alpha"))
            .expect("alpha remains present")
            .is_member()
    );
}

#[test]
fn unchanged_explicit_refresh_does_not_advance_generation() {
    let names = Arc::new(Mutex::new(vec!["alpha".to_string()]));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated("source", names)))
        .expect("source admission");
    let mut snapshot = registry.export_state();
    snapshot
        .set_membership(&ToolId::from("tool:alpha"), false)
        .expect("edit exported curation");
    let generation = snapshot.generation();

    assert_eq!(
        registry.refresh_sources().expect("no-op refresh"),
        generation
    );
    assert_eq!(registry.generation(), generation);
    registry
        .apply_state(snapshot)
        .expect("no-op refresh cannot fence an exported snapshot");
}

#[test]
fn real_membership_admission_fences_an_older_export() {
    let names = Arc::new(Mutex::new(vec!["alpha".to_string()]));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "source",
            Arc::clone(&names),
        )))
        .expect("source admission");
    let snapshot = registry.export_state();

    names.lock_recover().push("beta".to_string());
    registry.refresh_sources().expect("membership refresh");

    assert!(matches!(
        registry.apply_state(snapshot),
        Err(ReconfigureError::GenerationMismatch { .. })
    ));
}

#[test]
fn stale_refresh_retries_instead_of_overwriting_newer_curation() {
    let names = Arc::new(Mutex::new(vec!["alpha".to_string()]));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "source",
            Arc::clone(&names),
        )))
        .expect("source admission");

    names.lock_recover().push("beta".to_string());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let gated = Arc::new(MutableAdmissionSource::unarmed_gate(
        "source",
        Arc::clone(&names),
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    registry
        .upsert_source(Arc::clone(&gated) as Arc<dyn ToolSourceExecutor>)
        .expect("replace source with gated equivalent");

    names.lock_recover().push("gamma".to_string());
    gated.arm();
    let refreshing = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.refresh_sources())
    };
    entered.wait();

    let mut edited = registry.export_state();
    edited
        .set_membership(&ToolId::from("tool:alpha"), false)
        .expect("edit current curation");
    registry.apply_state(edited).expect("concurrent host delta");
    release.wait();
    refreshing
        .join()
        .expect("refresh thread")
        .expect("refresh retries");

    let final_state = registry.export_state();
    assert!(
        !final_state
            .get(&ToolId::from("tool:alpha"))
            .expect("alpha remains present")
            .is_member(),
        "refresh must preserve curation committed while it was reconciling"
    );
    assert!(final_state.contains(&ToolId::from("tool:gamma")));
}

#[tokio::test]
async fn binding_only_refresh_routes_to_the_new_source_and_fences_stale_work() {
    let source_a_enabled = Arc::new(AtomicBool::new(true));
    let source_b_enabled = Arc::new(AtomicBool::new(false));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(RoutedAdmissionSource::ungated(
            "source-a",
            Arc::clone(&source_a_enabled),
            "source-a",
        )))
        .expect("source A admission");
    registry
        .upsert_source(Arc::new(RoutedAdmissionSource::ungated(
            "source-b",
            Arc::clone(&source_b_enabled),
            "source-b",
        )))
        .expect("dormant source B admission");

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let gate = Arc::new(RoutedAdmissionSource::unarmed_gate(
        "source-z-gate",
        Arc::new(AtomicBool::new(false)),
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    registry
        .upsert_source(Arc::clone(&gate) as Arc<dyn ToolSourceExecutor>)
        .expect("gate source admission");

    let initial = execute_leaf_by_id(
        &registry,
        &ToolId::from("tool:alpha"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert_eq!(initial.value_for_projection(), json!("source-a"));
    let before = registry.export_state();
    let write_revision = registry.inner.read_recover().write_revision;
    gate.arm();
    let stale_refresh = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.refresh_sources())
    };
    entered.wait();

    source_a_enabled.store(false, Ordering::SeqCst);
    source_b_enabled.store(true, Ordering::SeqCst);
    registry
        .refresh_sources()
        .expect("binding-only source refresh");
    let refreshed_write_revision = registry.inner.read_recover().write_revision;

    release.wait();
    stale_refresh
        .join()
        .expect("stale refresh thread")
        .expect("stale refresh retries after the binding update");

    assert_eq!(registry.generation(), before.generation());
    assert_eq!(registry.export_state().entries(), before.entries());
    assert_eq!(
        refreshed_write_revision,
        write_revision + 1,
        "a private binding update must advance the write fence"
    );
    let result = execute_leaf_by_id(
        &registry,
        &ToolId::from("tool:alpha"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert_eq!(result.value_for_projection(), json!("source-b"));
}

#[tokio::test]
async fn identical_context_overlay_routes_to_the_context_provider() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(RoutedProvider { result: "base" }))
        .expect("base registry");
    let before = registry.export_state();

    let composed = registry
        .compose_session_catalog(vec![Arc::new(RoutedProvider { result: "context" })])
        .expect("identical context overlay");

    assert_eq!(composed.generation(), before.generation());
    assert_eq!(composed.export_state().entries(), before.entries());
    let result = execute_leaf_by_id(
        &composed,
        &ToolId::from("tool:alpha"),
        &json!({}),
        &test_attempt_context(),
    )
    .await;
    assert_eq!(result.value_for_projection(), json!("context"));
}

#[test]
fn same_generation_restore_fences_an_in_flight_refresh() {
    let names = Arc::new(Mutex::new(vec!["alpha".to_string()]));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "source",
            Arc::clone(&names),
        )))
        .expect("source admission");
    let mut restored = registry.export_state();
    restored
        .set_membership(&ToolId::from("tool:alpha"), false)
        .expect("edit restored curation");
    let public_generation = restored.generation();

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let gated = Arc::new(MutableAdmissionSource::unarmed_gate(
        "source",
        names,
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    registry
        .upsert_source(Arc::clone(&gated) as Arc<dyn ToolSourceExecutor>)
        .expect("replace source with gated equivalent");
    assert_eq!(registry.generation(), public_generation);

    gated.arm();
    let refreshing = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.refresh_sources())
    };
    entered.wait();

    let report = registry
        .restore_state(restored)
        .expect("same-generation restore");
    assert_eq!(report.generation, public_generation);
    release.wait();
    refreshing
        .join()
        .expect("refresh thread")
        .expect("refresh retries after restore");

    assert_eq!(registry.generation(), public_generation);
    assert!(
        !registry
            .export_state()
            .get(&ToolId::from("tool:alpha"))
            .expect("alpha remains present")
            .is_member(),
        "refresh must preserve curation installed by a same-generation restore"
    );
}

#[test]
fn removed_provider_destructor_reenters_registry_after_unlock() {
    let registry = ToolRegistry::from_tool_provider(Arc::new(ReentrantDropProvider {
        registry: None,
        read_completed: Arc::new(AtomicBool::new(false)),
    }))
    .expect("base registry");
    let registry_slot = Arc::new(Mutex::new(Some(registry.clone())));
    let read_completed = Arc::new(AtomicBool::new(false));
    let handle = registry
        .add_tool_provider(Arc::new(ReentrantDropProvider {
            registry: Some(Arc::clone(&registry_slot)),
            read_completed: Arc::clone(&read_completed),
        }))
        .expect("drop probe source admission");

    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let removing = {
        let registry = registry.clone();
        std::thread::spawn(move || {
            let result = registry.remove_source(&handle);
            let _ = finished_tx.send(result);
        })
    };

    finished_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("source removal deadlocked in the provider destructor")
        .expect("source removal");
    removing.join().expect("removal thread");
    assert!(
        read_completed.load(Ordering::SeqCst),
        "the provider destructor completed its public registry read"
    );
}

#[test]
fn concurrent_ambiguous_admission_commits_one_source_and_one_generation() {
    let registry = ToolRegistry::empty();
    let entered = Arc::new(Barrier::new(3));
    let release = Arc::new(Barrier::new(3));
    let names = Arc::new(Mutex::new(vec!["same".to_string()]));
    let source_a: Arc<dyn ToolSourceExecutor> = Arc::new(MutableAdmissionSource::gated(
        "source-a",
        Arc::clone(&names),
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    let source_b: Arc<dyn ToolSourceExecutor> = Arc::new(MutableAdmissionSource::gated(
        "source-b",
        names,
        Arc::clone(&entered),
        Arc::clone(&release),
    ));

    let first = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.upsert_source(source_a))
    };
    let second = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.upsert_source(source_b))
    };
    entered.wait();
    release.wait();

    let results = [
        first.join().expect("first admission"),
        second.join().expect("second admission"),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(registry.generation(), 1);
    let authority = registry.inner.read_recover();
    assert_eq!(authority.sources.len(), 1);
    let entry = authority
        .state
        .surface
        .get(&ToolId::from("tool:same"))
        .expect("one stable admitted binding")
        .clone();
    assert!(
        authority
            .sources
            .contains_key(entry.binding.source_key().expect("bound source"))
    );
}

#[test]
fn generation_overflow_leaves_source_and_surface_unmodified() {
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "existing",
            Arc::new(Mutex::new(vec!["existing".to_string()])),
        )))
        .expect("baseline source admission");
    registry.inner.write_recover().state.generation = u64::MAX;
    let before = registry.export_state();

    let error = registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "overflow",
            Arc::new(Mutex::new(vec!["overflow".to_string()])),
        )))
        .expect_err("overflow must refuse admission");

    assert!(matches!(error, ReconfigureError::Validation(message) if message.contains("overflow")));
    let authority = registry.inner.read_recover();
    assert_eq!(authority.sources.len(), 1);
    assert!(
        authority
            .sources
            .contains_key(&ToolSourceKey::Leaf("existing".to_string()))
    );
    drop(authority);
    assert_eq!(registry.export_state().entries(), before.entries());
    assert_eq!(registry.generation(), u64::MAX);
}

#[test]
fn write_revision_overflow_leaves_source_and_surface_unmodified() {
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "existing",
            Arc::new(Mutex::new(vec!["existing".to_string()])),
        )))
        .expect("baseline source admission");
    registry.inner.write_recover().write_revision = u64::MAX;
    let before = registry.export_state();
    let generation = registry.generation();

    let error = registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "overflow",
            Arc::new(Mutex::new(vec!["overflow".to_string()])),
        )))
        .expect_err("write revision overflow must refuse admission");

    assert!(
        matches!(error, ReconfigureError::Validation(message) if message.contains("write revision overflow"))
    );
    let authority = registry.inner.read_recover();
    assert_eq!(authority.sources.len(), 1);
    assert!(
        authority
            .sources
            .contains_key(&ToolSourceKey::Leaf("existing".to_string()))
    );
    assert_eq!(authority.write_revision, u64::MAX);
    drop(authority);
    assert_eq!(registry.export_state().entries(), before.entries());
    assert_eq!(registry.generation(), generation);
}

#[test]
fn write_revision_overflow_leaves_restored_surface_unmodified() {
    let names = Arc::new(Mutex::new(vec!["alpha".to_string()]));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated("source", names)))
        .expect("source admission");
    let before = registry.export_state();
    let generation = before.generation();
    let mut restored = before.clone();
    restored
        .set_membership(&ToolId::from("tool:alpha"), false)
        .expect("edit restored curation");
    registry.inner.write_recover().write_revision = u64::MAX;

    let error = registry
        .restore_state(restored)
        .expect_err("write revision overflow must refuse restore");

    assert!(
        matches!(error, ReconfigureError::Validation(message) if message.contains("write revision overflow"))
    );
    assert_eq!(registry.export_state().entries(), before.entries());
    assert_eq!(registry.generation(), generation);
    assert_eq!(registry.inner.read_recover().write_revision, u64::MAX);
}

/// Every mutator must advance the single write fence exactly once per write —
/// including writes that leave the admitted surface and generation untouched.
/// `generation` is the public surface identity and is deliberately *not* part
/// of this fence.
#[test]
fn every_mutator_advances_the_write_fence_once_per_write() {
    fn write_revision(registry: &ToolRegistry) -> u64 {
        registry.inner.read_recover().write_revision
    }

    let registry = ToolRegistry::empty();
    assert_eq!(write_revision(&registry), 0);

    // Source admission writes the source map and the surface.
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "first",
            Arc::new(Mutex::new(vec!["alpha".to_string()])),
        )))
        .expect("first source admission");
    let mut observed = write_revision(&registry);
    assert_eq!(observed, 1);

    // Re-admitting an identical source still writes the source map even
    // though the admitted surface and generation do not move.
    let generation = registry.generation();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "first",
            Arc::new(Mutex::new(vec!["alpha".to_string()])),
        )))
        .expect("identical re-admission");
    let next = write_revision(&registry);
    assert_eq!(next, observed + 1);
    assert_eq!(registry.generation(), generation);
    observed = next;

    // A generation-matched apply writes the surface.
    registry
        .apply_state(registry.export_state())
        .expect("apply_state at the current generation");
    let next = write_revision(&registry);
    assert_eq!(next, observed + 1);
    observed = next;

    // Restoring the identical snapshot still writes state even though the
    // surface and generation stay put.
    let generation = registry.generation();
    registry
        .restore_state(registry.export_state())
        .expect("identical restore");
    let next = write_revision(&registry);
    assert_eq!(next, observed + 1);
    assert_eq!(registry.generation(), generation);

    // Removing a source that bound no tools changes only the source map.
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "empty",
            Arc::new(Mutex::new(vec![])),
        )))
        .expect("empty source admission");
    let observed = write_revision(&registry);
    let generation = registry.generation();
    registry
        .remove_source_id("empty")
        .expect("remove the empty source");
    let next = write_revision(&registry);
    assert_eq!(next, observed + 1);
    assert_eq!(
        registry.generation(),
        generation,
        "no surface entries moved"
    );

    // Pinning copies the fence position and the overlay upsert commits once
    // on the pinned registry.
    let pinned = registry
        .pin_session_surface(vec![])
        .expect("pinned session surface");
    assert_eq!(write_revision(&pinned), write_revision(&registry) + 1);
}

/// `add_tool_provider` performs two writes — the live-source id bump and the
/// source admission — and each must move the fence.
#[test]
fn add_tool_provider_advances_the_fence_once_per_write() {
    let registry = ToolRegistry::empty();
    facade_ops::ToolRegistryFacadeOps::add_tool_provider(
        &registry,
        Arc::new(RoutedProvider { result: "late" }),
    )
    .expect("provider admission");
    assert_eq!(registry.inner.read_recover().write_revision, 2);
}

/// `refresh_and_pin_sources` captures the source map outside the write guard;
/// the single write fence must force the retry that re-reads a source
/// admitted while the capture was in flight, or the pinned registry would
/// silently lose it.
#[test]
fn refresh_cannot_lose_a_source_admitted_mid_capture() {
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let gate = Arc::new(MutableAdmissionSource::unarmed_gate(
        "gated",
        Arc::new(Mutex::new(vec!["alpha".to_string()])),
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::clone(&gate) as Arc<dyn ToolSourceExecutor>)
        .expect("gate source admission");
    gate.arm();

    let refresher = {
        let registry = registry.clone();
        std::thread::spawn(move || registry.refresh_and_pin_sources())
    };
    entered.wait();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "late",
            Arc::new(Mutex::new(vec!["beta".to_string()])),
        )))
        .expect("mid-capture admission lands");
    release.wait();
    let pinned = refresher
        .join()
        .expect("refresh thread")
        .expect("refresh retries the moved fence");
    let authority = pinned.inner.read_recover();
    assert!(
        authority
            .sources
            .contains_key(&ToolSourceKey::Leaf("late".to_string())),
        "the pinned registry must carry the source admitted mid-capture"
    );
    assert!(
        authority.state.surface.get_by_name("beta").is_some(),
        "the pinned surface must admit the late source's tool"
    );
}
