use super::*;
use crate::tests::host::send::*;

use futures_util::StreamExt;
use tokio::sync::Notify;
mod pins;
mod replay_lag;
mod terminal_windows;

/// D-RUNREPORT: reading an unchanged graph-less session invents no timestamp.
async fn graphless_session_reads_are_identical() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(&fixture.core, SessionId::fixture("graphless-read"))
        .await
        .durable()
        .await?;
    let store = lash_core::store::SessionStore::new(
        fixture.core.store_factory.clone(),
        session.session_id().clone(),
    )?;
    let first = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("created head");
    let again = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("created head");
    assert_eq!(
        serde_json::to_vec(&first.state.to_snapshot())?,
        serde_json::to_vec(&again.state.to_snapshot())?,
        "window reads adopted the wall clock"
    );
    let first = session.read().await?.expect("created head").to_snapshot();
    let again = session.read().await?.expect("created head").to_snapshot();
    assert_eq!(
        serde_json::to_vec(&first)?,
        serde_json::to_vec(&again)?,
        "read views adopted the wall clock"
    );
    let (_, first) = lash_core::runtime::load_durable_observation_head(&store)
        .await?
        .expect("created head");
    let (_, again) = lash_core::runtime::load_durable_observation_head(&store)
        .await?
        .expect("created head");
    assert_eq!(
        serde_json::to_vec(&first.to_snapshot())?,
        serde_json::to_vec(&again.to_snapshot())?,
        "observation reads adopted the wall clock"
    );
    let open = fixture
        .core
        .session(session.session_id().clone())
        .open()
        .await?;
    let admin = open.admin().state();
    let first = admin.export().await;
    let again = admin.export().await;
    assert!(
        first.session_graph.nodes.is_empty(),
        "admin invented an uncommitted frame"
    );
    assert_eq!(
        serde_json::to_vec(&first)?,
        serde_json::to_vec(&again)?,
        "admin reads adopted the wall clock"
    );
    Ok(())
}

/// A send to a faulted session answers the fault's typed error while its
/// input stays open: a faulted session admits nothing until an operator
/// clears the fault (ADR 0109 §9), and its follower reads the fault in the
/// same read as the input's binding (FIG-5915).
async fn a_faulted_sessions_open_input_answers_the_fault() -> Result<()> {
    let fixture = fixture(1).await?;
    let id = lash_kernel_value::identity::SessionId::parse("send-faulted")
        .expect("nonblank host identity");
    let session = created_session(&fixture.core, id.clone())
        .await
        .open()
        .await?;
    let record = crate::SessionFaultRecord::new(
        crate::SessionFaultOrigin::DriveAdmission,
        &crate::runtime::RuntimeError::new(
            crate::runtime::RuntimeErrorCode::RuntimeStoreCorrupt,
            "corrupt session data",
        ),
    );
    fixture
        .core
        .store_factory
        .record_session_fault(&id, &record, 1)
        .await?;

    let handle = session.send(TurnInput::text("after the fault")).await?;
    let answer = tokio::time::timeout(std::time::Duration::from_secs(20), handle.outcome())
        .await
        .expect("the faulted session's send answers");
    match answer {
        Err(crate::EmbedError::Runtime(error)) => {
            assert_eq!(
                error.code,
                crate::runtime::RuntimeErrorCode::RuntimeStoreCorrupt
            );
            assert_eq!(error.message, "corrupt session data");
        }
        other => panic!("a faulted session's open input answers its fault, not {other:?}"),
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "nothing ran");
    Ok(())
}

