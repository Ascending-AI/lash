//! Shared conformance for durable backends that fuse both artifact-store traits
//! ADR-0013's engine rebuild path depends on:
//! [`lashlang::LashlangArtifactStore`] (module artifacts + raw artifact bytes)
//! and [`lash_core::ProcessExecutionEnvStore`] (process-execution-env blobs).
//!
//! The two traits live in independent crates by design (the language crate does
//! not depend on the runtime kernel, which stays integration-agnostic). This
//! crate is the only layer depending on both, so the cross-namespace isolation
//! case — which requires a single store viewed through both traits — lives
//! here. Per-trait behavior is delegated to each owner's suite so there is one
//! source of truth for every contract.

use std::sync::Arc;

use lash_core::ProcessExecutionEnvStore;
use lash_core::testing::conformance::{
    ReopenableProcessExecutionEnvStore, process_execution_env_store_reopenable,
};
use lashlang::testing::conformance::{
    ReopenableLashlangArtifactStore, lashlang_artifact_store_reopenable,
};
use lashlang::{LashlangArtifactStore, ModuleArtifact, parse};

/// A durable store accessed through both artifact-store traits over the same
/// backing storage.
pub struct ArtifactStoreHandles {
    pub artifacts: Arc<dyn LashlangArtifactStore>,
    pub process_env: Arc<dyn ProcessExecutionEnvStore>,
}

/// Writers plus a factory that constructs post-write handles over the same
/// durable backing store.
pub struct ReopenableArtifactStore {
    pub open: ArtifactStoreHandles,
    pub reopen: Arc<dyn Fn() -> ArtifactStoreHandles + Send + Sync>,
}

fn sample_module_artifact(source: &str) -> ModuleArtifact {
    let program = parse(source).expect("parse sample lashlang module");
    ModuleArtifact::from_program(program).expect("build sample module artifact")
}

/// Run the full durable artifact-store suite: both trait contracts (delegated
/// to their owning crates' suites, including reopen), plus the 3-way
/// cross-namespace isolation that a single fused store must uphold. `make` must
/// return handles over a fresh, empty store on each call.
pub async fn artifact_store_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang_artifact_store_reopenable(|| {
        let handles = make();
        let reopen = Arc::clone(&handles.reopen);
        ReopenableLashlangArtifactStore {
            open: handles.open.artifacts,
            reopen: Arc::new(move || (reopen)().artifacts),
        }
    })
    .await;
    process_execution_env_store_reopenable(|| {
        let handles = make();
        let reopen = Arc::clone(&handles.reopen);
        ReopenableProcessExecutionEnvStore {
            open: handles.open.process_env,
            reopen: Arc::new(move || (reopen)().process_env),
        }
    })
    .await;
    cross_namespace_isolation(make().open).await;
}

/// All three keyspaces multiplexed onto a durable backend stay disjoint under an
/// identical key value. This is the case that would have caught a store keying
/// module artifacts, raw artifact bytes, and process-execution-env blobs all on
/// one column.
async fn cross_namespace_isolation(handles: ArtifactStoreHandles) {
    let artifact = sample_module_artifact("process delta(root: str) -> str { finish root }");
    let colliding_key = artifact.module_ref.as_str().to_string();

    handles
        .artifacts
        .put_module_artifact(&artifact)
        .await
        .expect("put module artifact");
    handles
        .artifacts
        .put_artifact_bytes(&colliding_key, "generic", b"raw-bytes")
        .await
        .expect("put raw bytes at the shared key");
    handles
        .process_env
        .put_process_execution_env(
            &lash_core::ProcessExecutionEnvRef::new(colliding_key.clone()),
            b"env-bytes",
        )
        .await
        .expect("put env at the shared key");

    let module = handles
        .artifacts
        .get_module_artifact(&artifact.module_ref)
        .await
        .expect("module artifact isolated from raw + env writes")
        .expect("module artifact present");
    assert_eq!(*module, artifact);
    assert_eq!(
        handles
            .artifacts
            .get_artifact_bytes(&colliding_key)
            .await
            .expect("raw bytes isolated"),
        Some(b"raw-bytes".to_vec()),
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&lash_core::ProcessExecutionEnvRef::new(colliding_key))
            .await
            .expect("env bytes isolated"),
        Some(b"env-bytes".to_vec()),
    );
}
