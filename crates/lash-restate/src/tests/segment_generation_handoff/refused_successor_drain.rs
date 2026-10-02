//! FIG-4750: the drain re-sends a process successor the newest build refused
//! to its sender's generation lane.
//!
//! Build N runs a process's first segments. Build N+1 registers while
//! segment 1 runs and cannot run the process's program, so it refuses
//! segment 2: the process parks `RetiredGeneration` for N and the stable
//! invocation ends. An operator marks N draining, and the laws run the
//! recovery leader's drain pass ([`lash_core::shift::drain_hand_over_slot`])
//! over the deployment's real process port:
//!
//! - **completes**: the pass re-sends segment 2 to `LashProcessWorkflow_g<N>`,
//!   build N runs it once and the process ends there; generation N then reads
//!   drained and passes the finalize precondition. While the park stood,
//!   finalize was refused.
//! - **recovers**: a leader that died after its send, and builds that died at
//!   the refusal's step and at the re-sent segment's start, change nothing:
//!   the next leader's pass finds the park or the started segment, and the
//!   segment still runs once.
//! - **stays routed**: the re-sent segment records its lane as its route, so
//!   a later drain pass does not hand it over again, the lost-run pass does
//!   not end it on the stable refusal's failed run, and a cancel reaches it.
//! - **gone**: with no deployment serving the generation's lane the re-send
//!   is refused typed, and the process keeps its park and its handover.
//!
//! The laws run on the server double over SQLite and PostgreSQL, and the
//! first two on a live `restate-server` (the `refused-successor-drain` suite
//! of `scripts/restate-suites.toml`), where the two builds are two endpoints
//! of one test process.

use super::*;

use lash_core::TestProcessRegistryWriteExt as _;
use lash_core::store::fleet_finalize::{FinalizeError, FinalizeRefusal, NoDeployments};
use lash_core::store::generation_drain::GenerationDrainStatus;
use lash_restate_test::CrashPoint;

/// How long a law waits on a step that only a wedge delays.
const WEDGE: Duration = Duration::from_secs(60);

/// What a law's stores need to outlive it.
struct Fixture {
    stores: Arc<dyn lash_core::StoreSet>,
    _directory: tempfile::TempDir,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

impl Fixture {
    async fn sqlite() -> Self {
        Self {
            stores: Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory()
                    .await
                    .expect("SQLite store set"),
            ),
            _directory: tempfile::tempdir().expect("fixture directory"),
            _database: None,
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "service fixture reads the isolated PostgreSQL gate configuration"
    )]
    async fn postgres() -> Self {
        let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL gate URL");
        let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
        let storage = lash_postgres_store::PostgresStorage::connect(database.url())
            .await
            .expect("PostgreSQL storage");
        let directory = tempfile::tempdir().expect("attachment directory");
        Self {
            stores: Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    directory.path().join("attachments"),
                )),
            )),
            _directory: directory,
            _database: Some(database),
        }
    }
}

/// A live deployment's endpoint: its serving task and the handle that ends
/// it.
struct Served {
    shutdown: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

/// The engine the two builds serve.
enum Server {
    Double(RestateTestServer),
    /// A live `restate-server`: both endpoints serve from the start, and a
    /// build joins the roll when the admin API registers its URL.
    Live {
        served: Vec<Served>,
    },
}

#[expect(
    clippy::disallowed_methods,
    reason = "the live laws read the suite's server and endpoint addresses"
)]
fn suite_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment sets {name}"))
}

#[expect(
    clippy::disallowed_methods,
    reason = "the live laws read which leg of the suite runs them"
)]
fn suite_leg_replays() -> bool {
    std::env::var("LASH_RESTATE_SUITE_LEG").is_ok_and(|leg| leg == "replay")
}

/// Builds N and N+1 over one engine and one store set. N is registered;
/// N+1, which runs another program, registers from segment 1's runner.
struct World {
    server: Server,
    connection: RestateConnection,
    admin: crate::RestateAdminClient,
    ingress: RestateIngressClient,
    stores: Arc<dyn lash_core::StoreSet>,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    runner_n: Arc<BuildRunner>,
    log: SegmentLog,
    register_next: Mutex<Option<BoxFuture>>,
    /// Whether every await suspends and replays: the live suite's replay
    /// leg.
    replays: bool,
    _fixture: Fixture,
}

