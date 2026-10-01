//! FIG-4454's committed-final recovery on the Restate tier: the operators the
//! shared laws take, and the store tiers the harness runs them over.
//!
//! - [`LiveConformanceHarness::child_invocation_expiry`] kills a group
//!   child's open invocation before its seat and purges it, as its
//!   retention's expiry does. It dispatches nothing: the opener's reopen is
//!   what re-sends the child.
//! - [`LiveConformanceHarness::deployment_change`] registers another build,
//!   of another drain generation, as the newest deployment of every lash
//!   service. Its session catalog holds the law's session at a marker below
//!   its admission window: the profile of a build whose session window
//!   excludes the marker the opener's build admits. A child that landed on it
//!   would be refused at the session gate and report its committed final
//!   lost.

// Test harness code: ambient env access is sanctioned here.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use lash_core::store::SessionCatalogStore as _;
use lash_core::store::StoreTestSupport as _;

use super::effect_group_conformance::{HarnessAdmin, LiveConformanceHarness};

/// The store tier a harness's endpoint and a law's runtime run over.
#[derive(Clone, Copy, Debug)]
pub(super) enum HarnessStoreTier {
    SqliteMemory,
    SqliteFile,
    /// An isolated database under `LASH_POSTGRES_DATABASE_URL`.
    Postgres,
}

/// What keeps a file or PostgreSQL tier's substrate alive for the
/// harness's lifetime.
pub(super) struct HarnessTierResources {
    _directory: tempfile::TempDir,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

impl HarnessStoreTier {
    pub(super) async fn open(
        self,
    ) -> (
        Arc<dyn lash_core::StoreSet>,
        Option<lash_sqlite_store::SqliteStoreSet>,
        Option<HarnessTierResources>,
    ) {
        match self {
            Self::SqliteMemory => {
                let stores = lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("open the endpoint's SQLite memory store set");
                (Arc::new(stores.clone()), Some(stores), None)
            }
            Self::SqliteFile => {
                let directory = tempfile::tempdir().expect("the SQLite file tier's directory");
                let stores =
                    lash_sqlite_store::SqliteStoreSet::open(directory.path().join("sqlite"))
                        .await
                        .expect("open the endpoint's SQLite file store set");
                (
                    Arc::new(stores),
                    None,
                    Some(HarnessTierResources {
                        _directory: directory,
                        _database: None,
                    }),
                )
            }
            Self::Postgres => {
                let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                    .expect("the PostgreSQL tier needs LASH_POSTGRES_DATABASE_URL");
                let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
                let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                    .await
                    .expect("connect the PostgreSQL tier");
                let directory = tempfile::tempdir().expect("the PostgreSQL tier's attachments");
                let stores = lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                        directory.path(),
                    )),
                );
                (
                    Arc::new(stores),
                    None,
                    Some(HarnessTierResources {
                        _directory: directory,
                        _database: Some(database),
                    }),
                )
            }
        }
    }
}

/// The generation of the build a deployment change registers.
const NEWER_BUILD: &str = "effect-group-conformance-newer";

impl LiveConformanceHarness {
    /// The retention sweep over one group child (ADR 0099 §8): the child's
    /// open invocation is killed, as an operator kills it, before its seat,
    /// and purged, as its retention's expiry purges it. Nothing is sent for
    /// the child.
    pub(super) fn child_invocation_expiry(&self) -> lash_conformance::ChildInvocationExpiry {
        let admin = self.harness_admin().clone();
        Arc::new(move |group_key: String, position: usize| {
            let admin = admin.clone();
            Box::pin(async move {
                let id = open_child_invocation(&admin, &group_key, position).await;
                match &admin {
                    HarnessAdmin::InProcess { server } => assert_eq!(
                        server.kill_and_await(&id).await,
                        Some(true),
                        "kill the open invocation of group {group_key}'s child {position}"
                    ),
                    HarnessAdmin::Live { admin_url } => {
                        crate::RestateAdminClient::new(crate::RestateConnection::new(
                            admin_url.clone(),
                        ))
                        .kill_invocation(&crate::RestateInvocationId::new(id.clone()))
                        .await
                        .unwrap_or_else(|error| panic!("kill `{id}`: {error}"));
                    }
                }
                admin.purge_invocation(&id).await;
            })
        })
    }

