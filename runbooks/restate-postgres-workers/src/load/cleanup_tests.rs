//! The workload's cleanup on a real Restate handler and SQL process registry.

#![allow(deprecated, reason = "the pinned SDK retains the trait workflow API")]

use super::*;
use lash::restate::restate_sdk;
use lash::testing::wait_until;
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{RestateTestBackend, ServerConfig};
use restate_sdk::context::{ContextPromises, ContextWriteState, SharedWorkflowContext};
use restate_sdk::endpoint::Endpoint;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const SERVICE: &str = "WorkloadDeleteProbe";
const RESUME: &str = "resume";
const FINISH: &str = "finish";

#[allow(deprecated, reason = "the pinned SDK retains the trait workflow API")]
#[restate_sdk::workflow]
trait WorkloadDeleteProbe {
    async fn run() -> HandlerResult<u64>;
    #[shared]
    async fn release(promise: String) -> HandlerResult<String>;
}

struct Services {
    core: lash::LashCore,
    authority: lash::restate::RestateAuthorityId,
}

struct Probe {
    services: Arc<OnceLock<Services>>,
    stalled: Option<Arc<AtomicUsize>>,
    clock: Option<Arc<AtomicU64>>,
}

impl WorkloadDeleteProbe for Probe {
    async fn run(&self, context: WorkflowContext<'_>) -> HandlerResult<u64> {
        let services = self.services.get().expect("the core precedes the handler");
        let controller = Controller::new(
            context,
            services.authority.clone(),
            services.core.build_generation().clone(),
        );
        let ctx = controller.context();
        let cleaned = cleanup_model_children(
            ctx,
            &ProbeCleanup {
                inner: SessionProcessCleanup {
                    processes: services.core.processes(),
                    controller: &controller,
                },
                stalled: self.stalled.as_ref(),
                clock: self.clock.as_ref(),
            },
            ctx.key(),
        )
        .await?;
        ctx.set("cleaned", cleaned as u64);
        ctx.promise::<String>(RESUME).await?;
        ctx.promise::<String>(FINISH).await?;
        Ok(cleaned as u64)
    }

    async fn release(
        &self,
        ctx: SharedWorkflowContext<'_>,
        promise: String,
    ) -> HandlerResult<String> {
        ctx.resolve_promise(&promise, "released".to_owned());
        Ok(promise)
    }
}

fn services(backend: lash::Backend, authority: lash::restate::RestateAuthorityId) -> Services {
    let provider = lash::testing::TestProvider::builder()
        .kind("workload-delete")
        .complete(|_| async { Ok::<_, lash::provider::LlmTransportError>(Default::default()) })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .plugin(lash::testing::process_engine_plugin_fixture())
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder("mock-model")
                .context_window_tokens(200_000)
                .build()
                .unwrap(),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "workload-delete",
            "law",
        ))
        .unwrap();
    Services { core, authority }
}

async fn child(backend: lash::Backend, session: &str) -> lash::ProcessId {
    backend
        .process_registry()
        .register_process(lash::testing::held_engine_registration(
            Value::Null,
            lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                SessionId::fixture(session),
            )),
            lash::process::Lifetime::Detached,
        ))
        .await
        .unwrap()
        .id
}

