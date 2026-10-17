//! FIG-6333: a host closes an id lash may never have seen, so
//! [`SendOutcome::Unknown`](lash::SendOutcome::Unknown) has a definite end.
//! A forsake answers the send lash holds, or leaves a tombstone that refuses
//! every later send of the id, whether or not lash has created the session
//! (FIG-6355).

use super::*;

fn forsaken_refusal(error: &EmbedError) -> Option<&lash_core::InputId> {
    match error {
        EmbedError::Runtime(error)
            if error.code == lash_core::RuntimeErrorCode::TurnInputForsaken =>
        {
            error.forsaken_input()
        }
        _ => None,
    }
}

/// The handle a forsake answers when a send of the id was accepted first.
fn accepted(forsaking: lash::Forsaking) -> lash::SendHandle {
    match forsaking {
        lash::Forsaking::Accepted(handle) => *handle,
        lash::Forsaking::Forsaken => panic!("the forsake answers the accepted input"),
        _ => panic!("an unknown forsake outcome"),
    }
}

/// Forsake, then send: the send is refused with its typed cause, accepts
/// nothing and runs nothing, and every probe of the id answers `Forsaken`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forsaken_id_refuses_its_send_and_answers_forsaken() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(&fixture.core, SessionId::fixture("forsake-before-send"))
        .await
        .open()
        .await?;
    let id = lash::TurnId::fixture("forsaken-before-send");
    for forsaking in [
        session.forsake(id.clone()).await?,
        session.forsake(id.clone()).await?,
        session.durable().forsake(id.clone()).await?,
    ] {
        assert!(
            matches!(forsaking, lash::Forsaking::Forsaken),
            "an id no send holds is forsaken, idempotently"
        );
    }
    for outcome in [
        session.attach_id(id.clone()).outcome().await?,
        session.durable().attach_id(id.clone()).outcome().await?,
    ] {
        assert!(
            matches!(outcome, lash::SendOutcome::Forsaken { .. }),
            "a forsaken id answers Forsaken: {outcome:?}"
        );
        assert_eq!(outcome.status(), lash::TurnStatus::Forsaken);
        assert!(outcome.run().is_none());
    }
    let input_id = session.input_id(&id);
    let refused = session
        .send(TurnInput::text("arrives after the forsake"))
        .id(id.clone())
        .await
        .map(|_| ())
        .expect_err("a forsaken id refuses its send");
    assert_eq!(forsaken_refusal(&refused), Some(&input_id), "{refused:?}");
    let refused = session
        .send_batch([
            (
                lash::TurnId::fixture("beside-the-forsaken"),
                TurnInput::text("beside"),
            ),
            (id.clone(), TurnInput::text("arrives after the forsake")),
        ])
        .await
        .map(|_| ())
        .expect_err("a batch naming a forsaken id is refused whole");
    assert_eq!(forsaken_refusal(&refused), Some(&input_id), "{refused:?}");
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    assert!(matches!(
        session.attach_id(id).outcome().await?,
        lash::SendOutcome::Forsaken { .. }
    ));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "nothing ran");
    fixture.core.shutdown().await?;
    Ok(())
}

/// Send, then forsake: lash holds the input, so the forsake writes nothing
/// and answers the send's own outcome, whether the send is still running or
/// settled, and the id still answers a retry of the same content.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forsaking_a_sent_id_answers_the_send() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = created_session(&fixture.core, SessionId::fixture("forsake-after-send"))
        .await
        .open()
        .await?;
    let id = lash::TurnId::fixture("sent-before-forsake");
    let sent = session.send(TurnInput::text(HELD)).id(id.clone()).await?;
    provider_called(&fixture, 1).await;
    let running = accepted(session.forsake(id.clone()).await?);
    fixture.release.notify_one();
    let answered = sent.outcome().await?;
    for outcome in [
        running.outcome().await?,
        accepted(session.forsake(id.clone()).await?)
            .outcome()
            .await?,
    ] {
        assert_eq!(outcome.status(), lash::TurnStatus::Answered, "{outcome:?}");
        assert_eq!(outcome.run(), answered.run());
        assert_eq!(
            outcome
                .output()
                .and_then(|output| output.assistant_message()),
            Some(format!("echo: {HELD}").as_str())
        );
    }
    let retried = session.send(TurnInput::text(HELD)).id(id).await?;
    assert_eq!(retried.outcome().await?.run(), answered.run());
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        1,
        "the input ran once"
    );
    fixture.core.shutdown().await?;
    Ok(())
}

