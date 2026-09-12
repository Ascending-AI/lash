use std::sync::Arc;

use lash_core::SessionStoreFactory;
use lash_core::facade_support::{NativeEffectHost, NativeRuntimeEffectController};
use lash_core::runtime::{TurnAddress, TurnCancelOutcome, TurnCancelRequest, TurnWorkDriver};
use lash_core::{
    AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, EffectJournalRetirement,
    ExecutionScope, LeaseOwnerIdentity, ProcessRegistrar, RuntimeErrorCode, ScopedEffectController,
    TurnCancelClosureAuthorization, TurnCancelClosureProposal, TurnCancelIntentSnapshot,
    TurnControlBinding,
};
use lash_sqlite_store::SqliteEffectHost;
use lash_sqlite_store::SqliteSessionStoreFactory;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn turn_control_local_gate_is_owned_and_awaited_by_sqlite_host() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let host = Arc::new(
        SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
            .await
            .unwrap(),
    );
    let controller = NativeRuntimeEffectController::default();
    let address = TurnAddress::new("local-session", "local-turn");
    let scoped = ScopedEffectController::borrowed(&controller, address.execution_scope()).unwrap();
    let binding = host.turn_control_binding(&scoped).await.unwrap();
    let resolver = match &binding {
        TurnControlBinding::HostOwned { resolver, .. }
        | TurnControlBinding::RunScoped { resolver, .. } => *resolver,
    };
    // ActiveTurnControl is crate-private. Exercise its exact cancel-gate identity
    // through the public resolver seam, without adding a testing-only facade.
    let cancel_key = resolver
        .await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await
        .unwrap();
    assert_eq!(
        host.peek_await_event(&cancel_key)
            .await
            .expect("host must recognize the turn cancel gate"),
        None
    );
    let stop_wait = CancellationToken::new();
    stop_wait.cancel();
    let result = host.await_await_event(&cancel_key, stop_wait, None).await;
    assert!(
        !matches!(result, Err(ref error) if error.code == RuntimeErrorCode::AwaitEventUnknownOrRevoked),
        "host wait rejected its turn gate: {result:?}"
    );
    assert!(matches!(binding, TurnControlBinding::HostOwned { .. }));

    // An external request must resolve the gate the Local turn actually watches.
    let store: Arc<dyn lash_core::RuntimePersistence> =
        Arc::new(lash_core::runtime::InMemorySessionStore::new());
    lash_core::testing::store_fixtures::bind_conformance_session(&store, &address.session_id).await;
    let receipt = TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store)
        .request_cancel(TurnCancelRequest::new(address, "external-cancel", None))
        .await
        .unwrap();
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    let terminal = resolver
        .peek_await_event(&cancel_key)
        .await
        .unwrap()
        .expect("external cancellation reaches the Local turn gate");
    assert_eq!(
        host.await_await_event(&cancel_key, CancellationToken::new(), None)
            .await
            .unwrap(),
        terminal
    );
}

#[tokio::test]
async fn turn_control_durable_journaled_binding_remains_run_scoped() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let host = SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
        .await
        .unwrap();
    let address = TurnAddress::new("durable-session", "durable-turn");
    let scoped = host.scoped(address.execution_scope()).unwrap();
    let binding = host.turn_control_binding(&scoped).await.unwrap();
    match binding {
        TurnControlBinding::RunScoped {
            resolver,
            durable_cancel_after_llm,
            ..
        } => {
            assert!(durable_cancel_after_llm);
            let key = resolver
                .await_event_key(
                    &address.execution_scope(),
                    AwaitEventWaitIdentity::TurnCancelGate,
                )
                .await
                .unwrap();
            assert_eq!(host.peek_await_event(&key).await.unwrap(), None);
        }
        TurnControlBinding::HostOwned { .. } => {
            panic!("durable controller must own its journaled turn control")
        }
    }
}

#[tokio::test]
async fn native_turn_control_reopens_through_durable_core_authority() {
    let dir = tempfile::tempdir().expect("temporary durable-core directory");
    let factory = SqliteSessionStoreFactory::new(dir.path());
    let address = TurnAddress::new("native-reopen-session", "native-reopen-turn");
    let create = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: address.session_id.clone(),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let first_store = factory.create_store(&create).await.expect("create store");
    let first = TurnWorkDriver::for_session(
        Arc::new(NativeEffectHost::default()),
        address.session_id.clone(),
        first_store,
    )
    .request_cancel(TurnCancelRequest::new(
        address.clone(),
        "native-persistent-cancel",
        None,
    ))
    .await
    .expect("resolve persistent Native cancellation");
    assert!(matches!(first.outcome, TurnCancelOutcome::Requested(_)));

    let reopened = factory
        .open_existing_store_by_id(&address.session_id)
        .await
        .expect("reopen catalog")
        .expect("session exists");
    let repeated = TurnWorkDriver::for_session(
        Arc::new(NativeEffectHost::default()),
        address.session_id.clone(),
        reopened,
    )
    .request_cancel(TurnCancelRequest::new(
        address,
        "native-persistent-cancel",
        None,
    ))
    .await
    .expect("observe cancellation after fresh Native host reopen");
    assert!(matches!(
        repeated.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence)
            if evidence.request_id == "native-persistent-cancel"
    ));
}