async fn witness(double: RestateTestBackend<dyn lash::StoreSet>, storage: &str) {
    let cell = Arc::new(OnceLock::from(services(
        double.lash_backend(),
        double
            .restate()
            .restate_effect_host()
            .authority_id()
            .clone(),
    )));
    double
        .server()
        .register(
            Endpoint::builder()
                .bind(
                    Probe {
                        services: cell.clone(),
                        stalled: None,
                        clock: None,
                    }
                    .serve(),
                )
                .build(),
        )
        .await
        .unwrap();
    double.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            cell.get()
                .unwrap()
                .core
                .durable_process_worker_config()
                .unwrap(),
        )
        .unwrap(),
    );
    let session = "child-appears-after-cleanup";
    let target = format!("{SERVICE}/{session}/run");
    let ingress = double.ingress();
    let run = tokio::spawn({
        let ingress = ingress.clone();
        async move {
            ingress
                .call_workflow_empty::<u64>(SERVICE, session, "run")
                .await
        }
    });
    let server = double.server();
    let invocation = || {
        server
            .invocations()
            .into_iter()
            .find(|i| i.target == target)
    };
    let promises = |id: &str| {
        server
            .journal(id)
            .unwrap_or_default()
            .iter()
            .filter(|e| e.ty == lash_restate_test::protocol::MessageType::GetPromiseCommand)
            .count()
    };
    wait_until("cleanup returns and the delete parks", || {
        invocation().is_some_and(|i| promises(&i.id) == 1)
    })
    .await;
    let parked = invocation().unwrap();
    let recorded = server.journal(&parked.id).unwrap();
    let appeared = child(double.lash_backend(), session).await;
    let sibling = child(double.lash_backend(), "other-session").await;
    // Kill even on always_replay: both its suspended and streaming attempts
    // must re-enter cleanup after the stored child appeared.
    if !server.crash(&parked.id) {
        assert!(
            parked.suspensions > 0,
            "the suspended host replays when resumed"
        );
    }
    ingress
        .call_workflow_json::<_, String>(SERVICE, session, "release", &RESUME)
        .await
        .unwrap();
    wait_until("the replay reaches its second promise", || {
        let current = invocation().unwrap();
        if let Some((code, message)) = &current.last_failure {
            assert!(
                *code != 570,
                "{storage}: replay diverged [{code}]: {message}"
            );
        }
        promises(&current.id) == 2
    })
    .await;
    let replayed = server.journal(&parked.id).unwrap();
    assert_eq!(replayed.get(..recorded.len()), Some(&recorded[..]));
    ingress
        .call_workflow_json::<_, String>(SERVICE, session, "release", &FINISH)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
    let rows = cell.get().unwrap().core.processes();
    assert!(
        rows.get(&appeared)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .is_live()
    );
    assert!(
        rows.get(&sibling)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .is_live()
    );
    let finished = invocation().unwrap();
    assert_eq!(finished.status, "completed");
    assert!(finished.attempts >= 2);
    eprintln!(
        "{storage}: stored child appeared; replay kept {} journal entries; cleaned=0",
        recorded.len()
    );
}

#[derive(Clone, Copy)]
enum Storage {
    Memory,
    File,
    Postgres,
}

async fn law(storage: Storage, replay: bool) {
    let config = ServerConfig::default().always_replay(replay);
    let root = tempfile::tempdir().unwrap();
    let seed = 4348;
    match storage {
        Storage::Memory => {
            witness(
                lash_restate_test::backend(seed, config)
                    .await
                    .unwrap()
                    .erase_store_type(),
                "sqlite-memory",
            )
            .await
        }
        Storage::File => {
            let double = lash_restate_test::backend_with_store_set(
                seed,
                config,
                Default::default(),
                |clock| async {
                    Ok(Arc::new(
                        lash::sqlite::SqliteStoreSet::open_with_clock(root.path(), clock)
                            .await
                            .unwrap(),
                    ) as Arc<dyn lash::StoreSet>)
                },
            )
            .await
            .unwrap();
            witness(double, "sqlite-file").await;
        }
        Storage::Postgres => {
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .expect("the PostgreSQL gate supplies its URL");
            let database = lash::postgres::testing::IsolatedDatabase::create(&url).await;
            let storage = lash::postgres::PostgresStorage::connect(database.url())
                .await
                .unwrap();
            let double = lash_restate_test::backend_with_store_set(
                seed,
                config,
                Default::default(),
                |clock| async {
                    Ok(Arc::new(lash::postgres::PostgresStoreSet::with_clock(
                        &storage,
                        Arc::new(lash::persistence::FileAttachmentStore::new(root.path())),
                        Default::default(),
                        clock,
                    )) as Arc<dyn lash::StoreSet>)
                },
            )
            .await
            .unwrap();
            witness(double, "postgres").await;
        }
    }
}

