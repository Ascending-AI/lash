use super::*;

async fn authorize_restate_completion_closure(
    host: &Arc<dyn lash_core::EffectHost>,
    factory: &lash_sqlite_store::SqliteSessionStoreFactory,
    session: &str,
    physical_scope: &lash_core::ExecutionScope,
) -> (
    Arc<dyn lash_core::RuntimePersistence>,
    lash_core::SessionExecutionLease,
    lash_core::TurnCancelClosureAuthorization,
) {
    use lash_core::SessionStoreFactory as _;

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RestateParticipantCrashBoundary {
    AfterOwnerRegister,
    BeforeOwnerRelease,
}

struct RestateParticipantCrashHost {
    inner: Arc<lash_restate::RestateEffectHost>,
    boundary: RestateParticipantCrashBoundary,
    marker: std::path::PathBuf,
}

impl RestateParticipantCrashHost {
    async fn stop_at_boundary(&self) -> ! {
        std::fs::write(&self.marker, b"durable boundary reached\n")
            .expect("write live Restate participant crash marker");
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for RestateParticipantCrashHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for RestateParticipantCrashHost {
    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        self.inner.scoped(scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::ExecutionScope,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.inner.scoped_static(scope)
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self.inner.await_event_resolver()
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        self.inner
            .register_turn_cancel_closure_participant(participant_id, scope)
            .await?;
        if self.boundary == RestateParticipantCrashBoundary::AfterOwnerRegister {
            self.stop_at_boundary().await;
        }
        Ok(())
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &lash_core::ExecutionScope,
    ) -> Result<(), lash_core::RuntimeError> {
        if self.boundary == RestateParticipantCrashBoundary::BeforeOwnerRelease {
            self.stop_at_boundary().await;
        }
        self.inner
            .release_turn_cancel_closure_participant(participant_id, scope)
            .await
    }
}

fn live_restate_participant_host(ingress_url: String) -> Arc<lash_restate::RestateEffectHost> {
    Arc::new(lash_restate::RestateEffectHost::new(
        lash_restate::RestateConnection::with_client(ingress_url, reqwest::Client::new()),
        lash_restate::RestateAuthorityId::new("agent-workbench-tests")
            .expect("valid live Restate authority"),
    ))
}

#[test]
#[ignore = "spawned and killed by the live Restate participant lifecycle law"]
fn live_restate_participant_protocol_crash_child() {
    run_async_test_on_stack_budget_multi_thread("workbench-participant-crash-child", 2, || async {
        use lash_core::SessionStoreFactory as _;

        let ingress_url = std::env::var("RESTATE_INGRESS_URL").expect("child Restate ingress");
        let catalog =
            std::env::var("LASH_RESTATE_PARTICIPANT_CRASH_CATALOG").expect("child catalog path");
        let scenario =
            std::env::var("LASH_RESTATE_PARTICIPANT_CRASH_SCENARIO").expect("child scenario");
        let marker = std::env::var_os("LASH_RESTATE_PARTICIPANT_CRASH_MARKER")
            .map(std::path::PathBuf::from)
            .expect("child marker");
        let boundary = match std::env::var("LASH_RESTATE_PARTICIPANT_CRASH_BOUNDARY")
            .expect("child boundary")
            .as_str()
        {
            "register" => RestateParticipantCrashBoundary::AfterOwnerRegister,
            "release" => RestateParticipantCrashBoundary::BeforeOwnerRelease,
            boundary => panic!("unknown Restate participant crash boundary {boundary}"),
        };
        let inner = live_restate_participant_host(ingress_url);
        let host: Arc<dyn lash_core::EffectHost> = Arc::new(RestateParticipantCrashHost {
            inner,
            boundary,
            marker,
        });
        let factory = lash_sqlite_store::SqliteSessionStoreFactory::new(catalog);
        factory.bind_effect_host(&host);
        let scope = lash_core::ExecutionScope::process(format!(
            "live-restate-participant-crash-{scenario}"
        ));
        if boundary == RestateParticipantCrashBoundary::AfterOwnerRegister {
            let (store, lease, authorization) = authorize_restate_completion_closure(
                &host,
                &factory,
                &format!("live-restate-participant-crash-{scenario}"),
                &scope,
            )
            .await;
            store
                .authorize_turn_cancel_closure(&lease.fence(), &authorization)
                .await
                .expect("register boundary never returns before the parent kills this child");
        } else {
            factory
                .retire_turn_cancel_closure_scope(&scope)
                .await
                .expect("release boundary never returns before the parent kills this child");
        }
        panic!("live Restate participant child passed its deterministic kill boundary");
    });
}

fn kill_live_restate_participant_child(
    catalog: &std::path::Path,
    scenario: &str,
    boundary: &str,
    marker: &std::path::Path,
) {
    let mut child = std::process::Command::new(
        std::env::current_exe().expect("locate live Restate test binary"),
    )
    .args([
        "live_restate_participant_protocol_crash_child",
        "--ignored",
        "--nocapture",
    ])
    .env("LASH_RESTATE_PARTICIPANT_CRASH_CATALOG", catalog)
    .env("LASH_RESTATE_PARTICIPANT_CRASH_SCENARIO", scenario)
    .env("LASH_RESTATE_PARTICIPANT_CRASH_BOUNDARY", boundary)
    .env("LASH_RESTATE_PARTICIPANT_CRASH_MARKER", marker)
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .spawn()
    .expect("spawn live Restate participant child");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        if let Some(status) = child
            .try_wait()
            .expect("poll live Restate participant child")
        {
            panic!("live Restate participant child exited before {boundary}: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "live Restate participant child did not reach {boundary} boundary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child
        .kill()
        .expect("kill child at live Restate participant boundary");
    let status = child.wait().expect("reap live Restate participant child");
    assert!(!status.success(), "the live boundary child must be killed");
}

async fn prove_live_restate_participant_crash_windows(
    host: &Arc<lash_restate::RestateEffectHost>,
    effect_host: &Arc<dyn lash_core::EffectHost>,
    data_dir: &std::path::Path,
) {
    use lash_core::{EffectHost as _, SessionStoreFactory as _};

    let register_scenario = "register";
    let register_catalog = data_dir.join("participant-crash-register-catalog");
    let register_marker = data_dir.join("participant-crash-after-register");
    let register_scope =
        lash_core::ExecutionScope::process("live-restate-participant-crash-register");
    kill_live_restate_participant_child(
        &register_catalog,
        register_scenario,
        "register",
        &register_marker,
    );
    let register_factory = lash_sqlite_store::SqliteSessionStoreFactory::new(&register_catalog);
    register_factory.bind_effect_host(effect_host);
    let register_session = lash_core::SessionId::from("live-restate-participant-crash-register");
    assert!(
        register_factory
            .pending_turn_cancel_closure_pins(&register_session)
            .await
            .expect("inspect register-crash local pins")
            .is_empty(),
        "the child died before local authorization"
    );
    assert!(
        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&register_scope).unwrap()
        )
        .await
        .is_err(),
        "the Restate owner retained the participant across the child crash"
    );
    register_factory
        .retire_turn_cancel_closure_scope(&register_scope)
        .await
        .expect("restart releases the orphan Restate participant");
    register_factory
        .retire_turn_cancel_closure_scope(&register_scope)
        .await
        .expect("Restate orphan release is idempotent");
    host.retire_effect_journal(
        lash_core::EffectJournalRetirement::for_scope(&register_scope).unwrap(),
    )
    .await
    .expect("Restate scope retires after orphan recovery");

    let release_scenario = "release";
    let release_catalog = data_dir.join("participant-crash-release-catalog");
    let release_factory = lash_sqlite_store::SqliteSessionStoreFactory::new(&release_catalog);
    release_factory.bind_effect_host(effect_host);
    let release_scope =
        lash_core::ExecutionScope::process("live-restate-participant-crash-release");
    let (store, lease, authorization) = authorize_restate_completion_closure(
        effect_host,
        &release_factory,
        "live-restate-participant-crash-release",
        &release_scope,
    )
    .await;
    let authority = lash_core::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host.clone(),
    );
    let settlement = authority
        .settle_authorized_closure(&authorization)
        .await
        .expect("settle release-crash closure");
    store
        .repair_orphaned_active_turn_inputs(
            authorization.session_id(),
            &lease.fence(),
            authorization.turn_id(),
            authorization.observed_intent(),
            Some(&settlement),
        )
        .await
        .expect("consume release-crash authorization")
        .into_applied()
        .expect("release-crash repair applies");
    drop(store);
    drop(release_factory);

    let release_marker = data_dir.join("participant-crash-before-release");
    kill_live_restate_participant_child(
        &release_catalog,
        release_scenario,
        "release",
        &release_marker,
    );
    let release_factory = lash_sqlite_store::SqliteSessionStoreFactory::new(&release_catalog);
    release_factory.bind_effect_host(effect_host);
    assert!(
        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&release_scope).unwrap()
        )
        .await
        .is_err(),
        "the Restate participant remains after local retirement crashes before release"
    );
    release_factory
        .retire_turn_cancel_closure_scope(&release_scope)
        .await
        .expect("restart retries local fence and Restate participant release");
    release_factory
        .retire_turn_cancel_closure_scope(&release_scope)
        .await
        .expect("post-crash Restate release is idempotent");
    host.retire_effect_journal(
        lash_core::EffectJournalRetirement::for_scope(&release_scope).unwrap(),
    )
    .await
    .expect("Restate owner retirement succeeds after release recovery");
    println!(
        "participant crash cuts passed: backend=restate after_owner_register=1 before_owner_release=1"
    );
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
            &effect_host,
            &factory_a,
            "live-restate-catalog-a",
            &scope,
        )
        .await;
        let (store_b, lease_b, authorization_b) = authorize_restate_completion_closure(
            &effect_host,
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
            effect_host.clone(),
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

        prove_live_restate_participant_crash_windows(&host, &effect_host, &data_dir).await;

        endpoint
            .stop_after_producers_closed_and_drained(&harness.state, Duration::from_secs(30))
            .await;
        let _ = std::fs::remove_dir_all(data_dir);
    });
}
