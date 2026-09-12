//! Backend-agnostic conformance for owner-bound Lashlang artifact storage.
//!
//! The suite proves exact owner edges, transfer, fencing, reclamation, and
//! durable reopen behavior. Generic raw-byte storage is deliberately absent:
//! every publication is a verified, content-addressed module artifact.

use std::sync::Arc;

use lash_core::{ArtifactOwner, ExecutionScope};

use crate::{DurabilityTier, LashlangArtifactStore, ModuleArtifact, parse};

/// A writer plus a factory that constructs a post-write store handle over the
/// same durable backing store.
pub struct ReopenableLashlangArtifactStore {
    pub open: Arc<dyn LashlangArtifactStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn LashlangArtifactStore> + Send + Sync>,
}

fn sample_module_artifact(source: &str) -> ModuleArtifact {
    let program = parse(source).expect("parse sample lashlang module");
    ModuleArtifact::from_program(program).expect("build sample module artifact")
}

fn execution_owner(id: &str) -> ArtifactOwner {
    ArtifactOwner::execution(ExecutionScope::RuntimeOperation {
        operation_id: id.to_string(),
    })
}

/// Run the ownership contract against a fresh store.
pub async fn lashlang_artifact_store<F>(make: F, expected_tier: DurabilityTier)
where
    F: Fn() -> Arc<dyn LashlangArtifactStore>,
{
    let first = make();
    let second = make();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "factory reused one store Arc"
    );
    drop((first, second));
    assert_eq!(make().durability_tier(), expected_tier);
    owner_lifecycle(make()).await;
    failed_registration_reclaims_staging_owner(make()).await;
    transfer_is_idempotent(make()).await;
    retirement_fences_late_publication(make()).await;
    slow_writer_is_fenced_after_retirement(make()).await;
    hostile_module_references_are_rejected(make()).await;
}

async fn failed_registration_reclaims_staging_owner(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process failed(root: str) -> str { finish root }");
    let staged = execution_owner("failed-registration");
    store
        .publish_module_artifact(&staged, &artifact)
        .await
        .expect("protect before registration");
    store
        .retire_module_artifact_owner(&staged)
        .await
        .expect("fence and reclaim abandoned registration");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read failed-registration artifact")
            .is_none()
    );
}

/// Run the ownership contract plus persistence across a reopened handle.
pub async fn lashlang_artifact_store_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableLashlangArtifactStore,
{
    lashlang_artifact_store(|| make().open, DurabilityTier::Durable).await;
    survives_reopen(make()).await;
}

async fn owner_lifecycle(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process alpha(root: str) -> str { finish root }");
    let first = ArtifactOwner::host("host-a");
    let second = ArtifactOwner::host("host-b");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read missing module")
            .is_none()
    );

    store
        .publish_module_artifact(&first, &artifact)
        .await
        .expect("publish first owner");
    store
        .retain_module_artifact(&second, &artifact.module_ref)
        .await
        .expect("retain second owner");
    store
        .release_module_artifact(&first, &artifact.module_ref)
        .await
        .expect("release first owner");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read second-owned module")
            .is_some(),
        "one owner's release must not affect another owner"
    );

    store
        .release_module_artifact(&second, &artifact.module_ref)
        .await
        .expect("release final owner");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read reclaimed module")
            .is_none(),
        "the final exact release must reclaim the artifact"
    );
    store
        .release_module_artifact(&second, &artifact.module_ref)
        .await
        .expect("repeated release is idempotent");
}

async fn transfer_is_idempotent(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process beta(root: str) -> str { finish root }");
    let staged = execution_owner("module-transfer");
    let process = ArtifactOwner::process("process-beta");
    store
        .publish_module_artifact(&staged, &artifact)
        .await
        .expect("stage module");
    store
        .transfer_module_artifact(&staged, &process, &artifact.module_ref)
        .await
        .expect("transfer module");
    store
        .transfer_module_artifact(&staged, &process, &artifact.module_ref)
        .await
        .expect("replayed transfer is idempotent");
    store
        .release_module_artifact(&process, &artifact.module_ref)
        .await
        .expect("release process owner");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read reclaimed transfer")
            .is_none()
    );
}

async fn retirement_fences_late_publication(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process gamma(root: str) -> str { finish root }");
    let abandoned = execution_owner("abandoned-module-writer");
    store
        .publish_module_artifact(&abandoned, &artifact)
        .await
        .expect("stage module before abandonment");
    store
        .retire_module_artifact_owner(&abandoned)
        .await
        .expect("retire abandoned owner");
    assert!(
        store
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read abandoned module")
            .is_none()
    );
    assert!(
        store
            .publish_module_artifact(&abandoned, &artifact)
            .await
            .is_err(),
        "retirement must fence a late writer"
    );
}

async fn slow_writer_is_fenced_after_retirement(store: Arc<dyn LashlangArtifactStore>) {
    let artifact = sample_module_artifact("process slow(root: str) -> str { finish root }");
    let abandoned = execution_owner("slow-module-writer");
    let writer = store.publish_module_artifact(&abandoned, &artifact);
    store
        .retire_module_artifact_owner(&abandoned)
        .await
        .expect("retire before the paused writer is polled");
    assert!(
        writer.await.is_err(),
        "a writer paused across retirement must remain fenced"
    );
}

async fn survives_reopen(reopenable: ReopenableLashlangArtifactStore) {
    let ReopenableLashlangArtifactStore { open, reopen } = reopenable;
    let artifact = sample_module_artifact("process epsilon(root: str) -> str { finish root }");
    let first = ArtifactOwner::host("reopen-host-first");
    let second = ArtifactOwner::host("reopen-host-second");
    open.publish_module_artifact(&first, &artifact)
        .await
        .expect("publish module");
    open.retain_module_artifact(&second, &artifact.module_ref)
        .await
        .expect("retain second owner");
    open.release_module_artifact(&first, &artifact.module_ref)
        .await
        .expect("sever first owner before reopen");
    drop(open);

    let reopened = reopen();
    assert!(
        reopened
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read after reopen")
            .is_some()
    );
    reopened
        .release_module_artifact(&first, &artifact.module_ref)
        .await
        .expect("retry interrupted owner sever after reopen");
    reopened
        .release_module_artifact(&second, &artifact.module_ref)
        .await
        .expect("release final owner after reopen");
    assert!(
        reopened
            .get_module_artifact(&artifact.module_ref)
            .await
            .expect("read reclaimed after reopen")
            .is_none()
    );
}

async fn hostile_module_references_are_rejected(store: Arc<dyn LashlangArtifactStore>) {
    for raw in ["", "nul\0reference"] {
        let module_ref: crate::ModuleRef = serde_json::from_value(serde_json::json!(raw)).unwrap();
        assert!(store.get_module_artifact(&module_ref).await.is_err());
        let mut artifact = sample_module_artifact("finish true");
        artifact.module_ref = module_ref;
        assert!(
            store
                .publish_module_artifact(&ArtifactOwner::host("hostile-test"), &artifact)
                .await
                .is_err()
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
