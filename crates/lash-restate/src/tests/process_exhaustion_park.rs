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

/// Fails every attempt live, retryably, until `fixed`; then completes.
struct FlakyRunner {
    fixed: AtomicBool,
    runs: AtomicUsize,
}

#[async_trait::async_trait]
impl RestateProcessRunner for FlakyRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if !self.fixed.load(Ordering::SeqCst) {
            return Err(PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "the process's store is unreachable",
            )));
        }
        Ok(process_success(serde_json::json!({ "resumed": "completed" })).into())
    }
}

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

/// L-E4/L-E6 on Restate: a segment that exhausts its retries pauses, the
/// reconcile pass parks its process exactly once with `EngineRetryExhausted` and no
/// terminal evidence, and a resume under a fixed build completes the
/// process once and closes the park.
#[tokio::test]
pub(super) async fn an_exhausted_process_parks_and_completes_when_resumed() {
    let server = lash_restate_test::RestateTestServer::new(
        lash_restate_test::ServerConfig::default().with_seed(3675),
    )
    .expect("start the server double");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let runner = Arc::new(FlakyRunner {
        fixed: AtomicBool::new(false),
        runs: AtomicUsize::new(0),
    });
    let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
    let session_stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the session store set");

    let sessions = session_stores.session_store_factory();
    let endpoint = crate::services::bind_lash_services(
        Endpoint::builder(),
        crate::services::LashServiceParts {
            effect_host: &host,
            ingress: RestateIngressClient::new(connection.clone()),
            admin: crate::RestateAdminClient::new(connection.clone()),
            attachments: Arc::clone(&sessions) as Arc<dyn lash_core::AttachmentReferrers>,
            sessions,
            process_workflow: LashProcessWorkflowImpl::new_for_test(
                Arc::clone(&runner),
                Arc::clone(&registry),
                Arc::clone(&stores.continuations),
            )
            .with_retry_max_attempts(MAX_ATTEMPTS),
            session_shifts: crate::RestateSessionShiftsSlot::new(),
            build_generation: lash_core::engine::BuildGeneration::for_test("exhaustion-park"),
            namespace: crate::RestateNamespace::default(),
            fleet: crate::object_state::FleetView::default(),
        },
    )
    .build();
    server
        .register(endpoint)
        .await
        .expect("register the endpoint on the server double");
    let deployment = RestateProcessDeployment::new_for_test(
        connection.clone(),
        Arc::clone(&registry),
        Arc::clone(&stores.continuations),
    );
    let admin = crate::RestateAdminClient::new(connection.clone());

    let process_id = registry
        .register_process(executed_registration())
        .await
        .expect("register the process")
        .id;
    let port: Arc<dyn lash_core::ProcessWorkSubstrate> = deployment.test_process_work();
    let verdict = deliver_process_start_now(
        &stores.start_ledger,
        &registry,
        &port,
        &stores.clock,
        &process_id,
    )
    .await;
    assert_eq!(
        verdict,
        lash_core::runtime::shift::relay::RelayVerdict::Delivered,
        "the armed start is delivered: {verdict:?}"
    );
    let target = format!("LashProcessWorkflow/{process_id}/run");
    let paused = wait_for_status(&server, &target, "paused").await;
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        usize::try_from(MAX_ATTEMPTS).expect("small"),
        "the segment runs its attempt bound and no more"
    );

    // The next pass reconciles the pause into a park.
    reconcile_parked_processes(&admin, &registry, &stores.continuations).await;
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
        MAX_ATTEMPTS,
        "the park counts the engine's attempts"
    );
    assert!(
        message.contains("the process's store is unreachable"),
        "the park carries the last failure: {message}"
    );
    assert_eq!(
        park.engine
            .as_ref()
            .map(lash_core::store::EnginePark::as_str),
        Some(paused.id.as_str()),
        "the park holds the paused invocation as its engine handle"
    );

    // A further pass over the same pause writes nothing.
    reconcile_parked_processes(&admin, &registry, &stores.continuations).await;
    assert_eq!(
        process_feed(&registry, &process_id).await,
        vec![(
            park.park_id,
            lash_core::store::ParkEventKind::Parked {
                reason: park.reason.clone()
            }
        )],
        "the feed records the park exactly once"
    );
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read the process again")
            .and_then(|record| record.park().map(|park| park.attempts)),
        Some(1),
        "a repeated reconcile does not re-park"
    );

    // Fix the build and resume through the park: the retry completes the
    // process once and the park closes.
    runner.fixed.store(true, Ordering::SeqCst);
    let resumed = crate::resume_parked_process(
        &admin,
        &crate::services::DEFAULT_NAMESPACE,
        &registry,
        &process_id,
    )
    .await
    .expect("resume the parked process");
    assert_eq!(resumed.as_str(), paused.id.as_str());
    wait_for_status(&server, &target, "completed").await;
    let completed = registry
        .get_process(&process_id)
        .await
        .expect("read the resumed process")
        .expect("the resumed process is retained");
    assert_eq!(completed.status(), lash_core::ProcessStatus::Completed);
    assert_eq!(completed.park(), None, "completion ends the park");
    assert_eq!(
        process_feed(&registry, &process_id).await,
        vec![
            (
                park.park_id,
                lash_core::store::ParkEventKind::Parked {
                    reason: park.reason.clone()
                }
            ),
            (
                park.park_id,
                lash_core::store::ParkEventKind::Unparked {
                    cause: lash_core::store::UnparkCause::ProcessTerminal {
                        status: lash_core::ProcessStatus::Completed
                    }
                }
            ),
        ],
        "the park closes once, when the resumed process completes"
    );
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
