//! Backend-agnostic conformance for [`crate::LashlangArtifactStore`].
//!
//! ADR-0013 makes durable engine rebuilds depend on artifact-store round-trips;
//! this suite is the executable contract for the two keyspaces this trait owns —
//! module artifacts and raw artifact bytes. Run it against every implementation
//! (the in-memory double here and the durable backends in `lash-sqlite-store` /
//! `lash-postgres-store`) so the doubles cannot drift from production behavior.
//!
//! The [`module_and_raw_namespaces_isolate`] case is the load-bearing one: a
//! durable backend multiplexes both keyspaces (plus process-execution-env blobs,
//! whose trait lives in `lash-core`) onto one physical store, and the two must
//! stay disjoint even when addressed by an identical key value rather than
//! silently clobbering under last-writer-wins.

use std::sync::Arc;

use crate::{
    DurabilityTier, Expr, LashlangArtifactStore, ModuleArtifact, Program, ResourceRefExpr, parse,
};

/// A writer plus a factory that constructs a post-write
/// [`LashlangArtifactStore`] handle over the same durable backing store.
pub struct ReopenableLashlangArtifactStore {
    pub open: Arc<dyn LashlangArtifactStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn LashlangArtifactStore> + Send + Sync>,
}

/// Build a real, self-consistent module artifact. `to_store_bytes` re-verifies
/// the content-derived `module_ref`, so the artifact cannot be fabricated with
/// an arbitrary key — it is compiled from source.
fn sample_module_artifact(source: &str) -> ModuleArtifact {
    let program = parse(source).expect("parse sample lashlang module");
    ModuleArtifact::from_program(program).expect("build sample module artifact")
}

/// Run the [`LashlangArtifactStore`] contract against the store produced by
/// `make`. `make` must return a fresh, empty store on each call.
pub async fn lashlang_artifact_store<F>(make: F, expected_tier: DurabilityTier)
where
    F: Fn() -> Arc<dyn LashlangArtifactStore>,
{
    let first = make();
    let second = make();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "lashlang_artifact_store factory reused one Arc across conformance roles"
    );
    drop((first, second));
    assert_eq!(
        make().durability_tier(),
        expected_tier,
        "lashlang artifact store must report its declared durability tier"
    );
    module_artifact_round_trips(make()).await;
    raw_artifact_round_trips(make()).await;
    raw_artifact_overwrite(make()).await;
    module_and_raw_namespaces_isolate(make()).await;
    current_trigger_manifest_replacement_diffs_keys(make()).await;
}

/// Run the full contract plus a durable reopen check across both keyspaces:
/// writes through the `open` handle must be visible through a `reopen` handle
/// over the same store.
pub async fn lashlang_artifact_store_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableLashlangArtifactStore,
{
    lashlang_artifact_store(|| make().open, DurabilityTier::Durable).await;
    survives_reopen(make()).await;
}

async fn module_artifact_round_trips(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process alpha(root: str) -> str { finish root }");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("get missing module artifact")
            .is_none(),
        "a fresh store must not resolve an unwritten module artifact"
    );

    store
        .put_module_artifact(&artifact)
        .await
        .expect("put module artifact");
    let loaded = store
        .get_module_artifact(&artifact.module_ref)
        .await
        .expect("get module artifact")
        .expect("module artifact present after put");
    assert_eq!(*loaded, artifact, "module artifact must round-trip");

    // Re-putting the same content-addressed artifact is idempotent.
    store
        .put_module_artifact(&artifact)
        .await
        .expect("re-put module artifact");
    let reloaded = store
        .get_module_artifact(&artifact.module_ref)
        .await
        .expect("get module artifact after re-put")
        .expect("module artifact present after re-put");
    assert_eq!(*reloaded, artifact);
}

async fn raw_artifact_round_trips(store: Arc<dyn LashlangArtifactStore>) {
    assert!(
        store
            .get_artifact_bytes("raw:missing")
            .await
            .expect("get missing raw artifact")
            .is_none(),
        "a fresh store must not resolve unwritten raw artifact bytes"
    );

    store
        .put_artifact_bytes("raw:sample", "generic", b"raw-artifact-payload")
        .await
        .expect("put raw artifact bytes");
    assert_eq!(
        store
            .get_artifact_bytes("raw:sample")
            .await
            .expect("get raw artifact bytes"),
        Some(b"raw-artifact-payload".to_vec()),
        "raw artifact bytes must round-trip"
    );
}

fn trigger_artifact(subscription_key: &str) -> ModuleArtifact {
    let registration = Expr::ReceiverCall {
        receiver: Box::new(Expr::ResourceRef(ResourceRefExpr::resolved(
            vec!["triggers".into()],
            "Triggers",
            "triggers",
        ))),
        operation: "register".into(),
        args: vec![Expr::Record(vec![
            ("source".into(), Expr::Record(Vec::new())),
            ("target".into(), Expr::Null),
            ("inputs".into(), Expr::Record(Vec::new())),
            (
                "subscription_key".into(),
                Expr::String(subscription_key.into()),
            ),
        ])],
    };
    ModuleArtifact::from_program(Program::block(vec![registration]))
        .expect("build trigger manifest artifact")
}

