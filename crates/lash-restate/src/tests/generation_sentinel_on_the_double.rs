//! L7 (FIG-3795 §4.4): the generation sentinel on the server double.
//!
//! A segment's journal is recorded under drain generation `G_a`. The code
//! behind the same deployment id is then swapped for a build of `G_b` — what
//! ADR 0043 forbids and only an operator mistake can do — and the double
//! replays the invocation there. The replay parks the process typed
//! `RetiredGeneration`, naming `G_a`, before it replays any command past the
//! sentinel: the runner is not re-entered, no command is added to the
//! journal, and the attempt fails retryably so the invocation keeps its
//! journal. Swapped back to a build of `G_a`, the resumed invocation replays
//! the kept journal and completes the process once.

use super::*;
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{RestateTestServer, ServerConfig};
use restate_sdk::service::Service;
use restate_sdk::service::macro_support::ServiceBoxFuture;

use crate::process::{RestateProcessWorkflowInput, RestateProcessWorkflowPayload};

/// The one service of the swapped deployment: whichever build of the
/// process workflow is current serves each request.
struct Swappable<S> {
    current: Arc<Mutex<Arc<S>>>,
}

impl<S: Service<Future = ServiceBoxFuture>> Service for Swappable<S> {
    type Future = ServiceBoxFuture;

    fn handle(&self, req: restate_sdk::endpoint::ContextInternal) -> Self::Future {
        let current = Arc::clone(&self.current.lock_recover());
        current.handle(req)
    }
}

/// The swappable service's definition, discovered as `S` is.
fn swappable<S>(current: Arc<Mutex<Arc<S>>>) -> restate_sdk::service::ServiceDefinition
where
    S: Service<Future = ServiceBoxFuture> + Discoverable + Send + Sync + 'static,
{
    restate_sdk::service::macro_support::service_definition(Swappable { current }, S::discover())
}

/// Runs one segment to its terminal, holding the first entry open so the
/// test can replay the journal it left; logs every entry.
#[derive(Default)]
struct HeldRunner {
    entries: AtomicUsize,
}

#[async_trait::async_trait]
impl RestateProcessRunner for HeldRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &crate::SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        if self.entries.fetch_add(1, Ordering::SeqCst) == 0 {
            // The first attempt is crashed here by the test.
            std::future::pending::<()>().await;
        }
        Ok(process_success(serde_json::json!("replayed under its own generation")).into())
    }
}

const MAX_ATTEMPTS: u64 = 3;

async fn wait_for(server: &RestateTestServer, target: &str, done: impl Fn(&str) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if server
            .invocations()
            .iter()
            .any(|view| view.target == target && done(view.status))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "`{target}` never reached the expected status: {:#?}",
            server.invocations()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn commands(server: &RestateTestServer, id: &str) -> Vec<(MessageType, Option<String>)> {
    server
        .journal(id)
        .expect("the invocation's journal")
        .into_iter()
        .filter(|entry| entry.ty.is_command())
        .map(|entry| (entry.ty, entry.name))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l7_a_journal_replayed_under_another_generation_parks_before_any_effect() {
    let server = RestateTestServer::new(ServerConfig::default().with_seed(0x3795_d007))
        .expect("start the server double");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = RestateIngressClient::new(connection);
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let runner = Arc::new(HeldRunner::default());
    let build = |generation: &'static str| {
        Arc::new(
            LashProcessWorkflowImpl::new(
                Arc::clone(&runner),
                Arc::clone(&registry),
                Arc::clone(&stores.continuations),
                ingress.clone(),
                test_restate_authority_id(),
                lash_core::engine::BuildGeneration::for_test(generation),
            )
            .with_retry_max_attempts(MAX_ATTEMPTS)
            .serve(),
        )
    };
    let (recorded, swapped) = (build("G_a"), build("G_b"));
    let current = Arc::new(Mutex::new(Arc::clone(&recorded)));
    let deployment = server
        .register(
            Endpoint::builder()
                .bind(swappable(Arc::clone(&current)))
                .build(),
        )
        .await
        .expect("register the deployment");

    let process_id = registry
        .register_process(rerunnable_registration())
        .await
        .expect("register the process")
        .id;
    ingress
        .send_workflow_json(
            "LashProcessWorkflow",
            process_id.as_str(),
            "run",
            &RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
                process_id: process_id.clone(),
                registration: rerunnable_registration(),
                execution_context: ProcessExecutionContext::default(),
                segment_ordinal: 0,
                sender_generation: None,
            }),
        )
        .await
        .expect("send segment 0");
    let target = format!("LashProcessWorkflow/{process_id}/run");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while runner.entries.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "segment 0 never ran"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let invocation = server
        .invocations()
        .into_iter()
        .find(|view| view.target == target)
        .expect("the segment's invocation");
    let journaled = commands(&server, &invocation.id);
    assert_eq!(
        journaled.get(1),
        Some(&(
            MessageType::RunCommand,
            Some(crate::sentinel::GENERATION_SENTINEL.to_string())
        )),
        "the sentinel is the journal's first command: {journaled:?}"
    );
    assert!(journaled.len() > 2, "the first attempt journaled past it");

    // The same deployment id now runs a build of another generation.
    *current.lock_recover() = Arc::clone(&swapped);
    assert!(server.crash(&invocation.id), "crash the held attempt");
    wait_for(&server, &target, |status| status == "paused").await;

    let view = server
        .invocations()
        .into_iter()
        .find(|view| view.id == invocation.id)
        .expect("the invocation");
    assert_eq!(
        view.pinned_deployment_id,
        deployment.as_str(),
        "the same deployment"
    );
    assert_eq!(
        runner.entries.load(Ordering::SeqCst),
        1,
        "no replay under the other generation entered the runner"
    );
    assert_eq!(
        commands(&server, &invocation.id),
        journaled,
        "no command was journaled past the recorded prefix"
    );
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .expect("the process");
    assert_eq!(record.outcome, None, "a refused replay writes no terminal");
    let park = record.park.expect("the refused replay parks the process");
    assert!(
        matches!(
            &park.reason,
            lash_core::store::ParkReason::RetiredGeneration { message, .. }
                if message.contains("RetiredGeneration")
        ),
        "the park is typed RetiredGeneration: {park:?}"
    );
    assert_eq!(
        park.build_generation,
        Some(lash_core::engine::BuildGeneration::for_test("G_a")),
        "the park names the generation that recorded the journal"
    );

    // Back on a build of the recorded generation, the kept journal replays
    // and the process completes once.
    *current.lock_recover() = recorded;
    assert_eq!(server.resume(&invocation.id), Some(true), "resume");
    wait_for(&server, &target, |status| status == "completed").await;
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .expect("the process");
    assert_eq!(
        record.outcome,
        Some(process_success(serde_json::json!(
            "replayed under its own generation"
        ))),
        "the resumed journal completes the process"
    );
    assert!(record.park.is_none(), "the completion closes the park");
}
