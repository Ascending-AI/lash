use super::*;

#[tokio::test]
pub(super) async fn turn_run_uses_configured_native_effect_host_without_explicit_effects()
-> Result<()> {
    let recorder = Arc::new(RecordingNativeEffectController::default());
    let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(Arc::new(lash_core::facade_support::NativeEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("native-default-effect-host").open().await?;

    let output = session.turn(TurnInput::text("inline")).run().await?;

    assert_eq!(output.assistant_message(), Some("echo: inline"));
    let invocations = recorder.invocations();
    assert!(
        invocations
            .iter()
            .any(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
    );
    assert!(invocations.iter().all(|record| {
        record
            .turn_id
            .as_deref()
            .is_some_and(|turn_id| !turn_id.trim().is_empty())
    }));
    Ok(())
}

#[tokio::test]
pub(super) async fn durable_configured_effect_host_scopes_plain_turn_entry_points() -> Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let effect_host = Arc::new(DurableNoopEffectHost::default());
    let core = LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
            dir.path().join("attachments"),
        )))
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .process_env_store(Arc::new(DurableInMemoryProcessEnvStore::default()))
        .effect_host(effect_host.clone())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("durable-default-effect-host").open().await?;
    let events = RecordingEvents::default();

    session
        .turn(TurnInput::text("stream to"))
        .turn_id("durable-stream-to")
        .stream_to(&events)
        .await?;
    let run = session
        .turn(TurnInput::text("run"))
        .turn_id("durable-run")
        .run()
        .await?;
    let mut stream = session
        .turn(TurnInput::text("stream"))
        .turn_id("durable-stream")
        .stream()?;
    while let Some(activity) = stream.next().await {
        activity?;
    }
    stream.finish().await?;

    session
        .enqueue(TurnInput::text("queued"))
        .id("durable-queued-input")
        .send()
        .await?;
    session
        .queued_turn()
        .drain_id("durable-queue-drain")
        .run()
        .await?
        .expect("queued turn should run");

    assert_eq!(run.assistant_message(), Some("echo: run"));
    assert_eq!(
        effect_host.selected_scopes(),
        vec![
            lash_core::ExecutionScope::turn("durable-default-effect-host", "durable-stream-to"),
            lash_core::ExecutionScope::turn("durable-default-effect-host", "durable-run"),
            lash_core::ExecutionScope::turn("durable-default-effect-host", "durable-stream"),
            lash_core::ExecutionScope::queue_drain(
                "durable-default-effect-host",
                "durable-queue-drain"
            ),
        ]
    );
    let effect_turn_ids = effect_host
        .controller
        .invocations()
        .into_iter()
        .filter(|invocation| invocation.kind == lash_core::RuntimeEffectKind::LlmCall)
        .filter_map(|invocation| invocation.turn_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        effect_turn_ids,
        BTreeSet::from([
            TurnId::from("durable-stream-to"),
            TurnId::from("durable-run"),
            TurnId::from("durable-stream"),
            TurnId::from("durable-queue-drain"),
        ])
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn advanced_turn_preserves_a_custom_effect_scope() -> Result<()> {
    let recorder = Arc::new(RecordingNativeEffectController::default());
    let effect_host = lash_core::facade_support::NativeEffectHost::new(recorder.clone());
    let custom_scope = lash_core::ExecutionScope::runtime_operation("custom-foreground-scope");
    let scoped_effect_controller = effect_host.scoped(custom_scope.clone())?;
    let core = standard_core();
    let session = core.session("custom-effect-scope").open().await?;

    let output = session
        .turn(TurnInput::text("custom"))
        .advanced()
        .run_with_scope(scoped_effect_controller)
        .await?;

    assert_eq!(output.assistant_message(), Some("echo: custom"));
    let llm = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm.execution_scope, custom_scope);
    Ok(())
}

