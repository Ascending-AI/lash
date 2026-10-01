//! SessionTurn process-runner laws exercised by Restate's durable worker.

use super::*;
use lash_core::testing::TestTurnDrive;
use lash_core::testing::runtime_helpers::RecordingDeploymentStore;

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
    factory: Arc<dyn lash_core::DeploymentStore>,
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

/// A host session-turn start of `child` under the environment it captured,
/// published to the store every worker of this file reads.
async fn registration_for(child: &SessionId) -> ProcessRegistration {
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
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(
        persist_session_turn_env_ref(RECOVERY_PROCESS_ENV_STORE.as_ref()).await,
    ))
}

async fn worker_for(
    engine_backend: lash_core::Backend,
    registry: Arc<dyn ProcessRegistry>,
    session_factory: Arc<dyn lash_core::DeploymentStore>,
    provider: lash_core::facade_support::ProviderHandle,
    extra_plugins: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
) -> DurableProcessWorker {
    worker_with_models(
        engine_backend,
        registry,
        session_factory,
        lash_core::testing::standard_test_models(provider),
        extra_plugins,
    )
    .await
}

/// A SessionTurn worker whose deployment installs `models`.
async fn worker_with_models(
    engine_backend: lash_core::Backend,
    registry: Arc<dyn ProcessRegistry>,
    session_factory: Arc<dyn lash_core::DeploymentStore>,
    models: Arc<dyn lash_core::RuntimeModels>,
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
        .map_process_env_store(|_| RECOVERY_PROCESS_ENV_STORE.clone())
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
    runtime_host.providers.models = models;
    DurableProcessWorker::new(lash_core_worker::DurableProcessWorkerConfig::new(
        Arc::new(plugin_host),
        runtime_host,
        restate_process_work(registry, continuation_store()),
        Arc::new(lash_core::NoSessionWork::new()),
        lash_core::testing::runtime_lease_owner(),
    ))
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
    factory: Arc<dyn lash_core::DeploymentStore>,
) -> lash_core::facade_support::LashRuntime {
    parent_runtime_with_models(
        registry,
        factory,
        lash_core::testing::standard_test_models(answering_provider("parent lives")),
    )
    .await
}

