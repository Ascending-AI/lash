//! The detached resolver's receiver can end while its acquisition is unrecorded.

#![allow(clippy::disallowed_methods)]

use super::*;
use crate::durable_wait::{RestateDurableWaitAddress, durable_wait_index_object_key};
use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupPorts, ArtifactCleanupRelay, StoreSetAuthorities,
};
use lash_core::runtime::drive::relay::relay_due;
use lash_core::{ArtifactReferrer, AttachmentReferrers, ReferrerClaim, StoreError, StoreSet};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};
use std::num::NonZeroUsize;

const ACQUIRE: &str = "process-attach-acquire";
const ATTACH: &str = "LashProcessAttach";
const BOUND: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum Fault {
    Incompatible,
    WriterFenced,
    Transient,
}

impl Fault {
    fn refusal(self) -> Option<lash_core::store::StoreRefusal> {
        match self {
            Self::Incompatible => Some(lash_core::store::StoreRefusal::Incompatible {
                refusal: lash_core_store::compat::CompatRefusal::Unstamped {
                    component: "attachment_refs".to_owned(),
                    writing_release: Some("attachment-law".to_owned()),
                },
            }),
            Self::WriterFenced => Some(lash_core::store::StoreRefusal::WriterFenced {
                recorded: 2,
                writable: lash_core_store::compat::VersionRange::exactly(1),
            }),
            Self::Transient => None,
        }
    }

    fn error(self, attempt: usize) -> Option<StoreError> {
        match self.refusal() {
            Some(refusal) => Some(refusal.into_store_error()),
            None if attempt == 0 => Some(StoreError::Contended),
            None => None,
        }
    }
}

/// Counts only the receiver's acquisition and holds its redrive until cleanup.
struct Acquisitions {
    inner: Arc<dyn AttachmentReferrers>,
    receiver: Mutex<Option<ArtifactReferrer>>,
    attempts: AtomicUsize,
    resume: tokio::sync::Semaphore,
    fault: Mutex<Option<Fault>>,
}

#[async_trait::async_trait]
impl AttachmentReferrers for Acquisitions {
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
        ids: &[lash_core::AttachmentId],
    ) -> Result<(), StoreError> {
        let receiver = self.receiver.lock_recover().as_ref() == Some(claim.referrer());
        if receiver {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt > 0 {
                self.resume
                    .acquire()
                    .await
                    .expect("redrive permit")
                    .forget();
            }
            if let Some(error) = self
                .fault
                .lock_recover()
                .as_ref()
                .and_then(|fault| fault.error(attempt))
            {
                return Err(error);
            }
        }
        self.inner.acquire_attachment_refs(claim, ids).await
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        id: &lash_core::AttachmentId,
    ) -> Result<(), StoreError> {
        self.inner.forget_attachment_ref(referrer, id).await
    }
    async fn end_attachment_referrer(&self, referrer: &ArtifactReferrer) -> Result<(), StoreError> {
        self.inner.end_attachment_referrer(referrer).await
    }
    async fn session_referrer_state(
        &self,
        session: &SessionId,
    ) -> Result<lash_core::store::SessionReferrerState, StoreError> {
        self.inner.session_referrer_state(session).await
    }
    async fn attachment_referrers(
        &self,
        id: &lash_core::AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, StoreError> {
        self.inner.attachment_referrers(id).await
    }
}

type PostgresResources = (
    lash_postgres_store::PostgresStorage,
    lash_postgres_store::testing::IsolatedDatabase,
    tempfile::TempDir,
);

enum Engine {
    Double(Box<RestateTestBackend<dyn StoreSet>>),
    Live(LiveRestateBackend<dyn StoreSet>),
}

struct World {
    engine: Engine,
    acquisitions: Arc<Acquisitions>,
    _postgres: Option<PostgresResources>,
}

