//! The frame switch's pending follow-on on the session head (ADR 0101 §3,
//! FIG-3542): recovery after a lost drive, chain depth, and the recovery bound.

use super::*;

const SEED: u64 = 0x5_b100;

/// Expire the session lane at each of the first `remaining` post-commit
/// deliveries: a worker that dies right after a commit, before the follow-on
/// that commit owes has run.
struct ExpireLeaseAfterEachRetainedCommit {
    clock: Arc<lash_core::testing::TestClock>,
    remaining: std::sync::atomic::AtomicUsize,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for ExpireLeaseAfterEachRetainedCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery
            && self
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        {
            self.clock
                .advance(lash_core::facade_support::LeaseTimings::default().ttl_ms() + 1);
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A direct turn whose frame switch commits, and whose lane then dies before
/// the follow-on runs: the follow-on is left owed on the head (FIG-3542).
struct OwedFollowOn {
    double: lash_restate_test::RestateTestBackend,
    backend: lash_core::Backend,
    runtime: Arc<tokio::sync::Mutex<LashRuntime>>,
    store: Arc<RecordingStore>,
    requests: Arc<std::sync::Mutex<Vec<lash_core::llm::types::LlmRequest>>>,
    clock: Arc<lash_core::testing::TestClock>,
}

async fn owed_follow_on(
    switch_turn: &str,
    tasks: &[&str],
    replies: Vec<LlmResponse>,
    lapses: usize,
) -> OwedFollowOn {
    Box::pin(owed_follow_on_with(
        switch_turn,
        tasks,
        replies,
        lapses,
        lash_core::QueuedWorkBatchingConfig::new(1),
    ))
    .await
}

/// [`owed_follow_on`] under `batching`.
async fn owed_follow_on_with(
    switch_turn: &str,
    tasks: &[&str],
    replies: Vec<LlmResponse>,
    lapses: usize,
    batching: lash_core::QueuedWorkBatchingConfig,
) -> OwedFollowOn {
    let double = kernel_double(
        SEED,
        lash_restate_test::ServerConfig::default().time(lash_restate_test::TimeMode::Manual),
    )
    .await;
    let backend = double.lash_backend();
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let replies = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        replies,
    )));
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |request| {
            captured.lock_recover().push(request);
            let reply = replies
                .lock_recover()
                .pop_front()
                .expect("a provider call the test scripted");
            async move { Ok(reply) }
        })
        .build();
    let clock = double.test_clock();
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        batching,
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(TerminalControlTool {
            controls: tasks
                .iter()
                .enumerate()
                .map(|(index, task)| lash_core::ToolControl::SwitchAgentFrame {
                    frame_key: lash_core::FrameKey::from_caller_material(&format!(
                        "fig3542-frame-{index}"
                    ))
                    .expect("non-empty caller material"),
                    initial_nodes: Vec::new(),
                    task: Some((*task).to_string()),
                })
                .collect(),
        }),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterEachRetainedCommit {
        clock: Arc::clone(&clock),
        remaining: std::sync::atomic::AtomicUsize::new(lapses),
    }));
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root").clone(),
            TurnId::from(switch_turn).clone(),
        ))
        .await
        .expect("open the scope's handler");
    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("switch frames"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the committed switch returns with its follow-on owed");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(run.turns.len(), 1);
    assert!(matches!(
        run.turns[0].outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    OwedFollowOn {
        double,
        backend,
        runtime: Arc::new(tokio::sync::Mutex::new(runtime)),
        store,
        requests,
        clock,
    }
}

fn text_reply(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

fn switch_reply(index: usize) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: format!("fig3542-switch-{index}"),
            tool_name: format!("terminal_tool_{index}"),
            input_json: "{}".to_string(),
            replay: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

async fn owed(store: &RecordingStore) -> Option<lash_core::store::PendingFollowOn> {
    lash_core::store::SessionCommitStore::load_session_head_meta(store)
        .await
        .expect("load the head")
        .expect("the session has a head")
        .pending_follow_on
}

async fn drain(owed: &mut OwedFollowOn, drain_id: &str) -> AssembledTurn {
    let handler = owed
        .double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            TurnId::from(drain_id),
        ))
        .await
        .expect("open the scope's handler");
    let turn = owed
        .runtime
        .lock()
        .await
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), handler.scoped()))
        .await
        .expect("the drain runs")
        .ran()
        .expect("the drain answers a turn");
    handler.close().await.expect("close the scope's handler");
    turn
}