/// A run never executes on a runtime a host opened to read
/// ([`observe_with_state`](crate::SessionBuilder::observe_with_state)): the
/// session's turns run on its session actor (ADR 0132 §3), whose committed
/// head moves on while the reader's snapshot stays where it was loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_never_runs_on_a_session_opened_to_observe() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = lash_kernel_value::identity::SessionId::from("send-observed");
    let host = created_session(&fixture.core, session_id.clone())
        .await
        .open()
        .await?;
    host.send(TurnInput::text("first"))
        .id(lash_vocabulary::identity::TurnId::parse("observed-first")
            .expect("nonblank host identity"))
        .output()
        .await?;

    let policy = lash_core::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    );
    let store = lash_execution_contracts::deployment::admit_session_view(
        &fixture.core.store_factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::PersistedSessionConfig::from_policy(&policy),
            head: lash_core::SessionCreationHead::Config,
            retention: lash_core::Retention::UntilGc,
        },
    )
    .await?;
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the first turn committed a head")
    .state;
    let observer = created_session(&fixture.core, session_id.clone())
        .await
        .observe_with_state(state)
        .await?;
    assert_eq!(observer.read_view().turn_index(), 1);

    let tail = fixture.core.live_replay_store.tail_cursor(&session_id);
    let second = host
        .send(TurnInput::text("second"))
        .id(lash_vocabulary::identity::TurnId::parse("observed-second")
            .expect("nonblank host identity"))
        .await?;
    assert_eq!(
        second.cursor(),
        &tail,
        "FIG-6934: send follows from the publication tail, not the host's stale resident revision"
    );
    second.outcome().await?;
    let head = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the session has a head");
    assert_eq!(
        head.state.turn_index, 2,
        "the session's actor ran the run over the committed head"
    );
    assert_eq!(
        observer.read_view().turn_index(),
        1,
        "no run executed on the observer's runtime"
    );
    Ok(())
}

async fn replay_gaps_reach_both_streams_and_sinks() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-replay-gap")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let handle = session
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("gap-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    fixture
        .core
        .live_replay_store
        .invalidate_session(&session.session_id())
        .await
        .expect("invalidate the replay to create a known gap");
    let mut events = handle.events();
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
        .await
        .expect("the stream reports its gap")
        .expect("gap item");
    assert!(
        matches!(event, Err(EmbedError::Send(error)) if matches!(*error, crate::SendError::ObservationGap(_)))
    );
    // The sink follows from the same cursor, so it meets the same gap.
    let sink = RecordingEvents::default();
    let followed = tokio::spawn(async move {
        handle
            .outcome_into(&sink)
            .await
            .map(|outcome| (outcome, sink))
    });
    fixture.release.notify_one();
    // The stream goes on past its gap and ends once the run settles.
    let mut after_gap = 0;
    while let Some(item) = tokio::time::timeout(std::time::Duration::from_secs(20), events.next())
        .await
        .expect("the stream ends once the run settles")
    {
        item.expect("one gap, then the run's activity");
        after_gap += 1;
    }
    assert!(after_gap > 0, "the stream observes on past its gap");
    let (sunk, sink) = followed.await.expect("the sink follower")?;
    assert_eq!(sunk.status(), crate::TurnStatus::Answered);
    assert!(
        !sunk.gaps().is_empty(),
        "a sink cannot take a gap in-stream, so its answer reports it: {:?}",
        sunk.gaps()
    );
    assert!(
        !sink.snapshot().await.is_empty(),
        "the sink observes on past its gap"
    );
    Ok(())
}

