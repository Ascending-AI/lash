//! SessionTurn process-runner laws exercised by Restate's durable worker.

use super::*;
use lash_core::testing::TestTurnDrive;
use lash_core::testing::runtime_helpers::RecordingSessionStoreFactory;
use lash_core::{IngressStore, SessionCommitStore};
fn parked_provider(
    started: tokio::sync::mpsc::Sender<()>,
) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("mock")
        .complete(move |_| {
            let started = started.clone();
            async move {
                let _ = started.send(()).await;
                std::future::pending::<()>().await;
                unreachable!("the parked model call never completes")
            }
        })
        .build()
        .into_handle()
}

async fn parked_worker(
    registry: Arc<dyn ProcessRegistry>,
    factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> (
    DurableProcessWorker,
    tokio::sync::mpsc::Receiver<()>,
    lash_restate_test::RestateTestBackend,
) {
    let (started, receiver) = tokio::sync::mpsc::channel(1);
    let double = lash_restate_test::backend(1, lash_restate_test::ServerConfig::default())
        .await
        .expect("boot the SessionTurn server double");
    (
        worker_for(
            double.lash_backend(),
            registry,
            factory,
            parked_provider(started),
            Vec::new(),
        )
        .await,
        receiver,
        double,
    )
}

fn registration_for(child: &SessionId) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::SessionTurn {
            definition_key: "test-session-turn:v1".to_string(),
            create_request: Box::new(
                lash_core::SessionCreateRequest::child_session(
                    "test-parent",
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )
                .with_session_id(child),
            ),
            turn_input: Box::new(lash_core::TurnInput::text("run the child turn")),
            result: lash_core::SessionTurnResult::Turn,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
}

async fn worker_for(
    engine_backend: lash_core::Backend,
    registry: Arc<dyn ProcessRegistry>,
    session_factory: Arc<dyn lash_core::SessionStoreFactory>,
    provider: lash_core::facade_support::ProviderHandle,
    extra_plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> DurableProcessWorker {
    let mut plugins = vec![
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn lash_core::facade_support::PluginFactory>,
    ];
    plugins.extend(extra_plugins);
    let plugin_host = lash_core::facade_support::PluginHost::new(plugins);
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(engine_backend)
        .map_session_store_factory(|_| session_factory)
        .into_backend();
    let mut runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        backend,
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_lease_timings(
        lash_core::facade_support::LeaseTimings::from_ttl(Duration::from_millis(120))
            .expect("short child lease timings"),
    );
    runtime_host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            restate_process_work(registry, continuation_store()),
            Arc::new(lash_core::NoSessionWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(lash_core::SessionPolicy {
            provider_id: "mock".to_string(),
            ..recovery_session_policy()
        }),
    )
    .expect("valid SessionTurn worker")
}

fn answering_provider(text: &str) -> lash_core::facade_support::ProviderHandle {
    let text = text.to_string();
    lash_core::testing::TestProvider::builder()
        .kind("mock")
        .complete(move |_| {
            let text = text.clone();
            async move {
                Ok(lash_core::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle()
}

async fn parent_runtime(
    registry: Arc<dyn ProcessRegistry>,
    factory: Arc<dyn lash_core::SessionStoreFactory>,
) -> lash_core::facade_support::LashRuntime {
    let parent = SessionId::from("test-parent");
    let policy = lash_core::SessionPolicy {
        session_id: Some(parent.clone()),
        provider_id: "mock".to_string(),
        ..recovery_session_policy()
    };
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            session_id: parent.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: policy.clone(),
        })
        .await
        .expect("create parent session store");
    let state = lash_core::RuntimeSessionState {
        session_id: parent.clone(),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    let backend =
        lash_core::testing::runtime_helpers::LayeredBackend::over(memory_engine_backend().await)
            .map_session_store_factory(|_| factory)
            .wire_process_work(|_| restate_process_work(registry, continuation_store()))
            .into_backend();
    let mut host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(answering_provider("parent lives")),
    );
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_id(&parent)
        .with_policy(policy)
        .with_initial_state(state)
        .with_plugin_factories(lash_core::testing::test_standard_protocol_factories())
        .with_store(store)
        .with_queued_work(Arc::new(lash_core::NoSessionWork::new()))
        .with_process_work(backend.process_work())
        .build(),
    )
    .await
    .expect("build parent runtime")
}

async fn run(
    worker: &DurableProcessWorker,
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    registration: &ProcessRegistration,
    context: Arc<ReplayableRecordingContext>,
    authority: RestateAuthorityId,
    namespace: RestateNamespace,
) -> Result<lash_core::ProcessRunOutcome, PluginError> {
    let controller = RestateRuntimeEffectController::with_options(
        context,
        authority,
        RestateEffectControllerOptions::default().process_segment_drive(),
    )
    .in_namespace(namespace);
    worker
        .run_process_segment_with_scoped_effect_controller(
            process_id.clone(),
            registration.clone(),
            ProcessExecutionContext::default(),
            lash_core::ProcessExecutionWriteAuthority::invocation(
                process_id.clone(),
                format!("session-turn-law:{process_id}"),
            ),
            controller
                .process_scope_for_test(
                    recorded_process_admission(registry.as_ref(), process_id).await,
                )
                .expect("scope SessionTurn process"),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
}

fn assert_completed(outcome: &lash_core::ProcessRunOutcome) {
    assert!(
        matches!(
            outcome,
            lash_core::ProcessRunOutcome::Terminal { output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Completed)
        ),
        "the SessionTurn process completed: {outcome:#?}"
    );
}

async fn cancel_child(
    worker: &DurableProcessWorker,
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    context: &Arc<ReplayableRecordingContext>,
) {
    let cancel = lash_core::CancelRequest::new(
        lash_core::CancelOrigin::OperatorRequested,
        format!("actor:fixture:{process_id}"),
        1,
    );
    worker
        .request_process_cancel(process_id, &cancel)
        .await
        .expect("commit process cancellation");
    let record = registry
        .get_process(process_id)
        .await
        .expect("read process after cancellation")
        .expect("process remains registered");
    worker
        .request_session_turn_child_stop(&record, &cancel)
        .await
        .expect("request the child's durable turn stop");
    context.commit_process_cancel();
}

fn assert_cancelled(outcome: &lash_core::ProcessRunOutcome) {
    assert!(
        matches!(
            outcome,
            lash_core::ProcessRunOutcome::Terminal { output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
        ),
        "the child process must settle cancelled: {outcome:#?}"
    );
}

#[tokio::test]
async fn redelivery_after_metadata_only_create_finishes_initialisation() {
    let registry = process_registry();
    let child = SessionId::from("metadata-only-worker-child");
    let registration = registration_for(&child);
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let session_factory = memory_session_store_factory().await;
    let ProcessInput::SessionTurn { create_request, .. } = registration.input.as_ref() else {
        unreachable!("SessionTurn registration");
    };
    session_factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            session_id: child.clone(),
            relation: create_request
                .as_ref()
                .clone()
                .with_caused_by(lash_core::CausalRef::Process {
                    process_id: process_id.clone(),
                })
                .relation,
            pending_observer_intents: Vec::new(),
            policy: recovery_session_policy(),
        })
        .await
        .expect("create metadata-only child");
    let partial = session_factory
        .open_existing_store_by_id(&child)
        .await
        .expect("open metadata-only child")
        .expect("metadata-only child exists");
    assert!(
        lash_core::store::load_persisted_session_state(partial.as_ref())
            .await
            .expect("load partial session")
            .is_none(),
        "precondition: the child has no committed head"
    );
    let worker = worker_for(
        memory_engine_backend().await,
        Arc::clone(&registry),
        Arc::clone(&session_factory),
        answering_provider("finished create answered"),
        Vec::new(),
    )
    .await;
    let outcome = run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::new(ReplayableRecordingContext::default()),
        test_restate_authority_id(),
        RestateNamespace::default(),
    )
    .await
    .expect("run SessionTurn after metadata-only create");
    assert_completed(&outcome);
    assert!(
        lash_core::store::load_persisted_session_state(partial.as_ref())
            .await
            .expect("load completed session")
            .is_some(),
        "redelivery finishes initialisation before the turn"
    );
}

#[tokio::test]
async fn cancelled_mid_turn_subagent_retains_durable_rows() {
    let registry = process_registry();
    let child = SessionId::from("cancelled-worker-child");
    let registration = registration_for(&child);
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register parked SessionTurn")
        .id;
    let factory = memory_session_store_factory().await;
    let (worker, mut started, _double) =
        parked_worker(Arc::clone(&registry), Arc::clone(&factory)).await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let mut attempt = Box::pin(run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::clone(&context),
        RestateAuthorityId::new("lash-restate-test-1")
            .expect("use the server double's authority identity"),
        RestateNamespace::default(),
    ));
    tokio::select! {
        received = started.recv() => assert_eq!(received, Some(())),
        outcome = attempt.as_mut() => panic!("the child completed before cancellation: {outcome:?}"),
    }
    let store = factory
        .open_existing_store_by_id(&child)
        .await
        .expect("open child before cancellation")
        .expect("the parked child has durable rows");
    assert!(
        store
            .load_session_meta()
            .await
            .expect("load child metadata")
            .is_some()
    );
    cancel_child(&worker, &registry, &process_id, &context).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .expect("cancelled child process settles")
        .expect("cancelled child process returns a terminal outcome");
    assert_cancelled(&outcome);
    assert!(
        store
            .load_session_meta()
            .await
            .expect("load retained child")
            .is_some()
    );
    assert!(
        store
            .list_pending_turn_inputs(&child)
            .await
            .expect("read retained child inputs")
            .is_empty(),
        "the retained child has no claimable input"
    );
    assert!(
        factory
            .open_existing_store_by_id(&child)
            .await
            .expect("reopen retained child")
            .is_some(),
        "the child stays reopenable"
    );
}

#[tokio::test]
async fn failed_final_child_commit_cancellation_stays_recoverable() {
    let registry = process_registry();
    let child = SessionId::from("failed-final-commit-worker-child");
    let registration = registration_for(&child);
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = Arc::new(RecordingSessionStoreFactory::over(
        memory_session_store_factory().await,
    ));
    let (worker, mut started, _double) =
        parked_worker(Arc::clone(&registry), Arc::clone(&factory) as Arc<_>).await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let authority = RestateAuthorityId::new("lash-restate-test-1").expect("test authority");
    let mut attempt = Box::pin(run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::clone(&context),
        authority.clone(),
        RestateNamespace::default(),
    ));
    tokio::select! {
        received = started.recv() => assert_eq!(received, Some(())),
        outcome = attempt.as_mut() => panic!("the child completed before the fault: {outcome:?}"),
    }
    let store = factory
        .store_for(&child)
        .expect("the child has a durable store");
    store.fail_next_runtime_commit(lash_core::StoreError::Contended);
    cancel_child(&worker, &registry, &process_id, &context).await;
    let failed = tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .expect("the failed commit returns");
    assert!(
        failed.is_err(),
        "a failed child commit must not terminalize: {failed:?}"
    );
    assert!(
        store
            .load_session_meta()
            .await
            .expect("load retained child")
            .is_some()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    context.start_replay_allowing_journal_extension();
    let replay = run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::clone(&context),
        authority,
        RestateNamespace::default(),
    )
    .await
    .expect("redelivery settles the retained child");
    assert_cancelled(&replay);
    assert!(
        store
            .list_pending_turn_inputs(&child)
            .await
            .expect("read settled child inputs")
            .is_empty()
    );
}

