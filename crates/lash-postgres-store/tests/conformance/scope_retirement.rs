//! Scope-exact retirement witnesses: the quiescence gate and the admission
//! lock the retirement shares with every other scope atom (FIG-2499).

use super::*;

/// A quiescence-gated retirement leaves a draining scope's rows alone and
/// fences nothing; once the drain settles it removes the rows and leaves the
/// fence (FIG-2499 fix round 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_quiescent_retirement_waits_for_the_drain() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres quiescent retirement test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let database_url = database_url().expect("configured Postgres database URL");
    let scope_id =
        lash_conformance::effect_group_quiescent_retirement_waits_for_live_children(|executors| {
            let database_url = database_url.clone();
            let storage = sync_await(async move {
                PostgresStorage::connect(&database_url)
                    .await
                    .expect("PostgreSQL effect-group host")
            });
            let host = storage.effect_host();
            if let Some(executors) = executors {
                host.register_group_executors(executors)
                    .expect("a freshly connected host has no resolver yet");
            }
            Arc::new(host) as Arc<dyn EffectHost>
        })
        .await;
    let pool = storage.pool();
    let count = |sql: &'static str| {
        let scope_id = scope_id.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(scope_id)
                .fetch_one(pool)
                .await
                .expect("count journal rows")
        }
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM lash_runtime_effect_replay WHERE scope_id = $1").await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM lash_runtime_effect_group WHERE scope_id = $1").await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM lash_effect_scope_retirements WHERE scope_id = $1").await,
        1
    );
    drop(database_lock);
}

/// Scope retirement serializes behind the same advisory lock every admission
/// path takes (`lock_scope`, namespace 563): while a transaction holds the
/// scope's lock, the retirement blocks rather than fencing and deleting
/// around it. Removing `lock_scope` from the retirement lets this retirement
/// complete under the held lock and fails the test (FIG-2499 fix round 1,
/// mutation witness).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_scope_retirement_waits_for_the_admission_lock() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres retirement lock test: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    reset(&storage).await;
    let scope =
        ExecutionScope::runtime_operation(format!("held-lock-{}", uuid::Uuid::new_v4().simple()));
    let key = scope
        .journal_identity()
        .expect("runtime-operation journal identity")
        .key()
        .to_string();
    let mut holder = storage
        .pool()
        .begin()
        .await
        .expect("lock holder transaction");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 563))")
        .bind(&key)
        .execute(&mut *holder)
        .await
        .expect("hold the scope's admission lock");
    let host: Arc<dyn EffectHost> = Arc::new(storage.effect_host());
    let retiring_scope = scope.clone();
    let mut retirement = tokio::spawn(async move {
        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&retiring_scope)
                .expect("runtime-operation scopes retire"),
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), &mut retirement)
            .await
            .is_err(),
        "retirement must wait for the admission lock"
    );
    holder.commit().await.expect("release the lock");
    let deleted = tokio::time::timeout(std::time::Duration::from_secs(30), retirement)
        .await
        .expect("retirement completes once the lock is released")
        .expect("retirement task")
        .expect("retirement succeeds");
    assert_eq!(deleted, 0, "an empty scope retires nothing but its fence");
    let fences: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_effect_scope_retirements WHERE scope_id = $1",
    )
    .bind(&key)
    .fetch_one(storage.pool())
    .await
    .expect("count fences");
    assert_eq!(fences, 1);
    drop(database_lock);
}