async fn serve_live(endpoint: Endpoint, bind: &str) -> Served {
    let listener = tokio::net::TcpListener::bind(suite_env(bind))
        .await
        .expect("bind the live endpoint");
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
        crate::serve_endpoint(
            listener,
            endpoint,
            crate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
            async {
                let _ = stopped.await;
            },
        )
        .await;
    });
    Served { shutdown, serving }
}

impl World {
    async fn start(fixture: Fixture, live: bool, ends_on_cancel: bool) -> Self {
        let double = (!live).then(|| {
            RestateTestServer::new(ServerConfig::default().with_seed(seed()))
                .expect("start the server double")
        });
        let (connection, admin) = match &double {
            Some(server) => {
                let connection =
                    RestateConnection::with_transport(server.ingress_url(), server.transport());
                let admin = crate::RestateAdminClient::new(connection.clone());
                (connection, admin)
            }
            None => (
                RestateConnection::new(suite_env("RESTATE_INGRESS_URL")),
                crate::RestateAdminClient::new(RestateConnection::new(suite_env(
                    "RESTATE_ADMIN_URL",
                ))),
            ),
        };
        let ingress = RestateIngressClient::new(connection.clone());
        let stores = Arc::clone(&fixture.stores);
        let registry = stores.process_registry();
        let continuations = stores.process_continuations();
        let sessions = stores.session_store_factory();
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        host.bind_usage_accounting(stores.usage_accounting());
        let log = SegmentLog::default();
        let runner_n = Arc::new(BuildRunner::new(
            "N",
            PROGRAM,
            ends_on_cancel,
            Arc::clone(&log),
        ));
        let runner_next = Arc::new(BuildRunner::new(
            "N+1",
            NEXT_PROGRAM,
            ends_on_cancel,
            Arc::clone(&log),
        ));
        let endpoint = |runner: Arc<BuildRunner>, build: &'static str| {
            crate::services::bind_lash_services(
                Endpoint::builder(),
                crate::services::LashServiceParts {
                    effect_host: &host,
                    ingress: ingress.clone(),
                    admin: admin.clone(),
                    attachments: Arc::clone(&sessions) as Arc<dyn lash_core::AttachmentReferrers>,
                    sessions: Arc::clone(&sessions),
                    process_workflow: LashProcessWorkflowImpl::new(
                        runner,
                        Arc::clone(&registry),
                        Arc::clone(&continuations),
                        ingress.clone(),
                        Arc::new(lash_core::attachments::NoopAttachmentReferrers),
                        test_restate_authority_id(),
                        generation(build),
                        &crate::services::DEFAULT_NAMESPACE,
                    ),
                    session_shifts: crate::RestateSessionShiftsSlot::new(),
                    build_generation: generation(build),
                    namespace: crate::RestateNamespace::default(),
                    fleet: crate::object_state::FleetView::default(),
                },
            )
            .build()
        };
        let endpoint_n = endpoint(Arc::clone(&runner_n), "N");
        let endpoint_next = endpoint(runner_next, "N+1");
        let (server, register_next): (Server, BoxFuture) = match double {
            Some(server) => {
                server
                    .register_with(endpoint_n, "build-N", DeploymentHooks::default())
                    .await
                    .expect("register build N");
                let registering = server.clone();
                (
                    Server::Double(server),
                    Box::pin(async move {
                        registering
                            .register_with(endpoint_next, "build-N+1", DeploymentHooks::default())
                            .await
                            .expect("register build N+1");
                    }),
                )
            }
            None => {
                let served = vec![
                    serve_live(endpoint_n, "RSD_A_BIND").await,
                    serve_live(endpoint_next, "RSD_B_BIND").await,
                ];
                // Forced: a law before this one on the suite's server left
                // N+1 the newest registration of the stable names.
                admin
                    .register_deployment(&suite_env("RSD_A_URL"), true)
                    .await
                    .expect("register build N on the live server");
                let registering = admin.clone();
                (
                    Server::Live { served },
                    Box::pin(async move {
                        registering
                            .register_deployment(&suite_env("RSD_B_URL"), true)
                            .await
                            .expect("register build N+1 on the live server");
                    }),
                )
            }
        };
        Self {
            server,
            connection,
            admin,
            ingress,
            stores,
            registry,
            continuations,
            runner_n,
            log,
            register_next: Mutex::new(Some(register_next)),
            replays: live && suite_leg_replays(),
            _fixture: fixture,
        }
    }

    fn double(&self) -> Option<&RestateTestServer> {
        match &self.server {
            Server::Double(server) => Some(server),
            Server::Live { .. } => None,
        }
    }

    /// The lane only build N serves.
    fn lane(&self) -> String {
        crate::services::DEFAULT_NAMESPACE
            .generation(crate::LashService::ProcessWorkflow, generation("N"))
            .name()
            .into_owned()
    }

    async fn record(&self, process_id: &ProcessId) -> lash_core::ProcessRecord {
        self.registry
            .get_process(process_id)
            .await
            .expect("read the process")
            .expect("the process exists")
    }

    fn runs_of(&self, ordinal: u64) -> Vec<SegmentRun> {
        self.log
            .lock_recover()
            .iter()
            .filter(|run| run.ordinal == ordinal)
            .cloned()
            .collect()
    }

    /// Register a process, arm an awaiter of its terminal on the stable
    /// root, and send its segment 0: N runs segments 0 and 1, N+1 registers
    /// inside segment 1's runner, and segment 2 goes to the stable lane.
    async fn start_process(
        &self,
    ) -> (
        ProcessId,
        tokio::task::JoinHandle<Result<ProcessAwaitOutput, String>>,
    ) {
        let process_id = self
            .registry
            .register_process(executed_registration())
            .await
            .expect("register the process")
            .id;
        let awaiter = {
            let ingress = self.ingress.clone();
            let request = RestateProcessAwaitRequest {
                process_id: process_id.clone(),
            };
            let key = process_id.to_string();
            tokio::spawn(async move {
                ingress
                    .call_lash_workflow::<_, ProcessAwaitOutput>(
                        PROCESS_WORKFLOW,
                        &key,
                        "await_terminal",
                        &request,
                    )
                    .await
                    .map_err(|error| error.to_string())
            })
        };
        let register = self
            .register_next
            .lock_recover()
            .take()
            .expect("a world rolls once");
        self.runner_n.on_segment(HANDING_OVER, register);
        self.ingress
            .send_lash_workflow(
                PROCESS_WORKFLOW,
                &process_segment_workflow_key(&process_id, 0),
                "run",
                &RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration: executed_registration(),
                    execution_context: ProcessExecutionContext::default(),
                    segment_ordinal: 0,
                    sender_generation: generation("N"),
                }),
            )
            .await
            .expect("send segment 0");
        (process_id, awaiter)
    }

    /// Wait until N+1 refused the successor: the process is parked
    /// `RetiredGeneration` for N and the stable invocation ended.
    async fn refused(&self, process_id: &ProcessId) -> lash_core::store::ProcessPark {
        let deadline = tokio::time::Instant::now() + WEDGE;
        loop {
            let record = self.record(process_id).await;
            let ended =
                self.successor_runs(process_id).await.iter().any(|run| {
                    run.target_service_name == PROCESS_WORKFLOW && !run.is_still_active()
                });
            if let Some(park) = record.park().filter(|_| ended) {
                assert!(
                    matches!(
                        park.reason,
                        lash_core::store::ParkReason::RetiredGeneration { .. }
                    ),
                    "the refused successor parked RetiredGeneration: {park:?}"
                );
                assert_eq!(
                    park.build_generation,
                    Some(generation("N")),
                    "the park carries the sender's generation"
                );
                return park.clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "build N+1 never refused the successor: {:?}",
                self.record(process_id).await
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The `run` invocations of the successor's workflow key, on every lane.
    async fn successor_runs(&self, process_id: &ProcessId) -> Vec<crate::RestateInvocationStatus> {
        self.admin
            .segment_runs(
                &crate::services::DEFAULT_NAMESPACE,
                &[process_segment_workflow_key(process_id, SUCCESSOR)],
            )
            .await
            .expect("read the successor's runs")
    }

    async fn mark_draining(&self) {
        self.stores
            .generation_drain()
            .mark_draining(&generation("N"), 1)
            .await
            .expect("mark build N draining");
    }

    /// One drain pass of a recovery leader on build N+1: a fresh process
    /// port over the deployment's ingress and stores, as a leader that just
    /// took the lease builds it.
    async fn drain_pass(&self) -> lash_core::shift::DrainHandOverPass {
        self.drain_pass_led_by(generation("N+1")).await
    }

    /// [`Self::drain_pass`], with the recovery lease held by a build of
    /// `own`.
    async fn drain_pass_led_by(
        &self,
        own: lash_core::engine::BuildGeneration,
    ) -> lash_core::shift::DrainHandOverPass {
        let port = crate::process::RestateProcessIngressRunner::new(
            self.connection.clone(),
            Arc::clone(&self.registry),
            Arc::clone(&self.continuations),
            lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
        );
        let drain = self.stores.generation_drain();
        lash_core::shift::drain_hand_over_slot(
            &lash_core::shift::ReconcileProcesses {
                registry: self.registry.as_ref(),
                port: &port,
                drain: drain.as_ref(),
                generation: &own,
            },
            lash_core::shift::DrainHandOverCursor::default(),
            std::num::NonZeroUsize::new(16).expect("non-zero"),
        )
        .await
        .expect("the drain pass reads the stores")
    }

    /// Generation N's drain status, over the engine's own deployment
    /// registry.
    async fn drain_status(&self) -> GenerationDrainStatus {
        let ledgers = Arc::clone(&self.stores);
        GenerationDrainStatus::collect(
            self.stores.generation_drain().as_ref(),
            self.stores.session_delete_ledger().as_ref(),
            move |kind| ledgers.obligation_ledger(kind),
            &crate::RestateDeploymentRegistry::new(self.admin.clone()),
            &generation("N"),
            1_000,
        )
        .await
        .expect("read generation N's drain status")
    }

    async fn finish(self) {
        if let Server::Live { served } = self.server {
            for Served { shutdown, serving } in served {
                let _ = shutdown.send(());
                let _ = serving.await;
            }
        }
    }
}

async fn ended(
    awaiter: tokio::task::JoinHandle<Result<ProcessAwaitOutput, String>>,
) -> ProcessAwaitOutput {
    tokio::time::timeout(WEDGE, awaiter)
        .await
        .expect("the re-sent successor never ended the process")
        .expect("the awaiter task")
        .expect("the awaiter is answered")
}

/// What every law holds once the process ended on build N: segment 2 ran
/// once, on N's lane, and generation N reads drained and passes the finalize
/// precondition.
async fn assert_ran_once_on_its_lane_and_drained(world: &World, process_id: &ProcessId) {
    let on_n = SegmentRun {
        build: "N",
        ordinal: SUCCESSOR,
        admitted_by: Some(generation("N")),
    };
    let entries = world.runs_of(SUCCESSOR);
    if world.replays {
        // Under replay the runner is entered again each time the invocation
        // resumes: every entry is the one start N admitted, and the single
        // lane invocation below is what counts the run.
        assert!(
            !entries.is_empty() && entries.iter().all(|entry| *entry == on_n),
            "the re-sent successor runs on build N alone: {entries:?}"
        );
    } else {
        assert_eq!(
            entries,
            vec![on_n],
            "the re-sent successor runs once, on build N"
        );
    }
    let lane = world.lane();
    let runs = world.successor_runs(process_id).await;
    let on_lane: Vec<_> = runs
        .iter()
        .filter(|run| run.target_service_name == lane)
        .collect();
    assert_eq!(on_lane.len(), 1, "one re-sent invocation: {runs:?}");
    let stable: Vec<_> = runs
        .iter()
        .filter(|run| run.target_service_name == PROCESS_WORKFLOW)
        .collect();
    assert_eq!(stable.len(), 1, "one refused stable invocation: {runs:?}");
    assert!(
        stable[0].completed_with_failure(),
        "the refusal ended the stable invocation: {runs:?}"
    );

    // The drain: nothing of the process is left on generation N, so the
    // generation reads drained and finalize's precondition holds once its
    // deployment is retired.
    let deadline = tokio::time::Instant::now() + WEDGE;
    let status = loop {
        let status = world.drain_status().await;
        if status.drained() {
            break status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "generation N never drained: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        (status.live_processes, status.parked_processes),
        (0, 0),
        "{status:?}"
    );
    lash_core::store::fleet_finalize::require_retired(&status, &NoDeployments)
        .await
        .expect("a drained generation whose deployment is retired finalizes");
    let pass = world.drain_pass().await;
    assert_eq!(
        (pass.pass.handled, pass.pass.deferred),
        (0, 0),
        "a drained generation leaves the drain pass nothing"
    );
}

/// A successor the newest build refused runs to completion on its
/// generation's lane, and the generation then drains and finalizes.
async fn a_refused_successor_completes_on_its_lane_and_the_generation_drains(world: World) {
    let (process_id, awaiter) = world.start_process().await;
    let park = world.refused(&process_id).await;
    assert_eq!(park.attempts, 1, "one refusal");
    assert!(
        world.runs_of(SUCCESSOR).is_empty(),
        "zero dispatch into N+1's runner"
    );
    assert!(!awaiter.is_finished(), "the refusal publishes no terminal");

    // The park holds generation N's drain: finalize is refused.
    world.mark_draining().await;
    let held = world.drain_status().await;
    assert_eq!(
        (held.live_processes, held.parked_processes),
        (1, 1),
        "{held:?}"
    );
    assert!(
        matches!(
            lash_core::store::fleet_finalize::require_retired(&held, &NoDeployments).await,
            Err(FinalizeError::Refused(
                FinalizeRefusal::GenerationNotDrained { .. }
            ))
        ),
        "a generation holding a refused successor does not finalize"
    );

    let pass = world.drain_pass().await;
    assert_eq!(
        (pass.pass.handled, pass.pass.deferred),
        (1, 0),
        "the drain re-sent the refused successor"
    );
    assert_eq!(
        ended(awaiter).await,
        process_success(serde_json::json!({ "build": "N" })),
        "the process ended on build N"
    );
    assert_ran_once_on_its_lane_and_drained(&world, &process_id).await;
    world.finish().await;
}

/// A crash between the refusal and the re-send recovers. On the double the
/// refusing build dies before its refusal's step result is journaled and
/// build N dies at the re-sent segment's start. On every engine a leader
/// dies after its send, before it records anything: the next leader's pass
/// meets the segment already started on its lane.
async fn a_crash_between_the_refusal_and_the_re_send_recovers(world: World) {
    let lane = world.lane();
    let (process_id, awaiter) = world.start_process().await;
    let successor_key = process_segment_workflow_key(&process_id, SUCCESSOR);
    if let Some(server) = world.double() {
        server.crash_on(
            CrashRule::new(CrashPoint::BeforeRunResult {
                name: Some("lash.segment.successor-window".to_string()),
            })
            .service(PROCESS_WORKFLOW)
            .handler("run")
            .key(&successor_key)
            .within_attempts(1),
        );
        server.crash_on(
            CrashRule::new(CrashPoint::BeforeRunResult {
                name: Some("lash.segment.start".to_string()),
            })
            .service(&lane)
            .handler("run")
            .key(&successor_key)
            .within_attempts(1),
        );
    }
    // Build N holds the re-sent segment in its runner until the second
    // leader's pass has run.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    world.runner_n.on_segment(SUCCESSOR, {
        let (entered, release) = (Arc::clone(&entered), Arc::clone(&release));
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
        })
    });
    world.refused(&process_id).await;
    world.mark_draining().await;

    // The first leader: its send was accepted, and it died there.
    world
        .ingress
        .send_lash_workflow(
            &lane,
            &successor_key,
            "run",
            &RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                process_id: process_id.clone(),
                registration: executed_registration(),
                execution_context: ProcessExecutionContext::default(),
                segment_ordinal: SUCCESSOR,
                sender_generation: generation("N"),
            }),
        )
        .await
        .expect("the dead leader's send was accepted");

    // The next leader's passes: before and after the segment started.
    let first = world.drain_pass().await;
    assert_eq!(first.pass.deferred, 0, "{first:?}");
    tokio::time::timeout(WEDGE, entered.notified())
        .await
        .expect("build N runs the re-sent successor");
    let second = world.drain_pass().await;
    assert_eq!(
        (second.pass.handled, second.pass.deferred),
        (1, 0),
        "a segment running on its lane is left there: {second:?}"
    );
    let route = world
        .continuations
        .latest_segment_handover(&process_id)
        .await
        .expect("read the handover")
        .expect("the running successor retains its handover")
        .route;
    assert_eq!(route, lane, "the started segment recorded its lane");
    release.notify_one();
    assert_eq!(
        ended(awaiter).await,
        process_success(serde_json::json!({ "build": "N" })),
        "the process ended on build N"
    );
    assert_ran_once_on_its_lane_and_drained(&world, &process_id).await;
    world.finish().await;
}