#[tokio::test]
pub(super) async fn advanced_turn_rejects_mismatched_turn_scope_and_trace_identity() -> Result<()> {
    let recorder = Arc::new(RecordingNativeEffectController::default());
    let effect_host = lash_core::facade_support::NativeEffectHost::new(recorder.clone());
    let scoped_effect_controller = effect_host.scoped(lash_core::ExecutionScope::turn(
        "mismatched-turn-scope",
        "admitted-turn",
    ))?;
    let core = standard_core();
    let session = core.session("mismatched-turn-scope").open().await?;

    let error = session
        .turn(TurnInput::text("must refuse"))
        .turn_id("input-turn")
        .advanced()
        .run_with_scope(scoped_effect_controller)
        .await
        .expect_err("a foreground Turn scope must match the admitted trace identity");

    let EmbedError::Runtime(error) = error else {
        panic!("expected a runtime error, got {error}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::ExecutionScopeTurnIdMismatch
    );
    assert_eq!(
        error.message,
        "input trace_turn_id `input-turn` does not match execution scope id `admitted-turn`"
    );
    assert!(
        recorder
            .invocations()
            .into_iter()
            .all(|record| record.kind != lash_core::RuntimeEffectKind::LlmCall)
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_id_sets_execution_scope_and_trace_identity() -> Result<()> {
    let recorder = Arc::new(RecordingNativeEffectController::default());
    let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(Arc::new(lash_core::facade_support::NativeEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("stable-turn-id").open().await?;

    session
        .turn(TurnInput::text("stable"))
        .turn_id("stable-turn")
        .run()
        .await?;

    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm_invocation.turn_id.as_deref(), Some("stable-turn"));
    assert!(
        llm_invocation
            .replay_key
            .as_deref()
            .is_some_and(|key| key.contains("stable-turn"))
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn advanced_turn_id_precedence_prefers_builder_then_scope_fallback() -> Result<()>
{
    let recorder = Arc::new(RecordingNativeEffectController::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(Arc::new(lash_core::facade_support::NativeEffectHost::new(
            recorder.clone(),
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("turn-id-precedence").open().await?;

    let builder_wins_scope = ScopedEffectController::borrowed(
        recorder.as_ref(),
        lash_core::ExecutionScope::runtime_operation("scope-operation"),
    )?;
    session
        .turn(TurnInput::text("builder wins"))
        .turn_id("builder-turn")
        .advanced()
        .run_with_scope(builder_wins_scope)
        .await?;

    let scope_fallback = ScopedEffectController::borrowed(
        recorder.as_ref(),
        lash_core::ExecutionScope::turn("turn-id-precedence", "fallback-turn"),
    )?;
    session
        .turn(TurnInput::text("scope fallback"))
        .advanced()
        .run_with_scope(scope_fallback)
        .await?;

    let turn_ids = recorder
        .invocations()
        .into_iter()
        .filter(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .filter_map(|record| record.turn_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        turn_ids,
        BTreeSet::from([TurnId::from("builder-turn"), TurnId::from("fallback-turn")])
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn explicit_effect_controller_creates_turn_scope_internally() -> Result<()> {
    let recorder = RecordingNativeEffectController::default();
    let core = standard_core();
    let session = core.session("explicit-handler-effects").open().await?;

    session
        .turn(TurnInput::text("handler"))
        .turn_id("handler-turn")
        .run_with_effects(&recorder)
        .await?;

    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm_invocation.turn_id.as_deref(), Some("handler-turn"));
    Ok(())
}

#[tokio::test]
pub(super) async fn queued_turn_run_drains_ready_work_and_returns_none_when_idle() -> Result<()> {
    let requests = Arc::new(StdMutex::new(
        Vec::<Vec<lash_core::llm::types::LlmMessage>>::new(),
    ));
    let captured_requests = Arc::clone(&requests);
    let provider = crate::testing::TestProvider::builder()
        .kind("queued-next-prompt-shape")
        .complete(move |request| {
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests
                    .lock_recover()
                    .push(request.messages.clone());
                Ok(text_response("echo: queued work"))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .with_native_queued_work()
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("queued-turn-run").open().await?;
    session
        .enqueue(TurnInput::text("queued work"))
        .id("queued-request")
        .send()
        .await?;

    let output = session
        .queued_turn()
        .run()
        .await?
        .expect("queued turn should run");

    assert_eq!(output.assistant_message(), Some("echo: queued work"));
    {
        let requests = requests.lock_recover();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::to_string(&requests[0])
                .expect("serialize queued-next request user messages"),
            r#"[{"role":"User","starts_user_segment":true,"blocks":[{"Text":{"text":"queued work","response_meta":null,"cache_breakpoint":false}}]}]"#
        );
    }
    assert!(session.queued_turn().run().await?.ran().is_none());
    Ok(())
}

#[tokio::test]
pub(super) async fn queued_turn_id_sets_physical_activity_and_effect_identity() -> Result<()> {
    let recorder = Arc::new(RecordingNativeEffectController::default());
    let effect_controller: Arc<dyn lash_core::RuntimeEffectController> = recorder.clone();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(Arc::new(lash_core::facade_support::NativeEffectHost::new(
            effect_controller,
        )))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("host-identified-queued-turn").open().await?;
    session
        .enqueue(TurnInput::text("host identified queued work"))
        .id("host-identified-input")
        .send()
        .await?;
    let events = RecordingTurnIds::default();

    let output = session
        .queued_turn()
        .turn_id("host-queued-turn-id")
        .stream_to(&events)
        .await?
        .expect("queued turn should run");

    assert_eq!(
        output.assistant_message(),
        Some("echo: host identified queued work")
    );
    let activity_turn_ids = events.snapshot().await;
    assert!(!activity_turn_ids.is_empty());
    assert!(
        activity_turn_ids
            .iter()
            .all(|turn_id| turn_id == "host-queued-turn-id")
    );
    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(
        llm_invocation.turn_id.as_deref(),
        Some("host-queued-turn-id")
    );
    assert!(
        llm_invocation
            .replay_key
            .as_deref()
            .is_some_and(|key| key.contains("host-queued-turn-id"))
    );
    Ok(())
}

pub(super) fn assert_turn_started_first(activities: &[TurnActivity], expected_turn_id: &TurnId) {
    let starts = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::TurnStarted { turn_id } => Some(turn_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, vec![expected_turn_id]);
    assert!(matches!(
        activities.first().map(|activity| &activity.event),
        Some(TurnEvent::TurnStarted { turn_id }) if turn_id == expected_turn_id
    ));
}

#[tokio::test]
pub(super) async fn all_queued_builder_families_begin_with_turn_started() -> Result<()> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "queued-builder-turn-starts";
    let session = core.session(session_id).open().await?;
    let controller = RecordingNativeEffectController::default();

    session
        .enqueue(TurnInput::text("automatic queued builder"))
        .id("automatic-queued-input")
        .send()
        .await?;
    let automatic = session
        .queued_turn()
        .turn_id("automatic-queued-turn")
        .run()
        .await?
        .expect("automatic queued turn");
    assert_turn_started_first(
        &automatic.activities,
        &TurnId::from("automatic-queued-turn"),
    );

    session
        .enqueue(TurnInput::text("scoped automatic queued builder"))
        .id("scoped-automatic-queued-input")
        .send()
        .await?;
    let scoped_automatic = session
        .queued_turn()
        .turn_id("scoped-automatic-queued-turn")
        .run_with_effects(&controller)
        .await?
        .expect("scoped automatic queued turn");
    assert_turn_started_first(
        &scoped_automatic.activities,
        &TurnId::from("scoped-automatic-queued-turn"),
    );

    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    let selected = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                crate::persistence::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session_id),
                        "selected-start",
                    ),
                    "selected queued builder",
                    None,
                ),
            )
            .with_source_key("selected-turn-start"),
        )
        .await?;
    let selected_output = session
        .queued_turn()
        .batch_ids([selected.batch_id])
        .turn_id("selected-queued-turn")
        .run()
        .await?
        .turn
        .expect("selected queued turn");
    assert_turn_started_first(
        &selected_output.activities,
        &TurnId::from("selected-queued-turn"),
    );

    let scoped_selected = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                crate::persistence::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session_id),
                        "scoped-selected-start",
                    ),
                    "scoped selected queued builder",
                    None,
                ),
            )
            .with_source_key("scoped-selected-turn-start"),
        )
        .await?;
    let scoped_selected_output = session
        .queued_turn()
        .batch_ids([scoped_selected.batch_id])
        .turn_id("scoped-selected-queued-turn")
        .run_with_effects(&controller)
        .await?
        .turn
        .expect("scoped selected queued turn");
    assert_turn_started_first(
        &scoped_selected_output.activities,
        &TurnId::from("scoped-selected-queued-turn"),
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn queued_turn_id_accepts_exact_cancel_before_dispatch() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("queued-turn-id-pre-dispatch-cancel")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("cancel arrived too late"))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .with_native_queued_work()
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("pre-cancelled-queued-turn").open().await?;
    session
        .enqueue(TurnInput::text(
            "must be cancelled before provider dispatch",
        ))
        .id("pre-cancelled-input")
        .send()
        .await?;

    let receipt = session
        .request_turn_cancel(
            &TurnId::from("pre-cancelled-queued-turn-id"),
            "pre-dispatch-cancel-request",
            Some("test-host".to_string()),
            Some("cancel before queued dispatch".to_string()),
        )
        .await?;
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "pre-dispatch-cancel-request"
    ));

    let output = session
        .queued_turn()
        .turn_id("pre-cancelled-queued-turn-id")
        .run()
        .await?
        .expect("cancelled queued turn should settle");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        output.result.cancellation(),
        Some(lash_core::facade_support::TurnCancellationEvidence {
            request_id,
            origin: Some(origin),
            reason: Some(reason),
            ..
        }) if request_id == "pre-dispatch-cancel-request"
            && origin == "test-host"
            && reason == "cancel before queued dispatch"
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_started_identity_targets_cancellation_from_pull_stream() -> Result<()> {
    let provider = crate::testing::TestProvider::builder()
        .kind("turn-started-cancel-target")
        .complete(|_| async {
            std::future::pending::<()>().await;
            unreachable!("provider future should be dropped by exact cancellation")
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
    let session = core.session("turn-started-cancel-target").open().await?;
    let expected_turn_id = "turn-started-cancel-target-id";
    let mut stream = session
        .turn(TurnInput::text("wait for exact cancellation"))
        .turn_id(expected_turn_id)
        .stream()?;

    let first = stream.next().await.expect("turn start activity")?;
    let TurnEvent::TurnStarted { turn_id } = first.event else {
        panic!("first pull-stream activity must deliver turn identity");
    };
    assert_eq!(turn_id, expected_turn_id);
    let receipt = session
        .request_turn_cancel(
            &turn_id,
            "turn-started-cancel-request",
            Some("pull-stream-host".to_string()),
            Some("cancel from first activity".to_string()),
        )
        .await?;
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "turn-started-cancel-request"
    ));

    let report = stream.finish().await?;
    assert!(matches!(
        report.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
    ));
    assert!(matches!(
        report.cancellation(),
        Some(lash_core::facade_support::TurnCancellationEvidence {
            request_id,
            origin: Some(origin),
            ..
        }) if request_id == "turn-started-cancel-request" && origin == "pull-stream-host"
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn queued_turn_rejects_drain_id_with_turn_id_at_dispatch() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("conflicting-queued-turn-scope-ids")
        .open()
        .await?;

    let error = match session
        .queued_turn()
        .drain_id("durable-drain-id")
        .turn_id("physical-turn-id")
        .run()
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("dispatch must reject conflicting queued-turn scope identities"),
    };
    let EmbedError::Runtime(error) = error else {
        panic!("expected a runtime error, got {error}");
    };
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::ExecutionScopeTurnIdMismatch
    );
    assert_eq!(
        error.message,
        "`drain_id(...)` and `turn_id(...)` are mutually exclusive; keep `drain_id(...)` as the durable idempotency key for retried drains, or keep `turn_id(...)` as the host-minted physical turn identity"
    );
    Ok(())
}

/// FIG-1575: an exhausted queue and an unreachable one are opposite answers.
/// Only the exhausted queue is terminal, so the drain names which one it hit.
#[tokio::test]
pub(super) async fn an_exhausted_queue_reports_an_empty_claim_refusal() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(
            crate::testing::TestProvider::builder()
                .kind("empty-drain-reason")
                .complete(|_| async { Ok(text_response("echo")) })
                .build()
                .into_handle(),
        )
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("empty-drain-reason").open().await?;

    let drain = session.queued_turn().run().await?;

    assert!(
        matches!(
            drain,
            crate::QueuedTurnDrain::Empty(crate::EmptyQueuedDrainReason::ClaimRefused(
                crate::QueuedWorkClaimRefusal::Empty
            ))
        ),
        "an exhausted queue must report an empty claim refusal, got {drain:?}"
    );
    Ok(())
}