/// A send whose spec names a model key this host does not serve is refused
/// before the input is accepted, and nothing is enqueued (FIG-4374).
async fn a_send_under_an_unserved_profile_key_is_refused_before_acceptance() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-bad-route")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let error = session
        .send(TurnInput::text("route me nowhere"))
        .model("no-such-model")
        .await
        .map(|_handle| ())
        .expect_err("a key no registration serves is refused at send");
    assert!(
        matches!(&error, EmbedError::Runtime(runtime)
            if runtime.code == lash_core::RuntimeErrorCode::LlmProfileUnknown),
        "the refusal is the typed unknown-model refusal: {error:?}"
    );
    let store = lash_execution_contracts::deployment::live_session_view(
        &fixture.core.store_factory,
        &lash_kernel_value::identity::SessionId::from("send-bad-route"),
    )
    .await?
    .expect("the opened session has a store");
    assert!(
        store.list_pending_turn_inputs().await?.is_empty(),
        "the refused send accepted nothing"
    );
    // The refusal changed nothing: the session's recorded route still serves.
    session
        .send(TurnInput::text("keep the recorded route"))
        .await?;
    Ok(())
}
mod sqlite_memory {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn graphless_session_reads_are_identical() -> Result<()> {
        super::graphless_session_reads_are_identical().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_faulted_sessions_open_input_answers_the_fault() -> Result<()> {
        super::a_faulted_sessions_open_input_answers_the_fault().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn replay_gaps_reach_both_streams_and_sinks() -> Result<()> {
        super::replay_gaps_reach_both_streams_and_sinks().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_send_under_an_unserved_profile_key_is_refused_before_acceptance() -> Result<()> {
        super::a_send_under_an_unserved_profile_key_is_refused_before_acceptance().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence() -> Result<()> {
        super::a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    /// A cancel addressed to a queued input that a merging drain bound to
    /// another input's run, before that run applied it, reaches that run, and
    /// both inputs answer Cancelled.
    async fn cancel_finds_the_consuming_run_before_application() -> Result<()> {
        super::cancel_finds_the_consuming_run_before_application().await
    }
}

/// A send under a host id whose run already settled commits nothing and
/// answers from that run's evidence (D2 Q6).
async fn a_send_under_a_settled_id_commits_nothing_and_answers_its_evidence() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("send-settled-id")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let first = session
        .send(TurnInput::text("only once"))
        .id(lash_vocabulary::identity::TurnId::parse("settled-run")
            .expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(first.assistant_message(), Some("echo: only once"));
    let applied = session.durable().turn_input_applications().await?;
    assert_eq!(applied.len(), 1);
    session
        .durable()
        .send_parts()
        .await?
        .store
        .vacuum()
        .await
        .expect("vacuum retains the settled run's retry evidence");

    let again = session
        .send(TurnInput::text("only once"))
        .id(lash_vocabulary::identity::TurnId::parse("settled-run")
            .expect("nonblank host identity"))
        .await?;
    assert_eq!(again.input_id(), &applied[0].input_id);
    assert_eq!(
        session
            .attach_id(
                lash_vocabulary::identity::TurnId::parse("settled-run")
                    .expect("nonblank host identity")
            )
            .input_id(),
        again.input_id(),
        "a retry answers the original acceptance, which its id alone addresses"
    );
    let outcome = again.outcome().await?;
    assert_eq!(outcome.status(), crate::TurnStatus::Answered);
    let output = outcome.output().expect("a settled run has a report");
    assert_eq!(output.assistant_message(), Some("echo: only once"));

    let conflicting = session
        .send(TurnInput::text("different semantic input"))
        .id(lash_vocabulary::identity::TurnId::parse("settled-run")
            .expect("nonblank host identity"))
        .await;
    assert!(
        conflicting.is_err(),
        "a settled id must validate its submission digest"
    );

    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    assert_eq!(session.durable().turn_input_applications().await?, applied);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1, "nothing ran again");
    Ok(())
}

/// A cancel addressed to a queued input that a merging drain bound to
/// another input's run, before that run applied it, reaches that run, and
/// both inputs answer Cancelled.
async fn cancel_finds_the_consuming_run_before_application() -> Result<()> {
    let fixture = composing_fixture().await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse("cancel-bound-input")
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    let first = session
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("first-run").expect("nonblank host identity"))
        .await?;
    provider_called(&fixture, 1).await;
    let second = session
        .send(TurnInput::text(HELD))
        .id(lash_vocabulary::identity::TurnId::parse("consuming-run")
            .expect("nonblank host identity"))
        .await?;
    // The merging drain admits it with the second, as that run's last user
    // row: the provider holds the run on it.
    let third = session
        .send(TurnInput::text(format!("batched input, {HELD}")))
        .id(lash_vocabulary::identity::TurnId::parse("batched-id").expect("nonblank host identity"))
        .await?;
    fixture.release.notify_one();
    provider_called(&fixture, 2).await;
    let input_id = third.input_id().clone();
    let parts = session.durable().send_parts().await?;
    assert_eq!(
        parts.store.run_binding(&input_id).await?,
        Some(lash_vocabulary::identity::TurnId::from("consuming-run")),
        "the controlled barrier must hold after binding"
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .iter()
            .all(|application| application.input_id != input_id),
        "the controlled barrier must hold before application"
    );
    let receipt = session.attach(input_id).cancel().await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Cancelled { run, .. }
        if run.as_str() == "consuming-run"),
        "{receipt:?}"
    );
    fixture.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), second.outcome())
        .await
        .expect("the consuming run settles")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        third.outcome().await?.status(),
        crate::TurnStatus::Cancelled
    );
    first.outcome().await?;
    Ok(())
}