/// FIG-6355: a chat's first input can be the one whose send was lost before
/// lash created the session. The host forsakes its id anyway: the tombstone
/// is recorded under the session's id, a send of the id to the uncreated
/// session is refused as forsaken and creates nothing, and creating the
/// session does not reopen the id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_id_forsaken_before_its_session_exists_refuses_its_send() -> Result<()> {
    let fixture = fixture(1).await?;
    let session_id = SessionId::fixture("forsake-before-creation");
    let absent = fixture.core.session(session_id.clone()).durable().await?;
    let id = lash::TurnId::fixture("lost-first-send");
    for _ in 0..2 {
        assert!(
            matches!(absent.forsake(id.clone()).await?, lash::Forsaking::Forsaken),
            "an uncreated session's id is forsaken, idempotently"
        );
    }
    let input_id = absent.input_id(&id);
    let refused = absent
        .send(TurnInput::text("the lost first send"))
        .id(id.clone())
        .await
        .map(|_| ())
        .expect_err("a forsaken id refuses its send to the uncreated session");
    assert_eq!(forsaken_refusal(&refused), Some(&input_id), "{refused:?}");
    assert!(!absent.exists().await?, "the refused send created nothing");
    assert!(matches!(
        absent.attach_id(id.clone()).outcome().await?,
        lash::SendOutcome::Forsaken { .. }
    ));
    let created = fixture
        .core
        .session(session_id)
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let refused = created
        .send(TurnInput::text("the lost first send"))
        .id(id.clone())
        .await
        .map(|_| ())
        .expect_err("creating the session does not reopen a forsaken id");
    assert_eq!(forsaken_refusal(&refused), Some(&input_id), "{refused:?}");
    assert!(created.pending_turn_inputs().await?.is_empty());
    assert!(matches!(
        created.attach_id(id).outcome().await?,
        lash::SendOutcome::Forsaken { .. }
    ));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "nothing ran");
    fixture.core.shutdown().await?;
    Ok(())
}

/// FIG-6355: a forsake and the send that creates the session race on one id,
/// the forsake on a handle that may find no session yet. The store decides
/// under the session's write authority, which the forsake takes by session
/// id whether or not the session exists, so exactly one wins: the send is
/// accepted and the forsake answers its input, or the tombstone is recorded
/// and the send is refused. Each round staggers the forsake a little more,
/// so it lands before the creation, between the creation and the send, or
/// after the send.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forsake_racing_the_creating_send_has_exactly_one_winner() -> Result<()> {
    const ROUNDS: u64 = 12;
    let fixture = fixture(1).await?;
    let id = lash::TurnId::fixture("raced-first-send");
    for round in 0..ROUNDS {
        let session_id = SessionId::fixture(format!("forsake-creation-race-{round}"));
        let durable = fixture.core.session(session_id.clone()).durable().await?;
        let input_id = durable.input_id(&id);
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let forsake = tokio::spawn({
            let (durable, start, id) = (durable.clone(), Arc::clone(&start), id.clone());
            async move {
                start.wait().await;
                tokio::time::sleep(std::time::Duration::from_millis(round)).await;
                durable.forsake(id).await
            }
        });
        let send = tokio::spawn({
            let (core, start, id) = (fixture.core.clone(), Arc::clone(&start), id.clone());
            async move {
                start.wait().await;
                core.session(session_id)
                    .create(lash::SessionCreation::root(mock_session_spec()))
                    .await?
                    .send(TurnInput::text("the raced first send"))
                    .id(id)
                    .await
            }
        });
        let forsaking = forsake.await.expect("the forsake task completes")?;
        let sent = send.await.expect("the send task completes");
        let send_won = match (forsaking, sent) {
            (lash::Forsaking::Accepted(held), Ok(sent)) => {
                assert_eq!(held.input_id(), sent.input_id(), "round {round}");
                assert_eq!(
                    held.outcome().await?.status(),
                    lash::TurnStatus::Answered,
                    "round {round}: the forsake answers the accepted input's run"
                );
                true
            }
            (lash::Forsaking::Forsaken, Err(refused)) => {
                assert_eq!(
                    forsaken_refusal(&refused),
                    Some(&input_id),
                    "round {round}: {refused:?}"
                );
                false
            }
            (lash::Forsaking::Accepted(_), Err(refused)) => {
                panic!("round {round}: the forsake found an input the send refused: {refused:?}")
            }
            (lash::Forsaking::Forsaken, Ok(_)) => {
                panic!("round {round}: the send was accepted under a recorded tombstone")
            }
            _ => panic!("round {round}: unknown forsake outcome"),
        };
        let outcome = durable.attach_id(id.clone()).outcome().await?;
        assert_eq!(
            matches!(outcome, lash::SendOutcome::Forsaken { .. }),
            !send_won,
            "round {round}: the id's probe agrees with the winner: {outcome:?}"
        );
        assert_eq!(
            matches!(
                durable.forsake(id.clone()).await?,
                lash::Forsaking::Forsaken
            ),
            !send_won,
            "round {round}: the verdict is final"
        );
    }
    fixture.core.shutdown().await?;
    Ok(())
}
