//! Engine retry exhaustion parks a process (FIG-3675, R0c; the Restate leg
//! of the engine obligation L-E4/L-E6 for processes).
//!
//! The law runs the real `LashProcessWorkflow/run` handler on the in-process
//! server double, which retries on the handler's own retry policy and pauses
//! it on its last attempt, with virtual time making every backoff instant.
//! A segment that keeps failing live pauses after its deployment's attempt
//! bound. The deployment's sweep reconciles the pause into a process park
//! (`EngineRetryExhausted`, the invocation id as its engine handle) with no
//! terminal evidence, and a resume under a fixed build completes the process
//! once and closes the park.

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
/// sweep parks its process exactly once with `EngineRetryExhausted` and no
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
    let sessions = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the session store set")
        .session_store_factory();
    let endpoint = crate::services::bind_lash_services(
        Endpoint::builder(),
        crate::services::LashServiceParts {
            effect_host: &host,
            ingress: RestateIngressClient::new(connection.clone()),
            sessions,
            process_workflow: LashProcessWorkflowImpl::new_for_test(
                Arc::clone(&runner),
                Arc::clone(&registry),
                Arc::clone(&stores.continuations),
            )
            .with_retry_max_attempts(MAX_ATTEMPTS),
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
    deployment.install_park_reconciler(admin.clone());

    let process_id = ProcessId::from("r0c-exhausted-body");
    registry
        .register_process(rerunnable_registration(process_id.as_str()))
        .await
        .expect("register the process");
    let sweep = deployment.test_process_work();
    let _ = sweep
        .admit_pending_processes("r0c sweep")
        .await
        .expect("the sweep submits the process");
    let target = format!("LashProcessWorkflow/{process_id}/run");
    let paused = wait_for_status(&server, &target, "paused").await;
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        usize::try_from(MAX_ATTEMPTS).expect("small"),
        "the segment runs its attempt bound and no more"
    );

    // The next sweep reconciles the pause into a park.
    let _ = sweep
        .admit_pending_processes("r0c sweep")
        .await
        .expect("the next sweep");
    let parked = registry
        .get_process(&process_id)
        .await
        .expect("read the exhausted process")
        .expect("the exhausted process is retained");
    assert!(!parked.is_terminal(), "a park is non-terminal: {parked:?}");
    assert_eq!(parked.outcome, None, "a park writes no terminal evidence");
    let park = parked
        .park
        .as_deref()
        .cloned()
        .expect("the process is parked");
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

    // A further sweep over the same pause writes nothing.
    let _ = sweep
        .admit_pending_processes("r0c sweep")
        .await
        .expect("a further sweep");
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
            .and_then(|record| record.park.map(|park| park.attempts)),
        Some(1),
        "a repeated reconcile does not re-park"
    );

    // Fix the build and resume through the park: the retry completes the
    // process once and the park closes.
    runner.fixed.store(true, Ordering::SeqCst);
    let resumed = crate::resume_parked_process(&admin, &registry, &process_id)
        .await
        .expect("resume the parked process");
    assert_eq!(resumed.as_str(), paused.id.as_str());
    wait_for_status(&server, &target, "completed").await;
    let completed = registry
        .get_process(&process_id)
        .await
        .expect("read the resumed process")
        .expect("the resumed process is retained");
    assert_eq!(completed.status, lash_core::ProcessStatus::Completed);
    assert_eq!(completed.park, None, "completion ends the park");
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