#[tokio::test]
async fn crash_after_acceptance_redelivery_settles_retained_child_input() {
    let registry = process_registry();
    let child = SessionId::from("crashed-accepted-worker-child");
    let registration = registration_for(&child);
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = memory_session_store_factory().await;
    let (worker, mut started, _double) =
        parked_worker(Arc::clone(&registry), Arc::clone(&factory)).await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let authority = RestateAuthorityId::new("lash-restate-test-1").expect("test authority");
    let mut attempt = Box::pin(run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::clone(&context),
        authority.clone(),
        RestateNamespace::default(),
    ));
    tokio::select! {
        received = started.recv() => assert_eq!(received, Some(())),
        outcome = attempt.as_mut() => panic!("the child completed before the crash: {outcome:?}"),
    }
    let store = factory
        .open_existing_store_by_id(&child)
        .await
        .expect("open accepted child")
        .expect("the child is durable before the crash");
    assert!(
        !store
            .list_pending_turn_inputs(&child)
            .await
            .expect("read accepted child input")
            .is_empty(),
        "precondition: the crashed turn has accepted input"
    );
    drop(attempt);
    tokio::time::sleep(Duration::from_millis(400)).await;
    cancel_child(&worker, &registry, &process_id, &context).await;
    context.start_replay_allowing_journal_extension();
    let replay = run(
        &worker,
        &registry,
        &process_id,
        &registration,
        context,
        authority,
        RestateNamespace::default(),
    )
    .await
    .expect("redelivery settles the retained child");
    assert_cancelled(&replay);
    assert!(
        store
            .list_pending_turn_inputs(&child)
            .await
            .expect("read settled child inputs")
            .is_empty()
    );
}