/// FIG-3542: a follow-on whose drive died after the switch commit is never a
/// queue row. The next drain runs it first, in its frame, under its own turn
/// id, exactly once; host input that arrived meanwhile waits behind it.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig3542_an_owed_follow_on_runs_first_on_the_next_drain_and_once() {
    const TASK: &str = "the switched frame's task";
    let mut owed_run = Box::pin(owed_follow_on(
        "fig3542-switch",
        &[TASK],
        vec![
            switch_reply(0),
            text_reply("follow-on answer"),
            text_reply("host input answer"),
        ],
        1,
    ))
    .await;
    let follow_on = owed(&owed_run.store)
        .await
        .expect("the switch owes its follow-on");
    assert_eq!(
        follow_on.follow_on_turn_id,
        TurnId::from("fig3542-switch:agent-frame:1")
    );
    assert_eq!(follow_on.task, TASK);
    assert_eq!((follow_on.chain_depth, follow_on.attempts), (1, 0));
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            owed_run.store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("list queued work")
        .is_empty(),
        "a frame handoff is never a queue row: nothing can claim, reorder or cancel it"
    );
    let host_input = enqueue_idle_turn_input(
        owed_run.store.as_ref(),
        &SessionId::from("root"),
        "host input after the crash",
    )
    .await;

    let recovered = drain(&mut owed_run, "fig3542-drain-1").await;
    assert_eq!(recovered.assistant_output.safe_text, "follow-on answer");
    assert_eq!(
        recovered.state.current_frame_node_id.as_ref(),
        Some(&follow_on.frame_id),
        "the task runs in the frame it was handed to"
    );
    {
        let requests = owed_run.requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert!(request_contains_text(&requests[1], TASK));
        assert!(
            !request_contains_text(&requests[1], "host input after the crash"),
            "host input never overtakes the owed follow-on"
        );
    }
    assert_eq!(owed(&owed_run.store).await, None);
    assert!(
        lash_core::store::SessionCommitStore::committed_turn_exists(
            owed_run.store.as_ref(),
            &follow_on.follow_on_turn_id,
        )
        .await
        .expect("read the follow-on's receipt")
    );
    assert!(
        lash_core::store::TurnInputStore::list_pending_turn_inputs(
            owed_run.store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("list pending input")
        .iter()
        .any(|pending| pending.input.input_id == host_input.input_id),
        "the host input is still waiting behind the follow-on"
    );

    let answered = drain(&mut owed_run, "fig3542-drain-2").await;
    assert_eq!(answered.assistant_output.safe_text, "host input answer");
    let requests = owed_run.requests.lock_recover();
    assert_eq!(requests.len(), 3, "the follow-on ran exactly once");
    assert!(request_contains_text(
        &requests[2],
        "host input after the crash"
    ));
}

