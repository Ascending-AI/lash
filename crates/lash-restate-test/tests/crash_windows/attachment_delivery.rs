//! Acquisition failures between a committed process row and a journaled delivery.

use super::*;
use lash_core::{ArtifactReferrer, ReferrerClaim};
use lash_core::{AttachmentId, AttachmentReferrers, ProcessRegistry, StoreError, StoreSet};
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};
use lash_restate::{Call, RestateDurableWaitAddress, RestateProcessAttachRequest};

#[derive(Clone, Copy, Debug)]
enum Storage {
    Memory,
    File,
    Postgres,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Contended,
    StorageFailure,
    Incompatible,
    UnknownAttachment,
}

impl Fault {
    fn error(self, digest: &AttachmentId) -> StoreError {
        match self {
            Self::Contended => StoreError::Contended,
            Self::StorageFailure => StoreError::StorageFailure {
                backend: "acquisition-law",
                message: "one acquisition fault".into(),
            },
            Self::Incompatible => StoreError::Incompatible {
                refusal: lash_core::compat::CompatRefusal::Unstamped {
                    component: "attachment-law".into(),
                    writing_release: None,
                },
            },
            Self::UnknownAttachment => StoreError::UnknownAttachment {
                digest: digest.clone(),
            },
        }
    }
}

#[derive(Default)]
struct AcquisitionTrace {
    start: bool,
    fault: Mutex<Option<(ArtifactReferrer, Fault)>>,
    attempts: Mutex<Vec<ArtifactReferrer>>,
    registered: Mutex<Vec<ProcessId>>,
}

struct FaultingAttachments {
    inner: Arc<dyn AttachmentReferrers>,
    registry: Arc<dyn ProcessRegistry>,
    trace: Arc<AcquisitionTrace>,
}

#[async_trait::async_trait]
impl AttachmentReferrers for FaultingAttachments {
    async fn begin_attachment_write(
        &self,
        write: &lash_core::AttachmentWrite,
    ) -> Result<lash_core::AttachmentWriteFence, StoreError> {
        self.inner.begin_attachment_write(write).await
    }
    async fn complete_attachment_write(
        &self,
        write: &lash_core::AttachmentWrite,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.inner.complete_attachment_write(write, permit).await
    }
    async fn abort_attachment_write(
        &self,
        write: &lash_core::AttachmentWrite,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.inner.abort_attachment_write(write, permit).await
    }
    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        if ids.is_empty() {
            return self.inner.acquire_attachment_refs(claim, ids).await;
        }
        // For starts the id is minted by registration. A fixture marker
        // selects the first process record, after its row has committed.
        let fault = {
            let mut selected = self.trace.fault.lock().unwrap();
            let matches = selected.as_ref().is_some_and(|(target, _)| {
                target == claim.referrer()
                    || matches!(target, ArtifactReferrer::ProcessRecord(id) if id == ProcessId::fixture("start-input"))
                        && matches!(claim.referrer(), ArtifactReferrer::ProcessRecord(_))
            });
            matches.then(|| selected.take().unwrap().1)
        };
        self.trace
            .attempts
            .lock()
            .unwrap()
            .push(claim.referrer().clone());
        if self.trace.start
            && let ArtifactReferrer::ProcessRecord(id) = claim.referrer()
        {
            assert!(
                self.registry.get_process(id).await.unwrap().is_some(),
                "acquisition runs after the row committed"
            );
            self.trace.registered.lock().unwrap().push(id.clone());
        }
        match fault {
            Some(fault) => Err(fault.error(&ids[0])),
            None => self.inner.acquire_attachment_refs(claim, ids).await,
        }
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner.forget_attachment_ref(referrer, id).await
    }
    async fn end_attachment_referrer(&self, referrer: &ArtifactReferrer) -> Result<(), StoreError> {
        self.inner.end_attachment_referrer(referrer).await
    }
    async fn session_referrer_state(
        &self,
        id: &lash_core::SessionId,
    ) -> Result<lash_core::SessionReferrerState, StoreError> {
        self.inner.session_referrer_state(id).await
    }
    async fn attachment_referrers(
        &self,
        id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, StoreError> {
        self.inner.attachment_referrers(id).await
    }
}

struct Stores {
    stores: Arc<dyn StoreSet>,
    _directory: tempfile::TempDir,
    _database: Option<IsolatedDatabase>,
}