    /// Registers another build as the newest deployment, over the same
    /// stores, whose session catalog holds the law's session at a marker
    /// below its admission window (FIG-4454).
    pub(super) fn deployment_change(&self) -> lash_conformance::DeploymentChange {
        let admin = self.harness_admin().clone();
        let make_endpoint = self.another_build_maker();
        Arc::new(move |session: lash_core::SessionId| {
            let admin = admin.clone();
            let make_endpoint = Arc::clone(&make_endpoint);
            Box::pin(async move {
                let endpoint = make_endpoint(refusing_catalog(&session).await);
                match &admin {
                    HarnessAdmin::InProcess { server } => {
                        server
                            .register(endpoint)
                            .await
                            .expect("register the newer build on the server double");
                    }
                    HarnessAdmin::Live { admin_url } => {
                        serve_newer_build_live(admin_url, endpoint).await;
                    }
                }
            })
        })
    }
}

/// The id of group `group_key`'s child `position`'s open invocation.
async fn open_child_invocation(admin: &HarnessAdmin, group_key: &str, position: usize) -> String {
    match admin {
        HarnessAdmin::InProcess { server } => server
            .invocations()
            .into_iter()
            .filter(|view| view.status != "completed")
            .find_map(|view| {
                let (_service, rest) = view.target.split_once('/')?;
                let (key, handler) = rest.rsplit_once('/')?;
                if key != group_key || handler != "child" {
                    return None;
                }
                let input = server.journal(&view.id)?.first()?.input()?;
                let request: serde_json::Value = serde_json::from_slice(&input).ok()?;
                (request["body"]["position"].as_u64() == Some(position as u64))
                    .then(|| view.id.clone())
            })
            .unwrap_or_else(|| {
                panic!("an open invocation of group {group_key}'s child {position}")
            }),
        HarnessAdmin::Live { admin_url } => {
            #[derive(serde::Deserialize)]
            struct Row {
                id: String,
            }
            // The recovery laws run one child per group, so the group's one
            // open child invocation is the position's.
            let query = format!(
                "SELECT id FROM sys_invocation WHERE target_service_key = {} \
                 AND target_handler_name = 'child' AND status != 'completed'",
                crate::ingress::sql_string_literal(group_key)
            );
            let rows: Vec<Row> =
                crate::RestateAdminClient::new(crate::RestateConnection::new(admin_url.clone()))
                    .query_json(&query)
                    .await
                    .unwrap_or_else(|error| panic!("find group {group_key}'s child: {error}"));
            assert_eq!(
                rows.len(),
                1,
                "group {group_key} has one open child invocation, position {position}"
            );
            rows.into_iter()
                .next()
                .map(|row| row.id)
                .unwrap_or_default()
        }
    }
}

/// A session catalog holding `session` at the marker below this build's
/// oldest admitted one.
async fn refusing_catalog(session: &lash_core::SessionId) -> Arc<dyn lash_core::DeploymentStore> {
    // The catalog outlives the operator: the newer build reads it for as long
    // as it serves.
    let directory = Box::leak(Box::new(
        tempfile::tempdir().expect("the newer build's catalog directory"),
    ));
    let store = Arc::new(
        lash_sqlite_store::SqliteStore::open(&directory.path().join("session.db"))
            .await
            .expect("open the newer build's session catalog"),
    );
    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            session,
        ))
        .await
        .expect("admit the law's session in the newer build's catalog");
    store
        .stamp_session_state_version_for_testing(
            session,
            lash_core::OLDEST_SUPPORTED_SESSION_STATE_VERSION - 1,
        )
        .await
        .expect("hold the session at a marker the newer build refuses");
    assert!(
        matches!(
            lash_core::admit_session_state_generation(store.as_ref(), session).await,
            Err(lash_core::StoreError::SessionStateVersionUnsupported { .. })
        ),
        "the newer build's session admission refuses the law's session"
    );
    store
}