async fn authorize_completion_closure(
    host: &Arc<SqliteEffectHost>,
    factory: &SqliteSessionStoreFactory,
    session: &str,
    turn: &str,
    physical_scope: &ExecutionScope,
) -> (
    Arc<dyn lash_core::RuntimePersistence>,
    lash_core::SessionExecutionLease,
    TurnCancelClosureAuthorization,
) {
    let address = TurnAddress::new(session, turn);
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: address.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create catalog session");
    let executor_id = format!("{session}:executor");
    let lease = store
        .try_claim_session_execution_lease(
            &address.session_id,
            &LeaseOwnerIdentity::opaque(session, format!("{session}:incarnation")),
            &executor_id,
            60_000,
        )
        .await
        .expect("claim closure lane")
        .acquired()
        .expect("closure lane is free");
    let scoped = host
        .scoped(physical_scope.clone())
        .expect("scope effect owner");
    let binding = host
        .turn_control_binding(&scoped)
        .await
        .expect("bind exact effect owner");
    store
        .validate_turn_cancellation_binding(
            &address.session_id,
            &lease.fence(),
            binding.binding_id(),
            physical_scope,
        )
        .await
        .expect("persist admitted physical scope");
    let resolver = binding.resolver();
    let authorization = TurnCancelClosureAuthorization::new(
        address.clone(),
        binding.binding_id(),
        physical_scope.clone(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .expect("base key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await
            .expect("escalation key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnTerminal,
            )
            .await
            .expect("terminal key"),
        TurnCancelClosureProposal::CompletionSealed,
        TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .expect("materialize closure authorization");
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("register owner participant before local authorization");
    (store, lease, authorization)
}

#[tokio::test]
async fn direct_effect_retirement_waits_for_every_bound_catalog_participant() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let host = Arc::new(
        SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
            .await
            .expect("open effect owner"),
    );
    let registry = lash_sqlite_store::SqliteProcessRegistry::open(
        &dir.path().join("processes.sqlite"),
        dir.path().join("process-sessions"),
    )
    .await
    .expect("open separate process owner");
    let effect_host: Arc<dyn EffectHost> = host.clone();
    registry.bind_effect_host(&effect_host);
    let factory_a = SqliteSessionStoreFactory::new(dir.path().join("catalog-a"));
    let factory_b = SqliteSessionStoreFactory::new(dir.path().join("catalog-b"));
    factory_a.bind_effect_host(&effect_host);
    factory_b.bind_effect_host(&effect_host);
    let scope = ExecutionScope::process("shared-retirement-scope");
    let (store_a, lease_a, authorization_a) =
        authorize_completion_closure(&host, &factory_a, "catalog-a-session", "turn", &scope).await;
    let (store_b, lease_b, authorization_b) =
        authorize_completion_closure(&host, &factory_b, "catalog-b-session", "turn", &scope).await;

    let blocked = host
        .retire_effect_journal(EffectJournalRetirement::for_scope(&scope).unwrap())
        .await
        .expect_err("direct owner retirement must observe both catalogs");
    assert_eq!(blocked.code, RuntimeErrorCode::EffectScopeNotQuiescent);

    for (factory, store, lease, authorization) in [
        (&factory_a, store_a, lease_a, authorization_a),
        (&factory_b, store_b, lease_b, authorization_b),
    ] {
        let authority = lash_core::TurnCancellationAuthority::new(
            effect_host.turn_control_binding_id(),
            effect_host.clone(),
        );
        let settlement = authority
            .settle_authorized_closure(&authorization)
            .await
            .expect("settle closure at actual owner");
        store
            .repair_orphaned_active_turn_inputs(
                authorization.session_id(),
                &lease.fence(),
                authorization.turn_id(),
                authorization.observed_intent(),
                Some(&settlement),
            )
            .await
            .expect("consume local pin")
            .into_applied()
            .expect("repair applies");
        factory
            .retire_turn_cancel_closure_scope(&scope)
            .await
            .expect("retire one catalog and release its owner participant");
        if std::ptr::eq(factory, &factory_a) {
            assert!(
                host.retire_effect_journal(EffectJournalRetirement::for_scope(&scope).unwrap())
                    .await
                    .is_err(),
                "the second catalog still fences direct owner retirement"
            );
        }
    }
    host.retire_effect_journal(EffectJournalRetirement::for_scope(&scope).unwrap())
        .await
        .expect("owner retires only after every catalog released");
}

#[tokio::test]
async fn owner_retirement_before_authorization_refuses_the_catalog_without_a_pin() {
    let dir = tempfile::tempdir().expect("temporary database directory");
    let host = Arc::new(
        SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
            .await
            .expect("open effect owner"),
    );
    let registry = lash_sqlite_store::SqliteProcessRegistry::open(
        &dir.path().join("processes.sqlite"),
        dir.path().join("process-sessions"),
    )
    .await
    .expect("open separate process owner");
    let effect_host: Arc<dyn EffectHost> = host.clone();
    registry.bind_effect_host(&effect_host);
    let factory = SqliteSessionStoreFactory::new(dir.path().join("catalog"));
    factory.bind_effect_host(&effect_host);
    let scope = ExecutionScope::process("retired-before-authorization");
    let address = TurnAddress::new("late-catalog-session", "turn");
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: address.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create late catalog session");
    let lease = store
        .try_claim_session_execution_lease(
            &address.session_id,
            &LeaseOwnerIdentity::opaque("late-catalog", "late-catalog:incarnation"),
            "late-catalog:executor",
            60_000,
        )
        .await
        .expect("claim late lane")
        .acquired()
        .expect("late lane is free");
    let scoped = host
        .scoped(scope.clone())
        .expect("scope owner before retirement");
    let binding = host
        .turn_control_binding(&scoped)
        .await
        .expect("capture binding before retirement");
    store
        .validate_turn_cancellation_binding(
            &address.session_id,
            &lease.fence(),
            binding.binding_id(),
            &scope,
        )
        .await
        .expect("persist original scope before owner retirement");
    let resolver = binding.resolver();
    let authorization = TurnCancelClosureAuthorization::new(
        address.clone(),
        binding.binding_id(),
        scope,
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .unwrap(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await
            .unwrap(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                AwaitEventWaitIdentity::TurnTerminal,
            )
            .await
            .unwrap(),
        TurnCancelClosureProposal::CompletionSealed,
        TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .unwrap();
    host.retire_effect_journal(
        EffectJournalRetirement::for_scope(authorization.admitted_scope()).unwrap(),
    )
    .await
    .expect("retire owner before authorization");
    assert!(
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .is_err(),
        "owner fence must refuse late catalog authorization"
    );
    assert!(
        store
            .pending_turn_cancel_closure_pins()
            .await
            .expect("read local pins")
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn catalog_participant_identity_survives_precreation_symlink_reopen() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("temporary database directory");
    let real_root = dir.path().join("real-catalog");
    std::fs::create_dir_all(&real_root).expect("create real catalog root");
    let alias_root = dir.path().join("catalog-alias");
    symlink(&real_root, &alias_root).expect("create catalog root alias");
    let host = Arc::new(
        SqliteEffectHost::open(&dir.path().join("effects.sqlite"))
            .await
            .expect("open effect owner"),
    );
    let effect_host: Arc<dyn EffectHost> = host.clone();
    let alias_factory = SqliteSessionStoreFactory::new(&alias_root);
    alias_factory.bind_effect_host(&effect_host);
    assert!(
        !alias_factory.catalog_path().exists(),
        "binding precedes first catalog creation"
    );
    let scope = ExecutionScope::runtime_operation("aliased-catalog-owner");
    let (store, lease, authorization) = authorize_completion_closure(
        &host,
        &alias_factory,
        "aliased-catalog-session",
        "turn",
        &scope,
    )
    .await;
    assert!(
        host.retire_effect_journal(EffectJournalRetirement::for_scope(&scope).unwrap())
            .await
            .is_err(),
        "the aliased pre-creation binding registers its owner participant"
    );

    let authority = lash_core::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host,
    );
    let settlement = authority
        .settle_authorized_closure(&authorization)
        .await
        .expect("settle aliased catalog closure");
    store
        .repair_orphaned_active_turn_inputs(
            authorization.session_id(),
            &lease.fence(),
            authorization.turn_id(),
            authorization.observed_intent(),
            Some(&settlement),
        )
        .await
        .expect("consume aliased catalog pin")
        .into_applied()
        .expect("aliased catalog repair applies");

    let reopened = SqliteSessionStoreFactory::new(&real_root);
    let effect_host: Arc<dyn EffectHost> = host.clone();
    reopened.bind_effect_host(&effect_host);
    reopened
        .retire_turn_cancel_closure_scope(&scope)
        .await
        .expect("real-path reopen releases the alias-created participant");
    host.retire_effect_journal(EffectJournalRetirement::for_scope(&scope).unwrap())
        .await
        .expect("no stale alias participant permanently leaks");
}
