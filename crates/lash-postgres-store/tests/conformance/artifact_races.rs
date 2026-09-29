use super::*;
use lash_core_execution::{
    ArtifactReferrer, HostArtifactPin, ModuleArtifactStore as _, ReferrerClaim,
    ResolvedArtifactCleanup,
};

fn host_pin() -> (ArtifactReferrer, ReferrerClaim) {
    let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let claim = ReferrerClaim::unguarded(referrer.clone()).expect("host pin is unguarded");
    (referrer, claim)
}

fn end(referrer: ArtifactReferrer) -> ResolvedArtifactCleanup {
    ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

async fn lock_artifact_mutations<'a>(
    storage: &'a PostgresStorage,
    artifact_ref: &str,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut tx = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lash-artifact:lashlang_module:{artifact_ref}"))
        .execute(&mut *tx)
        .await
        .expect("lock artifact mutation key");
    tx
}

async fn wait_until_a_mutation_waits(storage: &PostgresStorage) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
             WHERE pid <> pg_backend_pid() AND datname = current_database()
               AND state = 'active' AND wait_event_type = 'Lock'
               AND query LIKE '%pg_advisory_xact_lock%')",
        )
        .fetch_one(storage.pool())
        .await
        .expect("inspect lock wait");
        if waiting {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mutation did not reach its artifact lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_referrer_end_preserves_an_edge_committed_ahead_of_it() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (a, a_claim) = host_pin();
    let (b, _) = host_pin();
    let artifact_ref = "artifact-race-preserve";
    store
        .publish_module_artifact(&a_claim, artifact_ref, b"bytes")
        .await
        .expect("publish first edge");

    let mut publisher = lock_artifact_mutations(&storage, artifact_ref).await;
    sqlx::query(
        "INSERT INTO lash_artifact_referrer_edges
        (namespace, artifact_ref, referrer_kind, referrer_id)
        VALUES ('lashlang_module', $1, $2, $3)",
    )
    .bind(artifact_ref)
    .bind(b.kind().as_str())
    .bind(b.canonical_id())
    .execute(&mut *publisher)
    .await
    .expect("stage second edge");

    let ending = store.clone();
    let end_a = tokio::spawn(async move { ending.end_module_referrer(&end(a)).await });
    wait_until_a_mutation_waits(&storage).await;
    publisher.commit().await.expect("commit second edge");
    end_a.await.expect("join end").expect("end first referrer");
    assert_eq!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read bytes"),
        Some(b"bytes".to_vec())
    );
    store
        .end_module_referrer(&end(b))
        .await
        .expect("end second referrer");
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read reclaimed bytes")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_final_referrer_ends_reclaim_bytes() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (a, a_claim) = host_pin();
    let (b, b_claim) = host_pin();
    let artifact_ref = "artifact-race-final";
    store
        .publish_module_artifact(&a_claim, artifact_ref, b"bytes")
        .await
        .expect("publish");
    store
        .acquire_module_artifact(&b_claim, artifact_ref)
        .await
        .expect("acquire");
    let blocker = lock_artifact_mutations(&storage, artifact_ref).await;
    let left_store = store.clone();
    let left = tokio::spawn(async move { left_store.end_module_referrer(&end(a)).await });
    let right_store = store.clone();
    let right = tokio::spawn(async move { right_store.end_module_referrer(&end(b)).await });
    wait_until_a_mutation_waits(&storage).await;
    blocker.commit().await.expect("release lock");
    left.await
        .expect("join first end")
        .expect("end first referrer");
    right
        .await
        .expect("join second end")
        .expect("end second referrer");
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read reclaimed bytes")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_referrer_fence_refuses_a_late_publisher() {
    let Some((_lock, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let store = storage.lashlang_artifact_store();
    let (referrer, claim) = host_pin();
    let artifact_ref = "artifact-race-late";
    store
        .publish_module_artifact(&claim, artifact_ref, b"bytes")
        .await
        .expect("publish");
    let blocker = lock_artifact_mutations(&storage, artifact_ref).await;
    let ending = store.clone();
    let retirement = tokio::spawn(async move { ending.end_module_referrer(&end(referrer)).await });
    wait_until_a_mutation_waits(&storage).await;
    let publishing = store.clone();
    let late = tokio::spawn(async move {
        publishing
            .publish_module_artifact(&claim, artifact_ref, b"bytes")
            .await
    });
    blocker.commit().await.expect("release lock");
    retirement.await.expect("join end").expect("fence referrer");
    assert!(matches!(
        late.await.expect("join late publish"),
        Err(lash_core_execution::ArtifactStoreError::ReferrerEnded { .. })
    ));
    assert!(
        store
            .get_module_artifact(artifact_ref)
            .await
            .expect("read reclaimed bytes")
            .is_none()
    );
}
