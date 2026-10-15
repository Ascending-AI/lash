//! FIG-6333: a host closes an id lash may never have seen, so
//! [`SendOutcome::Unknown`](crate::SendOutcome::Unknown) has a definite end.
//! A forsake answers the send lash holds, or leaves a tombstone that refuses
//! every later send of the id.

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

/// Forsake, then send: the send is refused with its typed cause, accepts
/// nothing and runs nothing, and every probe of the id answers `Forsaken`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forsaken_id_refuses_its_send_and_answers_forsaken() -> Result<()> {
    let fixture = fixture(1).await?;
    let session = fixture
        .core
        .session(SessionId::fixture("forsake-before-send"))
        .created()
        .await
        .open()
        .await?;
    let id = crate::TurnId::fixture("forsaken-before-send");
    for outcome in [
        session.forsake(id.clone()).await?.outcome().await?,
        session.forsake(id.clone()).await?.outcome().await?,
        session
            .durable()
            .forsake(id.clone())
            .await?
            .outcome()
            .await?,
        session.attach_id(id.clone()).outcome().await?,
        session.durable().attach_id(id.clone()).outcome().await?,
    ] {
        assert!(
            matches!(outcome, crate::SendOutcome::Forsaken { .. }),
            "a forsaken id answers Forsaken: {outcome:?}"
        );
        assert_eq!(outcome.status(), crate::TurnStatus::Forsaken);
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
                crate::TurnId::fixture("beside-the-forsaken"),
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
        crate::SendOutcome::Forsaken { .. }
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
    let session = fixture
        .core
        .session(SessionId::fixture("forsake-after-send"))
        .created()
        .await
        .open()
        .await?;
    let id = crate::TurnId::fixture("sent-before-forsake");
    let sent = session.send(TurnInput::text(HELD)).id(id.clone()).await?;
    provider_called(&fixture, 1).await;
    let running = session.forsake(id.clone()).await?;
    fixture.release.notify_one();
    let answered = sent.outcome().await?;
    for outcome in [
        running.outcome().await?,
        session.forsake(id.clone()).await?.outcome().await?,
    ] {
        assert_eq!(outcome.status(), crate::TurnStatus::Answered, "{outcome:?}");
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