/// Serves the newer build on the second endpoint the live recipe provides
/// and registers it as the newest deployment.
async fn serve_newer_build_live(admin_url: &str, endpoint: restate_sdk::endpoint::Endpoint) {
    let bind_addr = std::env::var("EG0_RESTATE_ENDPOINT_BIND")
        .expect("EG0_RESTATE_ENDPOINT_BIND must be set by `just effect-group-conformance-e2e`")
        .parse::<std::net::SocketAddr>()
        .expect("valid EG0_RESTATE_ENDPOINT_BIND");
    let endpoint_url = std::env::var("EG0_RESTATE_ENDPOINT_URL")
        .expect("EG0_RESTATE_ENDPOINT_URL must be set by `just effect-group-conformance-e2e`");
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("bind the newer build's endpoint");
    // The newer build serves for the rest of the law's process.
    tokio::spawn(crate::serve_endpoint(
        listener,
        endpoint,
        crate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
        std::future::pending::<()>(),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(bind_addr).await.is_err() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the newer build's endpoint did not open at {bind_addr}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let response = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build Restate admin client")
        .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "uri": endpoint_url, "force": true, "breaking": true }))
        .send()
        .await
        .expect("register the newer build");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "registering the newer build failed: {status} {body}"
    );
}

/// The newer build's generation.
pub(super) fn newer_build() -> lash_core::engine::BuildGeneration {
    lash_core::engine::BuildGeneration::for_test(NEWER_BUILD)
}

/// The drain status of `generation` over `stores`.
async fn drain_status(
    stores: &Arc<dyn lash_core::StoreSet>,
    registry: &crate::RestateDeploymentRegistry,
    generation: &lash_core::engine::BuildGeneration,
) -> lash_core::store::generation_drain::GenerationDrainStatus {
    let ledgers = Arc::clone(stores);
    lash_core::store::generation_drain::GenerationDrainStatus::collect(
        stores.generation_drain().as_ref(),
        stores.session_delete_ledger().as_ref(),
        move |kind| ledgers.obligation_ledger(kind),
        registry,
        generation,
        1_000,
    )
    .await
    .expect("read the generation's drain status")
}

/// FIG-4454 retirement coverage: a generation whose lane holds a group with
/// a committed child whose seat is owed is not drained, whoever opened the
/// group — the group index's committed entry is the obligation of record, and
/// a host-built opener has no root the store counts. Once the child seats,
/// the generation drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generation_holds_its_drain_while_a_committed_group_child_owes_its_seat() {
    let harness = LiveConformanceHarness::start_on(
        super::effect_group_conformance::HarnessServer::in_process(),
    )
    .await;
    let generation = lash_core::engine::BuildGeneration::for_test(
        super::effect_group_conformance::HARNESS_BUILD,
    );
    let stores = harness.law_stores();
    let registry = crate::RestateDeploymentRegistry::new(harness.admin_client());
    stores
        .generation_drain()
        .mark_draining(&generation, 1)
        .await
        .expect("mark the harness's generation draining");
    assert!(
        drain_status(&stores, &registry, &generation)
            .await
            .drained(),
        "a generation holding nothing drains"
    );
    let group =
        super::effect_group_rank_reservation::Group::open(harness.ingress(), "retirement", 2).await;
    group.make_ready().await;
    group.commit(0).await;
    let held = drain_status(&stores, &registry, &generation).await;
    assert_eq!(held.undrained_group_children, 1, "{held:?}");
    assert!(
        !held.drained(),
        "a committed child whose seat is owed holds its lane's generation"
    );
    group.seat(0).await;
    assert!(
        drain_status(&stores, &registry, &generation)
            .await
            .drained(),
        "the generation drains once the committed child seats"
    );
}

