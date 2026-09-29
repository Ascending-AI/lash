//! Shared conformance for durable backends that fuse both artifact-store traits
//! ADR-0013's engine rebuild path depends on:
//! [`lash_core::ModuleArtifactStore`] (module artifacts)
//! and [`lash_core::ProcessExecutionEnvStore`] (process-execution-env blobs).
//!
//! Both ports live in the execution kernel, but the module-artifact laws live in
//! lashlang, which builds the modules they publish. This crate depends on both,
//! so the cross-namespace isolation case — which requires a single store viewed
//! through both ports — lives here. Per-port behavior is delegated to each
//! owner's suite so there is one source of truth for every contract.

use std::sync::Arc;

use crate::ReopenableProcessExecutionEnvStore;
use lash_core::ProcessExecutionEnvStore;
use lashlang::ModuleArtifact;
use lashlang::testing::ast_builders as b;
use lashlang::testing::conformance::ReopenableLashlangArtifactStore;
use pretty_assertions::assert_eq;

/// A durable store accessed through both artifact-store traits over the same
/// backing storage.
pub struct ArtifactStoreHandles {
    pub artifacts: Arc<dyn lash_core::ModuleArtifactStore>,
    pub process_env: Arc<dyn ProcessExecutionEnvStore>,
}

/// Writers plus a factory that constructs post-write handles over the same
/// durable backing store.
pub struct ReopenableArtifactStore {
    pub open: ArtifactStoreHandles,
    pub reopen: Arc<dyn Fn() -> ArtifactStoreHandles + Send + Sync>,
}

/// `process <name>(root: str) -> str { finish root }`
///
/// The fixture only has to be a publishable module; what it computes is never
/// read.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn sample_module_artifact(process_name: &str) -> ModuleArtifact {
    let program = b::module(
        vec![b::process_returning(
            process_name,
            vec![b::param("root", lashlang::TypeExpr::Str)],
            lashlang::TypeExpr::Str,
            b::finish(b::var("root")),
        )],
        Vec::new(),
    );
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
        lash_core::DurabilityTier::Durable,
    )
    .await;
}

pub async fn lashlang_last_referrer_reclaims_module<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::last_referrer_reclaims_module(make().open.artifacts).await;
}

pub async fn lashlang_abandoned_start_reclaims_module<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::abandoned_start_reclaims_module(make().open.artifacts).await;
}

pub async fn lashlang_carry_preserves_module<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::carry_preserves_module(make().open.artifacts).await;
}

pub async fn lashlang_ended_referrer_fences_late_publication<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::ended_referrer_fences_late_publication(make().open.artifacts)
        .await;
}

pub async fn lashlang_hostile_module_references_are_rejected<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::hostile_module_references_are_rejected(make().open.artifacts)
        .await;
}

pub async fn lashlang_alpha_variants_publish_distinct_refs<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    lashlang::testing::conformance::alpha_variants_publish_distinct_refs(make().open.artifacts)
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

pub async fn process_env_last_referrer_reclaims_bytes<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::process_env_last_referrer_reclaims_bytes(
        make().open.process_env,
    )
    .await;
}

pub async fn process_env_carry_precedes_reclamation<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    crate::registration_macro_support::process_env_carry_precedes_reclamation(
        make().open.process_env,
    )
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
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn artifact_store_cross_namespace_isolation<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifacts = handles.artifacts;
    let artifact = sample_module_artifact("delta");
    let env_spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    );
    let env_ref = env_spec.stable_ref().expect("stable env ref");
    let env_bytes = env_spec.to_store_bytes().expect("encode env");
    let referrer = lash_core::ArtifactReferrer::HostPin(lash_core::HostArtifactPin::mint());
    let claim = lash_core::ReferrerClaim::unguarded(referrer).expect("host pin claim");

    artifacts
        .publish_module_artifact(
            &claim,
            artifact.module_ref().as_str(),
            &artifact.to_store_bytes().expect("encode module"),
        )
        .await
        .expect("publish module artifact");
    handles
        .process_env
        .publish_process_execution_env(&claim, &env_ref, &env_bytes)
        .await
        .expect("publish process environment");

    let module = artifacts
        .get_module_artifact(artifact.module_ref().as_str())
        .await
        .expect("module artifact isolated from environment writes")
        .expect("module artifact present");
    assert_eq!(module, artifact.to_store_bytes().expect("encode module"));
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("environment bytes isolated"),
        Some(env_bytes),
    );
}