#[tokio::test]
async fn redelivery_after_create_commit_reopens_child_and_runs_turn() {
    let registry = process_registry();
    let child = SessionId::from("committed-create-worker-child");
    let registration = registration_for(&child);
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = memory_session_store_factory().await;
    let parent = parent_runtime(Arc::clone(&registry), Arc::clone(&factory)).await;
    let ProcessInput::SessionTurn { create_request, .. } = registration.input.as_ref() else {
        unreachable!("SessionTurn registration");
    };
    parent
        .session_lifecycle_service()
        .expect("parent lifecycle service")
        .create_session(create_request.as_ref().clone().with_caused_by(
            lash_core::CausalRef::Process {
                process_id: process_id.clone(),
            },
        ))
        .await
        .expect("commit child create without accepting turn input");
    let store = factory
        .open_existing_store_by_id(&child)
        .await
        .expect("open created child")
        .expect("created child row exists");
    assert!(
        lash_core::store::load_persisted_session_state(store.as_ref())
            .await
            .expect("load committed child")
            .is_some(),
        "precondition: the child head is committed"
    );
    assert!(
        store
            .list_pending_turn_inputs(&child)
            .await
            .expect("read child turn inputs")
            .is_empty(),
        "precondition: no turn input was accepted"
    );
    let worker = worker_for(
        memory_engine_backend().await,
        Arc::clone(&registry),
        factory,
        answering_provider("redelivered child answered"),
        Vec::new(),
    )
    .await;
    let outcome = run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::new(ReplayableRecordingContext::default()),
        test_restate_authority_id(),
        RestateNamespace::default(),
    )
    .await
    .expect("redelivery reopens committed child");
    assert_completed(&outcome);
}

