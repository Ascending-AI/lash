//! Engine retry exhaustion parks a process (FIG-3675, R0c; the Restate leg
//! of the engine obligation L-E4/L-E6 for processes).
//!
//! The law runs the real `LashProcessWorkflow/run` handler on the in-process
//! server double, which retries on the handler's own retry policy and pauses
//! it on its last attempt, with virtual time making every backoff instant.
//! A segment that keeps failing live pauses after its deployment's attempt
//! bound. The deployment's reconcile pass turns the pause into a process park
//! (`EngineRetryExhausted`, the invocation id as its engine handle) with no
//! terminal evidence, and a resume under a fixed build completes the process
//! once and closes the park.

use super::process_session_turn_laws::{
    FAST, answering_provider, keyed_registration_for, llm_profiles_serving_fast,
    worker_with_llm_profiles,
};
use super::*;

const MAX_ATTEMPTS: u64 = 3;

async fn wait_for_status(
    server: &lash_restate_test::RestateTestServer,
    target: &str,
    status: &str,
) -> lash_restate_test::InvocationView {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(view) = server
            .invocations()
            .into_iter()
            .find(|view| view.target == target && view.status == status)
        {
            return view;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "`{target}` never reached `{status}`: {:?}",
            server.invocations()
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn process_feed(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Vec<(lash_core::store::ParkId, lash_core::store::ParkEventKind)> {
    registry
        .process_park_feed(
            lash_core::store::ParkFeedCursor::initial(),
            std::num::NonZeroUsize::new(64).expect("non-zero"),
        )
        .await
        .expect("read the process park feed")
        .events
        .into_iter()
        .filter(|event| event.target == *process_id)
        .map(|event| (event.park_id, event.kind))
        .collect()
}

struct OwnerProcessWorld {
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    control: crate::session_control::RestateSessionControl,
    work: Arc<dyn lash_core::ProcessWorkSubstrate>,
    endpoint: Endpoint,
}

impl OwnerProcessWorld {
    fn new(
        stores: Arc<lash_sqlite_store::SqliteStoreSet>,
        connection: crate::RestateConnection,
        runner: Arc<super::run_coordinator_on_the_double::owner_park::ProcessAttempts>,
    ) -> Self {
        let registry: Arc<dyn ProcessRegistry> = stores.process_registry();
        runner.observe_registry(&registry);
        let continuations = stores.process_continuations();
        let sessions = stores.session_store_factory();
        let admin = crate::RestateAdminClient::new(connection.clone());
        let ingress = RestateIngressClient::new(connection.clone());
        let generation = lash_core::engine::BuildGeneration::for_test("exhaustion-park");
        let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
        let endpoint = crate::services::bind_lash_services(
            Endpoint::builder(),
            crate::services::LashServiceParts {
                effect_host: &host,
                ingress: ingress.clone(),
                admin: admin.clone(),
                materials: stores.process_env_store(),
                attachments: sessions.clone() as Arc<dyn lash_core::AttachmentReferrers>,
                sessions: sessions.clone(),
                process_workflow: LashProcessWorkflowImpl::new_for_test(
                    runner,
                    registry.clone(),
                    continuations.clone(),
                )
                .with_retry_max_attempts(MAX_ATTEMPTS),
                session_shifts: crate::RestateSessionShiftsSlot::new(),
                build_generation: generation.clone(),
                namespace: Default::default(),
                fleet: Default::default(),
            },
        )
        .build();
        let work = RestateProcessDeployment::new_for_test(
            connection,
            registry.clone(),
            continuations.clone(),
        )
        .test_process_work();
        let control = crate::session_control::RestateSessionControl {
            admin,
            ingress,
            namespace: Default::default(),
            processes: registry.clone(),
            continuations: continuations.clone(),
            generation: lash_core::engine::EngineGeneration::fixed(generation),
            sessions,
            lost_processes: Default::default(),
            lost_runs: Default::default(),
        };
        Self {
            stores,
            registry,
            continuations,
            control,
            work,
            endpoint,
        }
    }
}

/// L02/L20: concurrent X exhausts its owner after a sibling final is
/// durable. Redrive reuses that X over the same journal; cancel requests
/// retain their identity through resume. Both paths survive store reopen.
#[tokio::test]
pub(super) async fn l20_an_exhausted_process_parks_and_completes_without_group_catalog() {
    for file in [false, true] {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let server = lash_restate_test::RestateTestServer::new(
                lash_restate_test::ServerConfig::default().with_seed(4892),
            )
            .unwrap();
            let connection = super::run_owner_park::NoGroupCatalog::connection(&server);
            let stores = Arc::new(if file {
                lash_sqlite_store::SqliteStoreSet::open(directory.path())
                    .await
                    .unwrap()
            } else {
                lash_sqlite_store::SqliteStoreSet::memory().await.unwrap()
            });
            let runner =
                Arc::new(super::run_coordinator_on_the_double::owner_park::ProcessAttempts::new());
            let mut world = OwnerProcessWorld::new(stores, connection.clone(), runner.clone());
            let deployment = server.register(world.endpoint.clone()).await.unwrap();
            let process = world
                .registry
                .register_process(executed_registration())
                .await
                .unwrap()
                .id;
            let verdict = deliver_process_start_now(
                &world
                    .stores
                    .obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
                &world.registry,
                &world.work,
                &world.stores.clock(),
                &process,
            )
            .await;
            assert_eq!(
                verdict,
                lash_core::runtime::shift::relay::RelayVerdict::Delivered
            );
            let target = format!("LashProcessWorkflow/{process}/run");
            runner.release_after_sibling(&server).await;
            let paused = wait_for_status(&server, &target, "paused").await;
            assert_eq!(runner.runs(), usize::try_from(MAX_ATTEMPTS).unwrap());
            reconcile_parked_processes(&world.control.admin, &world.registry, &world.continuations)
                .await;
            let parked = world.registry.get_process(&process).await.unwrap().unwrap();
            assert!(!parked.is_terminal());
            assert_eq!(parked.outcome(), None);
            let park = parked.park().cloned().unwrap();
            let lash_core::store::ParkReason::EngineRetryExhausted {
                attempts, message, ..
            } = &park.reason
            else {
                panic!("{park:?}");
            };
            assert_eq!(u64::from(*attempts), MAX_ATTEMPTS);
            assert!(message.contains("owner-local-X unavailable"), "{message}");
            assert_eq!(park.engine.as_ref().unwrap().as_str(), paused.id);
            reconcile_parked_processes(&world.control.admin, &world.registry, &world.continuations)
                .await;
            assert_eq!(
                process_feed(&world.registry, &process).await,
                vec![(
                    park.park_id,
                    lash_core::store::ParkEventKind::Parked {
                        reason: park.reason.clone()
                    }
                )]
            );
            assert_eq!(runner.sibling_bodies(), 1);
            let listed = world
                .registry
                .list_parked_processes(&lash_core::store::ProcessParkQuery {
                    reasons: None,
                    parked_at_or_before_ms: None,
                    after: None,
                    limit: std::num::NonZeroUsize::MIN,
                })
                .await
                .unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, process);
            assert_eq!(listed[0].park(), Some(&park));
            drop(listed);
            if file {
                server
                    .restart_deployment(&deployment, Endpoint::builder().build())
                    .await
                    .unwrap();
                drop(world);
                let stores = Arc::new(
                    lash_sqlite_store::SqliteStoreSet::open(directory.path())
                        .await
                        .unwrap(),
                );
                world = OwnerProcessWorld::new(stores, connection, runner.clone());
                server
                    .restart_deployment(&deployment, world.endpoint.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    world
                        .registry
                        .get_process(&process)
                        .await
                        .unwrap()
                        .unwrap()
                        .park(),
                    Some(&park)
                );
            }
            runner.restore();
            let cancel_request = lash_core::CancelRequest::new(
                lash_core::CancelOrigin::OperatorRequested,
                "l20-process-owner",
                42,
            );
            if cancel {
                world
                    .work
                    .deliver_cancel(&process, &cancel_request, "l20-process-cancel")
                    .await
                    .unwrap();
            }
            assert_eq!(
                lash_core::engine::SessionControlEngine::resume_process(
                    &world.control,
                    &process,
                    park.park_id
                )
                .await
                .unwrap(),
                lash_core::engine::EngineAck::Resumed
            );
            let original = wait_for_status(&server, &target, "completed").await;
            assert_eq!(
                original.id, paused.id,
                "L20 redrive uses the original journal"
            );
            let completed = world.registry.get_process(&process).await.unwrap().unwrap();
            let status = if cancel {
                lash_core::ProcessStatus::Cancelled
            } else {
                lash_core::ProcessStatus::Completed
            };
            assert_eq!(completed.status(), status);
            if cancel {
                assert_eq!(completed.cancel_request.as_deref(), Some(&cancel_request));
            }
            assert_eq!(
                runner.sibling_bodies(),
                1,
                "L02 process redrive reuses durable X"
            );
            assert!(
                server
                    .invocations()
                    .iter()
                    .all(|view| !view.target.contains("EffectGroup")),
                "L20 no group service is queried"
            );
            assert_eq!(completed.park(), None);
            assert_eq!(
                process_feed(&world.registry, &process).await,
                vec![
                    (
                        park.park_id,
                        lash_core::store::ParkEventKind::Parked {
                            reason: park.reason.clone()
                        }
                    ),
                    (
                        park.park_id,
                        if cancel {
                            lash_core::store::ParkEventKind::Cancelled {
                                cause: lash_core::store::ParkCancelCause::ProcessCancelled {
                                    origin: Some(lash_core::CancelOrigin::OperatorRequested),
                                },
                            }
                        } else {
                            lash_core::store::ParkEventKind::Unparked {
                                cause: lash_core::store::UnparkCause::ProcessTerminal { status },
                            }
                        }
                    ),
                ]
            );
        }
    }
}

/// FIG-4727: a process a session turn started parks with the profile key
/// its attempts could not bind, end to end on the server double — the leg
/// FIG-4631 left uncovered. The child's deployment does not serve the key
/// its create request names, so every attempt of the segment's `run` ends
/// retryably on the typed `llm_profile_unavailable` refusal and the double
/// pauses it once its attempts run out. The reconcile's recorded
/// `EngineRetryExhausted` park carries the key off the paused invocation's
/// last failure, and a resume on a deployment that serves the key again
/// runs the segment through: the child is created, answers, and the
/// process completes with its park closed.
///
/// `postgres_ingress` runs the same law over a PostgreSQL store set.
pub(super) async fn a_session_turn_started_process_parks_with_its_profile_key(
    double: &lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
) {
    let key = lash_core::LlmProfileKey::new(FAST);
    let stores = double.engine_stores();
    let registry = stores.process_registry();
    let continuations = stores.process_continuations();
    let sessions = stores.session_store_factory();
    let child = SessionId::from("session-turn-parked-key-child");
    let registration = keyed_registration_for(&child).await;
    let process_id = registry
        .register_process(registration)
        .await
        .expect("register the keyed SessionTurn")
        .id;

    // The deployment does not serve the child's key: each attempt ends
    // retryably on the typed refusal.
    double.install_process_worker(
        worker_with_llm_profiles(
            double.lash_backend(),
            Arc::clone(&registry),
            Arc::clone(&sessions),
            lash_core::testing::standard_test_llm_profiles(answering_provider("never asked")),
            Vec::new(),
        )
        .await,
    );
    let connection = crate::RestateConnection::with_transport(
        double.server().ingress_url(),
        double.server().transport(),
    );
    let admin = crate::RestateAdminClient::new(connection);
    let verdict = deliver_process_start_now(
        &stores.obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
        &registry,
        &Arc::clone(double.lash_backend().process_work().port()),
        &stores.clock(),
        &process_id,
    )
    .await;
    assert_eq!(
        verdict,
        lash_core::runtime::shift::relay::RelayVerdict::Delivered,
        "the armed start is delivered: {verdict:?}"
    );
    let target = format!(
        "{}/{process_id}/run",
        double.service_name(crate::LashService::ProcessWorkflow.base_name())
    );
    let paused = wait_for_status(double.server(), &target, "paused").await;

    // The next pass reconciles the pause into a park.
    reconcile_parked_processes(&admin, &registry, &continuations).await;
    let parked = registry
        .get_process(&process_id)
        .await
        .expect("read the exhausted process")
        .expect("the exhausted process is retained");
    assert!(!parked.is_terminal(), "a park is non-terminal: {parked:?}");
    assert_eq!(parked.outcome(), None, "a park writes no terminal evidence");
    let park = parked.park().cloned().expect("the process is parked");
    let lash_core::store::ParkReason::EngineRetryExhausted {
        attempts, message, ..
    } = &park.reason
    else {
        panic!("an exhausted process parks as EngineRetryExhausted: {park:?}");
    };
    assert_eq!(
        u64::from(*attempts),
        crate::PROCESS_HANDLER_MAX_ATTEMPTS,
        "the park counts the engine's attempts"
    );
    assert!(
        message.contains("llm_profile_unavailable"),
        "the park carries the last failure's typed record: {message}"
    );
    assert_eq!(
        park.reason.profile_key(),
        Some(&key),
        "the recorded park carries the key the session turn's attempts could not bind: {:?}",
        park.reason
    );
    assert_eq!(
        park.engine
            .as_ref()
            .map(lash_core::store::EnginePark::as_str),
        Some(paused.id.as_str()),
        "the park holds the paused invocation as its engine handle"
    );

    // A deployment that serves the key again: the resume runs the segment
    // where it stopped, the child is created and answered, and the park
    // closes.
    double.install_process_worker(
        worker_with_llm_profiles(
            double.lash_backend(),
            Arc::clone(&registry),
            Arc::clone(&sessions),
            llm_profiles_serving_fast(answering_provider("the resumed child answered")),
            Vec::new(),
        )
        .await,
    );
    let resumed = crate::resume_parked_process(
        &admin,
        &crate::services::DEFAULT_NAMESPACE,
        &registry,
        &process_id,
    )
    .await
    .expect("resume the parked process");
    assert_eq!(resumed.as_str(), paused.id.as_str());
    wait_for_status(double.server(), &target, "completed").await;
    let completed = registry
        .get_process(&process_id)
        .await
        .expect("read the resumed process")
        .expect("the resumed process is retained");
    assert_eq!(completed.status(), lash_core::ProcessStatus::Completed);
    assert_eq!(completed.park(), None, "completion ends the park");
    assert!(
        lash_core::runtime::live_session_view(&sessions, &child)
            .await
            .expect("open the child the resumed turn created")
            .is_some(),
        "the session turn's child ran on the resume"
    );
}

#[tokio::test]
async fn a_session_turn_started_process_parks_with_its_profile_key_on_sqlite() {
    let double = lash_restate_test::backend(0x4727, lash_restate_test::ServerConfig::default())
        .await
        .expect("boot the SessionTurn server double")
        .erase_store_type();
    a_session_turn_started_process_parks_with_its_profile_key(&double).await;
}