impl Stores {
    async fn new(
        storage: Storage,
        clock: Arc<dyn lash_core::Clock>,
        trace: Arc<AcquisitionTrace>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut database = None;
        let stores: Arc<dyn StoreSet> = match storage {
            Storage::Memory => Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                    .await
                    .unwrap(),
            ),
            Storage::File => Arc::new(
                lash_sqlite_store::SqliteStoreSet::open_with_clock(directory.path(), clock)
                    .await
                    .unwrap(),
            ),
            Storage::Postgres => {
                let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                    .expect("PostgreSQL laws require a provisioned database");
                let isolated = IsolatedDatabase::create(&url).await;
                let postgres = PostgresStorage::connect(isolated.url()).await.unwrap();
                let stores = PostgresStoreSet::with_clock(
                    &postgres,
                    Arc::new(lash::persistence::FileAttachmentStore::new(
                        directory.path(),
                    )),
                    Default::default(),
                    clock,
                );
                database = Some(isolated);
                Arc::new(stores)
            }
        };
        let registry = stores.process_registry();
        let stores = lash_core::testing::runtime_helpers::LayeredStores::over(stores)
            .map_attachment_referrers(|inner| {
                Arc::new(FaultingAttachments {
                    inner,
                    registry,
                    trace,
                })
            })
            .into_store_set();
        Self {
            stores,
            _directory: directory,
            _database: database,
        }
    }
}

enum Harness {
    Double(RestateTestBackend<dyn StoreSet>),
    Live {
        backend: LiveRestateBackend<dyn StoreSet>,
    },
}

impl Harness {
    async fn new(storage: Storage, live: bool, trace: Arc<AcquisitionTrace>) -> (Self, Stores) {
        let mut retained = None;
        if live {
            let backend = LiveRestateBackend::start_with_store_set(
                LiveConfig {
                    ingress_url: live_env("RESTATE_INGRESS_URL"),
                    admin_url: live_env("RESTATE_ADMIN_URL"),
                    endpoint_bind: live_env("CW_BIND").parse().unwrap(),
                    endpoint_url: live_env("CW_URL"),
                    run_tag: run_tag("attachment-delivery"),
                    namespace: RestateNamespace::default(),
                },
                |clock| async {
                    let stores = Stores::new(storage, clock, trace).await;
                    let ports = Arc::clone(&stores.stores);
                    retained = Some(stores);
                    Ok(ports)
                },
            )
            .await
            .unwrap();
            (Self::Live { backend }, retained.unwrap())
        } else {
            let mut config = ServerConfig::default().always_replay(
                std::env::var("LASH_RESTATE_TEST_ALWAYS_REPLAY").as_deref() == Ok("1"),
            );
            config.retry.max_attempts = Some(4);
            let backend = lash_restate_test::backend_with_store_set(
                0x4311,
                config,
                Default::default(),
                |clock| async {
                    let stores = Stores::new(storage, clock, trace).await;
                    let ports = Arc::clone(&stores.stores);
                    retained = Some(stores);
                    Ok(ports)
                },
            )
            .await
            .unwrap();
            (Self::Double(backend), retained.unwrap())
        }
    }

