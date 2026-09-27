use super::*;

/// Queued turn work for these laws: one durable process wake from `process`
/// whose `input` is the text the model sees.
fn process_wake_draft(
    session_id: &SessionId,
    process: &str,
    input: impl Into<String>,
) -> crate::persistence::QueuedWorkBatchDraft {
    let process_id = || lash_core::ProcessId::fixture(process);
    lash_core::runtime::process_wake_batch_draft(lash_core::ProcessWakeDelivery {
        version: lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("{process}-wake-1"),
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core::RuntimeInvocation {
            attribution: lash_core::RuntimeAttribution::for_session(session_id.clone()),
            subject: lash_core::runtime::RuntimeSubject::ProcessEvent {
                process_id: process_id(),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core::QueuedWorkAuthority::default(),
        input: input.into(),
        created_at_ms: 1,
    })
}

/// A queued row that is not available yet is not claimable, and it never
/// blocks a send: the engine drives the sent input past it and the row
/// stays queued for its time.
#[tokio::test]
pub(super) async fn a_not_yet_available_queued_row_does_not_block_a_send() -> Result<()> {
    let backend = memory_backend().await;
    let store_factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone().into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(
        crate::testing::TestProvider::builder()
            .kind("delayed-row-send")
            .complete(|_| async { Ok(text_response("sent turn completed")) })
            .build()
            .into_handle(),
    )
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("delayed-row-send").open().await?;
    let session_id = session.session_id();
    let store = lash_core::SessionStoreFactory::open_existing_store_by_id(
        store_factory.as_ref(),
        &session_id,
    )
    .await
    .expect("read the opened session's store")
    .expect("opened session retains its store");
    let delayed = store
        .enqueue_queued_work(
            process_wake_draft(&session_id, "delayed", "delayed work")
                .with_available_at_ms(u64::MAX / 2),
        )
        .await?;

    let output = session
        .send(TurnInput::text("sent input past a delayed row"))
        .output()
        .await?;

    assert_eq!(output.assistant_message(), Some("sent turn completed"));
    assert!(
        store
            .list_queued_work(&session_id)
            .await?
            .iter()
            .any(|batch| batch.batch_id == delayed.batch_id),
        "the delayed row stays queued for its time"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_run_uses_configured_effect_host_without_explicit_effects() -> Result<()> {
    let recorder = EffectRecorder::default();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        recorder.backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("configured-effect-host").open().await?;

    let output = session.send(TurnInput::text("inline")).output().await?;

    assert_eq!(output.assistant_message(), Some("echo: inline"));
    let invocations = recorder.invocations();
    assert!(
        invocations
            .iter()
            .any(|record| record.kind == lash_core::RuntimeEffectKind::LlmCall)
    );
    // Every effect but the drive's admission names its turn: admission runs
    // before a root exists, and names the session alone (FIG-3600).
    assert!(invocations.iter().all(|record| {
        record.kind == lash_core::RuntimeEffectKind::AdmitDrive
            || record
                .turn_id
                .as_deref()
                .is_some_and(|turn_id| !turn_id.trim().is_empty())
    }));
    Ok(())
}

#[tokio::test]
pub(super) async fn durable_configured_effect_host_scopes_plain_turn_entry_points() -> Result<()> {
    let recorder = EffectRecorder::default();
    let core = LashCore::standard_builder(
        recorder.backend().await.into(),
        crate::TurnBudget::Unbounded,
    )
    .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
    .map_backend(crate::tests::inline_session_work)
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("durable-default-effect-host").open().await?;
    let events = RecordingEvents::default();

    session
        .send(TurnInput::text("stream to"))
        .id("durable-stream-to")
        .output_into(&events)
        .await?;
    let run = session
        .send(TurnInput::text("run"))
        .id("durable-run")
        .output()
        .await?;
    let stream = session
        .send(TurnInput::text("stream"))
        .id("durable-stream")
        .await?;
    let mut activities = stream.events();
    while let Some(activity) = activities.next().await {
        activity?;
    }
    stream.output().await?;

    session
        .send(TurnInput::text("queued"))
        .id("durable-queue-drain")
        .output()
        .await?;

    assert_eq!(run.assistant_message(), Some("echo: run"));
    let scopes = recorder.scopes();
    assert_eq!(scopes.len(), 8, "one drive and turn scope per send");
    let mut drive_ids = BTreeSet::new();
    for (pair, turn_id) in scopes.as_chunks::<2>().0.iter().zip([
        "durable-stream-to",
        "durable-run",
        "durable-stream",
        "durable-queue-drain",
    ]) {
        let lash_core::ExecutionScope::QueueDrain {
            session_id,
            drain_id,
        } = &pair[0]
        else {
            panic!("admission must use the session drive scope: {:?}", pair[0]);
        };
        assert_eq!(session_id.as_str(), "durable-default-effect-host");
        assert!(drain_id.starts_with("drive:ti:"), "{drain_id}");
        assert!(drive_ids.insert(drain_id), "each send has its own drive");
        assert_eq!(
            pair[1],
            lash_core::ExecutionScope::turn("durable-default-effect-host", turn_id),
        );
    }
    let effect_turn_ids = recorder
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
pub(super) async fn turn_id_sets_execution_scope_and_trace_identity() -> Result<()> {
    let recorder = EffectRecorder::default();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        recorder.backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("stable-turn-id").open().await?;

    session
        .send(TurnInput::text("stable"))
        .id("stable-turn")
        .output()
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
pub(super) async fn turn_started_identity_targets_cancellation_from_pull_stream() -> Result<()> {
    let provider = crate::testing::TestProvider::builder()
        .kind("turn-started-cancel-target")
        .complete(|_| async {
            std::future::pending::<()>().await;
            unreachable!("provider future should be dropped by exact cancellation")
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .map_backend(crate::tests::inline_session_work)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("turn-started-cancel-target").open().await?;
    let expected_turn_id = "turn-started-cancel-target-id";
    let handle = session
        .send(TurnInput::text("wait for exact cancellation"))
        .id(expected_turn_id)
        .await?;
    let mut stream = handle.events();

    let first = stream.next().await.expect("turn start activity")?;
    let TurnEvent::TurnStarted { turn_id } = first.event else {
        panic!("first pull-stream activity must deliver turn identity");
    };
    assert_eq!(turn_id, expected_turn_id);
    let crate::CancelReceipt::Requested { receipt, .. } = session
        .cancel(crate::CancelTarget::Root(turn_id.clone()))
        .request_id("turn-started-cancel-request")
        .origin("pull-stream-host")
        .reason("cancel from first activity")
        .await?
    else {
        panic!("the running root must receive the cancellation request");
    };
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "turn-started-cancel-request"
    ));

    let report = handle.output().await?.result;
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
pub(super) async fn idle_queued_input_emits_typed_remote_application_and_durable_identity()
-> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .map_backend(crate::tests::inline_session_work)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("idle-input-application").open().await?;
    let cursor = session.observe().current_remote_observation().cursor;
    let empty_admission = session
        .durable()
        .send(TurnInput::text(""))
        .id("idle-empty-source")
        .accepted()
        .await?;
    let admission = session
        .durable()
        .send(TurnInput::text("queued canonical input"))
        .id("idle-source")
        .accepted()
        .await?;

    let root = session
        .attach(admission.input_id.clone())
        .outcome()
        .await?
        .root
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
    assert_eq!(live.source_key.as_deref(), Some("idle-source"));
    assert_eq!(live.turn_id, root.as_str());
    assert_eq!(live.checkpoint, None);
    assert!(
        session
            .read_view()
            .messages()
            .iter()
            .any(|message| message.id == live.committed_message_id),
        "typed evidence must identify the canonical committed message"
    );

    let durable = session.durable().remote_turn_input_applications().await?;
    assert_eq!(durable, vec![live.clone()]);
    Ok(())
}

#[tokio::test]
pub(super) async fn durable_application_read_survives_a_trimmed_live_replay_window() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        memory_backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .live_replay_store(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
            },
        ),
    ))
    .map_backend(crate::tests::inline_session_work)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("durable-input-application-gap").open().await?;
    let stale_cursor = session.observe().current_remote_observation().cursor;
    let admission = session
        .durable()
        .send(TurnInput::text("survives replay gap"))
        .id("gap-source")
        .accepted()
        .await?;
    let root = session
        .attach(admission.input_id.clone())
        .outcome()
        .await?
        .root
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

    let applications = session.durable().remote_turn_input_applications().await?;
    assert!(matches!(
        applications.as_slice(),
        [application]
            if application.input_id == admission.input_id
                && application.source_key.as_deref() == Some("gap-source")
                && application.turn_id == root
                && application.checkpoint.is_none()
                && session
                    .read_view()
                    .messages()
                    .iter()
                    .any(|message| message.id == application.committed_message_id)
    ));
    Ok(())
}

/// A core whose engine drives its sends in the background, answering every
/// model call with `answer`.
async fn answering_core(answer: &'static str) -> Result<LashCore> {
    explicit_ephemeral_facets_with_backend_work(LashCore::standard_builder(
        memory_backend().await.into(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(
        crate::testing::TestProvider::builder()
            .kind("mailbox-binding")
            .complete(move |_| async move { Ok(text_response(answer)) })
            .build()
            .into_handle(),
    )
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
}

/// The settled-root mailbox is shared by every driver in the process, and a
/// keyed input's id derives from its session and key alone, so two stores
/// can hold the same session and input ids. A handle answers only from a
/// root its own stores ran: a root another store's driver left behind under
/// the same ids is not its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_send_never_answers_from_a_root_another_store_ran() -> Result<()> {
    let first = answering_core("answered by the first store").await?;
    let second = answering_core("answered by the second store").await?;
    let first_session = first.session("shared-session").open().await?;
    let second_session = second.session("shared-session").open().await?;

    // The first store's root settles and its driver deposits the report; no
    // handle takes it, so it stays in the mailbox under the shared ids.
    let unread = first_session
        .send(TurnInput::text("ask"))
        .id("shared-input")
        .await?;
    let shared_input = unread.input_id().clone();
    drop(unread);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let applied = first_session.durable().turn_input_applications().await?;
            if applied
                .iter()
                .any(|application| application.input_id == shared_input)
            {
                return Result::Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the first store's root settles")?;

    let second_handle = second_session
        .send(TurnInput::text("ask"))
        .id("shared-input")
        .await?;
    assert_eq!(second_handle.input_id(), &shared_input);
    let output = second_handle.output().await?;
    assert_eq!(
        output.assistant_message(),
        Some("answered by the second store")
    );
    Ok(())
}
