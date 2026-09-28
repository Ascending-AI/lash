use super::*;

const SEED: u64 = 0xb1_1d45;

/// A plain send runs its effects on the engine's own host with no effects
/// named by the caller: the model call is journaled under the send's turn.
#[tokio::test]
pub(super) async fn turn_run_uses_the_engine_host_without_explicit_effects() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("configured-effect-host").open().await?;

    let output = session
        .send(TurnInput::text("inline"))
        .id("inline-turn")
        .output()
        .await?;

    assert_eq!(output.assistant_message(), Some("echo: inline"));
    let llm_calls = journaled_llm_call_keys(&double);
    assert!(
        llm_calls
            .iter()
            .any(|key| key.contains("configured-effect-host:inline-turn:")),
        "the model call is journaled under the send's turn: {llm_calls:?}"
    );
    Ok(())
}

/// Every plain turn entry point — output into a sink, output, the event
/// stream, and a later send — drives its own root, and the engine journals
/// each root's model call under that root's turn and no other.
#[tokio::test]
pub(super) async fn plain_turn_entry_points_each_run_under_their_own_turn() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
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
    let llm_calls = journaled_llm_call_keys(&double);
    let turns = [
        "durable-stream-to",
        "durable-run",
        "durable-stream",
        "durable-queue-drain",
    ];
    for turn_id in turns {
        assert_eq!(
            llm_calls
                .iter()
                .filter(|key| key.contains(&format!("durable-default-effect-host:{turn_id}:")))
                .count(),
            1,
            "`{turn_id}` journals its one model call under its own turn: {llm_calls:?}"
        );
    }
    assert_eq!(
        llm_calls.len(),
        turns.len(),
        "no model call is journaled outside the four roots: {llm_calls:?}"
    );
    Ok(())
}

#[tokio::test]
pub(super) async fn turn_id_sets_execution_scope_and_trace_identity() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
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

    let llm_calls = journaled_llm_call_keys(&double);
    assert!(
        llm_calls
            .iter()
            .any(|key| key.contains("stable-turn-id:stable-turn:")),
        "the turn id names the model call's execution scope and replay key: {llm_calls:?}"
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
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
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
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
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
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
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
    explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
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

/// FIG-3980: a turn's journal does not grow with its transcript, and neither
/// session-driver handler journals a separate generation sentinel step.
///
/// Every turn sends the same large input, so each adds it and its echo to
/// the transcript the next turn's model request carries. The model-call
/// steps journal that request by digest, so a later turn's `LashTurn`
/// journal is the size of an earlier one's.
#[tokio::test]
pub(super) async fn a_turn_journals_its_request_by_digest_and_no_sentinel_step() -> Result<()> {
    use lash_restate_test::protocol::MessageType;

    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("request-digest").open().await?;
    let input = "x".repeat(8_000);
    let mut turn_bytes = Vec::new();
    for turn in 0..4 {
        session
            .send(TurnInput::text(format!("{turn} {input}")))
            .id(format!("t{turn}"))
            .output()
            .await?;
        let server = double.server();
        let turn_target = format!(
            "LashTurn/{}:request-digestt{turn}/run",
            "request-digest".len()
        );
        let turn_run = server
            .invocations()
            .into_iter()
            .find(|view| view.target == turn_target)
            .expect("the turn's LashTurn run");
        let journal = server.journal(&turn_run.id).unwrap_or_default();
        turn_bytes.push(
            journal
                .iter()
                .map(|entry| entry.payload.len())
                .sum::<usize>(),
        );
        let commands = journal
            .iter()
            .filter(|entry| entry.ty.is_command() && entry.ty != MessageType::InputCommand)
            .collect::<Vec<_>>();
        assert!(
            commands
                .first()
                .is_some_and(|first| first.ty == MessageType::RunCommand
                    && first
                        .name
                        .as_deref()
                        .is_some_and(|name| name.starts_with("lash:drive-root-start:"))),
            "the root's start marker is its first command: {:?}",
            commands.first()
        );
    }
    let server = double.server();
    for view in server.invocations() {
        let sentinels = server
            .journal(&view.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| entry.name.as_deref() == Some("lash.build.generation"))
            .count();
        if view.target.starts_with("LashSession/") || view.target.starts_with("LashTurn/") {
            assert_eq!(sentinels, 0, "{} journals no sentinel step", view.target);
        }
    }
    // Each turn carries 16 KB more transcript than the one before it.
    assert!(
        turn_bytes[3].abs_diff(turn_bytes[1]) < 1_024,
        "a LashTurn journal does not grow with the transcript: {turn_bytes:?}"
    );
    Ok(())
}
