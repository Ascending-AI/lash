use super::*;

async fn authorize_restate_completion_closure(
    host: &Arc<lash_restate::RestateEffectHost>,
    factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session: &str,
    physical_scope: &lash_core::ExecutionScope,
) -> (
    Arc<dyn lash_core::RuntimePersistence>,
    lash_core::SessionExecutionLease,
    lash_core::TurnCancelClosureAuthorization,
) {
    use lash_core::{EffectHost as _, SessionStoreFactory as _};

    let address = lash_core::runtime::TurnAddress::new(session, "turn");
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: address.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create live Restate catalog session");
    let lease = store
        .try_claim_session_execution_lease(
            &address.session_id,
            &lash_core::LeaseOwnerIdentity::opaque(session, format!("{session}:incarnation")),
            &format!("{session}:executor"),
            60_000,
        )
        .await
        .expect("claim live Restate closure lane")
        .acquired()
        .expect("live Restate closure lane is free");
    let scoped = host
        .scoped(physical_scope.clone())
        .expect("scope live Restate effect owner");
    let binding = host
        .turn_control_binding(&scoped)
        .await
        .expect("bind live Restate effect owner");
    store
        .validate_turn_cancellation_binding(
            &address.session_id,
            &lease.fence(),
            binding.binding_id(),
            physical_scope,
        )
        .await
        .expect("persist live Restate admitted scope");
    let resolver = binding.resolver();
    let authorization = lash_core::TurnCancelClosureAuthorization::new(
        address.clone(),
        binding.binding_id(),
        physical_scope.clone(),
        resolver
            .await_event_key(
                &address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await
            .expect("base key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await
            .expect("escalation key"),
        resolver
            .await_event_key(
                &address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnTerminal,
            )
            .await
            .expect("terminal key"),
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .expect("materialize live Restate closure authorization");
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("register live Restate participant before local authorization");
    (store, lease, authorization)
}

async fn settle_and_release_restate_completion_closure(
    effect_host: Arc<dyn lash_core::EffectHost>,
    factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    scope: &lash_core::ExecutionScope,
    store: Arc<dyn lash_core::RuntimePersistence>,
    lease: lash_core::SessionExecutionLease,
    authorization: lash_core::TurnCancelClosureAuthorization,
) {
    use lash_core::SessionStoreFactory as _;

    let authority = lash_core::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host,
    );
    let settlement = authority
        .settle_authorized_closure(&authorization)
        .await
        .expect("settle closure at live Restate owner");
    store
        .repair_orphaned_active_turn_inputs(
            authorization.session_id(),
            &lease.fence(),
            authorization.turn_id(),
            authorization.observed_intent(),
            Some(&settlement),
        )
        .await
        .expect("consume live Restate catalog pin")
        .into_applied()
        .expect("live Restate repair applies");
    factory
        .retire_turn_cancel_closure_scope(scope)
        .await
        .expect("release live Restate catalog participant");
}

#[test]
#[ignore = "requires a running Restate server; use `just agent-workbench-restate-e2e`"]
fn live_restate_closure_participants_serialize_direct_index_retirement() {
    run_async_test_on_stack_budget_multi_thread("workbench-closure-lifecycle-e2e", 4, || async {
        use lash_core::{EffectHost as _, SessionStoreFactory as _};

        let ingress_url = std::env::var("RESTATE_INGRESS_URL")
            .expect("RESTATE_INGRESS_URL must be set by the workbench Restate E2E recipe");
        let admin_url = std::env::var("RESTATE_ADMIN_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:19071".to_string());
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-closure-lifecycle-e2e-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create closure lifecycle E2E data dir");
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-closure-lifecycle-e2e")
            .complete(|_| async { Ok(text_response("unused")) })
            .build()
            .into_handle();
        let harness = live_workbench_restate_state_with_provider(
            &data_dir,
            ingress_url.clone(),
            provider,
            WorkbenchSessions::fresh(),
            ActiveTurns::default(),
        )
        .await;
        let mut endpoint = LiveRestateEndpoint::start(
            &admin_url,
            harness.state.clone(),
            harness.process_deployment,
            harness.process_worker,
        )
        .await;
        let host = Arc::new(lash_restate::RestateEffectHost::new(
            lash_restate::RestateConnection::with_client(ingress_url, reqwest::Client::new()),
            lash_restate::RestateAuthorityId::new("agent-workbench-tests")
                .expect("valid live Restate authority"),
        ));
        let effect_host: Arc<dyn lash_core::EffectHost> = host.clone();
        let factory_a =
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("closure-catalog-a"));
        let factory_b =
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("closure-catalog-b"));
        factory_a.bind_effect_host(&effect_host);
        factory_b.bind_effect_host(&effect_host);

        let scope = lash_core::ExecutionScope::process("live-restate-shared-owner");
        let (store_a, lease_a, authorization_a) = authorize_restate_completion_closure(
            &host,
            &factory_a,
            "live-restate-catalog-a",
            &scope,
        )
        .await;
        let (store_b, lease_b, authorization_b) = authorize_restate_completion_closure(
            &host,
            &factory_b,
            "live-restate-catalog-b",
            &scope,
        )
        .await;
        let retirement = || lash_core::EffectJournalRetirement::for_scope(&scope).unwrap();
        let blocked = host
            .retire_effect_journal(retirement())
            .await
            .expect_err("actual Restate index retirement observes both catalogs");
        assert_eq!(
            blocked.code,
            lash_core::RuntimeErrorCode::EffectScopeNotQuiescent
        );

        settle_and_release_restate_completion_closure(
            effect_host.clone(),
            &factory_a,
            &scope,
            store_a,
            lease_a,
            authorization_a,
        )
        .await;
        assert!(
            host.retire_effect_journal(retirement()).await.is_err(),
            "the second live catalog still fences direct index retirement"
        );
        settle_and_release_restate_completion_closure(
            effect_host,
            &factory_b,
            &scope,
            store_b,
            lease_b,
            authorization_b,
        )
        .await;
        host.retire_effect_journal(retirement())
            .await
            .expect("live Restate index retires after every participant releases");

        let late_scope = lash_core::ExecutionScope::process("live-restate-retire-first");
        let late_address =
            lash_core::runtime::TurnAddress::new("live-restate-late-catalog", "turn");
        let late_store = factory_a
            .create_store(&lash_core::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: late_address.session_id.clone(),
                relation: lash_core::SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .expect("create late live Restate catalog session");
        let late_lease = late_store
            .try_claim_session_execution_lease(
                &late_address.session_id,
                &lash_core::LeaseOwnerIdentity::opaque("late", "late:incarnation"),
                "late:executor",
                60_000,
            )
            .await
            .expect("claim late live Restate lane")
            .acquired()
            .expect("late live Restate lane is free");
        let late_scoped = host.scoped(late_scope.clone()).expect("scope late owner");
        let late_binding = host
            .turn_control_binding(&late_scoped)
            .await
            .expect("capture binding before owner retirement");
        late_store
            .validate_turn_cancellation_binding(
                &late_address.session_id,
                &late_lease.fence(),
                late_binding.binding_id(),
                &late_scope,
            )
            .await
            .expect("persist late original scope before owner retirement");
        let late_resolver = late_binding.resolver();
        let late_authorization = lash_core::TurnCancelClosureAuthorization::new(
            late_address.clone(),
            late_binding.binding_id(),
            late_scope.clone(),
            late_resolver
                .await_event_key(
                    &late_address.execution_scope(),
                    lash_core::AwaitEventWaitIdentity::TurnCancelGate,
                )
                .await
                .expect("late base key"),
            late_resolver
                .await_event_key(
                    &late_address.execution_scope(),
                    lash_core::AwaitEventWaitIdentity::TurnCancelEscalation,
                )
                .await
                .expect("late escalation key"),
            late_resolver
                .await_event_key(
                    &late_address.execution_scope(),
                    lash_core::AwaitEventWaitIdentity::TurnTerminal,
                )
                .await
                .expect("late terminal key"),
            lash_core::TurnCancelClosureProposal::CompletionSealed,
            lash_core::TurnCancelIntentSnapshot::Absent,
            &late_lease.fence(),
        )
        .expect("materialize late live Restate authorization");
        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&late_scope).unwrap(),
        )
        .await
        .expect("retire live Restate index before catalog authorization");
        late_store
            .authorize_turn_cancel_closure(&late_lease.fence(), &late_authorization)
            .await
            .expect_err("retired live Restate owner refuses late catalog authorization");
        assert!(
            late_store
                .pending_turn_cancel_closure_pins()
                .await
                .expect("read late live Restate catalog pins")
                .is_empty()
        );

        endpoint
            .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
            .await;
        let _ = std::fs::remove_dir_all(data_dir);
    });
}
