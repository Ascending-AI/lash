use super::*;

#[tokio::test]
pub(super) async fn turn_stream_finish_returns_committed_assistant_prose() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(semantic_group_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("turn-stream-last-group").open().await?;
    let mut stream = session.turn(TurnInput::text("stream groups")).stream()?;

    let mut activities = Vec::new();
    while let Some(activity) = stream.next_activity().await {
        activities.push(activity?);
    }
    let result = stream.finish().await?;

    assert_eq!(assistant_prose(&activities), "firstsecond");
    assert_eq!(result.assistant_message(), Some("first\n\nsecond"));
    assert_eq!(result.assistant_output.safe_text, "first\n\nsecond");
    assert!(result.is_success());
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_run_collects_activities_and_returns_committed_assistant_prose()
-> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(semantic_group_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("turn-run-last-group").open().await?;

    let collected = session.turn(TurnInput::text("run groups")).run().await?;

    assert_eq!(assistant_prose(&collected.activities), "firstsecond");
    assert_eq!(
        collected.result.assistant_message(),
        Some("first\n\nsecond")
    );
    assert_eq!(
        collected.result.assistant_output.safe_text,
        "first\n\nsecond"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn retry_status_streams_as_semantic_turn_event() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(retry_once_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("retry-status").open().await?;
    let events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("hello"))
        .stream_to(&events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let retry = events
        .snapshot()
        .await
        .into_iter()
        .find(|event| matches!(&event.event, TurnEvent::RetryStatus { .. }))
        .expect("retry status event");
    let TurnEvent::RetryStatus {
        wait_seconds,
        attempt,
        max_attempts,
        reason,
    } = retry.event
    else {
        unreachable!();
    };
    assert_eq!(wait_seconds, 0);
    assert_eq!(attempt, 1);
    assert_eq!(max_attempts, 2);
    assert!(reason.contains("retry me"));
    Ok(())
}

#[tokio::test]
pub(super) async fn control_turn_accepts_prebuilt_turn_input() -> Result<()> {
    let core = standard_core();
    let session = core.session("raw-turn").open().await?;

    let result = session
        .turn(TurnInput::text("raw input"))
        .turn_id("host-trace-id")
        .run()
        .await?;

    assert_eq!(assistant_prose(&result.activities), "echo: raw input");
    Ok(())
}

#[tokio::test]
pub(super) async fn queued_input_acceptance_streams_semantic_ack_with_id() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(checkpoint_gated_provider(entered_tx, release_rx))
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("queued-input").open().await?;
    let events = Arc::new(RecordingEvents::default());
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let turn = tokio::spawn(async move {
        turn_session
            .turn(TurnInput::text("hello"))
            .turn_id("queued-input-turn")
            .stream_to(turn_events.as_ref())
            .await
    });

    entered_rx.await.expect("provider entered first call");
    session
        .admin()
        .injection()
        .inject_turn_input(
            &TurnId::from("queued-input-turn"),
            Some("queue-1".to_string()),
            lash_core::PluginMessage::text(lash_core::MessageRole::User, "queued follow-up"),
        )
        .await?;
    release_tx.send(()).expect("release provider");
    let result = turn.await.expect("turn task")?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let events = events.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        &event.event,
        TurnEvent::QueuedInputAccepted {
            applications,
        } if applications.iter().any(|application| {
            application.source_key.as_deref() == Some("injection:queue-1")
                && application.turn_id.as_str() == "queued-input-turn"
                && application.checkpoint
                    == Some(lash_core::CheckpointKind::BeforeCompletion)
                && application.committed_message_id
                    == format!("m_ingress_{}", application.input_id)
        })
    )));
    let prose = events
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert!(prose.contains("after queued follow-up"));
    Ok(())
}

#[tokio::test]
pub(super) async fn pre_cancelled_token_yields_cancelled_outcome() -> Result<()> {
    let core = standard_core();
    let session = core.session("pre-cancelled").open().await?;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let output = session
        .turn(TurnInput::text("never runs"))
        .cancel(cancel)
        .run()
        .await?;

    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    let evidence = output
        .result
        .cancellation()
        .expect("local token cancellation evidence");
    assert_eq!(evidence.origin, None);
    assert_eq!(evidence.reason, None);
    Ok(())
}