/// Facade admission refuses a session with no explicitly selected store before
/// a queue or turn can be addressed.
#[tokio::test]
pub(super) async fn a_session_without_a_store_never_exposes_a_queue() -> Result<()> {
    let core = core_without_session_store();
    let error = match core.session("missing-store-drain-reason").open().await {
        Ok(_) => panic!("facade session admission requires a store"),
        Err(error) => error,
    };
    assert!(matches!(error, EmbedError::MissingSessionStore));
    Ok(())
}

/// An automatic drain names why it ran no turn, and a row that can never fit is
/// not such a reason: it is a terminal fault. Before FIG-1575 this path reached
/// a selected-drain refusal on a drain that selected nothing, and panicked.
#[tokio::test]
pub(super) async fn an_oversized_queued_row_fails_an_automatic_drain_by_name() -> Result<()> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(
            crate::testing::TestProvider::builder()
                .kind("oversized-queued-row")
                .complete(|_| async { Ok(text_response("echo")) })
                .build()
                .into_handle(),
        )
        .model(crate::tests::harness::model_spec("mock-model", None, 1_024))
        .store_factory(
            Arc::clone(&store_factory) as Arc<dyn crate::persistence::SessionStoreFactory>
        )
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("oversized-queued-row").open().await?;
    {
        use crate::persistence::SessionStoreFactory as _;

        let store = store_factory
            .create_store(&crate::persistence::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(session.session_id()),
                relation: crate::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await?;
        store
            .enqueue_queued_work(crate::persistence::QueuedWorkBatchDraft::new(
                session.session_id(),
                crate::persistence::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session.session_id()),
                        "oversized-frame",
                    ),
                    "w".repeat(64 * 1024),
                    None,
                ),
            ))
            .await?;
    }

    let error = session
        .queued_turn()
        .run()
        .await
        .expect_err("a row larger than the window cannot drain automatically");

    let EmbedError::Runtime(runtime) = &error else {
        panic!("expected a runtime error naming the oversized row, got {error:?}");
    };
    assert_eq!(
        runtime.code,
        lash_core::RuntimeErrorCode::QueuedWorkRowExceedsContextWindow,
        "the oversized row must be named, not panicked on: {error:?}"
    );
    Ok(())
}