/// The parent session's runtime on a deployment that installs `models`.
async fn parent_runtime_with_models(
    registry: Arc<dyn ProcessRegistry>,
    factory: Arc<dyn lash_core::DeploymentStore>,
    models: Arc<dyn lash_core::RuntimeModels>,
) -> lash_core::facade_support::LashRuntime {
    let parent = SessionId::from("test-parent");
    let policy = lash_core::SessionPolicy {
        session_id: Some(parent.clone()),
        ..recovery_session_policy()
    };
    let store = lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            session_id: parent.clone(),
            relation: lash_core::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            config: policy.clone().into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .expect("create parent session store");
    let state = lash_core::RuntimeSessionState {
        session_id: parent.clone(),
        policy: policy.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
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
    host.providers.models = models;
    Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            host,
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_id(&parent)
        .with_policy(policy)
        .with_initial_state(state)
        // The worker that reopens a child this parent creates runs the
        // standard protocol, so the parent records that protocol's namespace
        // (FIG-4398).
        .with_plugin_factories(vec![
            Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
                as Arc<dyn lash_core::facade_support::PluginFactory>,
        ])
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
        crate::tests::test_build_generation(),
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
    let registration = registration_for(&child).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let session_factory = memory_session_store_factory().await;
    let ProcessInput::SessionTurn { create_request, .. } = registration.input.as_ref() else {
        unreachable!("SessionTurn registration");
    };
    lash_core::runtime::admit_session_view(
        &session_factory,
        &lash_core::SessionStoreCreateRequest {
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
            config: recovery_session_policy().into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .expect("create metadata-only child");
    let partial = lash_core::runtime::live_session_view(&session_factory, &child)
        .await
        .expect("open metadata-only child")
        .expect("metadata-only child exists");
    assert!(
        partial
            .load_session_window(lash_core::store::WindowSelector::Current)
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
        partial
            .load_session_window(lash_core::store::WindowSelector::Current)
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
    let registration = registration_for(&child).await;
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
    let store = lash_core::runtime::live_session_view(&factory, &child)
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
            .list_pending_turn_inputs()
            .await
            .expect("read retained child inputs")
            .is_empty(),
        "the retained child has no admissible input"
    );
    assert!(
        lash_core::runtime::live_session_view(&factory, &child)
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
    let registration = registration_for(&child).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = Arc::new(RecordingDeploymentStore::over(
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
        lash_core::store::SessionCommitStore::load_session_meta(store.as_ref(), &child)
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
        lash_core::store::TurnInputStore::list_pending_turn_inputs(store.as_ref(), &child)
            .await
            .expect("read settled child inputs")
            .is_empty()
    );
}

#[tokio::test]
async fn crash_after_acceptance_redelivery_settles_retained_child_input() {
    let registry = process_registry();
    let child = SessionId::from("crashed-accepted-worker-child");
    let registration = registration_for(&child).await;
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
    let store = lash_core::runtime::live_session_view(&factory, &child)
        .await
        .expect("open accepted child")
        .expect("the child is durable before the crash");
    assert!(
        !store
            .list_pending_turn_inputs()
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
            .list_pending_turn_inputs()
            .await
            .expect("read settled child inputs")
            .is_empty()
    );
}

#[tokio::test]
async fn redelivery_after_create_commit_reopens_child_and_runs_turn() {
    let registry = process_registry();
    let child = SessionId::from("committed-create-worker-child");
    let registration = registration_for(&child).await;
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
    let store = lash_core::runtime::live_session_view(&factory, &child)
        .await
        .expect("open created child")
        .expect("created child row exists");
    assert!(
        store
            .load_session_window(lash_core::store::WindowSelector::Current)
            .await
            .expect("load committed child")
            .is_some(),
        "precondition: the child head is committed"
    );
    assert!(
        store
            .list_pending_turn_inputs()
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

/// The key a subagent tier names for its child.
const FAST: &str = "fast";

/// A host session-turn start of `child` that names the model key [`FAST`].
async fn keyed_registration_for(child: &SessionId) -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::SessionTurn {
            definition_key: "test-session-turn:v1".to_string(),
            create_request: Box::new(
                lash_core::SessionCreateRequest::child_session(
                    "test-parent",
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )
                .with_session_id(child)
                .with_model(lash_core::ModelKey::new(FAST)),
            ),
            turn_input: Box::new(lash_core::TurnInput::text("run the child turn")),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(
        persist_session_turn_env_ref(RECOVERY_PROCESS_ENV_STORE.as_ref()).await,
    ))
}

/// A deployment that serves the standard test model and [`FAST`].
fn models_serving_fast(
    provider: lash_core::facade_support::ProviderHandle,
) -> Arc<dyn lash_core::RuntimeModels> {
    Arc::new(
        lash_core::ModelRegistry::new()
            .register(
                "mock-model",
                lash_core::RegisteredModel::new(
                    lash_core::testing::test_model_metadata("mock-model"),
                    provider.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    FAST,
                    lash_core::RegisteredModel::new(
                        lash_core::testing::test_model_metadata("fast-wire"),
                        provider,
                    ),
                )
            })
            .expect("two distinct keys register"),
    )
}

/// A deployment that binds any recorded binding to its transport and mints
/// nothing: every catalog read for a key is counted and refused.
struct BindOnlyModels {
    provider: lash_core::facade_support::ProviderHandle,
    mints: std::sync::atomic::AtomicUsize,
}

impl lash_core::RuntimeModels for BindOnlyModels {
    fn snapshot(
        &self,
        key: &lash_core::ModelKey,
    ) -> Result<lash_core::RecordedModel, lash_core::ModelUnavailable> {
        self.mints.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(lash_core::ModelUnavailable::new(
            key.clone(),
            lash_core::ModelUnavailableReason::UnknownKey,
        ))
    }

    fn bind(
        &self,
        _recorded: &lash_core::RecordedModel,
    ) -> Result<lash_core::facade_support::ProviderHandle, lash_core::ModelUnavailable> {
        Ok(self.provider.clone())
    }
}

/// Commit `registration`'s child on a deployment that serves [`FAST`],
/// without accepting its turn input: the state a worker crash leaves after
/// the child's create commit.
async fn commit_keyed_child(
    registry: &Arc<dyn ProcessRegistry>,
    factory: &Arc<dyn lash_core::DeploymentStore>,
    registration: &ProcessRegistration,
    process_id: &ProcessId,
) {
    let parent = parent_runtime_with_models(
        Arc::clone(registry),
        Arc::clone(factory),
        models_serving_fast(answering_provider("parent lives")),
    )
    .await;
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
        .expect("commit the keyed child");
}

/// FIG-4531: a redelivered session-turn child that is already committed
/// reopens from its durable row before anything is read from the
/// deployment's catalog. The child recorded the binding its key minted; the
/// next deployment no longer mints the key, and the redelivery runs the
/// recorded binding to completion without one catalog read.
#[tokio::test]
async fn redelivery_of_a_committed_keyed_child_reopens_without_reading_the_catalog() {
    let registry = process_registry();
    let child = SessionId::from("committed-keyed-worker-child");
    let registration = keyed_registration_for(&child).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = memory_session_store_factory().await;
    commit_keyed_child(&registry, &factory, &registration, &process_id).await;

    let models = Arc::new(BindOnlyModels {
        provider: answering_provider("redelivered child answered"),
        mints: std::sync::atomic::AtomicUsize::new(0),
    });
    let worker = worker_with_models(
        memory_engine_backend().await,
        Arc::clone(&registry),
        factory,
        Arc::clone(&models) as Arc<dyn lash_core::RuntimeModels>,
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
    .expect("the redelivery reopens the committed child from its durable row");
    assert_completed(&outcome);
    assert_eq!(
        models.mints.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the committed child's redelivery read the catalog for its key"
    );
}

/// The plugin id and config namespace of [`DefaultsFactory`].
const DEFAULTS: &str = "partial-create-defaults";

/// [`DefaultsFactory`]'s recorded namespace.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct DefaultsConfig {
    value: String,
}

/// The owner of the [`DEFAULTS`] namespace: a creator's value, else the
/// parent's recorded value, else this deployment's default.
struct DefaultsOwner {
    default: &'static str,
}

impl lash_core::ConfigOwner for DefaultsOwner {
    type Create = DefaultsConfig;
    type Recorded = DefaultsConfig;
    type Refusal = String;

    fn implementation(&self) -> &str {
        "partial-create-defaults:1"
    }

    fn create(
        &self,
        input: Option<DefaultsConfig>,
        facts: lash_core::CreationFacts<'_, DefaultsConfig>,
    ) -> Result<Option<DefaultsConfig>, String> {
        Ok(Some(input.or_else(|| facts.parent.cloned()).unwrap_or(
            DefaultsConfig {
                value: self.default.to_string(),
            },
        )))
    }

    fn validate(
        &self,
        _value: &DefaultsConfig,
        _base: Option<&DefaultsConfig>,
        _facts: &lash_core::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// A plugin that owns one recorded namespace no starter of this file
/// records, so a creation on its deployment records the deployment's
/// default.
struct DefaultsFactory {
    default: &'static str,
}

struct DefaultsPlugin;

impl lash_core::plugin::SessionPlugin for DefaultsPlugin {
    fn id(&self) -> &'static str {
        DEFAULTS
    }

    fn register(&self, _reg: &mut lash_core::plugin::PluginRegistrar) -> Result<(), PluginError> {
        Ok(())
    }
}

impl lash_core::facade_support::PluginFactory for DefaultsFactory {
    fn id(&self) -> &'static str {
        DEFAULTS
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(DefaultsOwner {
            default: self.default,
        })
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, PluginError> {
        Ok(Arc::new(DefaultsPlugin))
    }
}

/// The plugin default of the deployment that admits the child.
const ADMITTING_DEFAULT: &str = "the-admitting-deployment-default";

/// A deployment's models with every catalog read for a key counted.
struct CountingModels {
    inner: Arc<dyn lash_core::RuntimeModels>,
    mints: std::sync::atomic::AtomicUsize,
}

impl lash_core::RuntimeModels for CountingModels {
    fn snapshot(
        &self,
        key: &lash_core::ModelKey,
    ) -> Result<lash_core::RecordedModel, lash_core::ModelUnavailable> {
        self.mints.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.snapshot(key)
    }

    fn bind(
        &self,
        recorded: &lash_core::RecordedModel,
    ) -> Result<lash_core::facade_support::ProviderHandle, lash_core::ModelUnavailable> {
        self.inner.bind(recorded)
    }
}

/// What the deployment that takes a partially created child's redelivery
/// changed since the child's admission.
#[derive(Clone, Copy, Debug)]
pub(super) enum Redeployment {
    /// [`FAST`] is still served, with other metadata.
    KeyMetadataChanged,
    /// [`FAST`] is no longer minted.
    KeyRemoved,
    /// [`DefaultsFactory`] is installed with another default.
    PluginDefaultsChanged,
}

impl Redeployment {
    pub(super) const ALL: [Self; 3] = [
        Self::KeyMetadataChanged,
        Self::KeyRemoved,
        Self::PluginDefaultsChanged,
    ];
}

/// FIG-4627: a child whose creation committed its catalog admission, with
/// its complete config as the created head at revision zero, and crashed
/// before its first runtime commit completes from that recorded config. The
/// redelivery lands on a deployment that changed what the creation resolved
/// against, and the child's first committed head is the recorded creation
/// config: no catalog read, no plugin default, no refusal.
///
/// `sessions` is the session catalog of the store under test. `Err` names
/// what the redelivery did instead.
pub(super) async fn a_partially_created_child_completes_from_its_recorded_creation_config(
    sessions: Arc<dyn lash_core::DeploymentStore>,
    redeployment: Redeployment,
) -> Result<(), String> {
    let registry = process_registry();
    let child = SessionId::from(format!("partial-create-{redeployment:?}"));
    let registration = keyed_registration_for(&child).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = Arc::new(RecordingDeploymentStore::over(sessions));
    // The crash: the child's catalog admission commits, and its first
    // runtime commit, the initial head, never lands.
    assert!(
        lash_core::store::SessionCommitStore::load_session_head_meta(factory.as_ref(), &child)
            .await
            .expect("read the child's head before its creation")
            .is_none(),
        "precondition: the child is not created"
    );
    let store = factory
        .store_for(&child)
        .expect("the catalog tracks the child");
    store.fail_next_runtime_commit(lash_core::StoreError::Contended);
    let context = Arc::new(ReplayableRecordingContext::default());
    let authority = test_restate_authority_id();
    let admitting = worker_with_models(
        memory_engine_backend().await,
        Arc::clone(&registry),
        Arc::clone(&factory) as Arc<_>,
        models_serving_fast(answering_provider("never asked")),
        vec![Arc::new(DefaultsFactory {
            default: ADMITTING_DEFAULT,
        })],
    )
    .await;
    let crashed = run(
        &admitting,
        &registry,
        &process_id,
        &registration,
        Arc::clone(&context),
        authority.clone(),
        RestateNamespace::default(),
    )
    .await;
    assert!(
        crashed.is_err(),
        "the attempt ends at its failed initial head commit: {crashed:?}"
    );
    drop(admitting);
    let created =
        lash_core::store::SessionCommitStore::load_session_head_meta(factory.as_ref(), &child)
            .await
            .expect("read the created head")
            .expect("the admission recorded the created head");
    assert_eq!(
        created.head_revision, 0,
        "precondition: the child has a created head and no committed one"
    );
    assert_eq!(
        created
            .config
            .model
            .as_ref()
            .map(|model| model.model.key().as_str()),
        Some(FAST),
        "precondition: the created head records the binding its key minted"
    );
    assert_eq!(
        created.config.plugin_config.get(DEFAULTS),
        Some(&serde_json::json!({ "value": ADMITTING_DEFAULT })),
        "precondition: the created head records the admitting deployment's default"
    );
    assert!(
        store.runtime_commits().is_empty(),
        "precondition: no runtime commit landed"
    );

    let provider = answering_provider("redelivered child answered");
    let (models, default): (Arc<dyn lash_core::RuntimeModels>, _) = match redeployment {
        Redeployment::KeyMetadataChanged => (
            Arc::new(
                lash_core::ModelRegistry::new()
                    .register(
                        FAST,
                        lash_core::RegisteredModel::new(
                            lash_core::ModelMetadata::builder("fast-wire")
                                .context_window_tokens(64_000)
                                .build()
                                .expect("valid changed metadata"),
                            provider,
                        ),
                    )
                    .expect("register the changed key"),
            ),
            ADMITTING_DEFAULT,
        ),
        Redeployment::KeyRemoved => (
            Arc::new(BindOnlyModels {
                provider,
                mints: std::sync::atomic::AtomicUsize::new(0),
            }),
            ADMITTING_DEFAULT,
        ),
        Redeployment::PluginDefaultsChanged => (
            models_serving_fast(provider),
            "the-redelivery-deployment-default",
        ),
    };
    let models = Arc::new(CountingModels {
        inner: models,
        mints: std::sync::atomic::AtomicUsize::new(0),
    });
    let redelivering = worker_with_models(
        memory_engine_backend().await,
        Arc::clone(&registry),
        Arc::clone(&factory) as Arc<_>,
        Arc::clone(&models) as Arc<dyn lash_core::RuntimeModels>,
        vec![Arc::new(DefaultsFactory { default })],
    )
    .await;
    context.start_replay_allowing_journal_extension();
    let outcome = run(
        &redelivering,
        &registry,
        &process_id,
        &registration,
        context,
        authority,
        RestateNamespace::default(),
    )
    .await
    .map_err(|error| format!("{redeployment:?}: the redelivery was refused: {error:?}"))?;
    assert_completed(&outcome);
    let mints = models.mints.load(std::sync::atomic::Ordering::SeqCst);
    let mut failures = Vec::new();
    if mints != 0 {
        failures.push(format!(
            "{redeployment:?}: the redelivery read the catalog for the recorded key {mints} time(s)"
        ));
    }
    let commits = store.runtime_commits();
    let first = commits
        .first()
        .expect("the redelivery committed the child's initial head");
    assert_eq!(
        first.expected_head_revision, 0,
        "the first commit publishes over the created head"
    );
    if first.config != created.config {
        failures.push(format!(
            "{redeployment:?}: the first committed head is not the recorded creation config:\n \
             recorded: {:?}\n committed: {:?}",
            created.config, first.config
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// Every [`Redeployment`] of the partial-create law over one session
/// catalog, with every case's failure reported.
pub(super) async fn partially_created_children_complete_from_their_recorded_creation_config(
    sessions: Arc<dyn lash_core::DeploymentStore>,
) {
    let mut failures = Vec::new();
    for redeployment in Redeployment::ALL {
        if let Err(failure) = a_partially_created_child_completes_from_its_recorded_creation_config(
            Arc::clone(&sessions),
            redeployment,
        )
        .await
        {
            failures.push(failure);
        }
    }
    assert!(
        failures.is_empty(),
        "a partially created child was re-resolved against the live deployment:\n{}",
        failures.join("\n")
    );
}

/// FIG-4627 on SQLite; `postgres_ingress` runs the same law on PostgreSQL.
#[tokio::test]
async fn a_partially_created_child_completes_from_its_recorded_creation_config_on_sqlite() {
    partially_created_children_complete_from_their_recorded_creation_config(
        memory_session_store_factory().await,
    )
    .await;
}

/// The park Restate's exhausted retries of an attempt that failed with
/// `failure` become, as the reconcile writes it from the engine's text.
fn park_of_exhausted_retries(failure: &str) -> lash_core::store::ParkReason {
    lash_core::store::ParkReason::engine_retry_exhausted(
        8,
        Some("500".to_string()),
        format!("[500] Handler failed with retryable error: {failure}"),
    )
}

/// The first attempt of a committed [`FAST`] child's process on a worker
/// that does not serve the key: the registry, store and registration a
/// retry needs, the context the attempt ran on, and how the attempt ended.
struct UnservedCommittedChild {
    registry: Arc<dyn ProcessRegistry>,
    factory: Arc<dyn lash_core::DeploymentStore>,
    registration: ProcessRegistration,
    process_id: ProcessId,
    context: Arc<ReplayableRecordingContext>,
    ended: Result<Result<lash_core::ProcessRunOutcome, PluginError>, AttemptFailure>,
}

async fn unserved_committed_child(child: &str) -> UnservedCommittedChild {
    let registry = process_registry();
    let registration = keyed_registration_for(&SessionId::from(child)).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let factory = memory_session_store_factory().await;
    commit_keyed_child(&registry, &factory, &registration, &process_id).await;
    let worker = worker_for(
        memory_engine_backend().await,
        Arc::clone(&registry),
        Arc::clone(&factory),
        answering_provider("never asked"),
        Vec::new(),
    )
    .await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let ended = context
        .attempt
        .run(Box::pin(run(
            &worker,
            &registry,
            &process_id,
            &registration,
            Arc::clone(&context),
            test_restate_authority_id(),
            RestateNamespace::default(),
        )))
        .await;
    UnservedCommittedChild {
        registry,
        factory,
        registration,
        process_id,
        context,
        ended,
    }
}

/// FIG-4531: a model key this worker does not serve ends a session-turn
/// attempt with the typed, retried `ModelUnavailable`, whether the child is
/// still to be created (the key cannot be minted) or already committed (its
/// recorded binding cannot be bound). Neither is the generic plugin-session
/// failure, and neither is the process's outcome. Either way the key
/// reaches the park the engine's exhausted retries become (FIG-4631).
#[tokio::test]
async fn a_session_turn_key_this_worker_does_not_serve_retries_typed() {
    let key = lash_core::ModelKey::new(FAST);

    // The child is still to be created: its key cannot be minted here. The
    // fault is met outside any step, so the handler ends the attempt with it.
    let registry = process_registry();
    let child = SessionId::from("unserved-key-fresh-child");
    let registration = keyed_registration_for(&child).await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register SessionTurn")
        .id;
    let worker = worker_for(
        memory_engine_backend().await,
        Arc::clone(&registry),
        memory_session_store_factory().await,
        answering_provider("never asked"),
        Vec::new(),
    )
    .await;
    let context = Arc::new(ReplayableRecordingContext::default());
    let error = match context
        .attempt
        .run(Box::pin(run(
            &worker,
            &registry,
            &process_id,
            &registration,
            Arc::clone(&context),
            test_restate_authority_id(),
            RestateNamespace::default(),
        )))
        .await
        .expect("no step's fault ends the attempt of a child that cannot be created")
    {
        Err(error) => error,
        Ok(outcome) => panic!("an unserved key ends the attempt, got: {outcome:#?}"),
    };
    let (code, model_key) = match &error {
        PluginError::Runtime(runtime) => (Some(runtime.code.clone()), runtime.model_key()),
        PluginError::RuntimeEffectController(controller) => {
            (Some(controller.code.clone()), controller.model_key())
        }
        _ => (None, None),
    };
    assert_eq!(
        code,
        Some(lash_core::RuntimeErrorCode::ModelUnavailable),
        "the refusal is typed model_unavailable: {error:?}"
    );
    assert_eq!(model_key, Some(&key), "the fault names the key: {error:?}");
    assert!(
        error.is_retryable() && !error.is_terminal(),
        "a deployment that serves the key repairs it: {error:?}"
    );
    assert_eq!(
        park_of_exhausted_retries(&error.attempt_failure_text()).model_key(),
        Some(&key),
        "the park of the exhausted retries names the key"
    );
    let ended = format!("{:?}", handler_error_from_plugin(error));
    assert!(
        ended.contains("Retryable") && ended.contains("lash.model_unavailable"),
        "the handler ends the attempt retryably, with the fault's record: {ended}"
    );

    // The child is committed: its recorded binding cannot be bound here. The
    // model call's step meets the fault, and the attempt ends there.
    let committed = Box::pin(unserved_committed_child("unserved-key-committed-child")).await;
    let ended = match committed.ended {
        Err(ended) => ended,
        Ok(returned) => panic!("the unbound model call ends the attempt, got: {returned:#?}"),
    };
    assert!(
        ended.failure.starts_with("model_unavailable: "),
        "the attempt fails with the typed code's text: {ended:?}"
    );
    assert_eq!(
        park_of_exhausted_retries(&ended.failure).model_key(),
        Some(&key),
        "the park of the exhausted retries names the key: {ended:?}"
    );
}

/// FIG-4631: a model bind fault is never journaled (FIG-4404). The step
/// that met it leaves no record, so the retry of the same invocation on a
/// deployment that serves the key runs the step again and completes. A
/// journaled fault would replay as the same refusal on every attempt.
#[tokio::test]
async fn a_model_bind_fault_is_never_journaled_and_its_retry_runs_the_step_again() {
    let committed = Box::pin(unserved_committed_child("never-journaled-bind-fault-child")).await;
    let ended = match committed.ended {
        Err(ended) => ended,
        Ok(returned) => panic!("the unbound model call ends the attempt, got: {returned:#?}"),
    };
    {
        let records = committed.context.records.lock_recover();
        assert!(
            !records.contains_key(&ended.effect),
            "the step that met the fault journaled a record: {ended:?}"
        );
        for (effect, bytes) in records.iter() {
            let record = String::from_utf8_lossy(bytes);
            assert!(
                !record.contains("model_unavailable"),
                "step `{effect}` journaled the bind fault: {record}"
            );
        }
    }
    assert!(
        committed
            .context
            .runs
            .lock_recover()
            .contains(&ended.effect),
        "the step that met the fault was issued: {ended:?}"
    );

    // The engine's retry: the journal replays, and the step runs again on a
    // deployment that binds the recorded model.
    committed.context.start_replay_allowing_journal_extension();
    let worker = worker_with_models(
        memory_engine_backend().await,
        Arc::clone(&committed.registry),
        Arc::clone(&committed.factory),
        Arc::new(BindOnlyModels {
            provider: answering_provider("the retry answered"),
            mints: std::sync::atomic::AtomicUsize::new(0),
        }) as Arc<dyn lash_core::RuntimeModels>,
        Vec::new(),
    )
    .await;
    let outcome = committed
        .context
        .attempt
        .run(Box::pin(run(
            &worker,
            &committed.registry,
            &committed.process_id,
            &committed.registration,
            Arc::clone(&committed.context),
            test_restate_authority_id(),
            RestateNamespace::default(),
        )))
        .await
        .expect("no step's fault ends the retry")
        .expect("the retry runs the model call on a deployment that serves the key");
    assert_completed(&outcome);
    assert!(
        committed
            .context
            .records
            .lock_recover()
            .contains_key(&ended.effect),
        "the retry journaled the step the fault had left unrecorded"
    );
}

/// FIG-4631: a recording context ends an attempt the way the engine does.
/// A step whose fault is retried journals nothing, nothing after it runs,
/// and its text is all that is kept; a value is journaled.
#[tokio::test]
async fn a_recording_context_ends_the_attempt_at_a_retried_fault_and_journals_nothing() {
    let fault = lash_core::RuntimeEffectControllerError::model_unavailable(
        &lash_core::ModelKey::new(FAST),
        "the recorded model cannot be bound on this worker",
    )
    .attempt_failure_text();
    let context = Arc::new(ReplayableRecordingContext::default());
    let ran_past_the_fault = AtomicBool::new(false);
    let ended = context
        .attempt
        .run(async {
            let Json(value) = context
                .run_json_or_retry_send("settled".to_string(), async { Ok::<u32, String>(7) })
                .await
                .expect("a value is journaled");
            assert_eq!(value, 7);
            let _ = context
                .run_json_or_retry_send("faulted".to_string(), {
                    let fault = fault.clone();
                    async move { Err::<u32, String>(fault) }
                })
                .await;
            ran_past_the_fault.store(true, Ordering::SeqCst);
        })
        .await
        .expect_err("the fault ends the attempt");
    assert_eq!(
        ended,
        AttemptFailure {
            effect: "faulted".to_string(),
            failure: fault,
        }
    );
    assert!(
        !ran_past_the_fault.load(Ordering::SeqCst),
        "nothing runs after the step that ended the attempt"
    );
    let records = context.records.lock_recover();
    assert_eq!(
        records.get("settled").map(Vec::as_slice),
        Some(br#"{"Ok":7}"#.as_slice()),
        "a settled step journals its value"
    );
    assert!(
        !records.contains_key("faulted"),
        "the faulted step journaled nothing"
    );
}

#[tokio::test]
async fn child_turn_panic_is_typed_and_the_parent_remains_alive() {
    let registry = process_registry();
    let child = SessionId::from("panicking-worker-child");
    let registration = registration_for(&child).await;
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
        lash_core::plugin::PluginSpec::new().with_before_turn(Arc::new(move |_| {
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
    let mut registration = registration_for(&child).await;
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
    let owned_store = lash_core::runtime::live_session_view(&factory, &child)
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
    .keyed_in(&scoped)
    .expect("a keyless host start is keyed in its scope");
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