async fn directory_entries(harness: &LiveConformanceHarness) -> Vec<serde_json::Value> {
    let generation = super::test_build_generation();
    harness.admin_client().query_json(&format!(
        "SELECT service_name, key, value_utf8 FROM state WHERE scope IS NULL AND service_key = '{}' AND (service_name = 'EffectGroupDrainIndex' OR service_name LIKE '%.EffectGroupDrainIndex') AND key != '_compat'",
        generation.as_str(),
    )).await.expect("read the generation directory")
}

async fn full_scan_count(harness: &LiveConformanceHarness) -> u64 {
    let rows: Vec<serde_json::Value> = harness.admin_client().query_json(
        "SELECT service_name, service_key, value_utf8 FROM state WHERE key = 'effect-group/v1/state'",
    ).await.expect("read the original full scan");
    rows.into_iter()
        .map(|row| {
            crate::effect_group::undrained_children_on(
                row["service_key"].as_str().expect("group key"),
                serde_json::from_str(row["value_utf8"].as_str().expect("record"))
                    .expect("record JSON"),
                &super::test_build_generation(),
            )
            .expect("decode the original record")
        })
        .sum()
}

async fn indexed_count(harness: &LiveConformanceHarness) -> u64 {
    use lash_core_store::store::fleet_finalize::DeploymentRegistry as _;
    crate::RestateDeploymentRegistry::new(harness.admin_client())
        .undrained_group_children(&super::test_build_generation())
        .await
        .expect("read retirement evidence")
}