/// The wedge FIG-1575 exists to prevent: a drain that could not take the lane
/// consumed nothing, and must never read as an exhausted queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_busy_execution_lane_is_never_reported_as_an_exhausted_queue() -> Result<()> {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let provider = hang_on_signal_provider(Arc::new(StdMutex::new(vec![started_tx])));
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory)
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let holder = core.session("busy-lane-drain-reason").open().await?;
    // Both runtimes recover before either owns the lane. Once the first drain
    // starts, the already-admitted peer must report lane contention rather
    // than trying to recover through the live holder.
    let peer = core.session("busy-lane-drain-reason").open().await?;
    holder
        .enqueue(TurnInput::text("hang queued"))
        .send()
        .await?;
    let drainer = holder.clone();
    let drain = tokio::spawn(async move { drainer.queued_turn().run().await });
    started_rx.await.expect("queued drain reached the provider");

    // The peer cannot take the lane the hung drain still holds.
    let peer_drain = peer.queued_turn().run().await?;

    assert!(
        matches!(
            peer_drain,
            crate::QueuedTurnDrain::Empty(crate::EmptyQueuedDrainReason::ExecutionLaneBusy)
        ),
        "a busy execution lane must never be reported as an exhausted queue, got {peer_drain:?}"
    );
    assert_eq!(holder.cancel_running_turns(), 1);
    drain.await.expect("drain task")?;
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_refuses_partial_key_break_without_settling_rows()
-> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-queued-turn-refusal")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("selected queued turn must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-queued-turn-key-break-refusal";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    let enqueue = |source_key: &'static str, merge_key: &'static str| {
        let store = Arc::clone(&store);
        async move {
            store
                .enqueue_queued_work(
                    crate::persistence::QueuedWorkBatchDraft::new(
                        session_id,
                        lash_core::DeliveryPolicy::EarliestSafeBoundary,
                        crate::persistence::TurnWorkPayload::agent_frame_task(
                            lash_core::facade_support::frame_node_id(
                                &SessionId::from(session_id),
                                "selected-refusal-frame",
                            ),
                            source_key,
                            None,
                        ),
                    )
                    .with_source_key(source_key)
                    .with_merge_key(merge_key),
                )
                .await
                .expect("enqueue selected-refusal row")
        }
    };
    let a1 = enqueue("selected-a1", "a").await;
    let _b1 = enqueue("selected-b1", "b").await;
    let a2 = enqueue("selected-a2", "a").await;

    let error = session
        .queued_turn()
        .batch_ids([a1.batch_id.clone(), a2.batch_id.clone()])
        .run()
        .await
        .expect_err("A1,B1,A2 cannot satisfy selected [A1,A2] atomically");
    match error {
        EmbedError::SelectedQueuedWorkDrainRefused {
            cause:
                SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                    unclaimed_batch_ids,
                },
        } => assert_eq!(unclaimed_batch_ids, vec![a2.batch_id]),
        other => panic!("expected typed selected-drain refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .queued_work()
            .await?
            .iter()
            .map(|batch| (batch.source_key.as_deref(), batch.enqueue_seq))
            .collect::<Vec<_>>(),
        vec![
            (Some("selected-a1"), 1),
            (Some("selected-b1"), 2),
            (Some("selected-a2"), 3),
        ]
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_redrives_an_interrupted_composition_exactly_or_not_at_all()
-> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-interrupted-composition")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("redrove interrupted composition"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-interrupted-composition";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    for source_key in ["interrupted-w1", "interrupted-w2"] {
        store
            .enqueue_queued_work(
                crate::persistence::QueuedWorkBatchDraft::new(
                    session_id,
                    lash_core::DeliveryPolicy::EarliestSafeBoundary,
                    crate::persistence::TurnWorkPayload::agent_frame_task(
                        lash_core::facade_support::frame_node_id(
                            &SessionId::from(session_id),
                            "interrupted-frame",
                        ),
                        source_key,
                        None,
                    ),
                )
                .with_source_key(source_key)
                .with_merge_key("interrupted-key"),
            )
            .await
            .expect("enqueue interrupted composition row");
    }
    let owner_a = lash_core::LeaseOwnerIdentity::opaque(
        "selected-interrupted-owner-a",
        "selected-interrupted-owner-a:incarnation",
    );
    let lease_a = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &owner_a,
            "owner-a-executor",
            60_000,
        )
        .await
        .expect("claim predecessor session execution lease")
        .acquired()
        .expect("predecessor session execution lane is free");
    let claim_a = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &lease_a.fence(),
            &owner_a,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim predecessor composition")
        .claim()
        .expect("predecessor composition exists");
    assert_eq!(
        claim_a
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1", "recording-qwb-2"]
    );
    assert_eq!(claim_a.claim_id, "recording-qwc:1:1");
    store
        .release_session_execution_lease(&lease_a.completion())
        .await
        .expect("release predecessor session execution lease");

    let error = session
        .queued_turn()
        .batch_ids(["recording-qwb-1"])
        .run()
        .await
        .expect_err("a selected drain cannot split an interrupted composition");
    match error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                required_batch_ids: vec![
                    "recording-qwb-1".to_string(),
                    "recording-qwb-2".to_string(),
                ],
            }
        ),
        other => panic!("expected interrupted-composition refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .raw_queued_work_for_testing()
            .into_iter()
            .map(|(batch, claim_id, _, _, _, _)| (batch.batch_id, claim_id))
            .collect::<Vec<_>>(),
        vec![
            (
                "recording-qwb-1".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-2".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
        ]
    );

    let output = session
        .queued_turn()
        .batch_ids(["recording-qwb-1", "recording-qwb-2"])
        .run()
        .await?
        .expect("the complete interrupted composition executes");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        output
            .activities
            .iter()
            .find_map(|activity| match &activity.event {
                TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids.clone()),
                _ => None,
            }),
        Some(vec![
            "recording-qwb-1".to_string(),
            "recording-qwb-2".to_string(),
        ])
    );
    assert!(store.raw_queued_work_for_testing().is_empty());
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_reports_claimed_now_and_already_satisfied_ids()
-> Result<()> {
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-idempotent-outcome")
        .complete(|_| async { Ok(text_response("selected outcome")) })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-idempotent-outcome";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    let batch = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session_id),
                        "selected-outcome-frame",
                    ),
                    "selected-outcome-task",
                    None,
                ),
            )
            .with_source_key("selected-outcome-source"),
        )
        .await
        .expect("enqueue selected outcome row");

    let claimed = session
        .queued_turn()
        .batch_ids([batch.batch_id.clone()])
        .run()
        .await?;
    assert!(claimed.turn.is_some());
    assert_eq!(
        claimed.satisfied,
        vec![crate::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
            batch_id: batch.batch_id.clone(),
        }]
    );

    let replay = session
        .queued_turn()
        .batch_ids([batch.batch_id.clone()])
        .run()
        .await?;
    assert!(replay.turn.is_none());
    assert_eq!(
        replay.satisfied,
        vec![
            crate::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                batch_id: batch.batch_id,
            },
        ]
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_deduplicates_absent_ids_with_free_or_busy_lane()
-> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-duplicate-absent")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("absent selection must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-duplicate-absent";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");

    let expected = vec![
        crate::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
            batch_id: "absent-batch".to_string(),
        },
    ];
    let lane_free = session
        .queued_turn()
        .batch_ids(["absent-batch", "absent-batch"])
        .run()
        .await?;
    assert!(lane_free.turn.is_none());
    assert_eq!(lane_free.satisfied, expected);

    let held_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-duplicate-absent-holder",
        "selected-duplicate-absent-holder:incarnation",
    );
    let held_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &held_owner,
            "held-executor",
            60_000,
        )
        .await
        .expect("claim held session execution lease")
        .acquired()
        .expect("session execution lane is initially free");
    let lane_busy = session
        .queued_turn()
        .batch_ids(["absent-batch", "absent-batch"])
        .run()
        .await?;
    assert!(lane_busy.turn.is_none());
    assert_eq!(lane_busy.satisfied, expected);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    store
        .release_session_execution_lease(&held_lease.completion())
        .await
        .expect("release held session execution lease");
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_deduplicates_present_claimable_id() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-duplicate-present")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("selected duplicate present outcome"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-duplicate-present";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    let batch = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session_id),
                        "selected-duplicate-present-frame",
                    ),
                    "selected-duplicate-present-task",
                    None,
                ),
            )
            .with_source_key("selected-duplicate-present-source"),
        )
        .await
        .expect("enqueue duplicate-selected row");

    let outcome = session
        .queued_turn()
        .batch_ids([batch.batch_id.clone(), batch.batch_id.clone()])
        .run()
        .await?;
    assert!(outcome.turn.is_some());
    assert_eq!(
        outcome.satisfied,
        vec![crate::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
            batch_id: batch.batch_id,
        }]
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_empty_selection_is_satisfied_noop() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-empty-noop")
        .complete(move |_| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("empty selection must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory)
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("selected-empty-noop").open().await?;
    session
        .enqueue(TurnInput::text("must remain queued"))
        .id("selected-empty-noop-input")
        .send()
        .await?;

    let outcome = session
        .queued_turn()
        .batch_ids(std::iter::empty::<String>())
        .run()
        .await?;
    assert!(outcome.turn.is_none());
    assert_eq!(outcome.satisfied, Vec::new());
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(
        session.queued_turn().run().await?.ran().is_some(),
        "the empty selection must leave unrestricted queued input pending"
    );
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_validates_every_interrupted_composition_before_mutating()
-> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-two-interrupted-compositions")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("refused selections must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-two-interrupted-compositions";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    for source_key in ["claim-a1", "claim-a2", "claim-b1", "claim-b2"] {
        store
            .enqueue_queued_work(
                crate::persistence::QueuedWorkBatchDraft::new(
                    session_id,
                    lash_core::DeliveryPolicy::EarliestSafeBoundary,
                    crate::persistence::TurnWorkPayload::agent_frame_task(
                        lash_core::facade_support::frame_node_id(
                            &SessionId::from(session_id),
                            "two-claims-frame",
                        ),
                        source_key,
                        None,
                    ),
                )
                .with_source_key(source_key)
                .with_merge_key("two-claims-key"),
            )
            .await
            .expect("enqueue two-claim row");
    }
    let predecessor_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-two-claims-predecessor",
        "selected-two-claims-predecessor:incarnation",
    );
    let predecessor_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &predecessor_owner,
            "predecessor-executor",
            60_000,
        )
        .await
        .expect("claim predecessor session execution lease")
        .acquired()
        .expect("predecessor session execution lane is free");
    let claim_a = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &predecessor_lease.fence(),
            &predecessor_owner,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor A")
        .claim()
        .expect("predecessor A exists");
    let claim_b = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &predecessor_lease.fence(),
            &predecessor_owner,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(2),
        )
        .await
        .expect("claim predecessor B")
        .claim()
        .expect("predecessor B exists");
    assert_eq!(
        claim_a
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1", "recording-qwb-2"]
    );
    assert_eq!(claim_a.claim_id, "recording-qwc:1:1");
    assert_eq!(
        claim_b
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-3", "recording-qwb-4"]
    );
    assert_eq!(claim_b.claim_id, "recording-qwc:3:1");
    store
        .release_session_execution_lease(&predecessor_lease.completion())
        .await
        .expect("release predecessor session execution lease");

    let partial_error = session
        .queued_turn()
        .batch_ids(["recording-qwb-1", "recording-qwb-2", "recording-qwb-3"])
        .run()
        .await
        .expect_err("full A plus partial B must refuse before reclaiming A");
    match partial_error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
                required_batch_ids: vec![
                    "recording-qwb-3".to_string(),
                    "recording-qwb-4".to_string(),
                ],
            }
        ),
        other => panic!("expected incomplete-B refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .raw_queued_work_for_testing()
            .into_iter()
            .map(|(batch, claim_id, _, _, _, _)| (batch.batch_id, claim_id))
            .collect::<Vec<_>>(),
        vec![
            (
                "recording-qwb-1".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-2".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-3".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
            (
                "recording-qwb-4".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
        ]
    );

    let complete_error = session
        .queued_turn()
        .batch_ids([
            "recording-qwb-1",
            "recording-qwb-2",
            "recording-qwb-3",
            "recording-qwb-4",
        ])
        .run()
        .await
        .expect_err("one selected drain claims exactly the earliest interrupted composition");
    match complete_error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                unclaimed_batch_ids: vec![
                    "recording-qwb-3".to_string(),
                    "recording-qwb-4".to_string(),
                ],
            }
        ),
        other => panic!("expected second-composition refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .raw_queued_work_for_testing()
            .into_iter()
            .map(|(batch, claim_id, _, _, _, _)| (batch.batch_id, claim_id))
            .collect::<Vec<_>>(),
        vec![
            (
                "recording-qwb-1".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-2".to_string(),
                Some("recording-qwc:1:1".to_string()),
            ),
            (
                "recording-qwb-3".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
            (
                "recording-qwb-4".to_string(),
                Some("recording-qwc:3:1".to_string()),
            ),
        ]
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_redrive_ignores_successor_max_rows() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-redrive-over-row-limit")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("redrove over successor row limit"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(2))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-redrive-over-row-limit";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    for source_key in [
        "selected-limit-w1",
        "selected-limit-w2",
        "selected-limit-w3",
    ] {
        store
            .enqueue_queued_work(
                crate::persistence::QueuedWorkBatchDraft::new(
                    session_id,
                    lash_core::DeliveryPolicy::EarliestSafeBoundary,
                    crate::persistence::TurnWorkPayload::agent_frame_task(
                        lash_core::facade_support::frame_node_id(
                            &SessionId::from(session_id),
                            "selected-limit-frame",
                        ),
                        source_key,
                        None,
                    ),
                )
                .with_source_key(source_key)
                .with_merge_key("selected-limit-key"),
            )
            .await
            .expect("enqueue selected row-limit row");
    }
    let predecessor_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-limit-predecessor",
        "selected-limit-predecessor:incarnation",
    );
    let predecessor_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &predecessor_owner,
            "predecessor-executor",
            60_000,
        )
        .await
        .expect("claim selected row-limit predecessor lease")
        .acquired()
        .expect("selected row-limit predecessor lane is free");
    let predecessor_claim = store
        .claim_ready_queued_work(
            &SessionId::from(session_id),
            &predecessor_lease.fence(),
            &predecessor_owner,
            crate::persistence::QueuedWorkClaimBoundary::Idle,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("claim selected row-limit predecessor")
        .claim()
        .expect("selected row-limit predecessor exists");
    assert_eq!(
        predecessor_claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1", "recording-qwb-2", "recording-qwb-3"]
    );
    store
        .release_session_execution_lease(&predecessor_lease.completion())
        .await
        .expect("release selected row-limit predecessor lease");

    let output = session
        .queued_turn()
        .batch_ids(["recording-qwb-1", "recording-qwb-2", "recording-qwb-3"])
        .run()
        .await?
        .expect("selected predecessor composition ignores successor max_rows=2");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        output
            .activities
            .iter()
            .find_map(|activity| match &activity.event {
                TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids.clone()),
                _ => None,
            }),
        Some(vec![
            "recording-qwb-1".to_string(),
            "recording-qwb-2".to_string(),
            "recording-qwb-3".to_string(),
        ])
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_reports_execution_lane_contention() -> Result<()> {
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let observed_provider_calls = Arc::clone(&provider_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("selected-execution-lane-busy")
        .complete(move |_request| {
            let observed_provider_calls = Arc::clone(&observed_provider_calls);
            async move {
                observed_provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("busy selection must not execute"))
            }
        })
        .build()
        .into_handle();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-execution-lane-busy";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("opened session retains its in-memory store");
    store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session_id),
                        "busy-frame",
                    ),
                    "busy-w1",
                    None,
                ),
            )
            .with_source_key("busy-w1"),
        )
        .await
        .expect("enqueue busy selected row");
    let held_owner = lash_core::LeaseOwnerIdentity::opaque(
        "selected-busy-holder",
        "selected-busy-holder:incarnation",
    );
    let held_lease = store
        .try_claim_session_execution_lease(
            &SessionId::from(session_id),
            &held_owner,
            "held-executor",
            60_000,
        )
        .await
        .expect("claim held session execution lease")
        .acquired()
        .expect("session execution lane is initially free");

    let error = session
        .queued_turn()
        .batch_ids(["recording-qwb-1"])
        .run()
        .await
        .expect_err("selected drain under a held lease is typed contention");
    match error {
        EmbedError::SelectedQueuedWorkDrainRefused { cause } => assert_eq!(
            cause,
            SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy
        ),
        other => panic!("expected execution-lane-busy refusal, got {other:?}"),
    }
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .queued_work()
            .await?
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec!["recording-qwb-1"]
    );
    store
        .release_session_execution_lease(&held_lease.completion())
        .await
        .expect("release held session execution lease");
    Ok(())
}

