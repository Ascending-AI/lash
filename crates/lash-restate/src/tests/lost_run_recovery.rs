#![allow(
    clippy::disallowed_methods,
    reason = "service laws read injected addresses and record host load"
)]

use super::*;
use lash_core::StoreSet;
use lash_core::engine::{BuildGeneration, ReconcileCursor};
use lash_core::runtime::drive::relay::{DeliveryFailure, ObligationDelivery, ObligationRelay};
use lash_core::runtime::drive::{ReconcileParts, ReconcileProcesses, RelayLanes, reconcile_once};
use lash_core::runtime::recovery_lease::RecoveryDuties;
use lash_core::store::{ObligationKind, ObligationLedger};
use std::num::NonZeroUsize;

#[derive(Debug)]
struct RecoveryTransport {
    inner: Arc<dyn HttpTransport>,
    pages: Mutex<Vec<(String, usize, String)>>,
    hold: AtomicBool,
    held: tokio::sync::Notify,
}

impl RecoveryTransport {
    fn process_query_count(&self) -> usize {
        self.pages
            .lock_recover()
            .iter()
            .filter(|(kind, _, _)| kind == "process")
            .count()
    }
}

#[async_trait::async_trait]
impl HttpTransport for RecoveryTransport {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        if request.url.ends_with("/query") {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("query");
            let query = body["query"].as_str().expect("SQL");
            if let Some((_, keys)) = query.split_once("target_service_key IN (") {
                let kind = if query.contains("LashProcessWorkflow") {
                    "process"
                } else {
                    "root"
                };
                self.pages.lock_recover().push((
                    kind.into(),
                    keys.split(", ").count(),
                    query.into(),
                ));
                if kind == "process" && self.hold.swap(false, Ordering::SeqCst) {
                    self.held.notify_one();
                    std::future::pending::<()>().await;
                }
            }
        }
        self.inner.send(request, timeout).await
    }
}

struct PulseRelay {
    ledger: Arc<dyn ObligationLedger>,
    delivered: AtomicUsize,
}

