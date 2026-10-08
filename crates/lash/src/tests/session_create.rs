//! A session id names one session: a second create of an id answers the
//! typed refusal and leaves the first session as it was.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_create_of_a_session_id_is_refused_already_exists() -> Result<()> {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let id = crate::SessionId::parse("created-once").expect("nonblank host identity");
    core.session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let first = core.session(id.clone()).open().await?;
    first.send(TurnInput::text("hello")).output().await?;
    let head = committed(&first).await;

    let Err(refused) = core
        .session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
    else {
        panic!("a second create of one id is refused");
    };
    assert!(
        matches!(&refused, EmbedError::SessionAlreadyExists { session_id } if session_id.as_str() == "created-once"),
        "a duplicate create is the typed SessionAlreadyExists, got {refused:?}"
    );
    let ids = |view: &lash_core::SessionReadView| {
        view.messages()
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&committed(&first).await),
        ids(&head),
        "the refused create leaves the session's head as it was"
    );
    Ok(())
}