/// The re-sent segment records its lane as its route: the drain does not
/// hand it over again, the lost-run pass does not end it on the refusal's
/// failed stable run, and a cancel reaches it.
async fn a_re_sent_successor_stays_on_its_recorded_lane(world: World) {
    let lane = world.lane();
    let (process_id, awaiter) = world.start_process().await;
    // The segment's first fact, as a real runner records one: it ends the
    // park, so recovery judges the process by its runs again.
    let entered = Arc::new(tokio::sync::Notify::new());
    world.runner_n.on_segment(SUCCESSOR, {
        let entered = Arc::clone(&entered);
        let registry = Arc::clone(&world.registry);
        let process_id = process_id.clone();
        Box::pin(async move {
            registry
                .set_process_wait(
                    &process_id,
                    lash_core::WaitState {
                        kind: lash_core::WaitKind::Signal {
                            name: "go".to_string(),
                            event_type: lash_core::runtime::process_signal_event_type("go")
                                .expect("valid signal event type"),
                            key: lash_core::runtime::process_signal_wait_key(&process_id, "go", 1),
                            ordinal: 1,
                        },
                        since_ms: 1,
                    },
                )
                .await
                .expect("the re-sent segment waits");
            entered.notify_one();
        })
    });
    world.refused(&process_id).await;
    world.mark_draining().await;
    let pass = world.drain_pass().await;
    assert_eq!((pass.pass.handled, pass.pass.deferred), (1, 0), "{pass:?}");
    tokio::time::timeout(WEDGE, entered.notified())
        .await
        .expect("build N runs the re-sent successor");
    let record = world.record(&process_id).await;
    assert!(
        record.park().is_none(),
        "the segment's first fact ended the park: {record:?}"
    );

    // The drain leaves it on its lane: no wake, no further hand-over. A
    // wake is an ingress send the pass awaits, so one it made is in the
    // double's invocation table once the pass returns.
    let server = world.double().expect("the route law runs on the double");
    let pass = world.drain_pass().await;
    assert_eq!((pass.pass.handled, pass.pass.deferred), (1, 0), "{pass:?}");
    assert!(
        server
            .invocations()
            .iter()
            .all(|view| !view.target.ends_with("/deliver_hand_over")),
        "a segment on its generation's lane is not woken to hand over: {:#?}",
        server.invocations()
    );
    let latest = world
        .continuations
        .latest_segment_handover(&process_id)
        .await
        .expect("read the handover")
        .expect("the running successor retains its handover");
    assert_eq!(
        (latest.segment_ordinal, latest.route.as_str()),
        (SUCCESSOR, lane.as_str()),
        "the successor still carries the process, on its recorded lane"
    );

    // The lost-run pass: the refusal's failed stable run is not this
    // segment's run, so the process is neither ended nor resubmitted.
    let lost = crate::process::park_reconcile::end_lost_process_runs(
        &world.admin,
        &world.ingress,
        &crate::services::DEFAULT_NAMESPACE,
        &world.registry,
        &world.continuations,
        &lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
        crate::session_control::RecoveryScan {
            limit: std::num::NonZeroUsize::new(16).expect("non-zero"),
            after: &mut None,
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        },
    )
    .await
    .expect("the lost-run pass");
    assert!(
        lost.ended.is_empty() && lost.resubmitted.is_empty() && lost.failed.is_empty(),
        "the lost-run pass leaves a segment running on its lane: {lost:?}"
    );
    assert!(
        !world.record(&process_id).await.is_terminal(),
        "the process still runs"
    );

    // A cancel reaches the segment on its lane and ends the process.
    tokio::time::timeout(
        WEDGE,
        world.ingress.call_lash_workflow::<_, ()>(
            PROCESS_WORKFLOW,
            process_id.as_str(),
            "cancel",
            &RestateProcessCancelRequest::new(
                process_id.clone(),
                lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    "actor:fixture:refused-successor-cancel",
                    7,
                ),
            ),
        ),
    )
    .await
    .expect("the cancel is answered")
    .expect("the cancel is accepted");
    let output = ended(awaiter).await;
    assert_eq!(
        output.terminal_status(),
        Some(lash_core::TerminalProcessStatus::Cancelled),
        "the cancel ended the process: {output:?}"
    );
    server.settle().await;
    let deliveries: Vec<String> = server
        .invocations()
        .into_iter()
        .filter(|view| view.target.ends_with("/deliver_cancel"))
        .map(|view| view.target)
        .collect();
    assert_eq!(
        deliveries,
        vec![format!(
            "{lane}/{}/deliver_cancel",
            process_segment_workflow_key(&process_id, SUCCESSOR)
        )],
        "the cancel was delivered to the segment's recorded lane"
    );
    assert_eq!(
        world.runs_of(SUCCESSOR).len(),
        1,
        "the successor ran once, on build N"
    );
    world.finish().await;
}