#[tokio::test]
async fn child_turn_panic_is_typed_and_the_parent_remains_alive() {
    let registry = process_registry();
    let child = SessionId::from("panicking-worker-child");
    let registration = registration_for(&child);
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = memory_session_store_factory().await;
    let mut parent = parent_runtime(Arc::clone(&registry), Arc::clone(&factory)).await;
    let panic_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let panic_plugin = Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "child-panic-test",
        lash_core::plugin::PluginSpec::new().with_prompt_contributor(Arc::new(move |_| {
            let panic_once = Arc::clone(&panic_once);
            Box::pin(async move {
                if panic_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    panic!("child turn payload only");
                }
                Ok(Vec::new())
            })
        })),
    )) as Arc<dyn lash_core::facade_support::PluginFactory>;
    let worker = worker_for(
        memory_engine_backend().await,
        Arc::clone(&registry),
        factory,
        answering_provider("child answer"),
        vec![panic_plugin],
    )
    .await;
    let previous = lash_core::panic_containment::set_loud(false);
    let outcome = run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::new(ReplayableRecordingContext::default()),
        test_restate_authority_id(),
        RestateNamespace::default(),
    )
    .await;
    lash_core::panic_containment::set_loud(previous);
    let err = outcome.expect_err("child panic surfaces as a typed failure");
    assert!(
        err.to_string()
            .contains("child_turn_panicked: child turn payload only"),
        "unexpected child panic: {err}"
    );
    let parent_id = SessionId::from("test-parent");
    let turn_id = TurnId::from("parent-after-child-panic");
    let controller = RestateRuntimeEffectController::new_for_test(Arc::new(
        ReplayableRecordingContext::default(),
    ));
    let scope = controller
        .scoped_effect_controller(durable_admission(&durable_turn_scope(&parent_id, &turn_id)))
        .expect("scope parent turn");
    let answer = parent
        .drive_turn(
            lash_core::TurnInput::text("continue parent"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            ),
        )
        .await
        .expect("parent survives child panic");
    assert_eq!(answer.assistant_output.safe_text, "parent lives");
}

