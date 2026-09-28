//! FIG-3980: `LashSession` and `LashTurn` fold the generation sentinel
//! (ADR 0106 §1, FIG-3795) into their first recorded step.
//!
//! A handler's journal is recorded under drain generation `G_a`: its first
//! command is its first recorded step, whose entry carries `G_a`, and no
//! separate sentinel step exists. The code behind the same deployment id is
//! then swapped for a build of `G_b` and the double replays the invocation
//! there. The replay parks typed `RetiredGeneration`, naming `G_a`, at that
//! first entry: the step's outcome never reaches the drive, no body runs
//! again, and nothing is journaled past it. Swapped back to a build of `G_a`,
//! the resumed invocation replays the kept journal and completes.

use super::*;
use lash_core::SessionDriver;
use lash_core::engine::{
    AdmitVerdict, DriveAbort, DriveOutcome, DriveRequest, DriveRequestId, DriveStop, RootOutcome,
    admission_body, drive_admission_replay_key,
};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{RestateTestServer, ServerConfig};
use restate_sdk::endpoint::{HandlerOptions, ServiceOptions};
use restate_sdk::service::Service;
use restate_sdk::service::macro_support::ServiceBoxFuture;

use crate::session_driver::{
    LASH_SESSION_DRIVE_VERSION, LashSession as _, LashSessionImpl, LashTurn as _, LashTurnImpl,
    RestateSessionDriveRequest, RestateTurnDriveRequest, turn_workflow_key,
};

const MAX_ATTEMPTS: u64 = 3;

/// The one service of the swapped deployment: whichever build is current
/// serves each request.
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

/// The drive both builds install: every recorded step it journals counts
/// its body's executions, and the first pass past the step is held open so
/// the test can crash the attempt that journaled it.
#[derive(Default)]
struct HeldDriver {
    bodies: Arc<AtomicUsize>,
    passes: AtomicUsize,
}