/// With no deployment serving the generation's lane the re-send is refused
/// typed, and the process keeps its park and its handover.
async fn a_refused_successor_of_a_gone_generation_keeps_its_work(world: World) {
    let (process_id, awaiter) = world.start_process().await;
    let park = world.refused(&process_id).await;
    world.mark_draining().await;
    // Another namespace's deployment serves no lane of this one: its port
    // addresses a generation lane nothing registered.
    let namespace =
        crate::RestateNamespace::new("gone").expect("a namespace nothing is registered in");
    let port = crate::process::RestateProcessIngressRunner::in_namespace(
        world.connection.clone(),
        Arc::clone(&world.registry),
        Arc::clone(&world.continuations),
        lash_core::engine::EngineGeneration::fixed(crate::tests::test_build_generation()),
        namespace,
    );
    let refusal = port
        .deliver_hand_over(&process_id, &generation("N"))
        .await
        .expect_err("no deployment serves the generation's lane");
    assert!(
        matches!(
            &refusal,
            PluginError::Runtime(error)
                if error.code == lash_core::RuntimeErrorCode::EngineServiceUnregistered
        ),
        "the refusal is typed: {refusal:?}"
    );
    let record = world.record(&process_id).await;
    assert_eq!(record.park(), Some(&park), "the process keeps its park");
    let handover = world
        .continuations
        .latest_segment_handover(&process_id)
        .await
        .expect("read the handover")
        .expect("the handover is retained");
    assert_eq!(
        (handover.segment_ordinal, handover.route.as_str()),
        (SUCCESSOR, PROCESS_WORKFLOW),
        "the handover is as its writer left it"
    );
    assert!(
        world.runs_of(SUCCESSOR).is_empty() && !awaiter.is_finished(),
        "nothing ran and nothing ended"
    );
    world.finish().await;
}