#[tokio::test]
pub(super) async fn idle_queued_input_emits_typed_remote_application_and_durable_identity()
-> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("idle-input-application").open().await?;
    let cursor = session.observe().current_remote_observation().cursor;
    let empty_admission = session
        .enqueue(TurnInput::text(""))
        .id("idle-empty-source")
        .send()
        .await?;
    let admission = session
        .enqueue(TurnInput::text("queued canonical input"))
        .id("idle-source")
        .send()
        .await?;

    session
        .queued_turn()
        .drain_id("idle-application-turn")
        .run()
        .await?
        .expect("queued input should run");

    let crate::observe::RemoteSessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_remote_cursor(
            &crate::remote::observations::RemoteSessionCursor::new(cursor),
        )?
    else {
        panic!("recent cursor should replay typed application");
    };
    let live = loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next_event())
                .await
                .expect("timed out waiting for typed idle application")
                .expect("remote observation event");
        let crate::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
            activity,
        } = event.event
        else {
            continue;
        };
        if let crate::remote::usage::RemoteTurnEvent::TurnInputApplied { applications } =
            activity.event
        {
            break applications;
        }
    };
    assert_eq!(
        live.len(),
        1,
        "only inputs materialized into the canonical message receive application evidence"
    );
    let live = &live[0];
    assert_ne!(live.input_id, empty_admission.input_id);
    assert_eq!(live.input_id, admission.input_id);
    assert_eq!(live.source_key.as_deref(), Some("host:idle-source"));
    assert_eq!(live.turn_id, "idle-application-turn");
    assert_eq!(live.checkpoint, None);
    assert!(
        session
            .read_view()
            .messages()
            .iter()
            .any(|message| message.id == live.committed_message_id),
        "typed evidence must identify the canonical committed message"
    );

    let durable = session.remote_turn_input_applications().await?;
    assert_eq!(durable, vec![live.clone()]);
    Ok(())
}