impl HeldDriver {
    async fn recorded_step(
        &self,
        controller: &ScopedEffectController<'_>,
        replay_key: String,
    ) -> Result<(), DriveAbort> {
        let address = EffectAddress::new(controller.execution_scope().clone(), replay_key)
            .map_err(|error| DriveAbort::Refused(runtime_error(error.to_string())))?;
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(address, RuntimeAttribution::default(), "first-step"),
            RuntimeEffectCommand::AdmitDrive {
                request: Box::new(lash_core::engine::AdmitRequest {
                    session: SessionId::from("held"),
                    request: DriveRequestId::new("held"),
                    build_generation: lash_core::engine::BuildGeneration::for_test("G_a"),
                }),
            },
        );
        let bodies = Arc::clone(&self.bodies);
        controller
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(move |_| async move {
                    bodies.fetch_add(1, Ordering::SeqCst);
                    Ok(RuntimeEffectOutcome::AdmitDrive {
                        verdict: Box::new(AdmitVerdict::Idle),
                    })
                }),
            )
            .await
            .map_err(|error| DriveAbort::Retry(error.into_runtime_error()))?;
        if self.passes.fetch_add(1, Ordering::SeqCst) == 0 {
            // The first attempt is crashed here by the test.
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn runtime_error(message: impl Into<String>) -> lash_core::RuntimeError {
    lash_core::RuntimeError::new(lash_core::RuntimeErrorCode::QueuedWork, message.into())
}

#[async_trait::async_trait]
impl SessionDriver for HeldDriver {
    async fn admit(
        &self,
        controller: ScopedEffectController<'_>,
        request: &DriveRequest,
        ordinal: u32,
    ) -> Result<AdmitVerdict, DriveAbort> {
        self.recorded_step(
            &controller,
            drive_admission_replay_key(&request.request, ordinal),
        )
        .await?;
        Ok(AdmitVerdict::Idle)
    }

    async fn run_root(
        &self,
        controller: ScopedEffectController<'_>,
        admitted: lash_core::engine::Admitted,
    ) -> Result<RootOutcome, DriveAbort> {
        let root = admitted.root().clone();
        self.recorded_step(&controller, format!("first-step:{root}"))
            .await?;
        Ok(RootOutcome::Committed {
            outcome: lash_core::facade_support::TurnOutcome::Finished(
                lash_core::facade_support::TurnFinish::AssistantMessage {
                    text: format!("answered {root}"),
                },
            ),
            root,
        })
    }
}

/// One swappable service on the double, with the driver both builds share.
struct Swap<S> {
    server: RestateTestServer,
    ingress: RestateIngressClient,
    driver: Arc<HeldDriver>,
    current: Arc<Mutex<Arc<S>>>,
    recorded: Arc<S>,
    swapped: Arc<S>,
}

impl<S> Swap<S>
where
    S: Service<Future = ServiceBoxFuture> + Discoverable + Send + Sync + 'static,
{
    async fn start(
        seed: u64,
        handler: &str,
        build: impl Fn(RestateSessionDriverSlot, lash_core::engine::BuildGeneration) -> S,
    ) -> Self {
        let server = RestateTestServer::new(ServerConfig::default().with_seed(seed))
            .expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let ingress = RestateIngressClient::new(connection);
        let driver = Arc::new(HeldDriver::default());
        let slot = RestateSessionDriverSlot::new();
        slot.install(Arc::clone(&driver) as Arc<dyn SessionDriver>);
        let generation = lash_core::engine::BuildGeneration::for_test;
        let recorded = Arc::new(build(slot.clone(), generation("G_a")));
        let swapped = Arc::new(build(slot, generation("G_b")));
        let current = Arc::new(Mutex::new(Arc::clone(&recorded)));
        let definition = restate_sdk::service::macro_support::service_definition(
            Swappable {
                current: Arc::clone(&current),
            },
            S::discover(),
        )
        .options(
            ServiceOptions::new().handler(
                handler,
                HandlerOptions::new()
                    .retry_policy_max_attempts(MAX_ATTEMPTS)
                    .retry_policy_pause_on_max_attempts(),
            ),
        );
        server
            .register(Endpoint::builder().bind(definition).build())
            .await
            .expect("register the deployment");
        Self {
            server,
            ingress,
            driver,
            current,
            recorded,
            swapped,
        }
    }

    async fn wait_for(&self, target: &str, status: &str) -> lash_restate_test::InvocationView {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(view) = self
                .server
                .invocations()
                .into_iter()
                .find(|view| view.target == target && view.status == status)
            {
                return view;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "`{target}` never reached `{status}`: {:#?}",
                self.server.invocations()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn commands(&self, id: &str) -> Vec<(MessageType, Option<String>)> {
        self.server
            .journal(id)
            .expect("the invocation's journal")
            .into_iter()
            .filter(|entry| entry.ty.is_command())
            .map(|entry| (entry.ty, entry.name))
            .collect()
    }

    /// The law, once the first attempt is held past its first recorded step.
    async fn replays_parked_under_another_generation(&self, target: &str, first_step: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while self.driver.passes.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first step never ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let invocation = self
            .server
            .invocations()
            .into_iter()
            .find(|view| view.target == target)
            .expect("the invocation");
        let journaled = self.commands(&invocation.id);
        assert_eq!(
            journaled,
            vec![
                (MessageType::InputCommand, None),
                (MessageType::RunCommand, Some(format!("lash:{first_step}"))),
            ],
            "the first recorded step is the first command; no sentinel step precedes it"
        );
        let first_entry = self
            .server
            .journal(&invocation.id)
            .expect("the journal")
            .into_iter()
            .find_map(|entry| entry.run_completion())
            .expect("the first step's completion")
            .expect("the first step succeeded");
        let first_entry: serde_json::Value =
            serde_json::from_slice(&first_entry).expect("a JSON entry");
        let recording = lash_core::engine::BuildGeneration::for_test("G_a");
        assert_eq!(
            first_entry["build_generation"],
            serde_json::json!(recording.as_str()),
            "the first entry carries the recording generation"
        );

        // The same deployment id now runs a build of another generation.
        *self.current.lock_recover() = Arc::clone(&self.swapped);
        assert!(self.server.crash(&invocation.id), "crash the held attempt");
        let view = self.wait_for(target, "paused").await;
        let failure = view.last_failure.expect("the parked attempt's failure").1;
        assert!(
            failure.contains("RetiredGeneration") && failure.contains(recording.as_str()),
            "the replay parks typed, naming the recording generation: {failure}"
        );
        assert_eq!(
            self.driver.bodies.load(Ordering::SeqCst),
            1,
            "no replay ran the step's body again"
        );
        assert_eq!(
            self.driver.passes.load(Ordering::SeqCst),
            1,
            "the refused entry's outcome never reached the drive"
        );
        assert_eq!(
            self.commands(&invocation.id),
            journaled,
            "no command was journaled past the first entry"
        );

        // Back on a build of the recorded generation, the kept journal
        // replays and the invocation completes.
        *self.current.lock_recover() = Arc::clone(&self.recorded);
        assert_eq!(self.server.resume(&invocation.id), Some(true), "resume");
        let view = self.wait_for(target, "completed").await;
        assert!(
            self.server
                .outcome(&view.id)
                .is_some_and(|outcome| outcome.is_ok()),
            "the resumed journal completes: {view:?}"
        );
        assert_eq!(
            self.driver.bodies.load(Ordering::SeqCst),
            1,
            "the resumed replay served the recorded step"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_drive_replayed_under_another_generation_parks_at_its_first_admission() {
    let swap = Swap::start(0x3980_5e55, "drive", |slot, generation| {
        LashSessionImpl::new(
            slot,
            test_restate_authority_id(),
            generation,
            &crate::services::DEFAULT_NAMESPACE,
        )
        .serve()
    })
    .await;
    let session = SessionId::from("folded");
    let request = DriveRequestId::new("r-folded");
    swap.ingress
        .send_object_json_idempotent(
            "LashSession",
            session.as_str(),
            "drive",
            &RestateSessionDriveRequest {
                drive_version: LASH_SESSION_DRIVE_VERSION,
                request: DriveRequest {
                    session: session.clone(),
                    request: request.clone(),
                    build_generation: lash_core::engine::BuildGeneration::for_test("G_a"),
                },
            },
            request.as_str(),
        )
        .await
        .expect("send the drive");
    swap.replays_parked_under_another_generation(
        &format!("LashSession/{session}/drive"),
        &drive_admission_replay_key(&request, 0),
    )
    .await;
    let outcome: DriveOutcome = swap
        .ingress
        .call_object_json_idempotent(
            "LashSession",
            session.as_str(),
            "drive",
            &RestateSessionDriveRequest {
                drive_version: LASH_SESSION_DRIVE_VERSION,
                request: DriveRequest {
                    session: session.clone(),
                    request: request.clone(),
                    build_generation: lash_core::engine::BuildGeneration::for_test("G_a"),
                },
            },
            request.as_str(),
        )
        .await
        .expect("the resumed drive's outcome");
    assert!(
        matches!(outcome.stop, DriveStop::Idle),
        "the resumed drive answers its recorded admission: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_run_replayed_under_another_generation_parks_at_its_first_step() {
    let swap = Swap::start(0x3980_7a11, "run", |slot, generation| {
        LashTurnImpl::new(
            slot,
            test_restate_authority_id(),
            generation,
            &crate::services::DEFAULT_NAMESPACE,
        )
        .serve()
    })
    .await;
    let session = SessionId::from("folded");
    let root = TurnId::from("root-folded");
    let admitted = admission_body::admitted(
        session.clone(),
        root.clone(),
        DriveRequestId::new("r-folded"),
        lash_core::engine::AdmissionId::new("r-folded:0"),
        0,
        lash_core::engine::BuildGeneration::for_test("G_a"),
        lash_core::engine::AdmittedWork::Queued {
            head: lash_core::BatchId::from("scripted-batch"),
        },
    );
    let key = turn_workflow_key(&session, &root);
    swap.ingress
        .send_workflow_json(
            "LashTurn",
            &key,
            "run",
            &RestateTurnDriveRequest {
                drive_version: LASH_SESSION_DRIVE_VERSION,
                sender_generation: Some(lash_core::engine::BuildGeneration::for_test("G_a")),
                admitted,
            },
        )
        .await
        .expect("send the root run");
    swap.replays_parked_under_another_generation(
        &format!("LashTurn/{key}/run"),
        &format!("first-step:{root}"),
    )
    .await;
}
