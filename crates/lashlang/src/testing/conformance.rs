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

use crate::{DurabilityTier, LashlangArtifactStore, ModuleArtifact, parse};

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
    hostile_artifact_namespaces(make()).await;
    raw_artifact_round_trips(make()).await;
    raw_artifact_overwrite(make()).await;
    module_and_raw_namespaces_isolate(make()).await;
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
}

async fn hostile_artifact_namespaces(store: Arc<dyn LashlangArtifactStore>) {
    for raw in ["", "nul\0reference"] {
        let module_ref: crate::ModuleRef = serde_json::from_value(serde_json::json!(raw)).unwrap();
        assert!(
            store.get_module_artifact(&module_ref).await.is_err(),
            "malformed module reference must not reach lookup"
        );
        let mut artifact = sample_module_artifact("finish true");
        artifact.module_ref = module_ref;
        assert!(
            store.put_module_artifact(&artifact).await.is_err(),
            "malformed module reference must not reach mutation"
        );
        assert!(
            store
                .put_artifact_bytes(raw, "opaque", b"hostile")
                .await
                .is_err(),
            "malformed artifact reference must not reach blob mutation"
        );
        assert!(
            store.get_artifact_bytes(raw).await.is_err(),
            "malformed artifact reference must not reach blob lookup"
        );
    }
    for raw in [
        "canary",
        "../canary",
        "'; DROP TABLE lash_artifact_blobs; --",
    ] {
        store
            .put_artifact_bytes(raw, "opaque", raw.as_bytes())
            .await
            .expect("opaque reference write");
    }
    for raw in [
        "canary",
        "../canary",
        "'; DROP TABLE lash_artifact_blobs; --",
    ] {
        assert_eq!(
            store
                .get_artifact_bytes(raw)
                .await
                .expect("opaque reference read"),
            Some(raw.as_bytes().to_vec())
        );
    }
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