#[tokio::test]
pub(super) async fn durable_application_read_survives_a_trimmed_live_replay_window() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .live_replay_store(Arc::new(
            lash_core::facade_support::InMemoryLiveReplayStore::new(
                lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                    max_events_per_session: 1,
                    ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
                },
            ),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("durable-input-application-gap").open().await?;
    let stale_cursor = session.observe().current_remote_observation().cursor;
    let admission = session
        .enqueue(TurnInput::text("survives replay gap"))
        .id("gap-source")
        .send()
        .await?;
    session
        .queued_turn()
        .drain_id("gap-application-turn")
        .run()
        .await?
        .expect("queued input should run");

    let mut recovery = session.observe().subscribe_and_recover_remote(
        crate::remote::observations::RemoteSessionCursor::new(stale_cursor),
    )?;
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), recovery.next())
        .await
        .expect("timed out waiting for replay gap")
        .expect("recovery stream item")?;
    assert!(matches!(
        item,
        crate::observe::RemoteSessionObservationStreamItem::Gap { .. }
    ));

    let applications = session.remote_turn_input_applications().await?;
    assert!(matches!(
        applications.as_slice(),
        [application]
            if application.input_id == admission.input_id
                && application.source_key.as_deref() == Some("host:gap-source")
                && application.turn_id.as_str() == "gap-application-turn"
                && application.checkpoint.is_none()
                && session
                    .read_view()
                    .messages()
                    .iter()
                    .any(|message| message.id == application.committed_message_id)
    ));
    Ok(())
}

