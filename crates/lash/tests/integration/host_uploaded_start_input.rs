//! Host uploads must survive the gap between registration and input acquisition.

use super::{BOUND, Engine, Storage, StorageKind};
use std::sync::{Arc, Mutex};

use lash_core::ArtifactReferrer;
use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupPorts, ArtifactCleanupRelay, StoreSetAuthorities,
};
use lash_core::runtime::drive::relay::{RelayVerdict, deliver_now};
use lash_core::testing::runtime_helpers::{LayeredBackend, LayeredStores};
use lash_core::testing::{ProcessRegistryFaults, RegistrationHoldPoint, TestClock};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{HandlerAttempt, ServerConfig};

async fn engine(
    storage: &Storage,
    kind: StorageKind,
    live: bool,
    clock: Arc<TestClock>,
    faults: &mut Option<Arc<ProcessRegistryFaults>>,
) -> Engine {
    let mut uri = None;
    let stores = storage.stores(kind, clock, &mut uri).await;
    let registry = Arc::new(ProcessRegistryFaults::new(stores.process_registry()));
    *faults = Some(Arc::clone(&registry));
    let stores = LayeredStores::over(stores)
        .map_process_registry(|_| registry)
        .into_store_set();
    if live {
        Engine::Live(
            LiveRestateBackend::start_with_store_set(
                LiveConfig {
                    ingress_url: std::env::var("RESTATE_INGRESS_URL").unwrap(),
                    admin_url: std::env::var("RESTATE_ADMIN_URL").unwrap(),
                    endpoint_bind: std::env::var("PC_BIND").unwrap().parse().unwrap(),
                    endpoint_url: std::env::var("PC_URL").unwrap(),
                    run_tag: format!(
                        "uploaded-start-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_nanos()
                    ),
                    namespace: lash_restate::RestateNamespace::default(),
                },
                |_| async { Ok(stores) },
            )
            .await
            .unwrap(),
        )
    } else {
        Engine::Double(
            lash_restate_test::backend_with_store_set(
                4288,
                ServerConfig::default(),
                lash_restate_test::DeploymentHooks::default(),
                |_| async { Ok(stores) },
            )
            .await
            .unwrap(),
        )
    }
}

fn core(engine: &Engine) -> lash::LashCore {
    let provider = lash_core::testing::TestProvider::builder()
        .kind("uploaded-start")
        .complete(|_| async {
            Ok::<_, lash_core::llm::transport::LlmTransportError>(Default::default())
        })
        .build()
        .into_handle();
    let session_work = match engine {
        Engine::Double(b) => b.explicit_reconcile_session_work(),
        Engine::Live(b) => b.explicit_reconcile_session_work(),
    };
    let backend = LayeredBackend::over(engine.backend())
        .with_session_work(session_work)
        .into_backend();
    lash::LashCore::standard_builder(
        backend,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    )
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .models(Arc::new(
        lash::ModelRegistry::new()
            .register(
                "mock-model",
                lash::RegisteredModel::new(mock_model_metadata(), provider),
            )
            .unwrap(),
    ))
    .model("mock-model")
    .build(lash_core::LeaseOwnerIdentity::opaque(
        "uploaded-start",
        "boot",
    ))
    .unwrap()
}

fn cleanup(backend: &lash_core::Backend) -> ArtifactCleanupRelay {
    ArtifactCleanupRelay::new(ArtifactCleanupPorts {
        ledger: backend.artifact_cleanup(),
        authorities: Arc::new(StoreSetAuthorities {
            effect_host: backend.effect_host(),
            sessions: backend.session_store_factory(),
            processes: backend.process_registry(),
            triggers: backend.trigger_store(),
        }),
        process_env: backend.process_env_store(),
        modules: backend.module_artifacts(),
        definitions: backend.definition_store(),
        engines: lash_core::ProcessEngineRegistry::new(),
        attachments: backend.attachment_referrers(),
        clock: backend.clock(),
    })
}