#[tokio::test]
async fn a_start_in_a_process_owned_session_records_its_owner_above_the_session() {
    let registry = process_registry();
    let factory = memory_session_store_factory().await;
    let parent = parent_runtime(Arc::clone(&registry), Arc::clone(&factory)).await;
    let parent_id = SessionId::from("test-parent");
    let parent_turn = lash_core::ScopeId::turn(parent_id.clone(), TurnId::from("owner-start-turn"));
    let child = SessionId::from("process-owned-worker-child");
    let mut registration = registration_for(&child);
    registration.provenance =
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(parent_id.as_str()));
    registration.ancestry = lash_core::Ancestry::from_scopes([
        parent_turn.clone(),
        lash_core::ScopeId::session(parent_id.clone()),
    ]);
    registration.session_capability = Some(parent_id.clone());
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register owner SessionTurn")
        .id;
    let worker = worker_for(
        memory_engine_backend().await,
        Arc::clone(&registry),
        Arc::clone(&factory),
        answering_provider("owned session answered"),
        Vec::new(),
    )
    .await;
    let outcome = run(
        &worker,
        &registry,
        &process_id,
        &registration,
        Arc::new(ReplayableRecordingContext::default()),
        test_restate_authority_id(),
        RestateNamespace::default(),
    )
    .await
    .expect("owner SessionTurn completes");
    assert_completed(&outcome);
    let owned_store = factory
        .open_existing_store_by_id(&child)
        .await
        .expect("open process-owned session")
        .expect("owner created child session");
    assert_eq!(
        owned_store
            .load_session_meta()
            .await
            .expect("load owned child metadata")
            .expect("owned child metadata exists")
            .owning_process_id,
        Some(process_id.clone())
    );

    let owned_turn = TurnId::from("owned-session-later-turn");
    let context = Arc::new(ReplayableRecordingContext::default());
    context
        .defer_process_workflows
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let controller = RestateRuntimeEffectController::new_for_test(context);
    let scoped = controller
        .scoped_effect_controller(durable_admission(&durable_turn_scope(&child, &owned_turn)))
        .expect("scope a later owned-session turn");
    let request = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new(child.as_str())),
        lash_core::Lifetime::Detached,
    )
    .keyed_in(&scoped);
    let started = parent
        .process_service()
        .expect("parent process service")
        .start_from_request(&parent_id, request, lash_core::ProcessOpScope::new(scoped))
        .await
        .expect("start in a later owned-session turn");
    let record = registry
        .get_process(&started.process_id)
        .await
        .expect("read later start")
        .expect("later start registered");
    assert_eq!(
        record.ancestry.scopes(),
        &[
            lash_core::ScopeId::turn(child.clone(), owned_turn),
            lash_core::ScopeId::session(child.clone()),
            lash_core::ScopeId::process(process_id),
            parent_turn,
            lash_core::ScopeId::session(parent_id),
        ]
    );
    assert_eq!(record.session_capability, Some(child));
}