async fn kill_at_drain_cut(cut: &'static str) {
    let harness = LiveConformanceHarness::start_on(
        super::effect_group_conformance::HarnessServer::in_process(),
    )
    .await;
    let server = harness.server_double().expect("server double");
    let group = Arc::new(
        super::effect_group_rank_reservation::Group::open(harness.ingress(), cut, 1).await,
    );
    group.make_ready().await;
    if cut == "after_seat" {
        group.commit(0).await;
    }
    let (reached, _release) = crate::effect_group::drain_cut::arm(&group.key, cut);
    let running = Arc::clone(&group);
    let task = tokio::spawn(async move {
        if cut == "before_commit" {
            let envelope = super::effect_group_conformance::witness_child(&running.key, 0);
            running
                .ingress
                .call_lash_object::<_, crate::effect_group::EffectGroupCommitChildResponse>(
                    "EffectGroupIndex",
                    &running.key,
                    "commit_child",
                    &crate::effect_group::EffectGroupCommitChildRequest {
                        replay_key: envelope.invocation.effect_replay_key().to_owned(),
                        committed: crate::effect_group::EffectGroupCommittedFinal::Held,
                    },
                )
                .await
                .map(|_| ())
        } else {
            running
                .ingress
                .call_lash_object::<_, crate::effect_group::EffectGroupRecordSettlementResponse>(
                    "EffectGroupIndex",
                    &running.key,
                    "record_settlement",
                    &crate::effect_group::EffectGroupRecordSettlementRequest {
                        position: 0,
                        terminal: crate::effect_group::EffectGroupSettlementTerminal::Cancelled,
                    },
                )
                .await
                .map(|_| ())
        }
    });
    tokio::time::timeout(Duration::from_secs(10), reached.notified())
        .await
        .expect("reach the exact kill window");
    assert_eq!(
        directory_entries(&harness).await.len(),
        1,
        "the generation directory must cover the commitment before this kill window"
    );
    let handler = if cut == "before_commit" {
        "commit_child"
    } else {
        "record_settlement"
    };
    let target = format!("EffectGroupIndex/{}/{handler}", group.key);
    let invocation = server
        .invocations()
        .into_iter()
        .find(|v| v.target == target && v.status != "completed")
        .expect("the stopped group invocation");
    assert_eq!(server.kill_and_await(&invocation.id).await, Some(true));
    assert!(
        task.await
            .expect("the killed ingress caller terminates")
            .is_err()
    );
    assert_eq!(
        indexed_count(&harness).await,
        full_scan_count(&harness).await
    );
    assert_eq!(
        indexed_count(&harness).await,
        0,
        "an orphan or seated child holds no drain"
    );
    assert_eq!(
        directory_entries(&harness).await.len(),
        1,
        "derived entries persist; only the group record decides its drain"
    );
    group.commit(0).await;
    assert_eq!(
        indexed_count(&harness).await,
        full_scan_count(&harness).await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_directory_survives_a_kill_between_registration_and_commit() {
    kill_at_drain_cut("before_commit").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_directory_survives_a_kill_after_the_seat() {
    kill_at_drain_cut("after_seat").await;
}

#[derive(Debug)]
struct CountPollReads {
    inner: Arc<dyn lash_http_transport::HttpTransport>,
    queries: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl lash_http_transport::HttpTransport for CountPollReads {
    async fn send(
        &self,
        request: lash_http_transport::HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<lash_http_transport::HttpResponse, lash_http_transport::LlmTransportError> {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("SQL request");
        self.queries
            .lock()
            .expect("query log")
            .push(body["query"].as_str().expect("SQL").to_owned());
        self.inner.send(request, timeout).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drain_directory_poll_reads_only_listed_groups() {
    use lash_core_store::store::fleet_finalize::DeploymentRegistry as _;
    let harness = LiveConformanceHarness::start_on(
        super::effect_group_conformance::HarnessServer::in_process(),
    )
    .await;
    let server = harness.server_double().expect("server double");
    let group =
        super::effect_group_rank_reservation::Group::open(harness.ingress(), "count-poll", 1).await;
    group.make_ready().await;
    group.commit(0).await;
    let mut other = server.object_state("EffectGroupIndex", &group.key);
    let raw = other
        .get_mut("effect-group/v1/state")
        .expect("group record");
    let mut record: serde_json::Value = serde_json::from_slice(raw).expect("record JSON");
    record["body"]["dispatch_route"] =
        serde_json::json!(format!("EffectGroupDispatch_g{}", newer_build().as_str()));
    *raw = serde_json::to_vec(&record).expect("other-generation record");
    for ordinal in 0..1024 {
        server.set_object_state(
            "EffectGroupIndex",
            &format!("other-{ordinal}"),
            other.clone(),
        );
    }
    for ordinal in 0..32 {
        let drained = super::effect_group_rank_reservation::Group::open(
            harness.ingress(),
            &format!("drained-{ordinal}"),
            1,
        )
        .await;
        drained.make_ready().await;
        drained.commit(0).await;
        drained.seat(0).await;
    }
    assert_eq!(
        directory_entries(&harness).await.len(),
        33,
        "this generation retains one directory entry per group, including drained groups"
    );
    let transport = Arc::new(CountPollReads {
        inner: server.transport(),
        queries: Default::default(),
    });
    let registry = crate::RestateDeploymentRegistry::new(crate::RestateAdminClient::new(
        crate::RestateConnection::with_transport(server.ingress_url(), transport.clone()),
    ));
    assert_eq!(
        registry
            .undrained_group_children(&super::test_build_generation())
            .await
            .expect("poll"),
        1
    );
    {
        let queries = transport.queries.lock().expect("query log");
        assert_eq!(
            queries.len(),
            34,
            "one directory read and 33 own-generation group reads, independent of 1024 other-generation groups"
        );
        assert!(
            queries
                .iter()
                .all(|query| query.contains("scope IS NULL AND service_key = ")),
            "each read must select a single partition key: {queries:?}"
        );
        assert!(
            queries.iter().all(|query| !query.contains("other-")),
            "no other-generation group is loaded"
        );
    }
    group.seat(0).await;
    transport.queries.lock().expect("query log").clear();
    assert_eq!(
        registry
            .undrained_group_children(&super::test_build_generation())
            .await
            .expect("empty poll"),
        0
    );
    assert_eq!(
        transport.queries.lock().expect("query log").len(),
        34,
        "drained groups remain listed, but other generations never enter the poll"
    );
}
