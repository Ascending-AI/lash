//! Shared conformance for durable backends that fuse both artifact-store traits
//! ADR-0013's engine rebuild path depends on:
//! [`lashlang::LashlangArtifactStore`] (module artifacts)
//! and [`lash_core::ProcessExecutionEnvStore`] (process-execution-env blobs).
//!
//! The two traits live in independent crates by design (the language crate does
//! not depend on the runtime kernel, which stays integration-agnostic). This
//! crate is the only layer depending on both, so the cross-namespace isolation
//! case — which requires a single store viewed through both traits — lives
//! here. Per-trait behavior is delegated to each owner's suite so there is one
//! source of truth for every contract.

use std::sync::Arc;

use crate::{ReopenableProcessExecutionEnvStore, process_execution_env_store_reopenable};
use lash_core::ProcessExecutionEnvStore;
use lashlang::testing::conformance::{
    ReopenableLashlangArtifactStore, lashlang_artifact_store_reopenable,
};
use lashlang::{LashlangArtifactStore, ModuleArtifact, parse};
use pretty_assertions::assert_eq;

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

/// The two typed keyspaces multiplexed onto a durable backend stay disjoint.
async fn cross_namespace_isolation(handles: ArtifactStoreHandles) {
    let artifact = sample_module_artifact("process delta(root: str) -> str { finish root }");
    let env_spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    );
    let env_ref = env_spec.stable_ref().expect("stable env ref");
    let env_bytes = env_spec.to_store_bytes().expect("encode env");
    let owner = lash_core::ArtifactOwner::host("fused-conformance");

    handles
        .artifacts
        .publish_module_artifact(&owner, &artifact)
        .await
        .expect("publish module artifact");
    handles
        .process_env
        .publish_process_execution_env(&owner, &env_ref, &env_bytes)
        .await
        .expect("publish process environment");

    let module = handles
        .artifacts
        .get_module_artifact(&artifact.module_ref)
        .await
        .expect("module artifact isolated from environment writes")
        .expect("module artifact present");
    assert_eq!(*module, artifact);
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("environment bytes isolated"),
        Some(env_bytes),
    );
}