macro_rules! law {
    ($module:ident, $storage:ident, $replay:expr $(, $ignore:meta)?) => {
        mod $module {
            use super::*;
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            $(#[$ignore])?
            async fn a_workload_delete_replayed_after_a_child_appeared_matches_its_journal() {
                law(Storage::$storage, $replay).await;
            }
        }
    };
}
law!(sqlite_memory, Memory, false);
law!(sqlite_memory_replay, Memory, true);
law!(sqlite_file, File, false);
law!(sqlite_file_replay, File, true);
law!(
    postgres,
    Postgres,
    false,
    ignore = "requires the PostgreSQL service gate"
);
law!(
    postgres_replay,
    Postgres,
    true,
    ignore = "requires the PostgreSQL service gate"
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the workload-delete live Restate suite"]
async fn live_restate_a_workload_delete_replayed_after_a_child_appeared_matches_its_journal() {
    let env = |name| std::env::var(name).unwrap();
    let key = format!(
        "child-appeared-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let cell = Arc::new(OnceLock::new());
    let backend = LiveRestateBackend::start_with_services(
        LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("WDP_BIND").parse().unwrap(),
            endpoint_url: env("WDP_URL"),
            run_tag: key.clone(),
            namespace: Default::default(),
        },
        {
            let cell = cell.clone();
            move |builder| {
                builder.bind(
                    Probe {
                        services: cell,
                        stalled: None,
                        clock: None,
                    }
                    .serve(),
                )
            }
        },
    )
    .await
    .unwrap();
    assert!(
        cell.set(services(
            backend.lash_backend(),
            lash::restate::RestateAuthorityId::new(format!("lash-live-{key}")).unwrap()
        ))
        .is_ok()
    );
    let target = format!("{SERVICE}/{key}/run");
    let ingress = backend.ingress();
    // An ingress response proves the partition serves handlers before its
    // admin journal queries begin. The readiness key is outside the witness.
    ingress
        .call_workflow_json::<_, String>(SERVICE, &format!("ready-{key}"), "release", &"ready")
        .await
        .unwrap();
    let run = tokio::spawn({
        let ingress = ingress.clone();
        let key = key.clone();
        async move {
            ingress
                .call_workflow_empty::<u64>(SERVICE, &key, "run")
                .await
        }
    });
    let host = || async {
        backend
            .invocations()
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.target == target)
            .unwrap()
    };
    let promises = |journal: &[String]| {
        journal
            .iter()
            .filter(|e| e.contains("Command: GetPromise"))
            .count()
    };
    let (parked, recorded) = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(i) = backend
                .invocations()
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.target == target)
            {
                let journal = backend.journal(&i.id).await.unwrap();
                if promises(&journal) == 1 {
                    break (i, journal);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let appeared = child(backend.lash_backend(), &key).await;
    let sibling = child(backend.lash_backend(), "other-session").await;
    backend.stop_serving(true);
    backend.start_serving().await.unwrap();
    ingress
        .call_workflow_json::<_, String>(SERVICE, &key, "release", &RESUME)
        .await
        .unwrap();
    let replayed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let current = host().await;
            if let Some(failure) = current.last_failure {
                assert!(
                    !failure.contains("Journal mismatch") && !failure.contains("RT0016"),
                    "live replay diverged: {failure}"
                );
            }
            let journal = backend.journal(&current.id).await.unwrap();
            if promises(&journal) == 2 {
                break journal;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(replayed.get(..recorded.len()), Some(&recorded[..]));
    ingress
        .call_workflow_json::<_, String>(SERVICE, &key, "release", &FINISH)
        .await
        .unwrap();
    until_live_completed(&backend, &target).await;
    let cleaned = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(cleaned, 0);
    let rows = cell.get().unwrap().core.processes();
    assert!(
        rows.get(&appeared)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .is_live()
    );
    assert!(
        rows.get(&sibling)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .is_live()
    );
    assert!(!parked.id.is_empty());
}

async fn until_live_completed(backend: &LiveRestateBackend, target: &str) {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let host = backend
                .invocations()
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.target == target)
                .unwrap();
            assert!(
                host.last_failure
                    .as_ref()
                    .is_none_or(|f| !f.contains("Journal mismatch")),
                "{host:?}"
            );
            if host.status == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

/// The deadline fixture acknowledges cancellation and blocks its terminal
/// read. Other fixtures use the production adapter for every operation; all
/// listings read the real SQL registry.
struct ProbeCleanup<'a, 'ctx> {
    inner: SessionProcessCleanup<'a, 'ctx>,
    stalled: Option<&'a Arc<AtomicUsize>>,
    clock: Option<&'a Arc<AtomicU64>>,
}

#[async_trait::async_trait]
impl WorkloadProcessCleanup for ProbeCleanup<'_, '_> {
    fn timestamp_ms(&self) -> u64 {
        match self.clock {
            Some(clock) => clock.load(Ordering::SeqCst),
            None => self.inner.timestamp_ms(),
        }
    }

    async fn owned(&self, session: &str) -> Result<Vec<lash::ProcessId>> {
        self.inner.owned(session).await
    }

    async fn cancel(&self, process: &lash::ProcessId) -> Result<()> {
        if self.stalled.is_some() {
            Ok(())
        } else {
            self.inner.cancel(process).await
        }
    }

    async fn await_terminal(&self, process: &lash::ProcessId) -> Result<()> {
        if let Some(stalled) = self.stalled {
            stalled.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        } else {
            self.inner.await_terminal(process).await
        }
    }
}

async fn deadline_law(replay: bool) {
    let double = lash_restate_test::backend(4349, ServerConfig::default().always_replay(replay))
        .await
        .unwrap();
    let cell = Arc::new(OnceLock::from(services(
        double.lash_backend(),
        double
            .restate()
            .restate_effect_host()
            .authority_id()
            .clone(),
    )));
    let stalled = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(AtomicU64::new(0));
    let key = "blocked-terminal";
    child(double.lash_backend(), key).await;
    double
        .server()
        .register(
            Endpoint::builder()
                .bind(
                    Probe {
                        services: cell,
                        stalled: Some(stalled.clone()),
                        clock: Some(clock.clone()),
                    }
                    .serve(),
                )
                .build(),
        )
        .await
        .unwrap();
    let run = tokio::spawn({
        let ingress = double.ingress();
        async move {
            ingress
                .call_workflow_empty::<u64>(SERVICE, key, "run")
                .await
        }
    });
    let server = double.server();
    let host = || {
        server
            .invocations()
            .into_iter()
            .find(|i| i.target == format!("{SERVICE}/{key}/run"))
            .unwrap()
    };
    wait_until("the terminal read blocks", || {
        stalled.load(Ordering::SeqCst) > 0
    })
    .await;
    let parked = host();
    let recorded = server.journal(&parked.id).unwrap();
    assert_eq!(
        recorded
            .iter()
            .filter(|e| e.name.as_deref() == Some("load.model-children.deadline"))
            .count(),
        1,
        "cleanup records its deadline before reading children"
    );
    // The retry begins after the original deadline, while its terminal
    // read has never returned. A fresh 20-second budget would park again.
    clock.store(20_001, Ordering::SeqCst);
    assert!(server.crash(&parked.id));
    let failure = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        failure.to_string().contains("journaled deadline elapsed"),
        "{failure}"
    );
    let replayed = server.journal(&parked.id).unwrap();
    assert_eq!(replayed.get(..recorded.len()), Some(&recorded[..]));
    assert_eq!(
        replayed
            .iter()
            .filter(|e| e.name.as_deref() == Some("load.model-children.deadline"))
            .count(),
        1
    );
    assert_eq!(
        stalled.load(Ordering::SeqCst),
        1,
        "the retried read expires before calling the child again"
    );
    let finished = host();
    assert!(finished.attempts >= 2, "{finished:?}");
    assert_eq!(finished.status, "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_cleanup_expires_at_its_original_journaled_deadline() {
    deadline_law(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_always_replayed_cleanup_expires_at_its_original_journaled_deadline() {
    deadline_law(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_turn_cleanup_settles_parked_children_and_preserves_other_sessions() {
    let double = lash_restate_test::backend(4350, ServerConfig::default())
        .await
        .unwrap();
    let cell = Arc::new(OnceLock::from(services(
        double.lash_backend(),
        double
            .restate()
            .restate_effect_host()
            .authority_id()
            .clone(),
    )));
    double.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            cell.get()
                .unwrap()
                .core
                .durable_process_worker_config()
                .unwrap(),
        )
        .unwrap(),
    );
    let key = "retired-session";
    let cancelled = child(double.lash_backend(), key).await;
    let sibling = child(double.lash_backend(), "other-session").await;
    // The law stands in for the child's engine: it waits for the
    // cancellation, then publishes the child's terminal through the SQL
    // registry and the Restate process substrate.
    let owner = tokio::spawn({
        let backend = double.lash_backend();
        let cancelled = cancelled.clone();
        async move {
            let registry = backend.process_registry();
            tokio::time::timeout(Duration::from_secs(30), async {
                while registry
                    .get_process(&cancelled)
                    .await
                    .unwrap()
                    .unwrap()
                    .cancel_request
                    .is_none()
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let output = lash::process::ProcessAwaitOutput::from_tool_output(
                lash::tools::ToolCallOutput::cancelled(lash::tools::ToolCancellation::runtime(
                    "the cleanup cancelled this child",
                )),
            );
            registry
                .complete_process(
                    &cancelled,
                    output.clone(),
                    lash::process::ProcessCompletionAuthority::workflow_key(&cancelled),
                )
                .await
                .unwrap();
            backend
                .process_work()
                .port()
                .publish_process_terminal(&cancelled, &output, "cleanup-law-terminal")
                .await
                .unwrap();
        }
    });
    double
        .server()
        .register(
            Endpoint::builder()
                .bind(
                    Probe {
                        services: cell.clone(),
                        stalled: None,
                        clock: None,
                    }
                    .serve(),
                )
                .build(),
        )
        .await
        .unwrap();
    let ingress = double.ingress();
    let run = tokio::spawn({
        let ingress = ingress.clone();
        async move {
            ingress
                .call_workflow_empty::<u64>(SERVICE, key, "run")
                .await
        }
    });
    let server = double.server();
    wait_until("cleanup settles its cancelled child", || {
        server.invocations().into_iter().any(|i| {
            i.target == format!("{SERVICE}/{key}/run")
                && server
                    .journal(&i.id)
                    .unwrap()
                    .iter()
                    .any(|e| e.ty == lash_restate_test::protocol::MessageType::GetPromiseCommand)
        })
    })
    .await;
    owner.await.unwrap();
    ingress
        .call_workflow_json::<_, String>(SERVICE, key, "release", &RESUME)
        .await
        .unwrap();
    ingress
        .call_workflow_json::<_, String>(SERVICE, key, "release", &FINISH)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1
    );
    let rows = cell.get().unwrap().core.processes();
    assert_eq!(
        rows.get(&cancelled).await.unwrap().unwrap().lifecycle,
        lash::process::ProcessStatus::Cancelled
    );
    assert!(
        rows.get(&sibling)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .is_live()
    );
}
