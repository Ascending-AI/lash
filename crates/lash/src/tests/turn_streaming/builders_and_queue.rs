use super::*;
use crate::support::TurnOutcome;
use futures_util::StreamExt as _;
use lash_core::TurnEvent;

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
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("turn-started-cancel-target").expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let expected_turn_id = "turn-started-cancel-target-id";
    let handle = session
        .send(TurnInput::text("wait for exact cancellation"))
        .id(crate::TurnId::parse(expected_turn_id).expect("nonblank host identity"))
        .await?;
    let mut stream = handle.events();

    let first = stream.next().await.expect("turn start activity")?;
    let TurnEvent::TurnStarted { turn_id } = first.event else {
        panic!("first pull-stream activity must deliver turn identity");
    };
    assert_eq!(turn_id, expected_turn_id);
    let crate::CancelReceipt::Cancelled { receipt, .. } = session
        .cancel(crate::CancelTarget::Run(turn_id.clone()))
        .request_id("turn-started-cancel-request")
        .origin("pull-stream-host")
        .reason("cancel from first activity")
        .await?
    else {
        panic!("the running run must receive the cancellation request");
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
pub(super) async fn idle_queued_input_emits_typed_application_and_durable_identity() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("idle-input-application").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let empty_admission = session
        .durable()
        .send(TurnInput::text(""))
        .id(crate::TurnId::parse("idle-empty-source").expect("nonblank host identity"))
        .await?
        .receipt()
        .clone();
    let admission = session
        .durable()
        .send(TurnInput::text("queued canonical input"))
        .id(crate::TurnId::parse("idle-source").expect("nonblank host identity"))
        .await?
        .receipt()
        .clone();

    let run = session
        .attach(admission.input_id.clone())
        .outcome()
        .await?
        .run()
        .cloned()
        .expect("queued input should run");

    let crate::observe::SessionObservationSubscription::Subscribed(mut subscription) =
        session.observe().subscribe_from_cursor(&cursor).await?
    else {
        panic!("recent cursor should replay typed application");
    };
    // Each run publishes the applications its opening committed: the empty
    // input opens its own run with an empty user row (ADR 0132 §4).
    let mut published = Vec::new();
    while !published
        .iter()
        .any(|application: &crate::TurnInputApplication| application.input_id == admission.input_id)
    {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
            .await
            .expect("timed out waiting for typed idle application")
            .expect("observation stream remains open")
            .expect("local observation event");
        let crate::observe::SessionObservationEventPayload::TurnActivity(activity) = &event.payload
        else {
            continue;
        };
        if let crate::TurnEvent::QueuedInputAccepted { applications } = &activity.event {
            published.extend(applications.iter().cloned());
        }
    }
    let committed = session
        .durable()
        .read()
        .await?
        .expect("the session has committed state");
    for application in &published {
        assert!(
            committed
                .messages()
                .iter()
                .any(|message| message.id == application.committed_message_id),
            "typed evidence must identify the canonical committed message: {application:?}"
        );
    }
    let live = published
        .iter()
        .find(|application| application.input_id == admission.input_id)
        .expect("the queued input's application");
    assert_ne!(live.input_id, empty_admission.input_id);
    assert_eq!(live.source_key.as_deref(), Some("idle-source"));
    assert_eq!(live.turn_id, run.as_str());
    assert_eq!(live.checkpoint, None);

    let durable = session.durable().turn_input_applications().await?;
    assert!(
        durable.contains(live),
        "the durable read answers the published application: {durable:?}"
    );
    for application in &durable {
        assert!(
            published.contains(application),
            "every durable application was published: {application:?}"
        );
    }
    Ok(())
}

#[tokio::test]
pub(super) async fn durable_application_read_survives_a_trimmed_live_replay_window() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .live_replay_store(Arc::new(
        lash_core::facade_support::InMemoryLiveReplayStore::new(
            lash_core::facade_support::InMemoryLiveReplayStoreConfig {
                max_events_per_session: 1,
                ..lash_core::facade_support::InMemoryLiveReplayStoreConfig::default()
            },
        ),
    ))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("durable-input-application-gap")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let stale_cursor = session
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;
    let admission = session
        .durable()
        .send(TurnInput::text("survives replay gap"))
        .id(crate::TurnId::parse("gap-source").expect("nonblank host identity"))
        .await?
        .receipt()
        .clone();
    let run = session
        .attach(admission.input_id.clone())
        .outcome()
        .await?
        .run()
        .cloned()
        .expect("queued input should run");

    let mut recovery = session.observe().subscribe_and_recover(stale_cursor);
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), recovery.next())
        .await
        .expect("timed out waiting for replay gap")
        .expect("recovery stream item")?;
    assert!(matches!(
        item,
        crate::observe::SessionObservationStreamItem::Gap { .. }
    ));

    let applications = session.durable().turn_input_applications().await?;
    let committed = session
        .durable()
        .read()
        .await?
        .expect("the session has committed state");
    assert!(matches!(
        applications.as_slice(),
        [application]
            if application.input_id == admission.input_id
                && application.source_key.as_deref() == Some("gap-source")
                && application.turn_id == run
                && application.checkpoint.is_none()
                && committed
                    .messages()
                    .iter()
                    .any(|message| message.id == application.committed_message_id)
    ));
    Ok(())
}

/// A core whose engine executes its sends in the background, answering every
/// model call with `answer`.
async fn answering_core(answer: &'static str) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(
        crate::testing::TestProvider::builder()
            .kind("mailbox-binding")
            .complete(move |_| async move { Ok(text_response(answer)) })
            .build()
            .into_handle(),
        mock_llm_profile_spec(),
    )
    .build(crate::testing::runtime_lease_owner())
}

/// A keyed input's id derives from its session and key alone, so two stores
/// can hold the same session and input ids. A handle answers only from a
/// run its own stores ran: a run another store's driver left behind under
/// the same ids is not its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_send_never_answers_from_a_run_another_store_ran() -> Result<()> {
    let first = answering_core("answered by the first store").await?;
    let second = answering_core("answered by the second store").await?;
    let first_session = first
        .session(crate::SessionId::parse("shared-session").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let second_session = second
        .session(crate::SessionId::parse("shared-session").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    // The first store's run settles; no handle takes its report.
    let unread = first_session
        .send(TurnInput::text("ask"))
        .id(crate::TurnId::parse("shared-input").expect("nonblank host identity"))
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
    .expect("the first store's run settles")?;

    let second_handle = second_session
        .send(TurnInput::text("ask"))
        .id(crate::TurnId::parse("shared-input").expect("nonblank host identity"))
        .await?;
    assert_eq!(second_handle.input_id(), &shared_input);
    let output = second_handle.output().await?;
    assert_eq!(
        output.assistant_message(),
        Some("answered by the second store")
    );
    Ok(())
}
