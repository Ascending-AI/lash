//! FIG-3980: `LashSession` and `LashTurn` fold the generation sentinel
//! (ADR 0106 §1, FIG-3795) into their first recorded step.
//!
//! A handler's journal is recorded under drain generation `G_a`: its first
//! command is its first recorded step, whose entry carries `G_a`, and no
//! separate sentinel step exists. A session shift's first command is its leg
//! start, which is ahead of its first admission so that a failed attempt
//! inside that admission is seen (FIG-4556), and carries `G_a` itself. The code behind the same deployment id is
//! then swapped for a build of `G_b` and the double replays the invocation
//! there. The replay parks typed `RetiredGeneration`, naming `G_a`, at that
//! first entry: the step's outcome never reaches the shift, no body runs
//! again, and nothing is journaled past it. Swapped back to a build of `G_a`,
//! the resumed invocation replays the kept journal and completes.

use super::*;
use lash_core::SessionShifts;
use lash_core::engine::{
    AdmitVerdict, RunOutcome, ShiftAbort, ShiftOutcome, ShiftRequest, ShiftRequestId, ShiftStop,
    shift_admission_replay_key,
};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{RestateTestServer, ServerConfig, TimeMode};
use restate_sdk::endpoint::{HandlerOptions, ServiceOptions};
use restate_sdk::service::Service;
use restate_sdk::service::macro_support::ServiceBoxFuture;

use crate::session_shifts::{
    LashSession as _, LashSessionImpl, LashTurn as _, LashTurnImpl, RestateRunRequest,
    RestateSessionShiftRequest, turn_invocation_key,
};

const MAX_ATTEMPTS: u64 = 3;

/// The handler options of the laws that crash and swap a held attempt: a
/// small attempt budget, then pause.
fn held_options() -> HandlerOptions {
    HandlerOptions::new()
        .retry_policy_max_attempts(MAX_ATTEMPTS)
        .retry_policy_pause_on_max_attempts()
}

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

/// The shift both builds install: every recorded step it journals counts
/// its body's executions, and the first pass past the step is held open so
/// the test can crash the attempt that journaled it.
#[derive(Default)]
struct HeldShifts {
    bodies: Arc<AtomicUsize>,
    passes: AtomicUsize,
}