/// ADR 0101 §3: the chain depth is part of the owed fact, so a recovered
/// follow-on that switches again continues the chain instead of restarting it.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig3542_chain_depth_survives_recovery() {
    let mut owed_run = Box::pin(owed_follow_on(
        "fig3542-chain",
        &["first task", "second task"],
        vec![switch_reply(0), switch_reply(1), text_reply("chain answer")],
        2,
    ))
    .await;
    assert_eq!(
        owed(&owed_run.store).await.map(|owed| owed.chain_depth),
        Some(1)
    );
    // The recovered follow-on switches again, and its lane dies too.
    let switched = drain(&mut owed_run, "fig3542-chain-drain-1").await;
    assert!(matches!(
        switched.outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    let second = owed(&owed_run.store)
        .await
        .expect("the second link is owed");
    assert_eq!(
        second.follow_on_turn_id,
        TurnId::from("fig3542-chain:agent-frame:2")
    );
    assert_eq!(second.chain_depth, 2, "depth continues across the recovery");
    assert_eq!(
        second.attempts, 0,
        "a new link starts its own recovery count"
    );
    let answered = drain(&mut owed_run, "fig3542-chain-drain-2").await;
    assert_eq!(answered.assistant_output.safe_text, "chain answer");
    assert_eq!(owed(&owed_run.store).await, None);
}

/// ADR 0101 §3: a follow-on recovered past the host's bound commits as the
/// failed turn `FollowOnRecoveryExhausted`, with its task as delivered input,
/// and the head no longer owes it. Nothing resets the count.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig3542_an_exhausted_follow_on_commits_failed_and_clears_the_head() {
    const TASK: &str = "a task that crashes its worker";
    let mut owed_run = Box::pin(owed_follow_on(
        "fig3542-exhaust",
        &[TASK],
        vec![switch_reply(0), text_reply("host input answer")],
        1,
    ))
    .await;
    let follow_on = owed(&owed_run.store).await.expect("owed");
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = owed_run.store.clone();
    let lane = lash_core::testing::store_fixtures::claim_session_execution_lease_for_test(
        &runtime_store,
        &SessionId::from("root"),
        "fig3542-crashing-worker",
    )
    .await;
    for _ in 0..lash_core::store::DEFAULT_MAX_FOLLOW_ON_RECOVERIES {
        lash_core::store::SessionCommitStore::raise_pending_follow_on_attempts(
            owed_run.store.as_ref(),
            &lane.authority(),
            &follow_on.follow_on_turn_id,
        )
        .await
        .expect("each crashed recovery raised the count");
    }
    lash_core::store::SessionExecutionLeaseStore::release_session_execution_lease(
        owed_run.store.as_ref(),
        &lane.authority(),
    )
    .await
    .expect("release the crashed worker's lane");
    enqueue_idle_turn_input(
        owed_run.store.as_ref(),
        &SessionId::from("root"),
        "host input behind the exhausted follow-on",
    )
    .await;

    let exhausted = drain(&mut owed_run, "fig3542-exhaust-drain-1").await;
    assert!(matches!(exhausted.outcome, TurnOutcome::Stopped(_)));
    assert!(
        exhausted.errors.iter().any(|issue| issue.code
            == Some(lash_core::TurnFailureCode::FollowOnRecoveryExhausted.into())),
        "the terminal carries the typed exhaustion: {:?}",
        exhausted.errors
    );
    assert_eq!(
        owed_run.requests.lock_recover().len(),
        1,
        "an exhausted follow-on never runs"
    );
    assert_eq!(owed(&owed_run.store).await, None);
    assert!(
        lash_core::store::SessionCommitStore::committed_turn_exists(
            owed_run.store.as_ref(),
            &follow_on.follow_on_turn_id,
        )
        .await
        .expect("read the follow-on's receipt"),
        "the failed turn is the follow-on's terminal record"
    );
    let answered = drain(&mut owed_run, "fig3542-exhaust-drain-2").await;
    assert_eq!(answered.assistant_output.safe_text, "host input answer");
    assert!(
        request_contains_text(&owed_run.requests.lock_recover()[1], TASK),
        "the task stays in the transcript as the failed turn's delivered input"
    );
}

/// Panics as the first prompt is built: a worker that dies inside a turn,
/// before the turn's first effect.
#[derive(Default)]
struct PanicOnceAtPromptBuild {
    fired: std::sync::atomic::AtomicBool,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicOnceAtPromptBuild {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            panic!("injected crash as the recovered follow-on's prompt is built");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// One drive of the session under `request`, recovering the lane first.
async fn drive(
    owed: &mut OwedFollowOn,
    request: &str,
) -> Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
    let handler = owed
        .double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            TurnId::from(request),
        ))
        .await
        .expect("open the scope's handler");
    let controller = handler.scoped();
    let mut runtime = owed.runtime.lock().await;
    let request = lash_core::engine::DriveRequest {
        session: SessionId::from("root"),
        request: lash_core::engine::DriveRequestId::new(request),
        build_generation: runtime.host.core.backend().build_generation().clone(),
    };
    let outcome = Box::pin(lash_core::drive::drive_session(
        &mut runtime,
        &controller,
        &request,
    ))
    .await;
    drop(runtime);
    drop(controller);
    handler.close().await.expect("close the scope's handler");
    outcome
}