#[tokio::test]
pub(super) async fn local_cancel_token_preserves_explicit_origin_hint() -> Result<()> {
    let core = standard_core();
    let session = core.session("pre-cancelled-with-origin").open().await?;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let output = session
        .turn(TurnInput::text("never runs"))
        .cancel_with_origin(cancel, Some("shutdown".to_string()))
        .run()
        .await?;

    assert!(matches!(
        output.result.cancellation(),
        Some(lash_core::facade_support::TurnCancellationEvidence {
            origin: Some(origin),
            ..
        }) if origin == "shutdown"
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn cancel_running_turns_stops_inflight_turn() -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let started_tx = Arc::clone(&started_tx);
            async move {
                if let Some(tx) = started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                // Hang until the turn is cancelled out from under us.
                std::future::pending::<()>().await;
                unreachable!("provider future should be dropped by cancellation")
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let session = core.session("cancel-inflight").open().await?;
    let stopper = session.clone();

    let externally_owned_cancel = CancellationToken::new();
    let stream = session
        .turn(TurnInput::text("hang forever"))
        .cancel_with_origin(externally_owned_cancel, Some("shutdown".to_string()))
        .stream()?;
    started_rx.await.expect("provider reached");
    assert_eq!(
        stopper.cancel_running_turns_with_origin(Some("user".to_string())),
        1
    );

    let result = stream.finish().await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert!(matches!(
        result.cancellation(),
        Some(lash_core::facade_support::TurnCancellationEvidence {
            origin: Some(origin),
            ..
        }) if origin == "user"
    ));
    // The registry entry is gone once the turn finished.
    assert_eq!(stopper.cancel_running_turns(), 0);
    Ok(())
}

pub(super) fn hang_on_signal_provider(
    started_tx: Arc<StdMutex<Vec<oneshot::Sender<()>>>>,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let started_tx = Arc::clone(&started_tx);
            async move {
                let user_text = last_user_text(&request);
                if user_text.contains("hang") {
                    if let Some(tx) = started_tx.lock_recover().pop() {
                        let _ = tx.send(());
                    }
                    // Hang until the turn is cancelled out from under us.
                    std::future::pending::<()>().await;
                    unreachable!("provider future should be dropped by cancellation")
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle()
}

pub(super) async fn wait_for_stable_build_count(builds: &AtomicUsize) -> usize {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut observed = builds.load(Ordering::SeqCst);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
            let current = builds.load(Ordering::SeqCst);
            if current == observed {
                return current;
            }
            observed = current;
        }
    })
    .await
    .expect("unknown-claimability hydration reaches an idle steady state")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn next_turn_notification_during_a_live_turn_has_bounded_hydrations() -> Result<()>
{
    let builds = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let first_entered = Arc::clone(&first_entered);
            let release_first = Arc::clone(&release_first);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                let first_entered = Arc::clone(&first_entered);
                let release_first = Arc::clone(&release_first);
                async move {
                    if provider_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        first_entered.notify_one();
                        release_first
                            .acquire()
                            .await
                            .expect("release semaphore remains open")
                            .forget();
                    }
                    Ok(text_response("live turn complete"))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
            builds: Arc::clone(&builds),
        }))
        .queued_work_execution_concurrency(1)
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("queued-work-live-lease").open().await?;
    let entered = first_entered.notified();
    let foreground = session.turn(TurnInput::text("foreground turn")).stream()?;
    entered.await;
    let baseline_builds = builds.load(Ordering::SeqCst);

    core.enqueue_turn_input(
        "queued-work-live-lease",
        TurnInput::text("queued while foreground owns the lease"),
        lash_core::TurnInputIngress::NextTurn,
        Some("queued-during-live-turn".to_string()),
    )
    .await?;
    wait_for_stable_build_count(&builds).await;

    let hydrations = builds
        .load(Ordering::SeqCst)
        .saturating_sub(baseline_builds);
    assert!(
        hydrations <= 5,
        "one live-lease notification must keep hydrations bounded, got {hydrations}"
    );

    release_first.add_permits(1);
    let foreground_result =
        tokio::time::timeout(std::time::Duration::from_secs(2), foreground.finish())
            .await
            .expect("foreground turn completes after release");
    match foreground_result {
        Ok(_) => {}
        Err(EmbedError::Runtime(error)) => {
            assert_eq!(error.code, lash_core::RuntimeErrorCode::StoreCommitFailed);
            assert!(
                error.message.contains("store head revision conflict"),
                "the only accepted concurrent-writer loss is head CAS, got: {error}"
            );
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[tokio::test]
pub(super) async fn create_only_factory_returns_to_idle_after_draining_unknown_claimability()
-> Result<()> {
    const MAX_TRANSIENT_HYDRATIONS_PER_NOTIFICATION: usize =
        lash_core::runtime::QUEUED_WORK_MAX_TRANSIENT_ATTEMPTS;

    let builds = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    provider_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(text_response("create-only queued work complete"))
                }
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(CreateOnlySessionStoreFactory {
        inner: lash_core::facade_support::InMemorySessionStoreFactory::new(),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .with_native_queued_work()
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
            builds: Arc::clone(&builds),
        }))
        .queued_work_execution_concurrency(1)
        .build(crate::testing::runtime_lease_owner())?;
    let baseline_builds = builds.load(Ordering::SeqCst);

    core.enqueue_turn_input(
        "create-only-factory-idles",
        TurnInput::text("queued through create-only factory"),
        lash_core::TurnInputIngress::NextTurn,
        Some("create-only-idle".to_string()),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while provider_calls.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the conservatively admitted queued turn reaches the provider");

    let request = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("create-only-factory-idles"),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store = lash_core::SessionStoreFactory::open_existing_store(&store_factory.inner, &request)
        .await
        .expect("open the create-only factory's inner store")
        .expect("the queued session exists");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let read = store
                .load_session()
                .await
                .expect("load the queued session")
                .expect("the queued session state exists");
            if read
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.turn_state.turn_index >= 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queued turn commits durably");

    let first_settled_builds = wait_for_stable_build_count(&builds).await;
    let first_hydrations = first_settled_builds.saturating_sub(baseline_builds);
    assert!(
        (1..=MAX_TRANSIENT_HYDRATIONS_PER_NOTIFICATION).contains(&first_hydrations),
        "one conservative notification must use one bounded hydration ladder, got {first_hydrations}"
    );

    core.enqueue_turn_input(
        "create-only-factory-idles",
        TurnInput::text("queued after the create-only factory idled"),
        lash_core::TurnInputIngress::NextTurn,
        Some("create-only-rearm".to_string()),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while provider_calls.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("enqueue and notify re-arm the idled create-only factory");

    let second_settled_builds = wait_for_stable_build_count(&builds).await;
    let second_hydrations = second_settled_builds.saturating_sub(first_settled_builds);
    assert!(
        (1..=MAX_TRANSIENT_HYDRATIONS_PER_NOTIFICATION).contains(&second_hydrations),
        "the re-armed notification must use one fresh bounded hydration ladder, got {second_hydrations}"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn native_queued_work_burst_reuses_one_hydrated_runtime() -> Result<()> {
    const INPUTS: usize = 8;
    let builds = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let seen_inputs = Arc::new(StdMutex::new(Vec::<String>::new()));
    let first_entered = Arc::new(tokio::sync::Notify::new());
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let seen_inputs = Arc::clone(&seen_inputs);
            let first_entered = Arc::clone(&first_entered);
            let release_first = Arc::clone(&release_first);
            move |request| {
                let provider_calls = Arc::clone(&provider_calls);
                let seen_inputs = Arc::clone(&seen_inputs);
                let first_entered = Arc::clone(&first_entered);
                let release_first = Arc::clone(&release_first);
                async move {
                    seen_inputs.lock_recover().push(last_user_text(&request));
                    if provider_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        first_entered.notify_one();
                        release_first
                            .acquire()
                            .await
                            .expect("release semaphore remains open")
                            .forget();
                    }
                    Ok(text_response("queued work complete"))
                }
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .with_native_queued_work()
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
            builds: Arc::clone(&builds),
        }))
        .build(crate::testing::runtime_lease_owner())?;
    assert_eq!(builds.load(Ordering::SeqCst), 1, "build-time validation");

    let entered = first_entered.notified();
    core.enqueue_turn_input(
        "queued-work-hydration-burst",
        TurnInput::text("queued input 0"),
        lash_core::TurnInputIngress::NextTurn,
        Some("queued-input-0".to_string()),
    )
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(1), entered)
        .await
        .expect("the first queued turn reaches the provider");
    for index in 1..INPUTS {
        core.enqueue_turn_input(
            "queued-work-hydration-burst",
            TurnInput::text(format!("queued input {index}")),
            lash_core::TurnInputIngress::NextTurn,
            Some(format!("queued-input-{index}")),
        )
        .await?;
    }

    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "the blocked run must be the only runtime hydration admitted for the session burst"
    );
    release_first.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let observed = seen_inputs.lock_recover().join("\n");
            if (0..INPUTS).all(|index| observed.contains(&format!("queued input {index}"))) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the hydrated runtime drains every queued input");
    let request = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from("queued-work-hydration-burst"),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let store =
        lash_core::SessionStoreFactory::open_existing_store(store_factory.as_ref(), &request)
            .await
            .expect("open the queued-work burst store")
            .expect("the queued-work burst store exists");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let read = store
                .load_session()
                .await
                .expect("load queued-work burst state")
                .expect("queued-work burst state exists");
            if read
                .checkpoint
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.turn_state.turn_index >= 2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the hydrated runtime durably commits the full burst");

    let observed = seen_inputs.lock_recover().join("\n");
    let mut previous = 0;
    for index in 0..INPUTS {
        let position = observed
            .find(&format!("queued input {index}"))
            .expect("every queued input reached the provider");
        assert!(position >= previous, "queued input order changed");
        previous = position;
    }
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "one hydrated runtime must serve the whole ordered burst"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn cancel_running_turns_sweeps_lock_queued_turns() -> Result<()> {
    // One opened session serializes turn execution on the runtime writer
    // lock, but a second turn is already registered while it waits for that
    // lock. A stop sweep must reach both: the executing turn aborts, and the
    // parked turn sees its cancelled token the moment it acquires the lock
    // instead of starting a fresh provider call after the user pressed stop.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let session = core.session("cancel-lock-queue").open().await?;

    let first = session.turn(TurnInput::text("hang one")).stream()?;
    started_rx.await.expect("first turn reached the provider");
    let second = session.turn(TurnInput::text("hang two")).stream()?;

    assert_eq!(session.cancel_running_turns(), 2);

    let first = first.finish().await?;
    let second = second.finish().await?;
    assert!(matches!(
        first.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert!(matches!(
        second.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert_eq!(session.cancel_running_turns(), 0);
    Ok(())
}

#[tokio::test]
pub(super) async fn cancel_running_turns_does_not_cross_separately_opened_handles() -> Result<()> {
    // Each open() builds its own runtime and cancel registry; the documented
    // scope of cancel_running_turns is the opened handle and its clones.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let handle_a = core.session("cancel-scope").open().await?;
    let handle_b = core.session("cancel-scope").open().await?;

    let hanging = handle_a.turn(TurnInput::text("hang here")).stream()?;
    started_rx.await.expect("turn reached the provider");

    // The other handle has its own registry: nothing to cancel there.
    assert_eq!(handle_b.cancel_running_turns(), 0);
    assert_eq!(handle_a.cancel_running_turns(), 1);

    let result = hanging.finish().await?;
    assert!(matches!(
        result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));

    // The untouched handle keeps working.
    let output = handle_b.turn(TurnInput::text("plain")).run().await?;
    assert_eq!(output.assistant_message(), Some("echo: plain"));
    Ok(())
}

#[tokio::test]
pub(super) async fn provider_abort_cancellation_commits_the_evidence_the_turn_streamed()
-> Result<()> {
    // No host cancel request explains this stop, so the evidence is minted
    // inside the sans-IO machine and rides the streamed `TurnOutcome`. The
    // committed report must carry that same request id: one cancellation is
    // one identity, never a machine-side id plus a commit-side `internal:
    // {turn_id}` mint.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let session = core.session("provider-abort-evidence").open().await?;

    let hanging = session
        .turn(TurnInput::text("hang here"))
        .turn_id("abort-evidence-turn")
        .stream()?;
    started_rx.await.expect("turn reached the provider");
    assert_eq!(session.cancel_running_turns(), 1);

    let result = hanging.finish().await?;
    let evidence = result
        .cancellation()
        .expect("a cancelled turn names the request that stopped it");
    assert!(
        evidence
            .request_id
            .starts_with("internal:provider-cancelled:"),
        "committed report must carry the machine-minted evidence, got `{}`",
        evidence.request_id
    );
    assert_ne!(evidence.request_id, "internal:abort-evidence-turn");
    Ok(())
}

#[tokio::test]
pub(super) async fn cancel_running_turns_reaches_queued_turn_drains() -> Result<()> {
    // Queued drains register in the same session registry as foreground
    // turns, so a stop sweep reaches them too.
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let session = core.session("cancel-queued-drain").open().await?;
    session
        .enqueue(TurnInput::text("hang queued"))
        .send()
        .await?;

    let drainer = session.clone();
    let drain = tokio::spawn(async move { drainer.queued_turn().run().await });
    started_rx.await.expect("queued drain reached the provider");
    assert_eq!(
        session.cancel_running_turns_with_origin(Some("user".to_string())),
        1
    );

    let output = drain
        .await
        .expect("drain task")?
        .expect("queued drain should produce a turn");
    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert!(matches!(
        output.result.cancellation(),
        Some(lash_core::facade_support::TurnCancellationEvidence {
            origin: Some(origin),
            ..
        }) if origin == "user"
    ));
    Ok(())
}

pub(super) async fn assert_session_turn_cancel_disposition(
    session_id: &SessionId,
    turn_id: &TurnId,
    disposition: lash_core::facade_support::TurnCancelDisposition,
    use_legacy_method: bool,
) -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(session_id).open().await?;
    let stream = session
        .turn(TurnInput::text("hang until session cancellation"))
        .turn_id(turn_id)
        .stream()?;
    started_rx.await.expect("turn reached the provider");

    let undelivered = session
        .enqueue(TurnInput::text("undelivered active-turn input"))
        .id(format!("{session_id}:undelivered"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await?;
    let request_id = format!("{session_id}:cancel");
    let receipt = if use_legacy_method {
        session
            .request_turn_cancel(
                turn_id,
                request_id.clone(),
                Some("test-host".to_string()),
                Some("legacy defer default".to_string()),
            )
            .await?
    } else {
        session
            .request_turn_cancel_with_disposition(
                turn_id,
                request_id.clone(),
                Some("test-host".to_string()),
                Some("explicit disposition".to_string()),
                disposition,
            )
            .await?
    };
    assert!(matches!(
        receipt.outcome,
        lash_core::facade_support::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == request_id && evidence.undelivered == disposition
    ));

    let interrupted = stream.finish().await?;
    assert!(matches!(
        interrupted.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert_eq!(interrupted.cancel_input_outcome.affected_inputs.len(), 1);
    let affected = &interrupted.cancel_input_outcome.affected_inputs[0];
    assert_eq!(affected.input_id, undelivered.input_id);
    assert_eq!(affected.disposition, disposition);

    let store = store_factory
        .raw_store_for_testing(session_id)
        .expect("opened session retains its in-memory store");
    let pending = session.pending_turn_inputs().await?;
    match disposition {
        lash_core::facade_support::TurnCancelDisposition::Drop => {
            let raw_pending = store.raw_pending_turn_inputs_for_testing();
            let dropped = raw_pending
                .iter()
                .find(|(input_id, ..)| input_id == &undelivered.input_id)
                .expect("dropped input retains terminal lifecycle evidence");
            assert_eq!(dropped.2, lash_core::TurnInputState::Cancelled);
            assert!(
                pending
                    .iter()
                    .all(|input| input.input_id != undelivered.input_id),
                "dropped input must be absent from next-turn ingress"
            );
        }
        lash_core::facade_support::TurnCancelDisposition::Defer => {
            let deferred = pending
                .iter()
                .find(|input| input.input_id == undelivered.input_id)
                .expect("deferred input remains available for the next turn");
            assert_eq!(deferred.state, lash_core::TurnInputState::DeferredNextTurn);
            assert!(matches!(
                deferred.ingress,
                lash_core::TurnInputIngress::NextTurn
            ));
        }
    }
    let record = lash_core::store::TurnInputStore::turn_cancel_request(
        store.as_ref(),
        &lash_core::facade_support::TurnAddress::new(session_id, turn_id),
    )
    .await?
    .expect("cancelled turn has a durable request record");
    let settled = record.outcome.expect("cancel request record is settled");
    assert_eq!(settled.affected_inputs, vec![affected.clone()]);
    assert_eq!(record.request.undelivered, disposition);
    Ok(())
}

#[tokio::test]
pub(super) async fn request_turn_cancel_with_disposition_drops_undelivered_active_input()
-> Result<()> {
    assert_session_turn_cancel_disposition(
        &SessionId::from("session-cancel-explicit-drop"),
        &TurnId::from("session-cancel-explicit-drop:turn"),
        lash_core::facade_support::TurnCancelDisposition::Drop,
        false,
    )
    .await
}

#[tokio::test]
pub(super) async fn request_turn_cancel_legacy_method_defers_undelivered_active_input() -> Result<()>
{
    assert_session_turn_cancel_disposition(
        &SessionId::from("session-cancel-legacy-defer"),
        &TurnId::from("session-cancel-legacy-defer:turn"),
        lash_core::facade_support::TurnCancelDisposition::Defer,
        true,
    )
    .await
}

#[tokio::test]
pub(super) async fn active_steer_after_last_call_defers_to_next_turn_first_call() -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx = Arc::new(StdMutex::new(Some(started_tx)));
    let requests = Arc::new(StdMutex::new(Vec::<(
        String,
        Vec<lash_core::llm::types::LlmMessage>,
    )>::new()));
    let captured_requests = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let started_tx = Arc::clone(&started_tx);
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                let user_text = last_user_text(&request);
                captured_requests
                    .lock_recover()
                    .push((user_text.clone(), request.messages.clone()));
                if user_text == "primary hangs" {
                    if let Some(tx) = started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<()>().await;
                    unreachable!("provider future should be dropped by cancellation")
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("active-steer-interrupt-cancel").open().await?;
    let active_turn_id = "active-steer-interrupt-turn";
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        let stream = turn_session
            .turn(TurnInput::text("primary hangs"))
            .turn_id(active_turn_id)
            .stream()?;
        stream.finish().await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("primary turn should reach provider")
        .expect("provider started signal");
    let active = session
        .enqueue(TurnInput::text("deferred active steer"))
        .id("active-steer")
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await?;
    let queued = session
        .enqueue(TurnInput::text("cancelled next turn"))
        .id("cancelled-next")
        .send()
        .await?;
    let cancelled = session.cancel_pending_turn_input(&queued.input_id).await?;
    let crate::PendingTurnInputCancelOutcome::Cancelled(cancelled) = cancelled else {
        panic!("queued input should be cancellable before it is claimed: {cancelled:?}");
    };
    assert_eq!(cancelled.input_id, queued.input_id);

    assert_eq!(session.cancel_running_turns(), 1);
    let interrupted = turn.await.expect("turn task")?;
    assert!(matches!(
        interrupted.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));

    let pending = session.pending_turn_inputs().await?;
    assert_eq!(
        pending.len(),
        1,
        "only the unaccepted active steer should remain"
    );
    assert_eq!(pending[0].input_id, active.input_id);
    assert!(matches!(
        pending[0].ingress,
        lash_core::TurnInputIngress::NextTurn
    ));
    assert_eq!(
        pending[0].state,
        lash_core::TurnInputState::DeferredNextTurn
    );

    let drained = session
        .queued_turn()
        .run()
        .await?
        .expect("deferred active steer should run as the next turn");
    assert_eq!(
        drained.assistant_message(),
        Some("echo: deferred active steer")
    );
    assert!(session.pending_turn_inputs().await?.is_empty());
    let requests = requests.lock_recover().clone();
    assert_eq!(
        requests
            .iter()
            .filter(|(text, _)| text == "deferred active steer")
            .count(),
        1,
        "deferred active steer must be sent exactly once"
    );
    assert!(
        !requests
            .iter()
            .any(|(text, _)| text == "cancelled next turn"),
        "cancelled queued turn must not reach the provider"
    );
    let deferred_request = requests
        .iter()
        .find(|(text, _)| text == "deferred active steer")
        .map(|(_, messages)| messages)
        .expect("deferred active input provider request");
    assert_eq!(
        serde_json::to_string(&deferred_request)
            .expect("serialize deferred active-input request messages"),
        r#"[{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"primary hangs","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"deferred active steer","response_meta":null,"cache_breakpoint":false}}]}]"#
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn accepted_active_steer_interrupt_is_not_requeued() -> Result<()> {
    let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
    let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
    let (second_started_tx, second_started_rx) = oneshot::channel::<()>();
    let first_started_tx = Arc::new(StdMutex::new(Some(first_started_tx)));
    let release_first_rx = Arc::new(TokioMutex::new(Some(release_first_rx)));
    let second_started_tx = Arc::new(StdMutex::new(Some(second_started_tx)));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let captured_requests = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let first_started_tx = Arc::clone(&first_started_tx);
            let release_first_rx = Arc::clone(&release_first_rx);
            let second_started_tx = Arc::clone(&second_started_tx);
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                let user_text = last_user_text(&request);
                captured_requests.lock_recover().push(user_text.clone());
                if user_text == "primary waits for active steer" {
                    if let Some(tx) = first_started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_first_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    return Ok(text_response("first response"));
                }
                if user_text == "accepted active steer" {
                    if let Some(tx) = second_started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    std::future::pending::<()>().await;
                    unreachable!("accepted steer provider call should be dropped by cancellation")
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("accepted-active-steer-interrupt")
        .open()
        .await?;
    let active_turn_id = "accepted-active-steer-turn";
    let turn_session = session.clone();
    let turn = tokio::spawn(async move {
        let stream = turn_session
            .turn(TurnInput::text("primary waits for active steer"))
            .turn_id(active_turn_id)
            .stream()?;
        stream.finish().await
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), first_started_rx)
        .await
        .expect("first provider call should start")
        .expect("first provider signal");
    let active = session
        .enqueue(TurnInput::text("accepted active steer"))
        .id("accepted-active-steer")
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await?;
    release_first_tx
        .send(())
        .expect("release first provider response");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_started_rx)
        .await
        .expect("accepted active steer should start the follow-up provider call")
        .expect("second provider signal");

    assert_eq!(session.cancel_running_turns(), 1);
    let interrupted = turn.await.expect("turn task")?;
    assert!(matches!(
        interrupted.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert!(
        session.pending_turn_inputs().await?.is_empty(),
        "accepted active steer `{}` must be completed, not deferred after interrupt",
        active.input_id
    );
    assert!(
        session.queued_turn().run().await?.ran().is_none(),
        "accepted active steer must not replay as a later queued turn"
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(
        requests
            .iter()
            .filter(|text| text.as_str() == "accepted active steer")
            .count(),
        1,
        "accepted active steer should reach the provider once before cancellation"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[test]
pub(super) fn rlm_active_input_reaches_the_next_provider_iteration() -> Result<()> {
    run_async_test_on_stack_budget("rlm-active-input-next-iteration", || async {
        let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
        let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
        let first_started_tx = Arc::new(StdMutex::new(Some(first_started_tx)));
        let release_first_rx = Arc::new(TokioMutex::new(Some(release_first_rx)));
        let requests = Arc::new(StdMutex::new(
            Vec::<Vec<lash_core::llm::types::LlmMessage>>::new(),
        ));
        let captured_requests = Arc::clone(&requests);
        let call_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = crate::testing::TestProvider::builder()
            .kind("rlm-active-input-next-iteration")
            .complete(move |request| {
                let first_started_tx = Arc::clone(&first_started_tx);
                let release_first_rx = Arc::clone(&release_first_rx);
                let captured_requests = Arc::clone(&captured_requests);
                let call_index = Arc::clone(&call_index);
                async move {
                    captured_requests
                        .lock_recover()
                        .push(request.messages.clone());
                    match call_index.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                        0 => {
                            if let Some(tx) = first_started_tx.lock_recover().take() {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = release_first_rx.lock().await.take() {
                                let _ = rx.await;
                            }
                            Ok(text_response(&lashlang_block(
                                r#"print("first work complete")"#,
                            )))
                        }
                        1 => Ok(text_response(&lashlang_block(
                            r#"finish "active input delivered""#,
                        ))),
                        2 => Ok(text_response(&lashlang_block(r#"finish "later turn""#))),
                        other => panic!("unexpected provider call {other}"),
                    }
                }
            })
            .build()
            .into_handle();
        let core = explicit_ephemeral_facets(LashCore::rlm_builder(
            crate::TurnBudget::Unbounded,
            rlm_factory(),
        ))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
        let session = core
            .session("rlm-active-input-next-iteration")
            .open()
            .await?;
        let active_turn_id = "rlm-active-input-turn";
        let turn_session = session.clone();
        let turn = tokio::spawn(async move {
            turn_session
                .turn(TurnInput::text("perform two iterations"))
                .turn_id(active_turn_id)
                .require_finish()?
                .run()
                .await
        });

        first_started_rx.await.expect("first provider call started");
        session
            .enqueue(TurnInput::text("mid-turn injection marker"))
            .id("rlm-mid-turn-injection")
            .ingress(lash_core::TurnInputIngress::active_turn(
                active_turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ))
            .send()
            .await?;
        release_first_tx.send(()).expect("release first response");
        turn.await.expect("turn task")?;
        let committed_marker_count = session
            .read_view()
            .messages()
            .iter()
            .filter(|message| crate::message_text(message) == "mid-turn injection marker")
            .count();
        assert_eq!(
            committed_marker_count, 1,
            "active input must be one normal committed transcript message"
        );
        session
            .turn(TurnInput::text("later turn input"))
            .turn_id("rlm-later-turn")
            .require_finish()?
            .run()
            .await?;

        let requests = requests.lock_recover().clone();
        assert_eq!(requests.len(), 3, "two turns must execute three calls");
        let first_messages = serde_json::to_string(&requests[0]).expect("serialize first request");
        let second_messages =
            serde_json::to_string(&requests[1]).expect("serialize second request");
        assert!(!first_messages.contains("mid-turn injection marker"));
        assert!(
            second_messages.contains("mid-turn injection marker"),
            "active input was claimed but omitted from the next RLM provider request: {second_messages}"
        );
        assert_eq!(
            serde_json::to_string(&requests[1][..requests[1].len() - 1])
                .expect("serialize stable request message prefix"),
            r#"[{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"perform two iterations","response_meta":null,"cache_breakpoint":false}}]},{"role":"Assistant","blocks":[{"Text":{"text":"<lashlang>\nprint(\"first work complete\")\n</lashlang>","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","blocks":[{"Text":{"text":"history[1].output[0] (19 chars):\nfirst work complete","response_meta":null,"cache_breakpoint":false}}]},{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"mid-turn injection marker","response_meta":null,"cache_breakpoint":true}}]}]"#
        );
        assert_eq!(
            serde_json::to_string(&requests[2])?
                .matches("mid-turn injection marker")
                .count(),
            1,
            "later assembled history must contain the committed input exactly once"
        );
        assert!(session.pending_turn_inputs().await?.is_empty());
        Ok(())
    })
}

#[tokio::test]
pub(super) async fn await_queued_work_batch_resolves_when_drained() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let session = core.session("await-queued").open().await?;
    let receipt = session
        .admin()
        .commands()
        .refresh_tool_catalog("await queued work test", "await-queued-refresh")
        .await?;

    let waiter_session = session.clone();
    let waiter_batch = receipt.batch_id.clone();
    let waiter =
        tokio::spawn(async move { waiter_session.await_queued_work_batch(&waiter_batch).await });

    // Nothing has drained the batch yet, so the waiter must still be pending.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(!waiter.is_finished(), "waiter resolved before any drain");

    assert!(
        session.queued_turn().run().await?.ran().is_none(),
        "a session-command-only drain should not produce a model turn"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("waiter should resolve after the drain")
        .expect("waiter task")?;
    Ok(())
}

#[tokio::test]
pub(super) async fn await_queued_work_batch_resolves_immediately_for_unknown_batch() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let session = core.session("await-unknown").open().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        session.await_queued_work_batch("qwb:never-existed"),
    )
    .await
    .expect("unknown batch must resolve immediately")?;
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_stream_receives_semantic_activities() -> Result<()> {
    let core = standard_core();
    let session = core.session("semantic-stream").open().await?;
    let turn_events = RecordingEvents::default();

    let result = session
        .turn(TurnInput::text("semantic stream"))
        .cancel(CancellationToken::new())
        .stream_to(&turn_events)
        .await?;

    assert!(matches!(
        result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    assert!(
        turn_events
            .snapshot()
            .await
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::AssistantProseDelta { .. }))
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn run_collects_ordered_assistant_prose_activity() -> Result<()> {
    let core = standard_core();
    let session = core.session("main").open().await?;

    let result = session.turn(TurnInput::text("visible")).run().await?;

    assert_eq!(assistant_prose(&result.activities), "echo: visible");
    assert!(
        result
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::AssistantProseDelta { .. }))
    );
    assert!(
        !result
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. }))
    );
    assert!(
        !result
            .activities
            .iter()
            .any(|activity| matches!(&activity.event, TurnEvent::CodeBlockCompleted { .. }))
    );
    assert!(matches!(
        result.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(result.result.usage.output_tokens, 2);
    Ok(())
}

#[tokio::test]
pub(super) async fn core_catalog_and_actual_turn_resolve_the_identical_contract() -> Result<()> {
    let tools = ContractRecordingTools::default();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(tools.clone()))
        .build(crate::testing::runtime_lease_owner())?;
    let core_contract = core
        .tool_catalog()
        .resolve_contract("app_lookup")
        .expect("core catalog contract");
    let expected = serde_json::to_value(core_contract.as_ref()).expect("serialize core contract");
    let session = core.session("catalog-agreement").open().await?;
    tools.take_resolved();

    let output = session
        .turn(TurnInput::text("use the lookup tool"))
        .run()
        .await?;

    assert!(output.is_success());
    let turn_contracts = tools.take_resolved();
    assert!(
        !turn_contracts.is_empty(),
        "the actual turn must resolve the tool contract"
    );
    assert!(
        turn_contracts.iter().all(|contract| contract == &expected),
        "the core projection and actual turn path must use identical contracts"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn private_run_collector_records_ordered_activities() -> Result<()> {
    let collector = RunActivityCollector::default();

    collector
        .emit(test_activity(
            "code-1",
            TurnEvent::CodeBlockStarted {
                language: "lashlang".to_string(),
                code: "x = await tools.app_lookup({})?".to_string(),
                graph_key: None,
            },
        ))
        .await;
    collector
        .emit(test_activity(
            "tool-1",
            TurnEvent::ToolCallCompleted {
                call_id: Some("call-1".to_string()),
                name: "app_lookup".to_string(),
                args: serde_json::json!({}),
                output: lash_core::ToolCallOutput::success(serde_json::json!({ "ok": true })),
                duration_ms: 3,
                graph_key: None,
                parent_call_id: None,
            },
        ))
        .await;
    collector
        .emit(test_activity(
            "code-1",
            TurnEvent::CodeBlockCompleted {
                language: "lashlang".to_string(),
                output: String::new(),
                error: None,
                success: true,
                duration_ms: 4,
                tool_call_ids: vec!["call-1".to_string()],
                graph_key: None,
            },
        ))
        .await;

    let activities = collector.snapshot();
    assert_eq!(activities.len(), 3);
    assert!(matches!(
        &activities[0].event,
        TurnEvent::CodeBlockStarted { language, code, .. }
            if language == "lashlang" && code == "x = await tools.app_lookup({})?"
    ));
    assert!(matches!(
        &activities[1].event,
        TurnEvent::ToolCallCompleted { name, output, .. }
            if name == "app_lookup" && output.value_for_projection() == serde_json::json!({ "ok": true })
    ));
    assert_eq!(activities[0].correlation_id, activities[2].correlation_id);
    assert!(matches!(
        &activities[2].event,
        TurnEvent::CodeBlockCompleted { language, success, .. }
            if language == "lashlang" && *success
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_event_fanout_streams_to_collector_and_live_sink() -> Result<()> {
    let live = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(tool_roundtrip_provider())
        .model(mock_model_spec())
        .tools(Arc::new(AppTools))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::new(TestLocalProcessRegistry::default()))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("fanout-tool-events").open().await?;

    let output = session
        .turn(TurnInput::text("use tool"))
        .advanced()
        .collect_with_scope(
            live.as_ref(),
            turn_scope(&SessionId::from(session.session_id())),
        )
        .await?;

    assert!(matches!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    assert_eq!(
        serde_json::to_value(&output.activities).expect("recorded activities serialize"),
        serde_json::to_value(live.snapshot().await).expect("live activities serialize")
    );
    assert_eq!(assistant_prose(&output.activities), "done");
    assert_eq!(output.assistant_message(), Some("done"));
    assert!(output.is_success());
    let tool_completed = output
        .activities
        .iter()
        .find(|activity| matches!(&activity.event, TurnEvent::ToolCallCompleted { .. }))
        .expect("tool completion");
    assert!(matches!(
        &tool_completed.event,
        TurnEvent::ToolCallCompleted { name, output, .. }
            if name == "app_lookup" && output.value_for_projection() == serde_json::json!({ "ok": true })
    ));
    Ok(())
}

#[test]
pub(super) fn turn_run_batch_tool_runs_every_call_concurrently_and_preserves_order() -> Result<()> {
    run_async_test_on_stack_size("runtime-batch-tool-order-test", 8 * 1024 * 1024, || async {
        let tools = Arc::new(RuntimeBatchTools::new());
        let tool_provider: Arc<dyn ToolProvider> = tools.clone();
        let core =
            explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                .provider(runtime_batch_provider())
                .model(mock_model_spec())
                .tools(tool_provider)
                .plugin(runtime_batch_plugin())
                .store_factory(Arc::new(
                    lash_core::facade_support::InMemorySessionStoreFactory::new(),
                ))
                .process_registry(Arc::new(TestLocalProcessRegistry::default()))
                .build(crate::testing::runtime_lease_owner())?;
        let session = core.session("runtime-batch-tool-order").open().await?;

        let output = session.turn(TurnInput::text("run batch")).run().await?;

        assert_eq!(output.assistant_message(), Some("done"));
        let batch_completed = output
            .activities
            .iter()
            .find(|activity| {
                matches!(
                    &activity.event,
                    TurnEvent::ToolCallCompleted { name, .. } if name == "runtime_batch"
                )
            })
            .expect("batch completion");
        let TurnEvent::ToolCallCompleted {
            output: batch_output,
            ..
        } = &batch_completed.event
        else {
            unreachable!();
        };
        let batch_value = batch_output.value_for_projection();
        let results = batch_value
            .get("results")
            .and_then(serde_json::Value::as_array)
            .expect("batch results");
        let result_tools = results
            .iter()
            .map(|result| {
                assert_eq!(
                    result.get("success").and_then(serde_json::Value::as_bool),
                    Some(true)
                );
                result
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .expect("result tool")
            })
            .collect::<Vec<_>>();
        assert_eq!(result_tools, ["first", "formerly_serial", "last"]);

        let windows = tools.windows();
        assert_eq!(windows.len(), 3);
        let names = windows
            .iter()
            .map(|(name, _, _)| name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(names, BTreeSet::from(["first", "formerly_serial", "last"]));
        Ok(())
    })
}

#[test]
pub(super) fn batch_child_tool_calls_carry_parent_call_id_linkage() -> Result<()> {
    run_async_test_on_stack_size(
        "batch-child-parent-linkage-test",
        8 * 1024 * 1024,
        || async {
            let tools = Arc::new(RuntimeBatchTools::new());
            let tool_provider: Arc<dyn ToolProvider> = tools.clone();
            let core =
                explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
                    .provider(runtime_batch_provider())
                    .model(mock_model_spec())
                    .tools(tool_provider)
                    .plugin(runtime_batch_plugin())
                    .store_factory(Arc::new(
                        lash_core::facade_support::InMemorySessionStoreFactory::new(),
                    ))
                    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
                    .build(crate::testing::runtime_lease_owner())?;
            let session = core.session("batch-child-parent-linkage").open().await?;

            let output = session.turn(TurnInput::text("run batch")).run().await?;

            // The batch container call itself is a top-level standard-mode call: no
            // parent linkage and no code-block graph key.
            let (batch_call_id, batch_parent, batch_graph_key) = output
                .activities
                .iter()
                .find_map(|activity| match &activity.event {
                    TurnEvent::ToolCallStarted {
                        name,
                        call_id,
                        parent_call_id,
                        graph_key,
                        ..
                    } if name == "runtime_batch" => {
                        Some((call_id.clone(), parent_call_id.clone(), graph_key.clone()))
                    }
                    _ => None,
                })
                .expect("batch container ToolCallStarted");
            assert_eq!(
                batch_parent, None,
                "batch container must not carry a parent"
            );
            assert_eq!(batch_graph_key, None, "standard-mode call has no graph key");
            let batch_call_id = batch_call_id.expect("batch container call id");

            // Each batch child is delivered as its own tool event pointing back at the
            // batch call, so consumers reconstruct containment from real events.
            let child_parents = output
                .activities
                .iter()
                .filter_map(|activity| match &activity.event {
                    TurnEvent::ToolCallCompleted {
                        name,
                        parent_call_id,
                        graph_key,
                        ..
                    } if matches!(name.as_str(), "first" | "formerly_serial" | "last") => {
                        Some((name.clone(), parent_call_id.clone(), graph_key.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let child_names = child_parents
                .iter()
                .map(|(name, _, _)| name.clone())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                child_names,
                BTreeSet::from([
                    "first".to_string(),
                    "formerly_serial".to_string(),
                    "last".to_string(),
                ]),
                "every batch child must surface as its own tool event"
            );
            for (name, parent, graph_key) in &child_parents {
                assert_eq!(
                    parent.as_deref(),
                    Some(batch_call_id.as_str()),
                    "batch child {name} must link to the batch call"
                );
                assert_eq!(
                    graph_key, &None,
                    "standard-mode batch child {name} has no code-block graph key"
                );
            }
            Ok(())
        },
    )
}
