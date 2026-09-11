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

#[async_trait::async_trait]
impl ToolSourceExecutor for MutableAdmissionSource {
    fn id(&self) -> &str {
        self.id
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

    async fn execute(
        &self,
        tool: &str,
        _args: &serde_json::Value,
        _context: &crate::AttemptContext<'_>,
    ) -> ToolOutcome {
        ToolOutcome::ok(json!(tool))
    }
}

struct ReentrantDropProvider {
    registry: Option<Arc<Mutex<Option<ToolRegistry>>>>,
    read_completed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ToolProvider for ReentrantDropProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
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
fn source_revision_overflow_leaves_source_and_surface_unmodified() {
    let registry = ToolRegistry::empty();
    registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "existing",
            Arc::new(Mutex::new(vec!["existing".to_string()])),
        )))
        .expect("baseline source admission");
    registry.inner.write_recover().source_revision = u64::MAX;
    let before = registry.export_state();
    let generation = registry.generation();

    let error = registry
        .upsert_source(Arc::new(MutableAdmissionSource::ungated(
            "overflow",
            Arc::new(Mutex::new(vec!["overflow".to_string()])),
        )))
        .expect_err("source revision overflow must refuse admission");

    assert!(
        matches!(error, ReconfigureError::Validation(message) if message.contains("source revision overflow"))
    );
    let authority = registry.inner.read_recover();
    assert_eq!(authority.sources.len(), 1);
    assert!(
        authority
            .sources
            .contains_key(&ToolSourceKey::Leaf("existing".to_string()))
    );
    assert_eq!(authority.source_revision, u64::MAX);
    drop(authority);
    assert_eq!(registry.export_state().entries(), before.entries());
    assert_eq!(registry.generation(), generation);
}

#[test]
fn state_revision_overflow_leaves_restored_surface_unmodified() {
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
    registry.inner.write_recover().state_revision = u64::MAX;

    let error = registry
        .restore_state(restored)
        .expect_err("state revision overflow must refuse restore");

    assert!(
        matches!(error, ReconfigureError::Validation(message) if message.contains("state revision overflow"))
    );
    assert_eq!(registry.export_state().entries(), before.entries());
    assert_eq!(registry.generation(), generation);
    assert_eq!(registry.inner.read_recover().state_revision, u64::MAX);
}