    fn backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(backend) => backend.lash_backend(),
            Self::Live { backend, .. } => backend.lash_backend(),
        }
    }
    fn ingress(&self) -> lash_restate::RestateIngressClient {
        match self {
            Self::Double(backend) => backend.ingress(),
            Self::Live { backend, .. } => backend.ingress(),
        }
    }
    async fn run(&self, scope: lash_core::AdmittedScope, attempt: HandlerAttempt) {
        tokio::time::timeout(BOUND, async {
            match self {
                Self::Double(backend) => backend.run_in_handler(scope, attempt).await,
                Self::Live { backend, .. } => backend.run_in_handler(scope, attempt).await,
            }
        })
        .await
        .expect("handler finishes without a retry loop")
        .unwrap();
    }
    async fn attach_armed(&self, producer: &ProcessId) {
        tokio::time::timeout(BOUND, async {
            loop {
                let armed = match self {
                    Self::Double(backend) => backend.server().invocations().iter().any(|v| {
                        v.target.contains(producer.as_str())
                            && v.target.ends_with("/await_terminal")
                    }),
                    Self::Live { backend } => {
                        backend.invocations().await.unwrap().iter().any(|v| {
                            v.target.contains(producer.as_str())
                                && v.target.ends_with("/await_terminal")
                        })
                    }
                };
                if armed {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the attach is awaiting the producer before receiver retirement");
    }
    async fn await_attach(&self, invocation: &str) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match self {
                    Self::Double(backend) => {
                        if let Some(outcome) = backend.server().outcome(invocation) {
                            return outcome.map(|_| ()).map_err(|error| format!("{error:?}"));
                        }
                        if let Some(view) = backend
                            .server()
                            .invocations()
                            .into_iter()
                            .find(|v| v.id == invocation && v.status == "paused")
                        {
                            return Err(format!(
                                "attach paused after {} retries: {:?}",
                                view.retry_count, view.last_failure
                            ));
                        }
                    }
                    Self::Live { backend } => {
                        if let Some(outcome) = backend.outcome(invocation).await.unwrap() {
                            return outcome;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| "ended receiver attach never settled".to_owned())?
    }
    async fn assert_run(
        &self,
        invocation: &str,
        name: &str,
        expected: Option<&lash_core::ProcessAwaitOutput>,
    ) {
        match self {
            Self::Double(backend) => {
                let journal = backend.server().journal(invocation).unwrap();
                assert_eq!(
                    journal
                        .iter()
                        .filter(|entry| entry.ty == MessageType::RunCommand
                            && entry.name.as_deref().is_some_and(|n| n.contains(name)))
                        .count(),
                    1
                );
                if name == "process-attach-acquire" {
                    use lash_core::runtime::attachment_delivery::DeliveryAcquisition;
                    let results = journal
                        .iter()
                        .filter_map(|entry| entry.run_completion())
                        .filter_map(Result::ok)
                        .filter_map(|value| {
                            serde_json::from_slice::<DeliveryAcquisition>(&value).ok()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(results.len(), 1, "one completed acquisition verdict");
                    match results.as_slice() {
                        [DeliveryAcquisition::Held] => assert!(expected.is_some()),
                        [DeliveryAcquisition::ReceiverEnded { .. }] => assert!(expected.is_none()),
                        verdict => panic!("unexpected delivery acquisition: {verdict:?}"),
                    }
                } else {
                    let results = journal
                        .iter()
                        .filter_map(|entry| entry.run_completion())
                        .filter_map(Result::ok)
                        .filter_map(|value| {
                            serde_json::from_slice::<
                                Result<serde_json::Value, lash_core::RuntimeEffectControllerError>,
                            >(&value)
                            .ok()
                        })
                        .collect::<Vec<_>>();
                    assert!(!results.is_empty(), "the start records its result");
                    assert!(
                        results.iter().all(Result::is_ok),
                        "no terminal start refusal is journaled"
                    );
                }
            }
            Self::Live { backend, .. } => {
                let journal = backend.journal(invocation).await.unwrap();
                assert_eq!(
                    journal.iter().filter(|entry| entry.contains(name)).count(),
                    1,
                    "one live journaled acquisition step: {journal:?}"
                );
            }
        }
    }
    async fn handler_id(&self) -> String {
        match self {
            Self::Double(backend) => {
                backend
                    .server()
                    .invocations()
                    .into_iter()
                    .find(|v| v.target.starts_with("LashTestHandlerHost/"))
                    .unwrap()
                    .id
            }
            Self::Live { backend, .. } => {
                backend
                    .invocations()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|v| v.target.starts_with("LashTestHandlerHost/"))
                    .unwrap()
                    .id
            }
        }
    }
    async fn finish(self) {
        if let Self::Live { backend } = self {
            backend.finish().await;
            backend.stop_serving(true);
        }
    }
}

fn core(harness: &Harness) -> lash::LashCore {
    let session_work = match harness {
        Harness::Double(backend) => backend.explicit_reconcile_session_work(),
        Harness::Live { backend } => backend.explicit_reconcile_session_work(),
    };
    // The child turn runs through its process. A wall-clock reconciliation
    // must not race that turn for the session's drive fence.
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(harness.backend())
        .with_session_work(session_work)
        .into_backend();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("attachment-delivery")
        .complete(|_| async {
            Ok::<_, LlmTransportError>(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "child settled".into(),
                    response_meta: None,
                }],
                terminal_reason: lash_core::LlmTerminalReason::Stop,
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_model(
            provider,
            lash::ModelMetadata::builder("attachment-delivery")
                .context_window_tokens(100_000)
                .build()
                .unwrap(),
        )
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "attachment-delivery",
            "test",
        ))
        .unwrap();
    let worker =
        lash_core_worker::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap();
    match harness {
        Harness::Double(backend) => backend.install_process_worker(worker),
        Harness::Live { backend, .. } => backend.install_process_worker(worker),
    }
    core
}

async fn put(core: &lash::LashCore, harness: &Harness) -> lash_core::AttachmentRef {
    core.session("attachment-upload")
        .create(lash::SessionCreation::default())
        .await
        .unwrap();
    lash_core::facade_support::RuntimeAttachmentStore::new_with_clock(
        harness.backend().attachment_store(),
        harness.backend().attachment_referrers(),
        lash_core::RuntimeOwner::Session("attachment-upload".into()),
        harness.backend().clock(),
    )
    .put(
        b"delivery bytes".to_vec(),
        lash_core::AttachmentCreateMeta::new(
            lash_core::MediaType::parse("text/plain").unwrap(),
            None,
            None,
        ),
    )
    .await
    .unwrap()
}

async fn start_law(storage: Storage, live: bool) {
    let mut failures = Vec::new();
    for fault in [
        Fault::Contended,
        Fault::StorageFailure,
        Fault::Incompatible,
        Fault::UnknownAttachment,
    ] {
        let trace = Arc::new(AcquisitionTrace {
            start: true,
            ..Default::default()
        });
        let (harness, _stores) = Harness::new(storage, live, Arc::clone(&trace)).await;
        let core = core(&harness);
        let attachment = put(&core, &harness).await;
        let mut create_request = lash_core::SessionCreateRequest::root(
            lash_core::SessionStartPoint::Empty,
            Default::default(),
        );
        create_request.session_id = None;
        let start_key_bytes = run_tag("start-input");
        let start_key = lash_core::StartKey::for_host(&start_key_bytes);
        let request = lash_core::ProcessStartRequest::new(
            lash_core::ProcessInput::SessionTurn {
                definition_key: "attachment-delivery-session-turn".into(),
                create_request: Box::new(create_request),
                turn_input: Box::new(
                    lash::TurnInput::text("read")
                        .with_attachment(lash_core::AttachmentSource::stored(attachment.clone())),
                ),
                result: lash_core::SessionTurnOutcome::Turn,
            },
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_host_start_key(&start_key_bytes);
        *trace.fault.lock().unwrap() = Some((
            ArtifactReferrer::ProcessRecord(ProcessId::fixture("start-input")),
            fault,
        ));
        let results = Arc::new(Mutex::new(Vec::new()));
        let attempt: HandlerAttempt = {
            let core = core.clone();
            let results = Arc::clone(&results);
            Arc::new(move |scoped| {
                let core = core.clone();
                let request = request.clone();
                let results = Arc::clone(&results);
                Box::pin(async move {
                    let result = core.processes().start(request, scoped).await;
                    results.lock().unwrap().push(result);
                })
            })
        };
        let transient = matches!(fault, Fault::Contended | Fault::StorageFailure);
        harness
            .run(
                lash_core::AdmittedScope::runtime_operation(run_tag("starter")),
                attempt,
            )
            .await;
        let receipts = std::mem::take(&mut *results.lock().unwrap());
        if !transient {
            assert!(
                receipts.last().unwrap().is_err(),
                "a permanent acquisition refusal is recorded"
            );
            assert_eq!(
                trace.registered.lock().unwrap().len(),
                1,
                "permanent refusals are never reacquired"
            );
            if let Harness::Double(backend) = &harness {
                let journal = backend
                    .server()
                    .journal(&harness.handler_id().await)
                    .unwrap();
                let refusals = journal
                    .iter()
                    .filter_map(|entry| entry.run_completion())
                    .filter_map(Result::ok)
                    .filter_map(|value| {
                        serde_json::from_slice::<
                            Result<serde_json::Value, lash_core::RuntimeEffectControllerError>,
                        >(&value)
                        .ok()
                    })
                    .filter_map(Result::err)
                    .collect::<Vec<_>>();
                assert_eq!(refusals.len(), 1, "one recorded refusal");
                let expected = if matches!(fault, Fault::Incompatible) {
                    lash_core::RuntimeErrorCode::StoreIncompatible
                } else {
                    lash_core::RuntimeErrorCode::Plugin
                };
                if refusals[0].code != expected {
                    failures.push(format!(
                        "{fault:?}: {:?} instead of {expected:?}",
                        refusals[0].code
                    ));
                }
            }
            drop(core);
            harness.finish().await;
            continue;
        }
        let receipt = match receipts.last().unwrap().as_ref() {
            Ok(receipt) => receipt,
            Err(error) => {
                failures.push(format!("{fault:?} recorded a false start refusal: {error}"));
                drop(core);
                harness.finish().await;
                continue;
            }
        };
        assert_eq!(
            receipt.disposition,
            lash_core::ProcessRegistrationOutcome::Existing
        );
        assert_eq!(receipt.start_key.as_ref(), Some(&start_key));
        let acquired = trace.registered.lock().unwrap().clone();
        assert!(
            acquired.len() >= 2,
            "acquisition retried after registration"
        );
        assert!(
            acquired.iter().all(|id| id == receipt.process_id),
            "the same committed id is retried"
        );
        assert!(
            harness
                .backend()
                .attachment_referrers()
                .attachment_referrers(&attachment.id)
                .await
                .unwrap()
                .contains(&ArtifactReferrer::ProcessRecord(receipt.process_id.clone()))
        );
        let terminal =
            tokio::time::timeout(BOUND, core.processes().await_output(&receipt.process_id))
                .await
                .unwrap()
                .unwrap();
        assert!(
            matches!(terminal, lash_core::ProcessAwaitOutput::Settled { output } if matches!(output.outcome, lash_core::ToolCallOutcome::Success(_))),
            "the one child settles successfully"
        );
        let rows = harness
            .backend()
            .process_registry()
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one start produces one child");
        harness
            .assert_run(&harness.handler_id().await, "process-start-register", None)
            .await;
        drop(core);
        harness.finish().await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn delivery_law(storage: Storage, live: bool) {
    let mut failures = Vec::new();
    // The final case is the live-receiver StorageFailure countercase.
    for case in 0..3 {
        let trace = Arc::new(AcquisitionTrace::default());
        let (harness, _stores) = Harness::new(storage, live, Arc::clone(&trace)).await;
        let core = core(&harness);
        let attachment = put(&core, &harness).await;
        let registry = harness.backend().process_registry();
        let register = || {
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: json!({}),
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
        };
        let producer = registry.register_process(register()).await.unwrap().id;
        let receiver = if case == 1 {
            let receiver = registry.register_process(register()).await.unwrap().id;
            lash_core::ExecutionScope::Process {
                process_id: receiver,
            }
        } else {
            lash_core::ExecutionScope::runtime_operation(run_tag("receiver"))
        };
        let claim = lash_core::runtime::attachment_delivery::receiving_claim(&receiver).unwrap();
        let key = harness
            .backend()
            .effect_host()
            .await_event_key(
                &receiver,
                lash_core::AwaitEventWaitIdentity::Custom {
                    key: "terminal".into(),
                },
            )
            .await
            .unwrap();
        let workflow_key = RestateDurableWaitAddress::for_key(&key).workflow_key;
        let attach = harness
            .ingress()
            .send_workflow_json(
                "LashProcessAttach",
                &workflow_key,
                "run",
                &Call::new(RestateProcessAttachRequest {
                    process_id: producer.clone(),
                    key: key.clone(),
                }),
            )
            .await
            .unwrap();
        harness.attach_armed(&producer).await;
        if case == 1 {
            let lash_core::ExecutionScope::Process { process_id } = &receiver else {
                unreachable!()
            };
            registry
                .complete_process(
                    process_id,
                    lash_core::ProcessAwaitOutput::from_tool_output(
                        lash_core::ToolCallOutput::success(json!("done")),
                    ),
                    lash_core::ProcessCompletionAuthority::external_owner(),
                )
                .await
                .unwrap();
            registry
                .prune_terminal_processes(
                    u64::MAX,
                    None,
                    lash_core::ProjectionWatermark::NoProjector,
                )
                .await
                .unwrap();
        }
        if case < 2 {
            harness
                .backend()
                .effect_host()
                .retire_await_events_for_scope(&receiver)
                .await
                .unwrap();
            harness
                .backend()
                .attachment_referrers()
                .end_attachment_referrer(claim.referrer())
                .await
                .unwrap();
        } else {
            *trace.fault.lock().unwrap() = Some((claim.referrer().clone(), Fault::StorageFailure));
        }
        let terminal = lash_core::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::success_tool_value(lash_core::ToolValue::Attachment(
                lash_core::AttachmentSource::stored(attachment.clone()),
            )),
        );
        lash_core::runtime::attachment_delivery::acquire_completion_output(
            harness.backend().attachment_referrers().as_ref(),
            &producer,
            &terminal,
        )
        .await
        .unwrap();
        registry
            .complete_process(
                &producer,
                terminal.clone(),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .unwrap();
        harness
            .backend()
            .process_work()
            .port()
            .publish_process_terminal(&producer, &terminal, "delivery-law")
            .await
            .unwrap();
        if let Err(error) = harness.await_attach(attach.as_str()).await {
            failures.push(format!("receiver {receiver:?}: {error}"));
            drop(core);
            harness.finish().await;
            continue;
        }
        assert!(
            harness
                .backend()
                .attachment_store()
                .get(
                    &attachment.id,
                    lash_core::AttachmentReadPolicy::DEFAULT.max_blob_bytes
                )
                .await
                .is_ok(),
            "producer bytes remain available"
        );
        let edges = harness
            .backend()
            .attachment_referrers()
            .attachment_referrers(&attachment.id)
            .await
            .unwrap();
        assert_eq!(
            edges.contains(claim.referrer()),
            case == 2,
            "ended receivers gain no edge"
        );
        let attempts = trace
            .attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|r| *r == claim.referrer())
            .count();
        assert_eq!(
            attempts,
            if case == 2 { 2 } else { 1 },
            "only StorageFailure retries"
        );
        if case == 2 {
            let resolution = harness
                .backend()
                .effect_host()
                .await_await_event(&key, Default::default(), None)
                .await
                .unwrap();
            assert_eq!(
                resolution,
                lash_core::Resolution::Ok(serde_json::to_value(&terminal).unwrap())
            );
        }
        harness
            .assert_run(
                attach.as_str(),
                "process-attach-acquire",
                (case == 2).then_some(&terminal),
            )
            .await;
        drop(core);
        harness.finish().await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

async fn contrast_law(storage: Storage) {
    let trace = Arc::new(AcquisitionTrace::default());
    let (harness, _stores) = Harness::new(storage, false, Arc::clone(&trace)).await;
    let core = core(&harness);
    let attachment = put(&core, &harness).await;
    let receiver = lash_core::ExecutionScope::runtime_operation("contrast");
    let claim = lash_core::runtime::attachment_delivery::receiving_claim(&receiver).unwrap();
    *trace.fault.lock().unwrap() = Some((claim.referrer().clone(), Fault::Incompatible));
    let error = lash_core::runtime::attachment_delivery::acquire_under(
        harness.backend().attachment_referrers().as_ref(),
        &claim,
        std::slice::from_ref(&attachment.id),
    )
    .await
    .unwrap();
    let lash_core::runtime::attachment_delivery::DeliveryAcquisition::Refused { refusal: error } =
        error
    else {
        panic!("compatibility refusal is a typed acquisition result: {error:?}");
    };
    assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreIncompatible);
    assert!(error.is_terminal());
    assert!(!error.clone().into_runtime_error().is_retryable());
    let unknown = lash_core::attachments::content_id(b"never uploaded");
    let output = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success_tool_value(lash_core::ToolValue::Attachment(
            lash_core::AttachmentSource::stored(lash_core::AttachmentRef {
                id: unknown.clone(),
                ..attachment
            }),
        )),
    );
    let delivered = lash_core::runtime::attachment_delivery::deliver_output(
        harness.backend().attachment_referrers().as_ref(),
        &receiver,
        output,
    )
    .await
    .unwrap();
    assert_eq!(
        delivered,
        lash_core::runtime::attachment_delivery::source_gone_output(&unknown)
    );
    assert!(
        harness
            .backend()
            .attachment_referrers()
            .attachment_referrers(&unknown)
            .await
            .unwrap()
            .is_empty()
    );
}

macro_rules! laws {
    ($module:ident, $storage:expr $(, $service:literal)?) => {
        mod $module {
            use super::*;
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn start_input_acquisition_fault_retries_the_same_registered_process() { start_law($storage, false).await; }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn ended_receiver_terminal_delivery_settles_without_retry() { delivery_law($storage, false).await; }
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn acquisition_keeps_permanent_incompatibility_and_source_gone_distinct() { contrast_law($storage).await; }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "live Restate; crash-windows suite"]
            async fn live_restate_start_input_acquisition_fault_retries_the_same_registered_process() { start_law($storage, true).await; }
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "live Restate; crash-windows suite"]
            async fn live_restate_ended_receiver_terminal_delivery_settles_without_retry() { delivery_law($storage, true).await; }
        }
    };
}

laws!(sqlite_memory, Storage::Memory);
laws!(sqlite_file, Storage::File);
laws!(
    postgres,
    Storage::Postgres,
    "requires PostgreSQL; run through the pg16 service gate"
);