impl HeldShifts {
    async fn recorded_step(
        &self,
        controller: &ScopedEffectController<'_>,
        replay_key: String,
    ) -> Result<(), ShiftAbort> {
        let address = EffectAddress::new(controller.execution_scope().clone(), replay_key)
            .map_err(|error| ShiftAbort::Refused(runtime_error(error.to_string())))?;
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(address, RuntimeAttribution::default(), "first-step"),
            RuntimeEffectCommand::AdmitShift {
                request: Box::new(lash_core::engine::AdmitRequest {
                    run_start: lash_core::engine::RunStartNonce::new("fixture"),
                    session: SessionId::from("held"),
                    request: ShiftRequestId::new("held"),
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
                    Ok(RuntimeEffectOutcome::AdmitShift {
                        verdict: Box::new(AdmitVerdict::Idle),
                    })
                }),
            )
            .await
            .map_err(|error| ShiftAbort::Retry(error.into_runtime_error()))?;
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
impl SessionShifts for HeldShifts {
    async fn admit(
        &self,
        controller: ScopedEffectController<'_>,
        request: &ShiftRequest,
        _admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        _draining: Option<&lash_core::engine::BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        self.recorded_step(
            &controller,
            shift_admission_replay_key(&request.request, ordinal),
        )
        .await?;
        Ok(AdmitVerdict::Idle)
    }

    async fn execute_run(
        &self,
        controller: ScopedEffectController<'_>,
        admitted: lash_core::engine::Admitted,
    ) -> lash_core::engine::RunEnd {
        lash_core::engine::RunEnd::owing_nothing(
            async {
                let run = admitted.run().clone();
                self.recorded_step(&controller, format!("first-step:{run}"))
                    .await?;
                Ok(RunOutcome::Committed {
                    work_remaining: true,
                    kind: lash_core::store::RunTerminalKind::Answered,
                    run,
                })
            }
            .await,
        )
    }

    async fn close_run(
        &self,
        _controller: lash_core::ScopedEffectController<'_>,
        _session: &lash_core::SessionId,
        _run: &lash_core::TurnId,
    ) -> Result<(), lash_core::engine::ShiftAbort> {
        Ok(())
    }
}

/// One swappable service on the double, with the `SessionShifts` both builds share.
struct Swap<S> {
    server: RestateTestServer,
    ingress: RestateIngressClient,
    shifts: Arc<HeldShifts>,
    /// The slot's installation of `shifts`, kept for the swap's life.
    _installation: Arc<dyn SessionShifts>,
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
        options: HandlerOptions,
        time: TimeMode,
        build: impl Fn(RestateSessionShiftsSlot, lash_core::engine::BuildGeneration) -> S,
    ) -> Self {
        let server = RestateTestServer::new(ServerConfig::default().with_seed(seed).time(time))
            .expect("start the server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let ingress = RestateIngressClient::new(connection);
        let shifts = Arc::new(HeldShifts::default());
        let slot = RestateSessionShiftsSlot::new();
        let installation = slot.install(Arc::clone(&shifts) as Arc<dyn SessionShifts>);
        let generation = lash_core::engine::BuildGeneration::for_test;
        let recorded = Arc::new(build(slot.clone(), generation("G_a")));
        let swapped = Arc::new(build(slot.clone(), generation("G_b")));
        let current = Arc::new(Mutex::new(Arc::clone(&recorded)));
        let definition = restate_sdk::service::macro_support::service_definition(
            Swappable {
                current: Arc::clone(&current),
            },
            S::discover(),
        )
        .options(ServiceOptions::new().handler(handler, options));
        let endpoint = Endpoint::builder().bind(definition);
        let endpoint = if handler == "shift" {
            endpoint.bind(
                LashTurnImpl::new(
                    slot,
                    test_restate_authority_id(),
                    generation("G_a"),
                    &crate::services::DEFAULT_NAMESPACE,
                    crate::object_state::FleetView::default(),
                )
                .serve(),
            )
        } else {
            endpoint
        };
        server
            .register(endpoint.build())
            .await
            .expect("register the deployment");
        Self {
            server,
            ingress,
            shifts,
            _installation: installation,
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

    /// The law, once the first attempt is held past its first recorded step:
    /// `steps` are the run commands the handler journaled up to it, the first
    /// of which carries the generation.
    /// `target`'s invocation, once its first attempt is held past its first
    /// recorded step.
    async fn held(&self, target: &str) -> lash_restate_test::InvocationView {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while self.shifts.passes.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first step never ran"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.server
            .invocations()
            .into_iter()
            .find(|view| view.target == target)
            .expect("the invocation")
    }

    async fn replays_parked_under_another_generation(&self, target: &str, steps: &[String]) {
        let invocation = self.held(target).await;
        let journaled = self.commands(&invocation.id);
        let mut expected: Vec<_> = std::iter::once((MessageType::InputCommand, None))
            .chain(
                steps
                    .iter()
                    .map(|step| (MessageType::RunCommand, Some(step.clone()))),
            )
            .collect();
        if target.starts_with("LashSession/") {
            expected.push((MessageType::CallCommand, None));
        }
        assert_eq!(
            journaled, expected,
            "the step that carries the generation is the first command; no sentinel step \
             precedes it"
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
            self.shifts.bodies.load(Ordering::SeqCst),
            1,
            "no replay ran the step's body again"
        );
        assert_eq!(
            self.shifts.passes.load(Ordering::SeqCst),
            1,
            "the refused entry's outcome never reached the shift"
        );
        assert_eq!(
            self.commands(&invocation.id),
            journaled,
            "no command was journaled past the held step"
        );

        // Back on a build of the recorded generation, the kept journal
        // replays and the invocation completes.
        *self.current.lock_recover() = Arc::clone(&self.recorded);
        assert_eq!(self.server.resume(&invocation.id), Some(true), "resume");
        if target.starts_with("LashSession/") {
            let child = self
                .server
                .invocations()
                .into_iter()
                .find(|view| view.target.starts_with("LashTurn/") && view.target.ends_with("/run"))
                .expect("held admission invocation");
            assert!(
                self.server.crash(&child.id),
                "release the child's held attempt through replay"
            );
        }
        let view = self.wait_for(target, "completed").await;
        assert!(
            self.server
                .outcome(&view.id)
                .is_some_and(|outcome| outcome.is_ok()),
            "the resumed journal completes: {view:?}"
        );
        assert_eq!(
            self.shifts.bodies.load(Ordering::SeqCst),
            1,
            "the resumed replay served the recorded step"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_shift_replayed_under_another_generation_parks_at_its_leg_start() {
    let swap = Swap::start(
        0x3980_5e55,
        "shift",
        held_options(),
        TimeMode::auto(),
        |slot, generation| {
            LashSessionImpl::new(
                slot,
                test_restate_authority_id(),
                generation,
                &crate::services::DEFAULT_NAMESPACE,
            )
            .serve()
        },
    )
    .await;
    let session = SessionId::from("folded");
    let request = ShiftRequestId::new("r-folded");
    swap.ingress
        .send_object_json_idempotent(
            "LashSession",
            session.as_str(),
            "shift",
            &crate::Call::new(RestateSessionShiftRequest {
                request: ShiftRequest {
                    session: session.clone(),
                    request: request.clone(),
                    intended_lane: None,
                },
                handed_off: None,
            }),
            request.as_str(),
        )
        .await
        .expect("send the shift");
    swap.replays_parked_under_another_generation(
        &format!("LashSession/{session}/shift"),
        &["lash.shift.leg".to_owned()],
    )
    .await;
    let outcome: crate::Reply<ShiftOutcome> = swap
        .ingress
        .call_object_json_idempotent(
            "LashSession",
            session.as_str(),
            "shift",
            &crate::Call::new(RestateSessionShiftRequest {
                request: ShiftRequest {
                    session: session.clone(),
                    request: request.clone(),
                    intended_lane: None,
                },
                handed_off: None,
            }),
            request.as_str(),
        )
        .await
        .expect("the resumed shift's outcome");
    let outcome = outcome.body;
    assert!(
        matches!(outcome.stop, ShiftStop::Idle),
        "the resumed shift answers its recorded admission: {outcome:?}"
    );
}

/// A `LashTurn` deployment of `G_a`'s run handler, swappable to `G_b`, with
/// `options` on its `run` handler; and the target of the run it was sent.
async fn run_swap(
    seed: u64,
    options: HandlerOptions,
    time: TimeMode,
) -> (
    Swap<impl Service<Future = ServiceBoxFuture> + Discoverable + Send + Sync + 'static>,
    String,
) {
    let swap = Swap::start(seed, "run", options, time, |slot, generation| {
        LashTurnImpl::new(
            slot,
            test_restate_authority_id(),
            generation,
            &crate::services::DEFAULT_NAMESPACE,
            crate::object_state::FleetView::default(),
        )
        .serve()
    })
    .await;
    let request = ShiftRequest {
        session: SessionId::from("folded"),
        request: ShiftRequestId::new("r-folded"),
        intended_lane: None,
    };
    let key = turn_invocation_key(&request, 0);
    swap.ingress
        .send_lash_workflow(
            "LashTurn",
            &key,
            "run",
            &RestateRunRequest {
                sender_generation: Some(lash_core::engine::BuildGeneration::for_test("G_a")),
                request,
                ordinal: 0,
                rules: Default::default(),
                draining: None,
            },
        )
        .await
        .expect("send the run run");
    (swap, format!("LashTurn/{key}/run"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_execution_replayed_under_another_generation_parks_at_its_first_step() {
    let (swap, target) = run_swap(0x3980_7a11, held_options(), TimeMode::auto()).await;
    swap.replays_parked_under_another_generation(
        &target,
        &[
            crate::JournalStepKind::RecordedEffect.journal_name(&format!(
                "lash:{}",
                shift_admission_replay_key(&ShiftRequestId::new("r-folded"), 0)
            )),
        ],
    )
    .await;
}

/// FIG-5081: a run refused under another generation pauses on the turn
/// handler's own retry ladder, not the server's. On a server with Restate's
/// stock ladder (500 ms doubling to a minute: about 64 s across a turn
/// handler's attempts) every attempt of the swapped build refuses the kept
/// journal, and the invocation pauses within half a minute of virtual time
/// after the held attempt died, ready for a redrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_refused_under_another_generation_pauses_within_half_a_minute() {
    let (swap, target) =
        run_swap(0x5081_7a11, crate::turn_handler_options(), TimeMode::Manual).await;
    let invocation = swap.held(&target).await;
    *swap.current.lock_recover() = Arc::clone(&swap.swapped);
    let died_at = swap.server.now_ms();
    assert!(swap.server.crash(&invocation.id), "crash the held attempt");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let paused = loop {
        let view = swap
            .server
            .invocations()
            .into_iter()
            .find(|view| view.id == invocation.id)
            .expect("the invocation");
        if view.status == "paused" {
            break view;
        }
        let timers = swap.server.timers();
        if !timers.is_empty() {
            assert!(
                timers
                    .iter()
                    .all(|timer| timer.invocation == invocation.id && timer.kind == "retry"),
                "only the refused run's retry is pending: {timers:#?}"
            );
            swap.server.fire_next_timer();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "`{target}` never paused: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let failure = paused
        .last_failure
        .clone()
        .expect("the refused attempt's failure")
        .1;
    assert!(
        failure.contains("RetiredGeneration"),
        "every retry refused the kept journal: {failure}"
    );
    assert_eq!(
        u64::from(paused.retry_count),
        crate::TURN_HANDLER_MAX_ATTEMPTS,
        "the run paused at its attempt budget: {paused:#?}"
    );
    let waited = swap.server.now_ms() - died_at;
    assert!(
        waited <= 30_000,
        "the refused run paused {waited} ms after its held attempt died, on the server's ladder"
    );
}