#[tokio::test]
pub(super) async fn queued_turn_explicit_effects_create_queue_drain_scope_internally() -> Result<()>
{
    let recorder = RecordingNativeEffectController::default();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .without_queued_work()
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("queued-explicit-effects").open().await?;
    session
        .enqueue(TurnInput::text("queued handler"))
        .send()
        .await?;

    let output = session
        .queued_turn()
        .drain_id("handler-drain")
        .run_with_effects(&recorder)
        .await?
        .expect("queued turn should run");

    assert_eq!(output.assistant_message(), Some("echo: queued handler"));
    let llm_invocation = recorder
        .invocations()
        .into_iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("llm effect");
    assert_eq!(llm_invocation.turn_id.as_deref(), Some("handler-drain"));
    Ok(())
}

#[tokio::test]
pub(super) async fn selected_queued_turn_with_effects_preserves_batch_ids_and_scope() -> Result<()>
{
    let recorder = RecordingNativeEffectController::default();
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = "selected-explicit-effects";
    let session = core.session(session_id).open().await?;
    let store = store_factory
        .raw_store_for_testing(&SessionId::from(session_id))
        .expect("session store");
    let receipt = store
        .enqueue_queued_work(
            crate::persistence::QueuedWorkBatchDraft::new(
                session_id,
                crate::persistence::DeliveryPolicy::EarliestSafeBoundary,
                crate::persistence::TurnWorkPayload::agent_frame_task(
                    lash_core::facade_support::frame_node_id(
                        &SessionId::from(session_id),
                        "selected-handler",
                    ),
                    "selected handler",
                    None,
                ),
            )
            .with_source_key("selected-handler-batch"),
        )
        .await?;

    let outcome = session
        .queued_turn()
        .batch_ids([
            receipt.batch_id.as_str(),
            receipt.batch_id.as_str(),
            "absent-batch",
        ])
        .drain_id("selected-handler-drain")
        .run_with_effects(&recorder)
        .await?;
    assert_turn_started_first(
        &outcome.turn.expect("selected turn").activities,
        &TurnId::from("selected-handler-drain"),
    );
    assert_eq!(
        outcome.satisfied,
        vec![
            crate::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
                batch_id: receipt.batch_id.clone()
            },
            crate::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                batch_id: "absent-batch".into()
            },
        ]
    );
    let invocations = recorder.invocations();
    let llm = invocations
        .iter()
        .find(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
        .expect("borrowed controller executes llm effect");
    assert_eq!(llm.turn_id.as_deref(), Some("selected-handler-drain"));
    let events = crate::turn::RunActivityCollector::default();
    let retry = session
        .queued_turn()
        .batch_ids([receipt.batch_id])
        .drain_id("selected-handler-drain")
        .stream_to_with_effects(&events, &recorder)
        .await?;
    assert!(retry.settled_without_selected_turn());
    assert_eq!(recorder.invocations().len(), invocations.len());
    Ok(())
}
