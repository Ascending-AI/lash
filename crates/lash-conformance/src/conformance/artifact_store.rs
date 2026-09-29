//! Conformance for referrer-bound process environment storage.

use super::*;
use pretty_assertions::assert_eq;

/// A writer and a new handle over the same durable store.
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

#[expect(
    clippy::expect_used,
    reason = "conformance fixture validates its setup"
)]
fn host_claim() -> (crate::ArtifactReferrer, crate::ReferrerClaim) {
    let referrer = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    let claim = crate::ReferrerClaim::unguarded(referrer.clone()).expect("host pin claim");
    (referrer, claim)
}

fn cleanup(referrer: crate::ArtifactReferrer) -> crate::ResolvedArtifactCleanup {
    crate::ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

pub async fn process_execution_env_store_fresh_instances<F>(make: &F)
where
    F: Fn() -> Arc<dyn crate::ProcessExecutionEnvStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "process_execution_env_store");
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn process_env_last_referrer_reclaims_bytes(
    store: Arc<dyn crate::ProcessExecutionEnvStore>,
) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (first, first_claim) = host_claim();
    let (second, second_claim) = host_claim();
    store
        .publish_process_execution_env(&first_claim, &env_ref, &bytes)
        .await
        .expect("publish first edge");
    store
        .acquire_process_execution_env(&second_claim, &env_ref)
        .await
        .expect("acquire second edge");
    store
        .end_process_env_referrer(&cleanup(first))
        .await
        .expect("end first referrer");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(bytes.clone())
    );
    store
        .end_process_env_referrer(&cleanup(second.clone()))
        .await
        .expect("end last referrer");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    store
        .end_process_env_referrer(&cleanup(second))
        .await
        .expect("replayed end");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    let refusal = store
        .publish_process_execution_env(&second_claim, &env_ref, &bytes)
        .await
        .expect_err("ended pin is fenced");
    assert!(
        matches!(refusal, crate::ArtifactStoreError::ReferrerEnded { referrer } if referrer == second_claim.referrer().clone())
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn process_env_carry_precedes_reclamation(
    store: Arc<dyn crate::ProcessExecutionEnvStore>,
) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (source, source_claim) = host_claim();
    let (destination, destination_claim) = host_claim();
    store
        .publish_process_execution_env(&source_claim, &env_ref, &bytes)
        .await
        .expect("publish source");
    let transfer = crate::ResolvedArtifactCleanup {
        referrer: source.clone(),
        carries: vec![crate::ArtifactCarry {
            artifact: crate::ArtifactName {
                store: crate::ArtifactStoreId::ProcessEnv,
                artifact_ref: env_ref.as_str().to_owned(),
            },
            to: destination.clone(),
        }],
    };
    for _ in 0..2 {
        store
            .end_process_env_referrer(&transfer)
            .await
            .expect("carry is idempotent");
        assert_eq!(
            store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read"),
            Some(bytes.clone())
        );
    }
    let late = store
        .acquire_process_execution_env(&source_claim, &env_ref)
        .await
        .expect_err("source is fenced");
    assert!(
        matches!(late, crate::ArtifactStoreError::ReferrerEnded { referrer } if referrer == source)
    );
    store
        .end_process_env_referrer(&cleanup(destination))
        .await
        .expect("end destination");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    let late = store
        .acquire_process_execution_env(&destination_claim, &env_ref)
        .await
        .expect_err("destination is fenced");
    assert!(matches!(
        late,
        crate::ArtifactStoreError::ReferrerEnded { .. }
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn slow_process_env_writer_is_fenced(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (referrer, claim) = host_claim();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let writer_store = Arc::clone(&store);
    let writer = tokio::spawn(async move {
        resume_rx.await.expect("fence releases writer");
        writer_store
            .publish_process_execution_env(&claim, &env_ref, &bytes)
            .await
    });
    store
        .end_process_env_referrer(&cleanup(referrer.clone()))
        .await
        .expect("end while writer is paused");
    resume_tx.send(()).expect("resume writer");
    let refusal = writer
        .await
        .expect("writer joins")
        .expect_err("fenced writer");
    assert!(
        matches!(refusal, crate::ArtifactStoreError::ReferrerEnded { referrer: ended } if ended == referrer)
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn process_env_survives_reopen(reopenable: ReopenableProcessExecutionEnvStore) {
    let ReopenableProcessExecutionEnvStore { open, reopen } = reopenable;
    let open_identity = Arc::downgrade(&open);
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (first, first_claim) = host_claim();
    let (second, second_claim) = host_claim();
    open.publish_process_execution_env(&first_claim, &env_ref, &bytes)
        .await
        .expect("publish first edge");
    open.acquire_process_execution_env(&second_claim, &env_ref)
        .await
        .expect("acquire second edge");
    open.end_process_env_referrer(&cleanup(first.clone()))
        .await
        .expect("end first referrer");
    drop(open);
    let reopened = reopen();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopened)),
        "reopen reused the writer handle"
    );
    assert_eq!(
        reopened
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(bytes)
    );
    reopened
        .end_process_env_referrer(&cleanup(first))
        .await
        .expect("retry end after reopen");
    reopened
        .end_process_env_referrer(&cleanup(second))
        .await
        .expect("end final referrer");
    assert_eq!(
        reopened
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
}
