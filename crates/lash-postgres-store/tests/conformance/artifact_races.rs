use super::*;
use lashlang::LashlangArtifactStore as _;

fn artifact(source: &str) -> lashlang::ModuleArtifact {
    lashlang::ModuleArtifact::from_program(lashlang::parse(source).expect("parse module"))
        .expect("build module artifact")
}

async fn lock_artifact_mutations<'a>(
    storage: &'a PostgresStorage,
    artifact_ref: &lashlang::ModuleRef,
) -> sqlx::Transaction<'a, sqlx::Postgres> {
    let mut tx = storage.pool().begin().await.expect("begin blocker");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lash-artifact:lashlang_module:{artifact_ref}"))
        .execute(&mut *tx)
        .await
        .expect("lock exact artifact mutation key");
    tx
}

async fn wait_until_release_reaches_its_serialization_point(storage: &PostgresStorage) {
    for _ in 0..200 {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_stat_activity
                 WHERE pid <> pg_backend_pid()
                   AND datname = current_database()
                   AND state = 'active'
                   AND wait_event IS NOT NULL
                   AND (
                       query LIKE '%pg_advisory_xact_lock%'
                       OR query LIKE '%DELETE FROM lash_lashlang_artifacts AS artifact%'
                   )
             )",
        )
        .fetch_one(storage.pool())
        .await
        .expect("inspect release wait state");
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("release did not reach its artifact serialization point");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_artifact_release_observes_owner_that_commits_ahead_of_it() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres artifact race: database URL is not set");
        return;
    };
    reset(&storage).await;
    let store = storage.lashlang_artifact_store();
    let module = artifact("process race(root: str) -> str { finish root }");
    let owner_a = lash_core::ArtifactOwner::host("artifact-race-a");
    let owner_b = lash_core::ArtifactOwner::host("artifact-race-b");
    store
        .publish_module_artifact(&owner_a, &module)
        .await
        .expect("publish owner A");

    let mut publisher = lock_artifact_mutations(&storage, &module.module_ref).await;
    sqlx::query(
        "INSERT INTO lash_artifact_owners
         (namespace, artifact_ref, owner_kind, owner_id)
         VALUES ('lashlang_module', $1, 'host', 'artifact-race-b')",
    )
    .bind(module.module_ref.as_str())
    .execute(&mut *publisher)
    .await
    .expect("stage uncommitted owner B edge");

    let releasing_store = store.clone();
    let module_ref = module.module_ref.clone();
    let release = tokio::spawn(async move {
        releasing_store
            .release_module_artifact(&owner_a, &module_ref)
            .await
    });
    wait_until_release_reaches_its_serialization_point(&storage).await;
    assert!(
        !release.is_finished(),
        "release must wait behind the mutation lock"
    );
    publisher.commit().await.expect("commit owner B");
    release
        .await
        .expect("join release")
        .expect("release owner A");

    assert!(
        store
            .get_module_artifact(&module.module_ref)
            .await
            .expect("read B-owned artifact")
            .is_some(),
        "the owner that committed ahead of release must keep the bytes live"
    );
    store
        .release_module_artifact(&owner_b, &module.module_ref)
        .await
        .expect("release owner B");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_concurrent_final_artifact_releases_converge_to_absent_bytes() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres artifact race: database URL is not set");
        return;
    };
    reset(&storage).await;
    let store = storage.lashlang_artifact_store();
    let module = artifact("process releases(root: str) -> str { finish root }");
    let owner_a = lash_core::ArtifactOwner::host("final-release-a");
    let owner_b = lash_core::ArtifactOwner::host("final-release-b");
    store
        .publish_module_artifact(&owner_a, &module)
        .await
        .expect("publish owner A");
    store
        .retain_module_artifact(&owner_b, &module.module_ref)
        .await
        .expect("retain owner B");
    let blocker = lock_artifact_mutations(&storage, &module.module_ref).await;
    let left_store = store.clone();
    let left_ref = module.module_ref.clone();
    let left = tokio::spawn(async move {
        left_store
            .release_module_artifact(&owner_a, &left_ref)
            .await
    });
    let right_store = store.clone();
    let right_ref = module.module_ref.clone();
    let right = tokio::spawn(async move {
        right_store
            .release_module_artifact(&owner_b, &right_ref)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!left.is_finished() && !right.is_finished());
    blocker.commit().await.expect("release mutation gate");
    left.await.expect("join A").expect("release A");
    right.await.expect("join B").expect("release B");
    assert!(
        store
            .get_module_artifact(&module.module_ref)
            .await
            .expect("read after final releases")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_artifact_retirement_fences_a_late_publisher() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres artifact race: database URL is not set");
        return;
    };
    reset(&storage).await;
    let store = storage.lashlang_artifact_store();
    let module = artifact("process late(root: str) -> str { finish root }");
    let owner = lash_core::ArtifactOwner::execution(lash_core::ExecutionScope::runtime_operation(
        "late-publisher",
    ));
    store
        .publish_module_artifact(&owner, &module)
        .await
        .expect("publish the execution-owned artifact retirement will sever");
    let blocker = lock_artifact_mutations(&storage, &module.module_ref).await;
    let retiring_store = store.clone();
    let retiring_owner = owner.clone();
    let retirement = tokio::spawn(async move {
        retiring_store
            .retire_module_artifact_owner(&retiring_owner)
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !retirement.is_finished(),
        "retirement must wait at the exact artifact serialization key"
    );
    let publishing_store = store.clone();
    let publishing_module = module.clone();
    let publish = tokio::spawn(async move {
        publishing_store
            .publish_module_artifact(&owner, &publishing_module)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!publish.is_finished(), "publisher must wait for retirement");
    blocker.commit().await.expect("release exact artifact key");
    retirement
        .await
        .expect("join retirement")
        .expect("commit retirement fence");
    assert!(publish.await.expect("join publisher").is_err());
    assert!(
        store
            .get_module_artifact(&module.module_ref)
            .await
            .expect("read after late publisher")
            .is_none()
    );
}