impl World {
    async fn new(live: bool) -> Self {
        let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
            .ok()
            .filter(|url| !url.is_empty());
        assert!(
            url.is_some() || std::env::var("LASH_REQUIRE_POSTGRES").as_deref() != Ok("1"),
            "PostgreSQL is required"
        );
        let postgres = if let Some(url) = url {
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("PostgreSQL storage");
            Some((
                storage,
                database,
                tempfile::tempdir().expect("attachment directory"),
            ))
        } else {
            None
        };
        let acquisitions = Mutex::new(None);
        let make_stores = |clock: Arc<dyn Clock>| async {
            let stores: Arc<dyn StoreSet> = match &postgres {
                Some((storage, _, directory)) => {
                    Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                        storage,
                        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                            directory.path(),
                        )),
                        lash_core::WakeDeliveryConfig::default(),
                        clock,
                    ))
                }
                None => Arc::new(
                    lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(
                        lash_sqlite_store::SqliteStoreSetOptions::memory(),
                        clock,
                    )
                    .await
                    .map_err(|error| error.to_string())?,
                ),
            };
            let counted = Arc::new(Acquisitions {
                inner: stores.attachment_referrers(),
                receiver: Mutex::new(None),
                attempts: AtomicUsize::new(0),
                resume: tokio::sync::Semaphore::new(0),
                fault: Mutex::new(None),
            });
            *acquisitions.lock_recover() = Some(Arc::clone(&counted));
            Ok(
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .map_attachment_referrers(|_| counted as Arc<dyn AttachmentReferrers>)
                    .into_store_set(),
            )
        };
        let engine = if live {
            let env =
                |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
            Engine::Live(
                LiveRestateBackend::start_with_store_set(
                    LiveConfig {
                        ingress_url: env("RESTATE_INGRESS_URL"),
                        admin_url: env("RESTATE_ADMIN_URL"),
                        endpoint_bind: env("AA_BIND").parse().expect("endpoint bind"),
                        endpoint_url: env("AA_URL"),
                        run_tag: format!(
                            "attachment-attach-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .expect("epoch")
                                .as_nanos()
                        ),
                        namespace: Default::default(),
                    },
                    |clock| async {
                        make_stores(clock)
                            .await
                            .map_err(lash_restate_test::live::LiveError::Stores)
                    },
                )
                .await
                .expect("live deployment"),
            )
        } else {
            let mut config = ServerConfig::default();
            config.retry.max_attempts = Some(4);
            Engine::Double(Box::new(
                lash_restate_test::backend_with_store_set(
                    0x4287,
                    config,
                    lash_restate_test::DeploymentHooks::default(),
                    |clock| async {
                        make_stores(clock)
                            .await
                            .map_err(lash_restate_test::BackendError::Stores)
                    },
                )
                .await
                .expect("double deployment"),
            ))
        };
        Self {
            engine,
            acquisitions: acquisitions
                .into_inner()
                .expect("counter mutex")
                .expect("acquisition counter"),
            _postgres: postgres,
        }
    }

    fn backend(&self) -> lash_core::Backend {
        match &self.engine {
            Engine::Double(engine) => engine.lash_backend(),
            Engine::Live(engine) => engine.lash_backend(),
        }
    }

    fn ingress(&self) -> lash::restate::RestateIngressClient {
        match &self.engine {
            Engine::Double(engine) => engine.ingress(),
            Engine::Live(engine) => engine.ingress(),
        }
    }

    fn crash_acquisition(&self, crashed: Arc<tokio::sync::Notify>) {
        let listener: lash_restate_test::server::CrashListener =
            Arc::new(move |_| crashed.notify_one());
        let rule = CrashRule::new(CrashPoint::BeforeRunResult {
            name: Some(ACQUIRE.to_owned()),
        })
        .service(ATTACH)
        .handler("run");
        match &self.engine {
            Engine::Double(engine) => {
                assert!(engine.server().on_crash(listener));
                engine.server().crash_on(rule);
            }
            Engine::Live(engine) => {
                assert!(engine.on_crash(listener));
                engine.crash_on(rule);
            }
        }
    }

    async fn status(&self, key: &str) -> Option<String> {
        let target = format!("{ATTACH}/{key}/run");
        match &self.engine {
            Engine::Double(engine) => engine
                .server()
                .invocations()
                .into_iter()
                .find(|view| view.target == target)
                .map(|view| view.status.to_owned()),
            Engine::Live(engine) => engine
                .invocations()
                .await
                .ok()?
                .into_iter()
                .find(|view| view.target == target)
                .map(|view| view.status),
        }
    }

    async fn completed(&self, key: &str) {
        let status = tokio::time::timeout(BOUND, async {
            loop {
                if let Some(status) = self.status(key).await
                    && matches!(status.as_str(), "completed" | "paused")
                {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("resolver terminates within the bound");
        assert_eq!(
            status, "completed",
            "terminal acquisition refusals complete instead of retrying"
        );
    }

    async fn result(&self, key: &str) -> Result<(), String> {
        let target = format!("{ATTACH}/{key}/run");
        match &self.engine {
            Engine::Double(engine) => {
                let invocation = engine
                    .server()
                    .invocations()
                    .into_iter()
                    .find(|view| view.target == target)
                    .expect("attach invocation");
                match engine
                    .server()
                    .outcome(&invocation.id)
                    .expect("completed resolver outcome")
                {
                    Ok(bytes) => {
                        let reply: crate::Reply<()> =
                            serde_json::from_slice(&bytes).expect("resolver reply");
                        reply.into_body();
                        Ok(())
                    }
                    Err((_, message)) => Err(message),
                }
            }
            Engine::Live(engine) => {
                let invocation = engine
                    .invocations()
                    .await
                    .expect("live invocations")
                    .into_iter()
                    .find(|view| view.target == target)
                    .expect("live attach invocation");
                engine
                    .outcome(&invocation.id)
                    .await
                    .expect("live resolver outcome")
                    .expect("completed resolver")
            }
        }
    }

    async fn assert_acquisition_recorded(
        &self,
        key: &str,
        refusal: Option<&lash_core::store::StoreRefusal>,
        receiver_ended: bool,
    ) {
        let target = format!("{ATTACH}/{key}/run");
        match &self.engine {
            Engine::Double(engine) => {
                let invocation = engine
                    .server()
                    .invocations()
                    .into_iter()
                    .find(|view| view.target == target)
                    .expect("attach invocation");
                let journal = engine
                    .server()
                    .journal(&invocation.id)
                    .expect("attach journal");
                assert_eq!(
                    journal
                        .iter()
                        .filter(|entry| entry.name.as_deref() == Some(ACQUIRE))
                        .count(),
                    1
                );
                let outcomes: Vec<_> = journal
                    .iter()
                    .filter_map(|entry| entry.run_completion())
                    .collect();
                assert_eq!(outcomes.len(), 1, "the acquisition has one recorded result");
                let outcome: serde_json::Value = serde_json::from_slice(
                    outcomes[0]
                        .as_ref()
                        .expect("acquisition is journaled as a successful typed result"),
                )
                .expect("typed acquisition JSON");
                if let Some(refusal) = refusal {
                    assert_eq!(outcome["type"], "refused");
                    assert_eq!(
                        outcome["refusal"]["cause"]["refusal"],
                        serde_json::to_value(refusal).expect("refusal fields")
                    );
                } else if receiver_ended {
                    assert_eq!(outcome["type"], "receiver_ended");
                    assert_eq!(
                        outcome["referrer"],
                        serde_json::to_value(
                            self.acquisitions
                                .receiver
                                .lock_recover()
                                .as_ref()
                                .expect("receiving referrer")
                        )
                        .expect("ended referrer")
                    );
                } else {
                    assert_eq!(outcome["type"], "held");
                }
            }
            Engine::Live(engine) => {
                let invocation = engine
                    .invocations()
                    .await
                    .expect("live invocations")
                    .into_iter()
                    .find(|view| view.target == target)
                    .expect("live attach invocation");
                let journal = engine.journal(&invocation.id).await.expect("live journal");
                assert_eq!(
                    journal
                        .iter()
                        .filter(|entry| entry.ends_with(&format!(":{ACQUIRE}")))
                        .count(),
                    1,
                    "live production acquisition is journaled"
                );
            }
        }
    }

    async fn registry_state(&self, address: &RestateDurableWaitAddress) -> Vec<String> {
        let object_key = durable_wait_index_object_key(address);
        match &self.engine {
            Engine::Double(engine) => engine
                .server()
                .object_state("LashDurableWaitIndex", &object_key)
                .into_keys()
                .collect(),
            Engine::Live(_) => {
                #[derive(serde::Deserialize)]
                struct Row {
                    key: String,
                }
                let admin = RestateAdminClient::new(RestateConnection::new(
                    std::env::var("RESTATE_ADMIN_URL").expect("admin URL"),
                ));
                let rows: Vec<Row> = admin.query_json(&format!("SELECT key FROM state WHERE service_name = 'LashDurableWaitIndex' AND service_key = '{}'", object_key.replace('\'', "''"))).await.expect("wait registry state");
                rows.into_iter().map(|row| row.key).collect()
            }
        }
    }
}

struct Prepared {
    receiver: ProcessId,
    referrer: ArtifactReferrer,
    id: lash_core::AttachmentId,
    request: crate::Call<crate::RestateProcessAttachRequest>,
    address: RestateDurableWaitAddress,
}

async fn prepare_delivery(world: &World) -> Prepared {
    let backend = world.backend();
    let registry = backend.process_registry();
    let producer = registry
        .register_process(external_registration())
        .await
        .expect("producer")
        .id;
    let receiver = registry
        .register_process(external_registration())
        .await
        .expect("receiver")
        .id;
    let referrer = ArtifactReferrer::ProcessRecord(receiver.clone());
    *world.acquisitions.receiver.lock_recover() = Some(referrer.clone());
    let stored = backend
        .attachment_store()
        .put(
            b"attachment attach redrive".to_vec(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("text/plain").expect("media type"),
                None,
                None,
            ),
        )
        .await
        .expect("stored attachment bytes");
    let id = stored.id.clone();
    let write = lash_core::AttachmentWrite {
        attachment_id: id.clone(),
        claim: ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(producer.clone()))
            .expect("producer claim"),
    };
    let attachments = &world.acquisitions.inner;
    let lash_core::AttachmentWriteFence::Granted(permit) = attachments
        .begin_attachment_write(&write)
        .await
        .expect("begin upload")
    else {
        panic!("producer is live")
    };
    attachments
        .complete_attachment_write(&write, permit)
        .await
        .expect("upload evidence");
    let output =
        ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success_tool_value(
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(stored)),
        ));
    registry
        .complete_process(
            &producer,
            output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("producer terminal");
    backend
        .process_work()
        .port()
        .publish_process_terminal(&producer, &output, "producer-terminal")
        .await
        .expect("publish terminal");
    let key = backend
        .effect_host()
        .await_event_key(
            &ExecutionScope::Process {
                process_id: receiver.clone(),
            },
            AwaitEventWaitIdentity::Custom {
                key: "attachment-attach-redrive".to_owned(),
            },
        )
        .await
        .expect("receiver wait key");
    let address = RestateDurableWaitAddress::for_key(&key);
    let request = crate::Call::new(crate::RestateProcessAttachRequest {
        process_id: producer,
        key,
    });
    Prepared {
        receiver,
        referrer,
        id,
        request,
        address,
    }
}

async fn receiver_prune_law(live: bool) {
    println!(
        "host load {}",
        std::fs::read_to_string("/proc/loadavg").expect("host load")
    );
    let world = World::new(live).await;
    let backend = world.backend();
    let registry = backend.process_registry();
    let Prepared {
        receiver,
        referrer,
        id,
        request,
        address,
    } = prepare_delivery(&world).await;
    let attachments = &world.acquisitions.inner;
    let crashed = Arc::new(tokio::sync::Notify::new());
    world.crash_acquisition(Arc::clone(&crashed));
    world
        .ingress()
        .send_workflow_json(ATTACH, &address.workflow_key, "run", &request)
        .await
        .expect("send detached resolver");
    tokio::time::timeout(BOUND, crashed.notified())
        .await
        .expect("crash before acquisition recording");
    assert!(
        attachments
            .attachment_referrers(&id)
            .await
            .expect("first acquisition edges")
            .contains(&referrer)
    );
    assert!(
        world.registry_state(&address).await.is_empty(),
        "the crash precedes wait resolution"
    );
    registry
        .request_process_cancel(
            &receiver,
            lash_core::CancelOrigin::OperatorRequested,
            "attachment-law".to_owned(),
            None,
        )
        .await
        .expect("cancel receiver");
    registry
        .complete_process(
            &receiver,
            process_cancellation("receiver cancelled".to_owned(), None),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("external owner acknowledges cancellation");
    registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune receiver");
    assert!(
        !matches!(registry.get_process(&receiver).await, Ok(Some(_))),
        "receiver is pruned"
    );
    let relay = ArtifactCleanupRelay::new(ArtifactCleanupPorts {
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
        attachments: Arc::clone(attachments),
        clock: backend.clock(),
    });
    relay_due(
        &relay,
        backend.clock().as_ref(),
        NonZeroUsize::new(100).expect("cleanup limit"),
    )
    .await
    .expect("apply real cleanup");
    let claim = ReferrerClaim::unguarded(referrer.clone()).expect("receiver claim");
    assert!(
        matches!(attachments.acquire_attachment_refs(&claim, std::slice::from_ref(&id)).await, Err(StoreError::ArtifactReferrerEnded { referrer: ended }) if ended == referrer),
        "cleanup permanently fences the receiver"
    );
    assert!(
        !attachments
            .attachment_referrers(&id)
            .await
            .expect("cleaned edges")
            .contains(&referrer)
    );
    world.acquisitions.resume.add_permits(100);
    if let Engine::Live(engine) = &world.engine {
        engine
            .start_serving()
            .await
            .expect("restore crashed deployment");
    }
    let status = tokio::time::timeout(BOUND, async {
        loop {
            if let Some(status) = world.status(&address.workflow_key).await
                && matches!(status.as_str(), "completed" | "paused")
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("resolver terminates within the bound");
    assert_eq!(
        status, "completed",
        "an ended receiver abandons delivery instead of retrying"
    );
    world
        .result(&address.workflow_key)
        .await
        .expect("resolver completed successfully");
    assert_eq!(
        world.acquisitions.attempts.load(Ordering::SeqCst),
        2,
        "one lost acquisition and one terminal refusal, with no further acquisition"
    );
    assert!(
        !attachments
            .attachment_referrers(&id)
            .await
            .expect("final edges")
            .contains(&referrer)
    );
    assert!(
        world.registry_state(&address).await.is_empty(),
        "abandoned delivery recreates no wait or resolution"
    );
    world
        .assert_acquisition_recorded(&address.workflow_key, None, true)
        .await;
}

#[tokio::test]
async fn attachment_attach_redrive_after_receiver_prune_terminates() {
    receiver_prune_law(false).await;
}

#[tokio::test]
#[ignore = "requires live Restate; the attachment-attach suite runs this law"]
async fn live_restate_attachment_attach_redrive_after_receiver_prune_terminates() {
    receiver_prune_law(true).await;
}

async fn store_fault_law(live: bool, fault: Fault) {
    let world = World::new(live).await;
    let Prepared {
        referrer,
        id,
        request,
        address,
        ..
    } = prepare_delivery(&world).await;
    *world.acquisitions.fault.lock_recover() = Some(fault);
    world.acquisitions.resume.add_permits(100);
    world
        .ingress()
        .send_workflow_json(ATTACH, &address.workflow_key, "run", &request)
        .await
        .expect("send resolver");
    world.completed(&address.workflow_key).await;
    let result = world.result(&address.workflow_key).await;
    let refusal = fault.refusal();
    if let Some(refusal) = &refusal {
        result.expect("a permanent refusal completes the detached resolver");
        let reply: crate::Reply<Resolution> = tokio::time::timeout(
            BOUND,
            world.ingress().call_workflow_json(
                "LashDurableWaitWorkflow",
                &address.workflow_key,
                "await_resolution",
                &crate::Call::new(crate::RestateDurableWaitAwaitRequest {
                    key: request.body.key.clone(),
                    deadline: None,
                }),
            ),
        )
        .await
        .expect("live receiver is resolved")
        .expect("read receiver resolution");
        let Resolution::Err(error) = reply.into_body() else {
            panic!("a refused acquisition resolves an error for the live receiver")
        };
        assert!(
            error.message.contains(&refusal.to_string()),
            "refusal retains its diagnostic: {}",
            error.message
        );
        let expected_code = lash_sansio::FailureCode::from(&refusal.code());
        assert_eq!(
            error.code,
            lash_sansio::FailureCode::from_foreign_wire(&expected_code.namespaced()),
            "refusal retains its external completion code"
        );
        assert_eq!(
            world.acquisitions.attempts.load(Ordering::SeqCst),
            1,
            "a terminal refusal is acquired once"
        );
        assert!(
            !world
                .acquisitions
                .inner
                .attachment_referrers(&id)
                .await
                .expect("referrers")
                .contains(&referrer)
        );
        assert!(
            !world.registry_state(&address).await.is_empty(),
            "a live receiver keeps its refusal resolution"
        );
    } else {
        result.expect("a transient store fault retries and delivers");
        assert_eq!(
            world.acquisitions.attempts.load(Ordering::SeqCst),
            2,
            "one transient fault and one successful acquisition"
        );
        assert!(
            world
                .acquisitions
                .inner
                .attachment_referrers(&id)
                .await
                .expect("referrers")
                .contains(&referrer)
        );
        assert!(
            !world.registry_state(&address).await.is_empty(),
            "a live receiver keeps its resolution"
        );
    }
    world
        .assert_acquisition_recorded(&address.workflow_key, refusal.as_ref(), false)
        .await;
}

#[tokio::test]
async fn attachment_attach_compatibility_refusals_are_recorded() {
    for fault in [Fault::Incompatible, Fault::WriterFenced] {
        store_fault_law(false, fault).await;
    }
}

#[tokio::test]
#[ignore = "requires live Restate; the attachment-attach suite runs this law"]
async fn live_restate_attachment_attach_compatibility_refusals_are_recorded() {
    for fault in [Fault::Incompatible, Fault::WriterFenced] {
        store_fault_law(true, fault).await;
    }
}

#[tokio::test]
async fn attachment_attach_retries_only_transient_acquisition_faults() {
    store_fault_law(false, Fault::Transient).await;
}

#[tokio::test]
#[ignore = "requires live Restate; the attachment-attach suite runs this law"]
async fn live_restate_attachment_attach_retries_only_transient_acquisition_faults() {
    store_fault_law(true, Fault::Transient).await;
}