async fn current_trigger_manifest_replacement_diffs_keys(store: Arc<dyn LashlangArtifactStore>) {
    let first = trigger_artifact("old-key");
    let second = trigger_artifact("new-key");
    let owner = "session:manifest-owner";
    let initial = store
        .replace_current_trigger_manifest(owner, &first)
        .await
        .expect("install initial trigger manifest");
    assert!(initial.previous_module_ref.is_none());
    assert!(initial.diff.added.is_empty());
    assert!(initial.diff.removed.is_empty());

    let replacement = store
        .replace_current_trigger_manifest(owner, &second)
        .await
        .expect("replace current trigger manifest");
    assert_eq!(replacement.previous_module_ref, Some(first.module_ref));
    assert_eq!(
        replacement.diff.added,
        ["new-key".to_string()].into_iter().collect()
    );
    assert_eq!(
        replacement.diff.removed,
        ["old-key".to_string()].into_iter().collect()
    );
    let current = store
        .get_current_trigger_manifest(owner)
        .await
        .expect("load current trigger manifest")
        .expect("current trigger manifest exists");
    assert_eq!(current.module_ref, second.module_ref);
    assert!(current.manifest.contains("new-key"));
    assert!(!current.manifest.contains("old-key"));
    assert!(
        store
            .get_current_trigger_manifest("session:other-owner")
            .await
            .expect("load unrelated trigger manifest")
            .is_none()
    );
}

async fn raw_artifact_overwrite(store: Arc<dyn LashlangArtifactStore>) {
    store
        .put_artifact_bytes("raw:overwrite", "generic", b"first")
        .await
        .expect("put first raw bytes");
    store
        .put_artifact_bytes("raw:overwrite", "generic", b"second")
        .await
        .expect("overwrite raw bytes");
    assert_eq!(
        store
            .get_artifact_bytes("raw:overwrite")
            .await
            .expect("get overwritten raw bytes"),
        Some(b"second".to_vec()),
        "re-putting a raw artifact key must overwrite its bytes"
    );
}

/// The module and raw keyspaces must not clobber each other even when addressed
/// by the identical key value — the case that would have caught a backend
/// keying both namespaces on one column.
async fn module_and_raw_namespaces_isolate(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process gamma(root: str) -> str { finish root }");
    let colliding_key = artifact.module_ref.as_str().to_string();

    store
        .put_module_artifact(&artifact)
        .await
        .expect("put module artifact");
    store
        .put_artifact_bytes(&colliding_key, "generic", b"not-a-module-artifact")
        .await
        .expect("put raw bytes at the module key");

    let module = store
        .get_module_artifact(&artifact.module_ref)
        .await
        .expect("module artifact survives raw write at the same key")
        .expect("module artifact still present");
    assert_eq!(*module, artifact);
    assert_eq!(
        store
            .get_artifact_bytes(&colliding_key)
            .await
            .expect("raw bytes survive module write at the same key"),
        Some(b"not-a-module-artifact".to_vec()),
    );
}

async fn survives_reopen(reopenable: ReopenableLashlangArtifactStore) {
    let ReopenableLashlangArtifactStore { open, reopen } = reopenable;
    let open_identity = Arc::downgrade(&open);
    let artifact = sample_module_artifact("process epsilon(root: str) -> str { finish root }");

    open.put_module_artifact(&artifact)
        .await
        .expect("put module artifact");
    open.put_artifact_bytes("raw:reopen", "generic", b"raw-reopen")
        .await
        .expect("put raw bytes");
    let trigger_artifact = trigger_artifact("reopen-key");
    open.replace_current_trigger_manifest("session:reopen", &trigger_artifact)
        .await
        .expect("put current trigger manifest");
    drop(open);

    let reopen = reopen();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopen)),
        "lashlang artifact reopen factory reused the writer handle"
    );

    let module = reopen
        .get_module_artifact(&artifact.module_ref)
        .await
        .expect("get module artifact after reopen")
        .expect("module artifact survives reopen");
    assert_eq!(*module, artifact);
    assert_eq!(
        reopen
            .get_artifact_bytes("raw:reopen")
            .await
            .expect("get raw bytes after reopen"),
        Some(b"raw-reopen".to_vec()),
    );
    let current = reopen
        .get_current_trigger_manifest("session:reopen")
        .await
        .expect("get current trigger manifest after reopen")
        .expect("current trigger manifest survives reopen");
    assert_eq!(current.module_ref, trigger_artifact.module_ref);
    assert!(current.manifest.contains("reopen-key"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryLashlangArtifactStore;

    #[tokio::test]
    async fn in_memory_lashlang_artifact_store_satisfies_conformance() {
        lashlang_artifact_store(
            || Arc::new(InMemoryLashlangArtifactStore::new()) as Arc<dyn LashlangArtifactStore>,
            DurabilityTier::Inline,
        )
        .await;
    }
}