#[async_trait::async_trait]
impl ObligationRelay for PulseRelay {
    fn ledger(&self) -> &dyn ObligationLedger {
        self.ledger.as_ref()
    }
    async fn deliver(&self, _: ObligationDelivery<'_>) -> Result<(), DeliveryFailure> {
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct DrainPort(AtomicUsize);

#[async_trait::async_trait]
impl lash_core::ProcessWorkSubstrate for DrainPort {
    async fn deliver_process_start(&self, _: &lash_core::ProcessRecord) -> Result<(), PluginError> {
        panic!("only drain uses this port")
    }
    async fn await_process_terminal(
        &self,
        _: &ProcessId,
    ) -> Result<lash_core::ProcessTerminalWait, PluginError> {
        panic!("only drain uses this port")
    }
    async fn deliver_cancel(
        &self,
        _: &ProcessId,
        _: &lash_core::CancelRequest,
        _: &str,
    ) -> Result<(), PluginError> {
        panic!("only drain uses this port")
    }
    async fn publish_process_terminal(
        &self,
        _: &ProcessId,
        _: &ProcessAwaitOutput,
        _: &str,
    ) -> Result<(), PluginError> {
        panic!("only drain uses this port")
    }
    async fn deliver_hand_over(
        &self,
        _: &ProcessId,
        _: &BuildGeneration,
    ) -> Result<(), PluginError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct RecoveryDriver {
    stores: Arc<dyn StoreSet>,
    engine: RestateEngine,
    lanes: RelayLanes,
    relay: Arc<PulseRelay>,
    drain: DrainPort,
    generation: BuildGeneration,
    ticks: AtomicUsize,
    tick_starts: Mutex<Vec<std::time::Instant>>,
}

#[async_trait::async_trait]
impl lash_core::SessionDriver for RecoveryDriver {
    async fn reconcile(
        &self,
        cursor: &ReconcileCursor,
        page: NonZeroUsize,
    ) -> Result<ReconcileCursor, lash_core::StoreError> {
        self.tick_starts
            .lock_recover()
            .push(std::time::Instant::now());
        let sessions = self.stores.session_store_factory();
        let registry = self.stores.process_registry();
        let drain = self.stores.generation_drain();
        let clock = self.stores.clock();
        let relays: Vec<Arc<dyn ObligationRelay>> = vec![self.relay.clone()];
        let tick = reconcile_once(
            &ReconcileParts {
                sessions: sessions.as_ref(),
                work: self.engine.session_work_engine().as_ref(),
                scopes: &lash_core::engine::NoScopeClose,
                processes: Some(ReconcileProcesses {
                    registry: registry.as_ref(),
                    port: &self.drain,
                    drain: drain.as_ref(),
                    generation: &self.generation,
                }),
                clock: clock.as_ref(),
                duties: RecoveryDuties::ALL,
                relays: &relays,
                lanes: &self.lanes,
            },
            cursor,
            page,
        )
        .await;
        self.ticks.fetch_add(1, Ordering::SeqCst);
        Ok(tick.next)
    }
    async fn admit(
        &self,
        _: ScopedEffectController<'_>,
        _: &lash_core::engine::DriveRequest,
        _: u32,
        _: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<lash_core::engine::AdmitVerdict, lash_core::engine::DriveAbort> {
        panic!("the recovery schedule admits no drives")
    }
    async fn run_root(
        &self,
        _: ScopedEffectController<'_>,
        _: lash_core::engine::Admitted,
    ) -> lash_core::engine::RootRunEnd {
        panic!("the recovery schedule runs no roots")
    }
    async fn close_root(
        &self,
        _: ScopedEffectController<'_>,
        _: &SessionId,
        _: &TurnId,
    ) -> Result<(), lash_core::engine::DriveAbort> {
        panic!("the recovery schedule closes no roots")
    }
}

async fn seed(stores: &dyn StoreSet, start: usize, count: usize) {
    let registry = stores.process_registry();
    let sessions = stores.session_store_factory();
    for index in start..start + count {
        let record = registry
            .register_process(executed_registration())
            .await
            .expect("process");
        registry
            .set_external_ref(
                &record.id,
                ProcessExternalRef {
                    backend: "restate".into(),
                    id: record.id.to_string(),
                    metadata: None,
                    segment_ordinal: None,
                },
            )
            .await
            .expect("reference");
        let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
            &record.id,
            format!("fixture-{index}"),
        )
        .bind_attempt(1);
        let mut started = authority.invocation_started().expect("started");
        started.build_generation = Some(BuildGeneration::for_test("recovery-old"));
        registry
            .record_first_started_with_authority(&record.id, started, &authority)
            .await
            .expect("start");
        let session = SessionId::from(format!("recovery-{index:06}"));
        sessions
            .admit_session(&lash_core::SessionStoreCreateRequest {
                session_id: session.clone(),
                relation: lash_core::SessionRelation::Root,
                pending_observer_intents: vec![],
                config: lash_core::testing::mock_session_policy().into(),
                head: lash_core::SessionCreationHead::Config,
                owning_process_id: None,
            })
            .await
            .expect("session");
        sessions
            .bind_root_inputs(&session, &TurnId::from("root"), &[])
            .await
            .expect("root");
    }
    stores
        .generation_drain()
        .mark_draining(&BuildGeneration::for_test("recovery-old"), 1)
        .await
        .expect("drain");
}

fn driver(
    stores: Arc<dyn StoreSet>,
    transport: Arc<RecoveryTransport>,
    ingress_url: String,
    admin_url: String,
) -> Arc<RecoveryDriver> {
    let connection = RestateConnection::with_transport(ingress_url, transport.clone());
    let admin = RestateConnection::with_transport(admin_url, transport);
    let generation = BuildGeneration::for_test("recovery-new");
    let engine = RestateEngine::new(
        stores.clone(),
        RestateConfig::new(connection.clone(), admin, test_restate_authority_id())
            .stamped(generation.clone()),
    );
    Arc::new(RecoveryDriver {
        lanes: lash_conformance::deployment_tick_lanes(
            stores.clock(),
            lash_core::engine::RecoveryPassBudget::default(),
        ),
        relay: Arc::new(PulseRelay {
            ledger: stores.obligation_ledger(ObligationKind::ProcessStart),
            delivered: AtomicUsize::new(0),
        }),
        stores,
        engine,
        generation,
        drain: DrainPort::default(),
        ticks: AtomicUsize::new(0),
        tick_starts: Mutex::new(vec![]),
    })
}

fn transport(inner: Arc<dyn HttpTransport>, hold: bool) -> Arc<RecoveryTransport> {
    Arc::new(RecoveryTransport {
        inner,
        pages: Mutex::new(vec![]),
        hold: AtomicBool::new(hold),
        held: tokio::sync::Notify::new(),
    })
}

async fn page_law(stores: Arc<dyn StoreSet>) {
    let server = lash_restate_test::RestateTestServer::new(Default::default()).expect("double");
    let transport = transport(server.transport(), false);
    seed(stores.as_ref(), 0, 5).await;
    let driver = driver(
        stores,
        transport.clone(),
        server.ingress_url().to_string(),
        server.ingress_url().to_string(),
    );
    let sessions = driver.stores.session_store_factory();
    let clock = driver.stores.clock();
    let parks = lash_core::drive::StoreParkRecovery::new(sessions.as_ref(), clock.as_ref());
    let control =
        lash_core::SessionWorkEngine::control(driver.engine.session_work_engine().as_ref());
    let mut cursor = None;
    let mut seen = BTreeMap::<String, Vec<String>>::new();
    for index in 0..4 {
        transport.pages.lock_recover().clear();
        cursor = control
            .reconcile_parks(
                &parks,
                lash_core::engine::EnginePage {
                    after: cursor,
                    limit: NonZeroUsize::new(2).expect("page"),
                    budget: Duration::from_secs(30),
                },
            )
            .await
            .expect("page")
            .next;
        let pages = transport.pages.lock_recover().clone();
        for kind in ["process", "root"] {
            let inspected: usize = pages
                .iter()
                .filter(|(k, _, _)| k == kind)
                .map(|(_, n, _)| n)
                .sum();
            assert!(
                inspected > 0 && inspected <= 2,
                "{kind} inspected {inspected}, page budget 2: {pages:?}"
            );
            let query = pages
                .iter()
                .find(|(k, _, _)| k == kind)
                .expect("catalog page")
                .2
                .clone();
            let history = seen.entry(kind.into()).or_default();
            if index < 3 {
                assert!(
                    !history.contains(&query),
                    "the cursor advanced past healthy and failed rows"
                );
                history.push(query);
            } else {
                assert_eq!(
                    query, history[0],
                    "an exhausted catalog wraps and retries failed rows"
                );
            }
        }
    }
}

async fn growing_law(
    stores: Arc<dyn StoreSet>,
    inner: Arc<dyn HttpTransport>,
    ingress_url: String,
    admin_url: String,
) {
    println!(
        "host load {}",
        std::fs::read_to_string("/proc/loadavg")
            .expect("load")
            .trim()
    );
    seed(stores.as_ref(), 0, 129).await;
    let transport = transport(inner, true);
    let driver = driver(stores.clone(), transport.clone(), ingress_url, admin_url);
    let scheduled: Arc<dyn lash_core::SessionDriver> = driver.clone();
    let schedule = tokio::spawn(crate::session_reconcile::run(
        Arc::downgrade(&scheduled),
        Arc::downgrade(&scheduled),
    ));
    tokio::time::timeout(Duration::from_secs(60), transport.held.notified())
        .await
        .expect("recovery request held");
    let initial = driver.relay.delivered.load(Ordering::SeqCst);
    let appended = Arc::new(AtomicUsize::new(0));
    let producer = {
        let appended = appended.clone();
        let stores = stores.clone();
        tokio::spawn(async move {
            for index in 129..=usize::MAX {
                seed(stores.as_ref(), index, 1).await;
                appended.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
    };
    let progress = tokio::time::timeout(Duration::from_secs(60), async {
        while driver.ticks.load(Ordering::SeqCst) < 2
            || driver.relay.delivered.load(Ordering::SeqCst) <= initial
            || driver.drain.0.load(Ordering::SeqCst) == 0
            || transport.process_query_count() < 2
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    schedule.abort();
    let _ = schedule.await;
    producer.abort();
    let _ = producer.await;
    assert!(
        progress.is_ok(),
        "held recovery starved schedule: ticks {}, drain {}, due {} -> {}, starts {:?}, appended {}",
        driver.ticks.load(Ordering::SeqCst),
        driver.drain.0.load(Ordering::SeqCst),
        initial,
        driver.relay.delivered.load(Ordering::SeqCst),
        driver
            .tick_starts
            .lock_recover()
            .iter()
            .map(std::time::Instant::elapsed)
            .collect::<Vec<_>>(),
        appended.load(Ordering::SeqCst),
    );
    assert!(
        transport
            .pages
            .lock_recover()
            .iter()
            .all(|(_, n, _)| *n <= 64),
        "every catalog query is bounded"
    );
    assert!(
        appended.load(Ordering::SeqCst) > 0,
        "the producer kept growing the catalog"
    );
    let starts = driver.tick_starts.lock_recover();
    let cadence = starts[1].duration_since(starts[0]);
    println!(
        "recovery tick cadence {cadence:?}; appended {}",
        appended.load(Ordering::SeqCst)
    );
    assert!(
        cadence <= lash_core::runtime::drive::RECOVERY_TICK + Duration::from_secs(4),
        "recovery kept its cadence: {cadence:?}"
    );
    let pages = transport.pages.lock_recover();
    let process_queries: Vec<_> = pages
        .iter()
        .filter(|(kind, _, _)| kind == "process")
        .collect();
    assert!(process_queries.len() >= 2);
    assert_ne!(
        process_queries[0].2, process_queries[1].2,
        "the timed-out page did not pin its cursor"
    );
}

#[tokio::test]
async fn lost_run_recovery_inspects_at_most_its_page() {
    page_law(Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite"),
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn growing_lost_run_catalog_does_not_starve_drain_or_due_relays() {
    let server = lash_restate_test::RestateTestServer::new(Default::default()).expect("double");
    growing_law(
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite"),
        ),
        server.transport(),
        server.ingress_url().to_string(),
        server.ingress_url().to_string(),
    )
    .await;
}

#[derive(Debug)]
struct FailedRootTransport {
    inner: Arc<dyn HttpTransport>,
    hold_outcome: AtomicBool,
}

#[async_trait::async_trait]
impl HttpTransport for FailedRootTransport {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let body = if request.url.ends_with("/query") {
            let query: serde_json::Value = serde_json::from_slice(&request.body).expect("query");
            let query = query["query"].as_str().expect("SQL");
            if query.contains("LashTurn") {
                query.split_once("target_service_key IN (").map(|(_, keys)| {
                    let rows: Vec<_> = keys.trim_end_matches(')').split(", ").map(|key| {
                        let key = key.trim_matches('\'');
                        serde_json::json!({
                            "id": format!("failed-{key}"), "target": format!("LashTurn/{key}/run"),
                            "target_service_name": "LashTurn", "target_service_key": key,
                            "target_handler_name": "run", "status": "completed", "completion_result": "failure",
                        })
                    }).collect();
                    serde_json::json!({"rows": rows})
                })
            } else {
                None
            }
        } else if request.url.ends_with("/outcome") {
            if self.hold_outcome.swap(false, Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Some(
                serde_json::to_value(crate::Reply::at(
                    crate::compat::RESTATE_WIRE_VERSION,
                    None::<lash_core::engine::RootOutcome>,
                ))
                .expect("outcome"),
            )
        } else {
            None
        };
        if let Some(body) = body {
            return Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: HttpResponseBody::buffered(serde_json::to_vec(&body).expect("response")),
            });
        }
        self.inner.send(request, timeout).await
    }
}

#[tokio::test]
async fn a_timed_out_root_outcome_keeps_its_failure_and_advances_its_page() {
    println!(
        "host load {}",
        std::fs::read_to_string("/proc/loadavg")
            .expect("load")
            .trim()
    );
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite"),
    );
    seed(stores.as_ref(), 0, 5).await;
    let server = lash_restate_test::RestateTestServer::new(Default::default()).expect("double");
    let transport = transport(
        Arc::new(FailedRootTransport {
            inner: server.transport(),
            hold_outcome: AtomicBool::new(true),
        }),
        false,
    );
    let driver = driver(
        stores.clone(),
        transport,
        server.ingress_url().to_string(),
        server.ingress_url().to_string(),
    );
    let sessions = stores.session_store_factory();
    let clock = stores.clock();
    let parks = lash_core::drive::StoreParkRecovery::new(sessions.as_ref(), clock.as_ref());
    let control =
        lash_core::SessionWorkEngine::control(driver.engine.session_work_engine().as_ref());
    let page = || lash_core::engine::EnginePage {
        after: None,
        limit: NonZeroUsize::new(2).expect("page"),
        budget: Duration::from_millis(200),
    };
    let first = control
        .reconcile_parks(&parks, page())
        .await
        .expect("first page");
    let first_key = crate::session_driver::turn_workflow_key(
        &SessionId::from("recovery-000000"),
        &TurnId::from("root"),
    );
    assert!(
        first
            .failed
            .iter()
            .any(|(id, failure)| id.as_str() == first_key
                && failure.contains(&format!("failed-{first_key}"))
                && failure.contains("time budget exhausted")),
        "the timed-out item's failure survived the page deadline: {first:?}"
    );
    let second = control
        .reconcile_parks(&parks, page())
        .await
        .expect("next page");
    assert_eq!(
        second.ended_roots.len(),
        2,
        "later roots still settle: {second:?}"
    );
    assert!(
        second
            .ended_roots
            .iter()
            .all(|root| root.session.as_str() >= "recovery-000002"),
        "the failed page advanced: {second:?}"
    );
}

async fn postgres_stores() -> (sqlx::PgPool, String, Arc<dyn StoreSet>, tempfile::TempDir) {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL service URL");
    let admin = sqlx::PgPool::connect(&url).await.expect("PostgreSQL");
    let schema = format!("recovery_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .expect("isolated schema");
    let options: sqlx::postgres::PgConnectOptions = url.parse().expect("connection options");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(lash_postgres_store::PostgresStoreConfig::default().max_connections)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .expect("isolated pool");
    sqlx::raw_sql(lash_postgres_store::PostgresStorage::schema_ddl())
        .execute(&pool)
        .await
        .expect("provision schema");
    let storage = lash_postgres_store::PostgresStorage::from_pool(pool)
        .await
        .expect("storage");
    let attachments = tempfile::tempdir().expect("attachments");
    let stores: Arc<dyn StoreSet> = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    (admin, schema, stores, attachments)
}

async fn drop_schema(admin: sqlx::PgPool, schema: String) {
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .expect("drop owned schema");
    admin.close().await;
}

#[tokio::test]
#[ignore = "requires the PostgreSQL service gate"]
async fn postgres_lost_run_recovery_inspects_at_most_its_page() {
    let (admin, schema, stores, _attachments) = postgres_stores().await;
    page_law(stores).await;
    drop_schema(admin, schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the PostgreSQL service gate"]
async fn postgres_growing_lost_run_catalog_does_not_starve_drain_or_due_relays() {
    let (admin, schema, stores, _attachments) = postgres_stores().await;
    let server = lash_restate_test::RestateTestServer::new(Default::default()).expect("double");
    growing_law(
        stores,
        server.transport(),
        server.ingress_url().to_string(),
        server.ingress_url().to_string(),
    )
    .await;
    drop_schema(admin, schema).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the pinned live Restate service gate"]
async fn live_growing_lost_run_catalog_does_not_starve_drain_or_due_relays() {
    growing_law(
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite"),
        ),
        Arc::new(lash_http_transport::ReqwestHttpTransport::new()),
        std::env::var("RESTATE_INGRESS_URL").expect("live ingress"),
        std::env::var("RESTATE_ADMIN_URL").expect("live admin"),
    )
    .await;
}