/// FIG-3542, FIG-3600: the session drive recovers a follow-on the head owes
/// at admission, as a root of its own, before the input waiting behind it.
/// A direct turn that meets the owed follow-on runs nothing and answers the
/// typed `QueuedRunPending` hold; the drive then answers its input after the
/// follow-on, each once.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig3542_the_session_drive_recovers_an_owed_follow_on_before_the_input_behind_it()
 {
    const TASK: &str = "the switched frame's task";
    let mut owed_run = Box::pin(owed_follow_on(
        "fig3542-drive",
        &[TASK],
        vec![
            switch_reply(0),
            text_reply("follow-on answer"),
            text_reply("direct input answer"),
        ],
        1,
    ))
    .await;
    let follow_on = owed(&owed_run.store)
        .await
        .expect("the switch owes its follow-on");

    let direct = TurnId::from("fig3542-drive-direct");
    let handler = owed_run
        .double
        .open_handler(AdmittedScope::turn(SessionId::from("root"), direct.clone()))
        .await
        .expect("open the scope's handler");
    let held = owed_run
        .runtime
        .lock()
        .await
        .run_turn_assembled(
            TurnInput::text("direct input behind the follow-on"),
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect_err("a direct turn never recovers the follow-on");
    handler.close().await.expect("close the scope's handler");
    assert_eq!(
        held.code,
        lash_core::RuntimeErrorCode::QueuedRunPending,
        "the direct turn waits behind the follow-on: {held:?}"
    );
    assert_eq!(owed_run.requests.lock_recover().len(), 1);
    assert_eq!(owed(&owed_run.store).await, Some(follow_on.clone()));

    let outcome = drive(&mut owed_run, "fig3542-drive-engine")
        .await
        .expect("the session drive runs to its stop");
    let roots: Vec<_> = outcome
        .ran
        .iter()
        .map(|ran| match ran {
            lash_core::engine::RootOutcome::Committed { root, .. } => root.clone(),
            other => panic!("every admitted root commits: {other:?}"),
        })
        .collect();
    assert_eq!(
        roots,
        [
            TurnId::from(format!("follow-on:{}#0", follow_on.follow_on_turn_id)),
            direct,
        ],
        "the follow-on's recovery is admitted first, then the input behind it"
    );
    assert_eq!(outcome.stop, lash_core::engine::DriveStop::Idle);
    assert_eq!(owed(&owed_run.store).await, None);
    assert!(
        lash_core::store::SessionCommitStore::committed_turn_exists(
            owed_run.store.as_ref(),
            &follow_on.follow_on_turn_id,
        )
        .await
        .expect("read the follow-on's receipt")
    );
    let requests = owed_run.requests.lock_recover();
    assert_eq!(
        requests.len(),
        3,
        "the follow-on and the input each ran once"
    );
    assert!(request_contains_text(&requests[1], TASK));
    assert!(!request_contains_text(
        &requests[1],
        "direct input behind the follow-on"
    ));
    assert!(request_contains_text(
        &requests[2],
        "direct input behind the follow-on"
    ));
}

/// FIG-3542, FIG-3600: the recovery count a drive admission records is the
/// one its root raises from. A recovery that crashed after its raise and is
/// redriven continues the recovery it raised for; it neither raises again
/// nor spends the bound, so a follow-on the host allows one recovery still
/// runs.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig3542_a_redriven_recovery_raises_the_recorded_count_once() {
    const TASK: &str = "a task whose first recovery crashes";
    let owed_run = Box::pin(owed_follow_on_with(
        "fig3542-recount",
        &[TASK],
        vec![switch_reply(0), text_reply("follow-on answer")],
        1,
        lash_core::QueuedWorkBatchingConfig::new(1).with_max_follow_on_recoveries(1),
    ))
    .await;
    let follow_on = owed(&owed_run.store)
        .await
        .expect("the switch owes its follow-on");
    assert_eq!(follow_on.attempts, 0);

    // The recovery's worker dies as its follow-on's prompt is built: after
    // the raise, before any effect. On the double a worker death is the
    // handler's attempt failing retryably and the invocation replaying into
    // the redrive — a fresh drive could never adopt another invocation's
    // half-journaled turn, so `run_crashed_then_redriven` is the crash
    // simulation.
    owed_run
        .runtime
        .lock()
        .await
        .set_turn_phase_probe(Arc::new(PanicOnceAtPromptBuild::default()));
    let build_generation = owed_run
        .runtime
        .lock()
        .await
        .host
        .core
        .backend()
        .build_generation()
        .clone();
    let driven: Arc<Mutex<Option<lash_core::engine::DriveOutcome>>> = Arc::new(Mutex::new(None));
    let attempt_count = Arc::new(AtomicUsize::new(0));
    let attempt: lash_restate_test::HandlerAttempt = {
        let runtime = Arc::clone(&owed_run.runtime);
        let driven = Arc::clone(&driven);
        let attempt_count = Arc::clone(&attempt_count);
        let store = Arc::clone(&owed_run.store);
        let clock = Arc::clone(&owed_run.clock);
        Arc::new(move |scoped| {
            let runtime = Arc::clone(&runtime);
            let driven = Arc::clone(&driven);
            let attempt_count = Arc::clone(&attempt_count);
            let store = Arc::clone(&store);
            let clock = Arc::clone(&clock);
            let build_generation = build_generation.clone();
            Box::pin(async move {
                if attempt_count.fetch_add(1, Ordering::SeqCst) == 1 {
                    // The replayed attempt re-enters after the crash: the
                    // raise it replays is the one its crash recorded, and
                    // the crashed worker's lane has lapsed.
                    assert_eq!(
                        owed(&store).await.map(|owed| owed.attempts),
                        Some(1),
                        "the crashed recovery raised the count before its first effect"
                    );
                    clock.advance(lash_core::facade_support::LeaseTimings::default().ttl_ms() + 1);
                }
                let request = lash_core::engine::DriveRequest {
                    session: SessionId::from("root"),
                    request: lash_core::engine::DriveRequestId::new("fig3542-recount-drive"),
                    build_generation,
                };
                let mut runtime = runtime.lock().await;
                if let Ok(outcome) =
                    lash_core::drive::drive_session(&mut runtime, &scoped, &request).await
                {
                    *driven.lock_recover() = Some(outcome);
                }
            })
        })
    };
    // The crashed attempt's retry waits on a backoff timer and this double
    // keeps manual time, so pump pending timers while the crash-and-redrive
    // pair runs — the lane lapse the test wants is one of the timers fired.
    let pump = {
        let double = owed_run.double.clone();
        lash_core::task::spawn(async move {
            loop {
                if double.server().fire_next_timer().is_none() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        })
    };
    let redriven = owed_run
        .double
        .run_crashed_then_redriven(
            AdmittedScope::queue_drain(
                SessionId::from("root"),
                TurnId::from("fig3542-recount-drive"),
            ),
            Arc::clone(&attempt),
            attempt,
        )
        .await;
    pump.abort();
    redriven.expect("the crashed recovery's invocation redrives");

    let outcome = driven
        .lock_recover()
        .take()
        .expect("the redriven drive ran to its stop");
    assert!(
        matches!(
            outcome.ran.first(),
            Some(lash_core::engine::RootOutcome::Committed {
                outcome: TurnOutcome::Finished(_),
                ..
            })
        ),
        "the redrive continues its own recovery, within the bound: {outcome:?}"
    );
    assert_eq!(owed(&owed_run.store).await, None);
    assert_eq!(
        owed_run.requests.lock_recover().len(),
        2,
        "the follow-on answered on the redrive"
    );
}

/// FIG-3848 (B1): a root whose frame switch committed owes its follow-on, and
/// parks as that follow-on is recovered. The park names the logical root —
/// the one the follow-on's commit ends — so when a redrive under a restored
/// build recovers the follow-on and commits it, that commit clears the park:
/// the root is never left parked and terminal at once.
#[tokio::test(flavor = "multi_thread")]
pub(super) async fn fig3848_a_parked_root_owing_a_follow_on_is_cleared_when_the_follow_on_commits()
{
    let mut owed_run = Box::pin(owed_follow_on(
        "fig3848-parked",
        &["the switched frame's task"],
        vec![switch_reply(0), text_reply("follow-on answer")],
        1,
    ))
    .await;
    let follow_on = owed(&owed_run.store)
        .await
        .expect("the switch owes its follow-on");
    let root = lash_core::store::QueuedRunPosition::split_turn_id(&follow_on.follow_on_turn_id).0;
    assert_eq!(root, TurnId::from("fig3848-parked"));
    assert_ne!(
        follow_on.follow_on_turn_id, root,
        "the follow-on is a later physical turn"
    );
    let session = SessionId::from("root");
    let store: Arc<dyn lash_core::store::RuntimePersistence> = owed_run.store.clone();
    let park = store
        .record_turn_park(&lash_core::store::TurnParkWrite::refusal(
            session.clone(),
            root.clone(),
            lash_core::store::ParkReason::ReplayDivergence {
                message: "the follow-on diverged under another build".into(),
            },
            1_000,
        ))
        .await
        .expect("the root parks as its follow-on is recovered");
    let factory = owed_run.backend.session_store_factory();
    let redrive = factory
        .open_root_intent(
            &lash_core::store::RootIntentRequest {
                session_id: session.clone(),
                root: root.clone(),
                park: park.park_id,
                verb: lash_core::store::RootVerb::Redrive,
            },
            1_000,
        )
        .await
        .expect("redrive under the restored build");
    lash_core::drive::apply_control_intent(
        factory.as_ref(),
        &lash_core::engine::NoEngineControl,
        &lash_core::NoSessionWork::new(),
        &lash_core::engine::NoScopeClose,
        &redrive,
        owed_run.clock.as_ref(),
    )
    .await
    .expect("apply the redrive");

    let outcome = drive(&mut owed_run, "fig3848-redrive")
        .await
        .expect("the redriven drive runs to its stop");
    assert_eq!(outcome.stop, lash_core::engine::DriveStop::Idle);
    assert_eq!(owed(&owed_run.store).await, None);
    assert!(
        factory
            .root_terminal(&session, &root)
            .await
            .expect("read the root's terminal")
            .is_some(),
        "the follow-on's commit ends the root"
    );
    assert_eq!(
        store.load_turn_park(&session).await.expect("read the park"),
        None,
        "the follow-on's commit clears the root's park"
    );
}
