//! Scope-exact retirement witnesses: the quiescence gate and the admission
//! lock the retirement shares with every other scope atom (FIG-2499).

use super::*;

async fn postgres_completion_closure(
    host: &Arc<dyn EffectHost>,
    factory: &lash_postgres_store::PostgresSessionStoreFactory,
    session: &str,
    scope: &ExecutionScope,
) -> (
    Arc<dyn RuntimePersistence>,
    lash_core::SessionExecutionLease,
    lash_core::TurnCancelClosureAuthorization,
) {
    let address = lash_core::runtime::TurnAddress::new(session, "turn");
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: address.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create PostgreSQL closure session");
    let lease = store
        .try_claim_session_execution_lease(
            &address.session_id,
            &lash_core::LeaseOwnerIdentity::opaque(session, format!("{session}:incarnation")),
            &format!("{session}:executor"),
            60_000,
        )
        .await
        .expect("claim PostgreSQL closure lane")
        .acquired()
        .expect("PostgreSQL closure lane is free");
    let scoped = host.scoped(scope.clone()).expect("scope PostgreSQL owner");
    let binding = host
        .turn_control_binding(&scoped)
        .await
        .expect("bind PostgreSQL owner");
    store
        .validate_turn_cancellation_binding(
            &address.session_id,
            &lease.fence(),
            binding.binding_id(),
            scope,
        )
        .await
        .expect("persist PostgreSQL admitted scope");
    let resolver = binding.resolver();
    let authorization = lash_core::TurnCancelClosureAuthorization::new(
        address.clone(),
        binding.binding_id(),
        scope.clone(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .expect("PostgreSQL base key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await
            .expect("PostgreSQL escalation key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnTerminal,
            )
            .await
            .expect("PostgreSQL terminal key"),
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .expect("materialize PostgreSQL closure");
    (store, lease, authorization)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_direct_effect_retirement_serializes_with_bound_catalog() {
    let Some((database_lock, storage)) = storage().await else {
        eprintln!("skipping PostgreSQL closure-owner lifecycle test: database URL is not set");
        return;
    };
    reset(&storage).await;
    let host = Arc::new(storage.effect_host());
    let effect_host: Arc<dyn EffectHost> = host.clone();
    let factory = storage.session_store_factory();
    factory.bind_effect_host(&effect_host);
    let scope = ExecutionScope::process(format!(
        "postgres-closure-owner-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let (store, lease, authorization) =
        postgres_completion_closure(&effect_host, &factory, "postgres-closure-session", &scope)
            .await;
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("authorize PostgreSQL closure");

    let retirement = || lash_core::EffectJournalRetirement::for_scope(&scope).unwrap();
    let blocked = host
        .retire_effect_journal(retirement())
        .await
        .expect_err("direct PostgreSQL owner retirement observes catalog participant");
    assert_eq!(
        blocked.code,
        lash_core::RuntimeErrorCode::EffectScopeNotQuiescent
    );
    let authority = lash_core::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host.clone(),
    );
    let settlement = authority
        .settle_authorized_closure(&authorization)
        .await
        .expect("settle PostgreSQL closure at owner");
    store
        .repair_orphaned_active_turn_inputs(
            authorization.session_id(),
            &lease.fence(),
            authorization.turn_id(),
            authorization.observed_intent(),
            Some(&settlement),
        )
        .await
        .expect("consume PostgreSQL closure pin")
        .into_applied()
        .expect("PostgreSQL repair applies");
    factory
        .retire_turn_cancel_closure_scope(&scope)
        .await
        .expect("retire PostgreSQL catalog scope and release owner participant");
    host.retire_effect_journal(retirement())
        .await
        .expect("direct PostgreSQL owner retires after participant release");

    let late_scope = ExecutionScope::process(format!(
        "postgres-closure-retire-first-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let (late_store, late_lease, late_authorization) = postgres_completion_closure(
        &effect_host,
        &factory,
        "postgres-late-closure-session",
        &late_scope,
    )
    .await;
    host.retire_effect_journal(lash_core::EffectJournalRetirement::for_scope(&late_scope).unwrap())
        .await
        .expect("retire PostgreSQL owner before authorization");
    late_store
        .authorize_turn_cancel_closure(&late_lease.fence(), &late_authorization)
        .await
        .expect_err("retired PostgreSQL owner refuses late catalog authorization");
    assert!(
        late_store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read late PostgreSQL pins")
            .is_empty()
    );
    drop(database_lock);
}

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
