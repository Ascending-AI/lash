//! A session id names one session: a second create of an id answers the
//! typed refusal and leaves the first session as it was.

use super::*;

/// FIG-6047: authority is readable before a run, without hydrating a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_tool_access_needs_only_one_metadata_read_before_the_first_run() -> Result<()> {
    use lash_core::testing::runtime_helpers::LayeredBackend;
    use lash_core::testing::{Script, StoreOp};
    let reads = Script::new();
    let backend = LayeredBackend::over(sqlite_memory_store_backend().await)
        .map_session_store_factory(|inner| reads.wrap("authority-reader", inner))
        .into_backend();
    let core = standard_core_over(backend);
    core.shutdown().await?;
    let definition = crate::tools::ToolDefinition::raw(
        "tool:recorded",
        "recorded",
        "Recorded tool",
        serde_json::json!({"type":"object"}),
        serde_json::json!({}),
    )
    .expect("checked definition")
    .with_execution(std::time::Duration::from_secs(30));
    for (name, authority) in [
        (
            "restricted",
            crate::plugins::SessionToolAccess::restricted([definition.clone()])
                .expect("checked restricted authority")
                .with_hidden_tools(["recorded"])
                .expect("checked hiding"),
        ),
        ("ambient", crate::plugins::SessionToolAccess::ambient()),
    ] {
        // Contract identity caches are not part of the recorded definition.
        let authority: crate::plugins::SessionToolAccess = serde_json::from_value(
            serde_json::to_value(&authority).expect("encode recorded authority"),
        )
        .expect("decode recorded authority");
        let id = crate::SessionId::fixture(name);
        let created = core
            .session(id.clone())
            .create(crate::SessionCreation::root(
                authority.clone(),
                mock_session_spec(),
            ))
            .await?;
        let catalog = core.session(id).durable().await?;
        for handle in [created, catalog] {
            let heads = reads.calls(StoreOp::load_session_head_meta);
            let windows = reads.calls(StoreOp::load_session_window);
            let lookups = reads.calls(StoreOp::lookup_session);
            let recorded = handle.tool_access().await?.expect("created authority");
            assert_eq!(recorded, authority);
            if let Some(definitions) = recorded.restricted_tools() {
                assert_eq!(definitions.len(), 1);
                assert_eq!(definitions[0].manifest, definition.manifest);
                assert!(recorded.hides("recorded"));
            }
            assert_eq!(reads.calls(StoreOp::load_session_head_meta), heads + 1);
            assert_eq!(reads.calls(StoreOp::load_session_window), windows);
            assert_eq!(reads.calls(StoreOp::lookup_session), lookups);
        }
    }
    assert!(
        core.session(crate::SessionId::fixture("unknown-authority"))
            .durable()
            .await?
            .tool_access()
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_create_of_a_session_id_is_refused_already_exists() -> Result<()> {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let id = crate::SessionId::parse("created-once").expect("nonblank host identity");
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    let first = core.session(id.clone()).open().await?;
    first.send(TurnInput::text("hello")).output().await?;
    let head = committed(&first).await;

    let Err(refused) = core
        .session(id.clone())
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
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
