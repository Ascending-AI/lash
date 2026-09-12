//! Conformance for owner-bound process-execution environment storage.

use super::*;
use pretty_assertions::assert_eq;

/// A writer plus a factory that constructs a post-write handle over the same
/// backing store.
pub struct ReopenableProcessExecutionEnvStore {
    pub open: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn crate::ProcessExecutionEnvStore> + Send + Sync>,
}

fn sample_env_spec() -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        SessionPolicy::new(crate::TurnBudget::Unbounded),
    )
}

fn execution_owner(id: &str) -> crate::ArtifactOwner {
    crate::ArtifactOwner::execution(crate::ExecutionScope::RuntimeOperation {
        operation_id: id.to_string(),
    })
}

pub async fn process_execution_env_store<F>(make: F)
where
    F: Fn() -> Arc<dyn crate::ProcessExecutionEnvStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "process_execution_env_store");
    drop((first, second));
    super::hostile_input::process_environment_namespace(make()).await;
    process_env_owner_lifecycle(make()).await;
    failed_registration_reclaims_process_env(make()).await;
    process_env_transfer_and_fence(make()).await;
    slow_process_env_writer_is_fenced(make()).await;
}

async fn failed_registration_reclaims_process_env(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let staged = execution_owner("failed-env-registration");
    store
        .publish_process_execution_env(&staged, &env_ref, &bytes)
        .await
        .expect("protect env before registration");
    store
        .retire_process_execution_env_owner(&staged)
        .await
        .expect("fence and reclaim failed registration");
    assert!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read failed-registration env")
            .is_none()
    );
}

pub async fn process_execution_env_store_reopenable<F>(make: F)
where
    F: Fn() -> ReopenableProcessExecutionEnvStore,
{
    process_execution_env_store(|| make().open).await;
    process_env_survives_reopen(make()).await;
}

async fn process_env_owner_lifecycle(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let first = crate::ArtifactOwner::host("env-host-a");
    let second = crate::ArtifactOwner::host("env-host-b");

    store
        .publish_process_execution_env(&first, &env_ref, &bytes)
        .await
        .expect("publish env");
    store
        .publish_process_execution_env(&second, &env_ref, &bytes)
        .await
        .expect("publish second exact owner");
    store
        .release_process_execution_env(&first, &env_ref)
        .await
        .expect("release first owner");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read second-owned env"),
        Some(bytes.clone())
    );
    store
        .release_process_execution_env(&second, &env_ref)
        .await
        .expect("release final owner");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read reclaimed env"),
        None
    );
    store
        .release_process_execution_env(&second, &env_ref)
        .await
        .expect("repeated release is idempotent");
}

async fn process_env_transfer_and_fence(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let staged = execution_owner("env-transfer");
    let process = crate::ArtifactOwner::process(crate::ProcessRef::new(
        "env-process",
        crate::ProcessIncarnation::from_registration_sequence(1),
    ));
    store
        .publish_process_execution_env(&staged, &env_ref, &bytes)
        .await
        .expect("stage env");
    store
        .transfer_process_execution_env(&staged, &process, &env_ref)
        .await
        .expect("transfer env");
    store
        .transfer_process_execution_env(&staged, &process, &env_ref)
        .await
        .expect("replayed transfer is idempotent");
    store
        .retire_process_execution_env_owner(&staged)
        .await
        .expect("retire staging owner");
    assert!(
        store
            .publish_process_execution_env(&staged, &env_ref, &bytes)
            .await
            .is_err(),
        "retirement must fence a late publication"
    );
    store
        .release_process_execution_env(&process, &env_ref)
        .await
        .expect("release process owner");
    assert!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read reclaimed env")
            .is_none()
    );
}

async fn slow_process_env_writer_is_fenced(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let abandoned = execution_owner("slow-env-writer");
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let writer_store = Arc::clone(&store);
    let writer_owner = abandoned.clone();
    let writer = tokio::spawn(async move {
        resume_rx.await.expect("retirement releases slow writer");
        writer_store
            .publish_process_execution_env(&writer_owner, &env_ref, &bytes)
            .await
    });
    store
        .retire_process_execution_env_owner(&abandoned)
        .await
        .expect("retire while writer is paused");
    resume_tx.send(()).expect("resume slow writer");
    assert!(
        writer.await.expect("slow writer joins").is_err(),
        "a process-env writer paused across retirement must remain fenced"
    );
}

async fn process_env_survives_reopen(reopenable: ReopenableProcessExecutionEnvStore) {
    let ReopenableProcessExecutionEnvStore { open, reopen } = reopenable;
    let open_identity = Arc::downgrade(&open);
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let first = crate::ArtifactOwner::host("env-reopen-first");
    let second = crate::ArtifactOwner::host("env-reopen-second");
    open.publish_process_execution_env(&first, &env_ref, &bytes)
        .await
        .expect("publish env");
    open.publish_process_execution_env(&second, &env_ref, &bytes)
        .await
        .expect("publish second env owner");
    open.release_process_execution_env(&first, &env_ref)
        .await
        .expect("sever first owner before reopen");
    drop(open);
    let reopened = reopen();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopened)),
        "process execution env reopen factory reused the writer handle"
    );
    assert_eq!(
        reopened
            .get_process_execution_env(&env_ref)
            .await
            .expect("get env after reopen"),
        Some(bytes)
    );
    reopened
        .release_process_execution_env(&first, &env_ref)
        .await
        .expect("retry interrupted owner sever after reopen");
    reopened
        .release_process_execution_env(&second, &env_ref)
        .await
        .expect("release final env owner after reopen");
    assert_eq!(
        reopened
            .get_process_execution_env(&env_ref)
            .await
            .expect("read reclaimed env after reopen"),
        None
    );
}