async fn end_guard(backend: &lash_core::Backend, claim: lash_core::ReferrerClaim) {
    let id = backend
        .artifact_cleanup()
        .arm_cleanup(
            &claim.guard_cleanup().unwrap(),
            backend.clock().timestamp_ms(),
        )
        .await
        .unwrap();
    backend
        .artifact_cleanup()
        .nudge(claim.referrer(), backend.clock().timestamp_ms())
        .await
        .unwrap();
    let verdict = deliver_now(&cleanup(backend), &id, backend.clock().as_ref())
        .await
        .unwrap();
    assert!(
        matches!(verdict, RelayVerdict::Delivered | RelayVerdict::NotDue),
        "guard delivery: {verdict:?}"
    );
    tokio::time::timeout(BOUND, async {
        while backend
            .artifact_cleanup()
            .load_cleanup(&id)
            .await
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the cleanup obligation settled");
}

async fn sweep(backend: &lash_core::Backend) {
    lash::persistence::reclaim_unreferenced_attachments(
        backend.session_store_factory().as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: lash_core::EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap();
}

fn mock_model_metadata() -> lash::ModelMetadata {
    lash::ModelMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .unwrap()
}

fn request(input: lash_core::AttachmentRef) -> lash_core::ProcessStartRequest {
    let model = Some(lash::ModelConfig::new(lash::RecordedModel::mint(
        lash::ModelKey::new("mock-model"),
        mock_model_metadata(),
    )));
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "uploaded-start-input".into(),
            create_request: Box::new(lash_core::SessionCreateRequest::child(
                "upload-session",
                lash_core::SessionStartPoint::Empty,
                lash_core::SessionPolicy {
                    model,
                    ..lash_core::SessionPolicy::new(
                        lash_core::TurnBudget::Unbounded,
                        lash_core::MaxToolCalls::new(1024),
                    )
                },
                lash_core::PluginOptions::default(),
            )),
            turn_input: Box::new(
                lash::TurnInput::text("read the host upload")
                    .with_attachment(lash_core::AttachmentSource::stored(input)),
            ),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_host_start_key("uploaded-start")
}

async fn law(kind: StorageKind, live: bool, abandon: bool) {
    let storage = Storage::new(kind).await;
    let clock = Arc::new(TestClock::new(10_000));
    let mut faults = None;
    let first = engine(&storage, kind, live, Arc::clone(&clock), &mut faults).await;
    let core = core(&first);
    core.session("upload-session")
        .create(lash::SessionCreation::default())
        .await
        .unwrap();
    let backend = first.backend();
    let uploads = lash_core::facade_support::RuntimeAttachmentStore::new_with_clock(
        backend.attachment_store(),
        backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session("upload-session".into()),
        clock.clone(),
    )
    .with_upload_expiry_ms(1000);
    let bytes = b"unbound host upload\0\xff exact bytes".to_vec();
    let input = uploads
        .put(
            bytes.clone(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("application/octet-stream").unwrap(),
                None,
                None,
            ),
        )
        .await
        .unwrap();
    let refs = backend
        .attachment_referrers()
        .attachment_referrers(&input.id)
        .await
        .unwrap();
    assert_eq!(refs.len(), 1, "the host upload is unbound");
    let upload = refs[0].clone();
    assert!(matches!(upload, ArtifactReferrer::Upload(_)));
    let reached = Arc::new(tokio::sync::Notify::new());
    faults
        .unwrap()
        .hold_next_registration(RegistrationHoldPoint::AfterRegistering, {
            let reached = Arc::clone(&reached);
            Arc::new(move || reached.notify_one())
        });
    let start_request = request(input.clone());
    let start_result = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let request = start_request.clone();
        let start_result = Arc::clone(&start_result);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let start_result = Arc::clone(&start_result);
            Box::pin(async move {
                *start_result.lock().unwrap() = Some(core.processes().start(request, scoped).await);
            })
        })
    };
    let mut task = {
        let engine = first.clone();
        tokio::spawn(async move {
            engine
                .run(
                    lash_core::AdmittedScope::runtime_operation("uploaded-start-crash"),
                    attempt,
                )
                .await
        })
    };
    let _abort = super::AbortOnDrop(task.abort_handle());
    tokio::time::timeout(BOUND, async {
        tokio::select! {
            _ = reached.notified() => {},
            task_result = &mut task => panic!("host start ended before registration: {task_result:?}; {:?}", start_result.lock().unwrap().take()),
        }
    }).await.expect("host start reaches the registration crash point");
    let registry = backend.process_registry();
    let records = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let process_id = records[0].id.clone();
    let record = registry.get_process(&process_id).await.unwrap().unwrap();
    assert_eq!(record.input.stored_attachment_ids(), vec![input.id.clone()]);
    assert!(
        record.external_ref.is_none(),
        "the child has never been submitted"
    );
    assert!(
        !backend
            .attachment_referrers()
            .attachment_referrers(&input.id)
            .await
            .unwrap()
            .contains(&ArtifactReferrer::ProcessRecord(process_id.clone())),
        "crash before input acquisition"
    );

    // Kill the registering attempt. It cannot finish acquiring the input.
    match &first {
        Engine::Double(b) => {
            let invocation = b
                .server()
                .invocations()
                .into_iter()
                .find(|v| v.target.starts_with("LashTestHandlerHost/") && v.status == "running")
                .unwrap();
            assert_eq!(b.server().kill_and_await(&invocation.id).await, Some(true));
        }
        Engine::Live(b) => {
            b.stop_serving(true);
            let invocation = b
                .invocations()
                .await
                .unwrap()
                .into_iter()
                .find(|v| v.target.starts_with("LashTestHandlerHost/") && v.status != "completed")
                .unwrap();
            assert!(b.kill_and_await(&invocation.id).await.unwrap());
        }
    }
    assert!(
        tokio::time::timeout(BOUND, task)
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "the registering invocation was killed"
    );

    // Recovery is withheld. Only the independent upload expiry and sweep run.
    clock.advance(1001);
    end_guard(
        &backend,
        lash_core::ReferrerClaim::guarded(
            upload.clone(),
            lash_core::ArtifactCleanupPlan::AwaitUploadExpiry {
                expires_at_ms: 11_000,
            },
        )
        .unwrap(),
    )
    .await;
    assert!(
        !backend
            .attachment_referrers()
            .attachment_referrers(&input.id)
            .await
            .unwrap()
            .contains(&upload),
        "the upload expires independently of the accepted start"
    );
    sweep(&backend).await;
    assert_eq!(
        backend
            .attachment_store()
            .get(&input.id, bytes.len() as u64)
            .await
            .expect(
                "registered start must retain its host-uploaded input across a registration crash"
            )
            .bytes,
        bytes
    );

    let staged = backend
        .attachment_referrers()
        .attachment_referrers(&input.id)
        .await
        .unwrap();
    assert_eq!(staged.len(), 1, "only input staging remains");
    let staging = staged[0].clone();
    assert!(staging.kind().is_guarded());
    if let Engine::Live(engine) = &first {
        engine.stop_serving(true);
    }
    drop(uploads);
    drop(core);
    drop(registry);
    drop(backend);
    drop(first);

    // Reopen file-backed storage and construct a new engine, core and worker.
    let cold = engine(&storage, kind, live, Arc::clone(&clock), &mut None).await;
    let core = self::core(&cold);
    let backend = cold.backend();
    // Input staging's cleanup finishes the record acquisition even when the caller left.
    end_guard(
        &backend,
        lash_core::ReferrerClaim::guarded(
            staging.clone(),
            lash_core::ArtifactCleanupPlan::AwaitStart {
                starter: lash_core::AdmittedScope::runtime_operation("uploaded-start-crash")
                    .scope()
                    .journal_identity()
                    .unwrap(),
            },
        )
        .unwrap(),
    )
    .await;
    let refs = backend
        .attachment_referrers()
        .attachment_referrers(&input.id)
        .await
        .unwrap();
    assert!(
        refs.contains(&ArtifactReferrer::ProcessRecord(process_id.clone())),
        "recovery acquires before staging ends: {refs:?}"
    );
    assert!(!refs.contains(&staging), "recovery ends staging");
    sweep(&backend).await;
    assert_eq!(
        backend
            .attachment_store()
            .get(&input.id, bytes.len() as u64)
            .await
            .unwrap()
            .bytes,
        bytes
    );
    let worker =
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap();
    match &cold {
        Engine::Double(b) => b.install_process_worker(worker),
        Engine::Live(b) => b.install_process_worker(worker),
    }
    if !abandon {
        let receipt = Arc::new(Mutex::new(None));
        let attempt: HandlerAttempt = {
            let core = core.clone();
            let request = start_request;
            let receipt = Arc::clone(&receipt);
            Arc::new(move |scoped| {
                let core = core.clone();
                let request = request.clone();
                let receipt = Arc::clone(&receipt);
                Box::pin(async move {
                    *receipt.lock().unwrap() = Some(core.processes().start(request, scoped).await);
                })
            })
        };
        cold.run(
            lash_core::AdmittedScope::runtime_operation("uploaded-start-retry"),
            attempt,
        )
        .await
        .unwrap();
        assert_eq!(
            receipt.lock().unwrap().take().unwrap().unwrap().process_id,
            process_id
        );
    }
    let wiring = backend.process_work();
    let relay = lash_core::runtime::process_start::ProcessStartRelay::new(
        backend.obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
        backend.process_registry(),
        Arc::clone(wiring.port()),
        backend.clock(),
    );
    let verdict = relay.deliver_start(&process_id).await.unwrap();
    assert!(
        matches!(verdict, RelayVerdict::Delivered | RelayVerdict::NotDue),
        "the armed start is delivered: {verdict:?}"
    );
    tokio::time::timeout(BOUND, async {
        loop {
            if backend
                .process_registry()
                .get_process(&process_id)
                .await
                .unwrap()
                .unwrap()
                .is_terminal()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        backend
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        backend
            .attachment_store()
            .get(&input.id, bytes.len() as u64)
            .await
            .unwrap()
            .bytes,
        bytes
    );

    // The host key is reusable after prune; the old staging fence stays ended.
    let successor_uploads = lash_core::facade_support::RuntimeAttachmentStore::new_with_clock(
        backend.attachment_store(),
        backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session("upload-session".into()),
        clock.clone(),
    );
    let successor_input = successor_uploads
        .put(
            bytes.clone(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("application/octet-stream").unwrap(),
                None,
                None,
            ),
        )
        .await
        .unwrap();
    let pruned = backend
        .process_registry()
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .unwrap();
    assert_eq!(pruned.pruned_processes, 1);
    backend
        .attachment_referrers()
        .end_attachment_referrer(&ArtifactReferrer::ProcessRecord(process_id.clone()))
        .await
        .unwrap();
    let successor = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let request = request(successor_input.clone());
        let successor = Arc::clone(&successor);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let successor = Arc::clone(&successor);
            Box::pin(async move {
                *successor.lock().unwrap() = Some(core.processes().start(request, scoped).await);
            })
        })
    };
    cold.run(
        lash_core::AdmittedScope::runtime_operation("uploaded-start-successor"),
        attempt,
    )
    .await
    .unwrap();
    let successor = successor
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .expect("a reused host key must stage fresh uploaded input");
    assert_ne!(successor.process_id, process_id);
    tokio::time::timeout(BOUND, async {
        while !backend
            .process_registry()
            .get_process(&successor.process_id)
            .await
            .unwrap()
            .unwrap()
            .is_terminal()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        backend
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        backend
            .attachment_store()
            .get(&successor_input.id, bytes.len() as u64)
            .await
            .unwrap()
            .bytes,
        bytes
    );
}