/// FIG-4750 residuals 2 and 4 (FIG-4739): a refused successor is re-sent
/// with no drain mark, by a leader of its own generation.
///
/// The refusal is the whole reason to move the successor: only a build of
/// its sender's generation can run it. Nothing here marks N draining, and
/// the recovery lease is held by build N itself. The pass still re-sends
/// segment 2 to N's lane, and the process ends there, having run once.
async fn a_refused_successor_is_re_sent_with_no_drain_mark_by_its_own_generation(world: World) {
    let (process_id, awaiter) = world.start_process().await;
    world.refused(&process_id).await;

    let pass = world.drain_pass_led_by(generation("N")).await;
    assert_eq!(
        (pass.pass.handled, pass.pass.deferred),
        (1, 0),
        "the pass re-sent the refused successor: {pass:?}"
    );
    assert_eq!(
        ended(awaiter).await,
        process_success(serde_json::json!({ "build": "N" })),
        "the process ended on build N"
    );
    assert_eq!(
        world.runs_of(SUCCESSOR),
        vec![SegmentRun {
            build: "N",
            ordinal: SUCCESSOR,
            admitted_by: Some(generation("N")),
        }],
        "the re-sent successor ran once, on build N"
    );
    let lane = world.lane();
    let runs = world.successor_runs(&process_id).await;
    assert_eq!(
        runs.iter()
            .filter(|run| run.target_service_name == lane)
            .count(),
        1,
        "one re-sent invocation: {runs:?}"
    );
    let pass = world.drain_pass_led_by(generation("N")).await;
    assert_eq!(
        (pass.pass.handled, pass.pass.deferred),
        (0, 0),
        "an ended process leaves the pass nothing: {pass:?}"
    );
    world.finish().await;
}

