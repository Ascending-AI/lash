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

use crate::ReopenableProcessExecutionEnvStore;
use lash_core::ProcessExecutionEnvStore;
use lashlang::testing::conformance::ReopenableLashlangArtifactStore;
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

pub async fn lashlang_artifact_store_fresh_instances<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let make_store = || make().open.artifacts;
    lashlang::testing::conformance::lashlang_artifact_store_fresh_instances(&make_store).await;
}

pub async fn lashlang_artifact_store_reports_durable<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::lashlang_artifact_store_durability_tier(
        make().open.artifacts,
        lashlang::DurabilityTier::Durable,
    )
    .await;
}

pub async fn lashlang_artifact_owner_lifecycle<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::owner_lifecycle(make().open.artifacts).await;
}

pub async fn lashlang_failed_registration_reclaims_staging_owner<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::failed_registration_reclaims_staging_owner(
        make().open.artifacts,
    )
    .await;
}

pub async fn lashlang_artifact_transfer_is_idempotent<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::transfer_is_idempotent(make().open.artifacts).await;
}

pub async fn lashlang_artifact_retirement_fences_late_publication<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::retirement_fences_late_publication(make().open.artifacts).await;
}

pub async fn lashlang_slow_writer_is_fenced_after_retirement<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::slow_writer_is_fenced_after_retirement(make().open.artifacts)
        .await;
}

pub async fn lashlang_hostile_module_references_are_rejected<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::hostile_module_references_are_rejected(make().open.artifacts)
        .await;
}

pub async fn lashlang_artifact_survives_reopen<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make();
    let reopen = Arc::clone(&handles.reopen);
    lashlang::testing::conformance::survives_reopen(ReopenableLashlangArtifactStore {
        open: handles.open.artifacts,
        reopen: Arc::new(move || (reopen)().artifacts),
    })
    .await;
}

pub async fn process_execution_env_store_fresh_instances<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let make_store = || make().open.process_env;
    crate::registration_macro_support::process_execution_env_store_fresh_instances(&make_store)
        .await;
}

pub async fn process_environment_namespace<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::process_environment_namespace(make().open.process_env).await;
}

pub async fn process_env_owner_lifecycle<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::process_env_owner_lifecycle(make().open.process_env).await;
}

pub async fn failed_registration_reclaims_process_env<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::failed_registration_reclaims_process_env(
        make().open.process_env,
    )
    .await;
}

pub async fn process_env_transfer_and_fence<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::process_env_transfer_and_fence(make().open.process_env)
        .await;
}

pub async fn slow_process_env_writer_is_fenced<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::slow_process_env_writer_is_fenced(make().open.process_env)
        .await;
}

pub async fn process_env_survives_reopen<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make();
    let reopen = Arc::clone(&handles.reopen);
    crate::registration_macro_support::process_env_survives_reopen(
        ReopenableProcessExecutionEnvStore {
            open: handles.open.process_env,
            reopen: Arc::new(move || (reopen)().process_env),
        },
    )
    .await;
}

/// The two typed keyspaces multiplexed onto a durable backend stay disjoint.
pub async fn artifact_store_cross_namespace_isolation<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
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
