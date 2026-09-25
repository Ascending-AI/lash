//! The frame switch's pending follow-on on the session head (ADR 0101 §3,
//! FIG-3542): recovery after a lost drive, chain depth, and the recovery bound.

use super::*;

/// Expire the session lane at each of the first `remaining` post-commit
/// deliveries: a worker that dies right after a commit, before the follow-on
/// that commit owes has run.
struct ExpireLeaseAfterEachRetainedCommit {
    clock: Arc<ManualClock>,
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
                .advance_ms(lash_core::facade_support::LeaseTimings::default().ttl_ms() + 1);
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A direct turn whose frame switch commits, and whose lane then dies before
/// the follow-on runs: the follow-on is left owed on the head (FIG-3542).
struct OwedFollowOn {
    backend: lash_core::Backend,
    runtime: LashRuntime,
    store: Arc<RecordingStore>,
    requests: Arc<std::sync::Mutex<Vec<lash_core::llm::types::LlmRequest>>>,
}

async fn owed_follow_on(
    switch_turn: &str,
    tasks: &[&str],
    replies: Vec<LlmResponse>,
    lapses: usize,
) -> OwedFollowOn {
    let backend = memory_backend().await;
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
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
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
        clock,
        remaining: std::sync::atomic::AtomicUsize::new(lapses),
    }));
    let run = runtime
        .stream_turn_with_agent_frames(
            TurnInput::text("switch frames"),
            TurnOptions::new(
                CancellationToken::new(),
                host_turn_scope(
                    &runtime.host.core,
                    &SessionId::from("root"),
                    &TurnId::from(switch_turn),
                ),
            ),
        )
        .await
        .expect("the committed switch returns with its follow-on owed");
    assert_eq!(run.turns.len(), 1);
    assert!(matches!(
        run.turns[0].outcome,
        TurnOutcome::AgentFrameSwitch { .. }
    ));
    OwedFollowOn {
        backend,
        runtime,
        store,
        requests,
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
    owed.runtime
        .stream_next_queued_work(TurnOptions::new(
            CancellationToken::new(),
            backend_queued_scope(
                &owed.backend,
                &SessionId::from("root"),
                &TurnId::from(drain_id),
            ),
        ))
        .await
        .expect("the drain runs")
        .ran()
        .expect("the drain answers a turn")
}

/// FIG-3542: a follow-on whose drive died after the switch commit is never a
/// queue row. The next drain runs it first, in its frame, under its own turn
/// id, exactly once; host input that arrived meanwhile waits behind it.
#[tokio::test]
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
#[tokio::test]
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
#[tokio::test]
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