/// FIG-4750 residual 1 (FIG-4739): a segment on a generation lane sends its
/// successor to the same lane.
///
/// The process runs one segment more on build N. Its re-sent segment 2
/// crosses a boundary on N's lane, and segment 3 is sent straight to that
/// lane: the newest build never sees it, so it is never refused, the process
/// never parks again, and no further drain pass is needed for it to end.
async fn a_segment_on_a_generation_lane_sends_its_successor_to_its_own_lane(world: World) {
    world.runner_n.runs_segments(SEGMENTS + 1);
    let (process_id, awaiter) = world.start_process().await;
    world.refused(&process_id).await;
    world.mark_draining().await;
    let pass = world.drain_pass().await;
    assert_eq!((pass.pass.handled, pass.pass.deferred), (1, 0), "{pass:?}");

    // No second pass runs: the process ends on N by itself.
    assert_eq!(
        ended(awaiter).await,
        process_success(serde_json::json!({ "build": "N" })),
        "the process ended on build N"
    );
    let last = SUCCESSOR + 1;
    assert_eq!(
        world.runs_of(last),
        vec![SegmentRun {
            build: "N",
            ordinal: last,
            admitted_by: Some(generation("N")),
        }],
        "the lane segment's successor ran once, on build N"
    );
    let lane = world.lane();
    let runs = world
        .admin
        .segment_runs(
            &crate::services::DEFAULT_NAMESPACE,
            &[process_segment_workflow_key(&process_id, last)],
        )
        .await
        .expect("read the last segment's runs");
    assert_eq!(runs.len(), 1, "the successor was sent once: {runs:?}");
    assert_eq!(
        runs[0].target_service_name, lane,
        "the successor went straight to its generation's lane: {runs:?}"
    );
    let record = world.record(&process_id).await;
    assert_eq!(
        record.park().map(|park| park.attempts),
        None,
        "nothing refused the lane segment's successor: {record:?}"
    );
    world.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_successor_completes_on_its_lane_and_the_generation_drains_sqlite() {
    let world = World::start(Fixture::sqlite().await, false, false).await;
    a_refused_successor_completes_on_its_lane_and_the_generation_drains(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_refused_successor_completes_on_its_lane_and_the_generation_drains_postgres() {
    let world = World::start(Fixture::postgres().await, false, false).await;
    a_refused_successor_completes_on_its_lane_and_the_generation_drains(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_between_the_refusal_and_the_re_send_recovers_sqlite() {
    let world = World::start(Fixture::sqlite().await, false, false).await;
    a_crash_between_the_refusal_and_the_re_send_recovers(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_crash_between_the_refusal_and_the_re_send_recovers_postgres() {
    let world = World::start(Fixture::postgres().await, false, false).await;
    a_crash_between_the_refusal_and_the_re_send_recovers(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_sent_successor_stays_on_its_recorded_lane_sqlite() {
    let world = World::start(Fixture::sqlite().await, false, true).await;
    a_re_sent_successor_stays_on_its_recorded_lane(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_re_sent_successor_stays_on_its_recorded_lane_postgres() {
    let world = World::start(Fixture::postgres().await, false, true).await;
    a_re_sent_successor_stays_on_its_recorded_lane(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_successor_of_a_gone_generation_keeps_its_work_sqlite() {
    let world = World::start(Fixture::sqlite().await, false, false).await;
    a_refused_successor_of_a_gone_generation_keeps_its_work(world).await;
}

/// The laws on a live `restate-server`, one after the other: each world
/// registers build N anew, so a law never starts with N+1 the newest build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the refused-successor-drain suite"]
async fn live_restate_a_refused_successor_completes_recovers_and_the_generation_drains() {
    let world = World::start(Fixture::sqlite().await, true, false).await;
    a_refused_successor_completes_on_its_lane_and_the_generation_drains(world).await;
    let world = World::start(Fixture::sqlite().await, true, false).await;
    a_crash_between_the_refusal_and_the_re_send_recovers(world).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_successor_is_re_sent_with_no_drain_mark_by_its_own_generation_sqlite() {
    a_refused_successor_is_re_sent_with_no_drain_mark_by_its_own_generation(
        World::start(Fixture::sqlite().await, false, false).await,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_refused_successor_is_re_sent_with_no_drain_mark_by_its_own_generation_postgres() {
    a_refused_successor_is_re_sent_with_no_drain_mark_by_its_own_generation(
        World::start(Fixture::postgres().await, false, false).await,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_segment_on_a_generation_lane_sends_its_successor_to_its_own_lane_sqlite() {
    a_segment_on_a_generation_lane_sends_its_successor_to_its_own_lane(
        World::start(Fixture::sqlite().await, false, false).await,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_segment_on_a_generation_lane_sends_its_successor_to_its_own_lane_postgres() {
    a_segment_on_a_generation_lane_sends_its_successor_to_its_own_lane(
        World::start(Fixture::postgres().await, false, false).await,
    )
    .await;
}