async fn unavailable_input_is_refused(kind: StorageKind, live: bool) {
    let storage = Storage::new(kind).await;
    let clock = Arc::new(TestClock::new(10_000));
    let engine = engine(&storage, kind, live, clock.clone(), &mut None).await;
    let core = core(&engine);
    core.session("upload-session")
        .create(lash::SessionCreation::default())
        .await
        .unwrap();
    let backend = engine.backend();
    let uploads = lash_core::facade_support::RuntimeAttachmentStore::new_with_clock(
        backend.attachment_store(),
        backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session("upload-session".into()),
        clock.clone(),
    )
    .with_upload_expiry_ms(1000);
    let input = uploads
        .put(
            b"expired upload".to_vec(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("text/plain").unwrap(),
                None,
                None,
            ),
        )
        .await
        .unwrap();
    let upload = backend
        .attachment_referrers()
        .attachment_referrers(&input.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    clock.advance(1001);
    end_guard(
        &backend,
        lash_core::ReferrerClaim::guarded(
            upload,
            lash_core::ArtifactCleanupPlan::AwaitUploadExpiry {
                expires_at_ms: 11_000,
            },
        )
        .unwrap(),
    )
    .await;
    sweep(&backend).await;
    assert!(
        backend
            .attachment_store()
            .get(&input.id, b"expired upload".len() as u64)
            .await
            .is_err()
    );
    let result = Arc::new(Mutex::new(None));
    let attempt: HandlerAttempt = {
        let core = core.clone();
        let request = request(input);
        let result = Arc::clone(&result);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let result = Arc::clone(&result);
            Box::pin(async move {
                *result.lock().unwrap() = Some(core.processes().start(request, scoped).await);
            })
        })
    };
    engine
        .run(
            lash_core::AdmittedScope::runtime_operation("unavailable-host-input"),
            attempt,
        )
        .await
        .unwrap();
    assert!(
        result.lock().unwrap().take().unwrap().is_err(),
        "the missing upload is refused"
    );
    assert!(
        backend
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await
            .unwrap()
            .is_empty(),
        "unavailable input must be refused before publishing a process registration"
    );
}

macro_rules! laws {
    ($module:ident, $kind:ident, $live:expr $(, $ignore:literal)?) => {
        mod $module {
            use super::*;
            #[tokio::test]
            $(#[ignore = $ignore])?
            async fn host_uploaded_start_input_survives_registration_crash() { law(StorageKind::$kind, $live, false).await; }
            #[tokio::test]
            $(#[ignore = $ignore])?
            async fn host_uploaded_start_input_survives_starter_abandonment() { law(StorageKind::$kind, $live, true).await; }
            #[tokio::test]
            $(#[ignore = $ignore])?
            async fn unavailable_host_uploaded_start_input_is_refused_before_registration() { unavailable_input_is_refused(StorageKind::$kind, $live).await; }
        }
    };
}
laws!(double_sqlite_file, File, false);
laws!(double_postgres, Postgres, false, "requires PostgreSQL");
laws!(live_sqlite_file, File, true, "requires live Restate");
laws!(
    live_postgres,
    Postgres,
    true,
    "requires PostgreSQL and live Restate"
);
