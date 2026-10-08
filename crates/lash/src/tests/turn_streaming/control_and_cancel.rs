//! Turn control and cancellation laws on the durable substrate (FIG-5307):
//! SQLite memory stores, the core's node serving the session.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_stream_finish_returns_committed_assistant_prose() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(semantic_group_provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("turn-stream-last-group").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let handle = session.send(TurnInput::text("stream groups")).await?;
    let mut stream = handle.events();

    let mut activities = Vec::new();
    while let Some(activity) = stream.next_activity().await {
        activities.push(activity?);
    }
    let result = handle.output().await?.result;

    assert_eq!(assistant_prose(&activities), "firstsecond");
    assert_eq!(result.assistant_message(), Some("first\n\nsecond"));
    assert!(result.is_success());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_status_streams_as_semantic_turn_event() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(retry_once_provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("retry-status").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let events = RecordingEvents::default();

    let result = session
        .send(TurnInput::text("hello"))
        .output_into(&events)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_input_acceptance_streams_semantic_ack_with_id() -> Result<()> {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        checkpoint_gated_provider(entered_tx, release_rx),
        mock_llm_profile_spec(),
    )
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("queued-input").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let events = Arc::new(RecordingEvents::default());
    let turn_session = session.clone();
    let turn_events = Arc::clone(&events);
    let turn = tokio::spawn(async move {
        turn_session
            .send(TurnInput::text("hello"))
            .id(crate::TurnId::parse("queued-input-turn").expect("nonblank host identity"))
            .output_into(turn_events.as_ref())
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
    // The injected input is the session's next turn (ADR 0101): its run,
    // named by its source key, acknowledges it at its start and answers it.
    let next = RecordingEvents::default();
    let follow_up = session
        .attach_id(crate::TurnId::parse("injection:queue-1").expect("the injected input's run"))
        .output_into(&next)
        .await?;
    assert!(matches!(
        follow_up.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage { .. })
    ));
    let events = next.snapshot().await;
    assert!(
        events.iter().any(|event| matches!(
            &event.event,
            TurnEvent::QueuedInputAccepted {
                applications,
            } if applications.iter().any(|application| {
                application.source_key.as_deref() == Some("injection:queue-1")
                    && application.checkpoint.is_none()
                    && application.committed_message_id
                        == format!("m_ingress_{}", application.input_id)
            })
        )),
        "{:#?}",
        events
            .iter()
            .filter(|event| matches!(&event.event, TurnEvent::QueuedInputAccepted { .. }))
            .collect::<Vec<_>>()
    );
    let prose = events
        .into_iter()
        .filter_map(|event| match event.event {
            TurnEvent::AssistantProseDelta { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect::<String>();
    assert!(prose.contains("after queued follow-up"), "{prose}");
    Ok(())
}

fn hang_on_signal_provider(started_tx: Arc<StdMutex<Vec<oneshot::Sender<()>>>>) -> ProviderHandle {
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

async fn wait_for_stable_build_count(builds: &AtomicUsize) -> usize {
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
    .expect("unknown-admissibility hydration reaches an idle steady state")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_turn_notification_during_a_live_turn_has_bounded_hydrations() -> Result<()> {
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .plugin(Arc::new(QueuedWorkHydrationProbeFactory {
        builds: Arc::clone(&builds),
    }))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("queued-work-live-lease").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let entered = first_entered.notified();
    let foreground = session.send(TurnInput::text("foreground turn")).await?;
    // This core runs no session work: a waiter executes the input in its own
    // task, so the events follower is what starts the turn.
    let _foreground_events = foreground.events();
    entered.await;
    let baseline_builds = builds.load(Ordering::SeqCst);

    core.session(
        crate::SessionId::parse("queued-work-live-lease").expect("nonblank host identity"),
    )
    .durable()
    .await?
    .send(TurnInput::text("queued while foreground owns the lease"))
    .ingress(lash_core::TurnInputIngress::NextTurn)
    .id(crate::TurnId::parse("queued-during-live-turn").expect("nonblank host identity"))
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
        tokio::time::timeout(std::time::Duration::from_secs(2), foreground.output())
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

async fn assert_session_turn_cancel_disposition(
    session_id: &SessionId,
    turn_id: &TurnId,
    disposition: lash_core::facade_support::TurnCancelUndeliveredInputPolicy,
    default_disposition: bool,
) -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session((session_id).clone())
        .created()
        .await
        .open()
        .await?;
    let handle = session
        .send(TurnInput::text("hang until session cancellation"))
        .id((turn_id).clone())
        .await?;
    let mut events = handle.events();
    started_rx.await.expect("turn reached the provider");

    let undelivered = session
        .send(TurnInput::text("undelivered active-turn input"))
        .id(TurnId::fixture(format!("{session_id}:undelivered")))
        .ingress(lash_core::TurnInputIngress::active_turn(
            turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await?;
    let undelivered_id = undelivered.input_id().clone();
    let request_id = format!("{session_id}:cancel");
    let cancel = session
        .cancel(crate::CancelTarget::Run(turn_id.clone()))
        .request_id(request_id.clone())
        .origin("test-host")
        .reason("undelivered active input");
    let cancel = if default_disposition {
        cancel
    } else {
        cancel.undelivered(disposition)
    };
    let crate::CancelReceipt::Cancelled { receipt, .. } = cancel.await? else {
        panic!("the running run must receive a cancellation request");
    };
    assert!(matches!(
        receipt.outcome,
        lash_core::facade_support::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == request_id && evidence.undelivered == disposition
    ));

    // Drain the follower to its end: it settles with the live turn report
    // and leaves that answer on the handle, which output() then returns.
    while let Some(_activity) = events.next_activity().await {}
    let interrupted = handle.output().await?.result;
    assert!(matches!(
        interrupted.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert_eq!(interrupted.cancel_input_outcome.affected_inputs.len(), 1);
    let affected = &interrupted.cancel_input_outcome.affected_inputs[0];
    assert_eq!(affected.input_id, undelivered_id);
    assert_eq!(affected.disposition, disposition);

    match disposition {
        lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Drop => {
            let pending = session.durable().pending_turn_inputs().await?;
            assert!(
                pending
                    .iter()
                    .all(|input| input.input.input_id != undelivered_id),
                "dropped input must be absent from next-turn ingress"
            );
        }
        lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Defer => {
            let outcome = undelivered.outcome().await?;
            assert_eq!(outcome.status(), crate::TurnStatus::Answered);
            assert_eq!(
                outcome
                    .output()
                    .expect("deferred input ran")
                    .assistant_message(),
                Some("echo: undelivered active-turn input")
            );
            let pending = session.durable().pending_turn_inputs().await?;
            assert!(
                pending
                    .iter()
                    .all(|input| input.input.input_id != undelivered_id),
                "the session shift consumes the deferred input as the next run"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5321: the durable send report drops the cancel's affected inputs"]
async fn send_cancel_defaults_to_deferring_undelivered_active_input() -> Result<()> {
    assert_session_turn_cancel_disposition(
        &SessionId::from("session-cancel-legacy-defer"),
        &TurnId::from("session-cancel-legacy-defer:turn"),
        lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Defer,
        true,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5320: a cancelled durable turn commits no head, so its input leaves history"]
async fn active_steer_after_last_call_defers_to_next_turn_first_call() -> Result<()> {
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
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("active-steer-interrupt-cancel")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let active_turn_id = "active-steer-interrupt-turn";
    let primary = session
        .send(TurnInput::text("primary hangs"))
        .id(crate::TurnId::parse(active_turn_id).expect("nonblank host identity"))
        .await?;
    let turn = tokio::spawn(async move { primary.outcome().await });

    tokio::time::timeout(std::time::Duration::from_secs(1), started_rx)
        .await
        .expect("primary turn should reach provider")
        .expect("provider started signal");
    let active = session
        .send(TurnInput::text("deferred active steer"))
        .id(crate::TurnId::parse("active-steer").expect("nonblank host identity"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await?;
    let queued = session
        .send(TurnInput::text("cancelled next turn"))
        .id(crate::TurnId::parse("cancelled-next").expect("nonblank host identity"))
        .await?;
    let cancelled = queued.cancel().await?;
    assert!(
        matches!(cancelled, crate::CancelReceipt::Withdrawn { .. }),
        "queued input should be cancellable before it is claimed: {cancelled:?}"
    );

    let stopped = session
        .cancel(crate::CancelTarget::Run(TurnId::from(active_turn_id)))
        .undelivered(lash_core::facade_support::TurnCancelUndeliveredInputPolicy::Defer)
        .await?;
    assert!(matches!(stopped, crate::CancelReceipt::Cancelled { .. }));
    let interrupted = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("cancelled run settles")
        .expect("turn task")?;
    assert_eq!(interrupted.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        queued.outcome().await?.status(),
        crate::TurnStatus::Cancelled
    );

    let drained = tokio::time::timeout(std::time::Duration::from_secs(10), active.output())
        .await
        .expect("deferred steer settles")?;
    assert_eq!(
        drained.assistant_message(),
        Some("echo: deferred active steer")
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5320: a cancelled durable turn commits no head, so the steer it accepted is not recorded"]
async fn accepted_active_steer_interrupt_is_not_requeued() -> Result<()> {
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
                    // A tool call keeps the turn working, so its
                    // after-work checkpoint applies the accepted steer
                    // instead of finishing on a plain answer first.
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "primary-lookup".to_string(),
                            tool_name: "app_lookup".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                let steer_started = second_started_tx.lock_recover().take();
                if user_text == "accepted active steer"
                    && let Some(tx) = steer_started
                {
                    let _ = tx.send(());
                    std::future::pending::<()>().await;
                    unreachable!("accepted steer provider call should be dropped by cancellation");
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("accepted-active-steer-interrupt")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let active_turn_id = "accepted-active-steer-turn";
    let primary = session
        .send(TurnInput::text("primary waits for active steer"))
        .id(crate::TurnId::parse(active_turn_id).expect("nonblank host identity"))
        .await?;
    let turn = tokio::spawn(async move { primary.outcome().await });

    tokio::time::timeout(std::time::Duration::from_secs(1), first_started_rx)
        .await
        .expect("first provider call should start")
        .expect("first provider signal");
    let active = session
        .send(TurnInput::text("accepted active steer"))
        .id(crate::TurnId::parse("accepted-active-steer").expect("nonblank host identity"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await?;
    let active_id = active.input_id().clone();
    release_first_tx
        .send(())
        .expect("release first provider response");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_started_rx)
        .await
        .expect("accepted active steer should start the follow-up provider call")
        .expect("second provider signal");

    let stopped = session
        .cancel(crate::CancelTarget::Run(TurnId::from(active_turn_id)))
        .await?;
    assert!(matches!(stopped, crate::CancelReceipt::Cancelled { .. }));
    let interrupted = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("first run settles")
        .expect("turn task")?;
    assert_eq!(interrupted.status(), crate::TurnStatus::Cancelled);
    assert!(
        interrupted.output().is_some(),
        "the interrupted run commits its cancelled turn"
    );
    assert_eq!(
        active.outcome().await?.status(),
        crate::TurnStatus::Cancelled
    );
    assert!(
        session.durable().pending_turn_inputs().await?.is_empty(),
        "accepted active steer `{}` must be completed, not deferred after interrupt",
        active_id
    );
    assert_eq!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .filter(|application| application.input_id == active_id)
            .count(),
        1,
        "the accepted steer applies to the interrupted run once"
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_admitted_steer_cancel_reaches_its_run() -> Result<()> {
    let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
    let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
    let (second_started_tx, second_started_rx) = oneshot::channel::<()>();
    let first_started_tx = Arc::new(StdMutex::new(Some(first_started_tx)));
    let release_first_rx = Arc::new(TokioMutex::new(Some(release_first_rx)));
    let second_started_tx = Arc::new(StdMutex::new(Some(second_started_tx)));
    let provider = crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |request| {
            let first_started_tx = Arc::clone(&first_started_tx);
            let release_first_rx = Arc::clone(&release_first_rx);
            let second_started_tx = Arc::clone(&second_started_tx);
            async move {
                let user_text = last_user_text(&request);
                if user_text == "primary waits for active steer" {
                    if let Some(tx) = first_started_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_first_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    // A tool call keeps the turn working, so its
                    // after-work checkpoint applies the accepted steer
                    // instead of finishing on a plain answer first.
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "primary-lookup".to_string(),
                            tool_name: "app_lookup".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                let steer_started = second_started_tx.lock_recover().take();
                if user_text == "cancelled active steer"
                    && let Some(tx) = steer_started
                {
                    let _ = tx.send(());
                    std::future::pending::<()>().await;
                    unreachable!("cancelled steer provider call should be dropped by cancellation");
                }
                Ok(text_response(&format!("echo: {user_text}")))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("checkpoint-admitted-steer-cancel")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let active_turn_id = "checkpoint-admitted-steer-turn";
    let primary = session
        .send(TurnInput::text("primary waits for active steer"))
        .id(crate::TurnId::parse(active_turn_id).expect("nonblank host identity"))
        .await?;
    let turn = tokio::spawn(async move { primary.outcome().await });

    tokio::time::timeout(std::time::Duration::from_secs(1), first_started_rx)
        .await
        .expect("first provider call should start")
        .expect("first provider signal");
    let active = session
        .send(TurnInput::text("cancelled active steer"))
        .id(crate::TurnId::parse("checkpoint-admitted-steer").expect("nonblank host identity"))
        .ingress(lash_core::TurnInputIngress::active_turn(
            active_turn_id,
            lash_core::TurnInputCheckpointBoundary::AfterWork,
        ))
        .await?;
    release_first_tx
        .send(())
        .expect("release first provider response");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_started_rx)
        .await
        .expect("admitted active steer should start the follow-up provider call")
        .expect("second provider signal");

    // The steer is checkpoint-admitted and its turn is in flight: the input's
    // cancel resolves the consuming run from the admission's record.
    let stopped = active.cancel().await?;
    let crate::CancelReceipt::Cancelled { run, .. } = &stopped else {
        panic!("a checkpoint-admitted input's cancel reaches its run: {stopped:?}");
    };
    assert_eq!(run.as_str(), active_turn_id);
    let interrupted = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the cancelled run settles")
        .expect("turn task")?;
    assert_eq!(interrupted.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        active.outcome().await?.status(),
        crate::TurnStatus::Cancelled
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_event_fanout_streams_to_collector_and_live_sink() -> Result<()> {
    let live = Arc::new(RecordingEvents::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(tool_roundtrip_provider(), mock_llm_profile_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("fanout-tool-events").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("use tool"))
        .outcome_into(live.as_ref())
        .await?
        .into_output()
        .expect("an answered send has its output");

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
