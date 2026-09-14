//! FIG-1558: the durable provider pin is read and guarded at open and at
//! child/fork creation, never silently discarded.

use super::*;

/// FIG-1558: the recorded provider pin is a durable fact, so an open that
/// names a different provider is answered at open — typed and host-visible —
/// not silently discarded and deferred to the first turn.
#[tokio::test]
async fn conflicting_provider_at_open_is_refused_before_any_turn() -> Result<()> {
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let pinning = core
        .session("provider-pin-conflict")
        .store(Arc::clone(&store))
        .open()
        .await?;
    pinning
        .turn(TurnInput::text("pin the provider"))
        .run()
        .await?;
    assert_eq!(
        pinning.policy_snapshot().recorded_provider_id(),
        "embed-test"
    );
    drop(pinning);

    let error = match core
        .session("provider-pin-conflict")
        .store(Arc::clone(&store))
        .provider(other_kind_provider())
        .open()
        .await
    {
        Ok(_) => panic!("an open naming a different provider must be refused at open"),
        Err(error) => error,
    };
    match &error {
        crate::EmbedError::Session(lash_core::SessionError::ProviderMismatch {
            expected,
            actual,
            session_id,
        }) => {
            assert_eq!(expected, "embed-test", "the refusal names the recorded pin");
            assert_eq!(
                actual, "other-embed-test",
                "the refusal names the provider this open requested"
            );
            assert_eq!(session_id.as_str(), "provider-pin-conflict");
        }
        other => panic!("expected a typed provider-pin refusal, got: {other:?}"),
    }
    assert!(
        error.is_terminal(),
        "a provider-pin conflict is terminal, not a retryable open failure"
    );

    // The refused open changed nothing: the session still runs on its pin.
    let reopened = core
        .session("provider-pin-conflict")
        .store(store)
        .open()
        .await?;
    assert_eq!(
        reopened.policy_snapshot().recorded_provider_id(),
        "embed-test"
    );
    Ok(())
}

/// FIG-1558: child/fork creation inherits the recorded pin, and a create
/// request naming a different provider is refused rather than overwriting it.
#[tokio::test]
async fn child_create_inherits_the_recorded_provider_pin_and_refuses_a_conflict() -> Result<()> {
    let factory = Arc::new(RecordingStoreFactory::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(factory.clone())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("provider-pin-root").open().await?;

    let child_request =
        |session_id: &str, policy: Option<lash_core::SessionPolicy>| SessionCreateRequest {
            session_id: Some(SessionId::from(session_id)),
            relation: lash_core::SessionRelation::Child {
                parent_session_id: SessionId::from("provider-pin-root"),
                caused_by: None,
            },
            start: lash_core::SessionStartPoint::Empty,
            policy,
            plugin_source: lash_core::SessionPluginSource::CurrentSessionFork,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            context_overlay: lash_core::SessionContextOverlay::default(),
            plugin_options: lash_core::PluginOptions::default(),
            usage_source: None,
        };

    session
        .admin()
        .children()
        .create_session(child_request("provider-pin-child", None))
        .await?;
    assert_eq!(
        factory.provider_ids(),
        vec!["embed-test".to_string(), "embed-test".to_string()],
        "a child created without a policy carries the parent's recorded pin"
    );

    let mut conflicting = session.policy_snapshot();
    conflicting.provider_id = "other-embed-test".to_string();
    let error = session
        .admin()
        .children()
        .create_session(child_request(
            "provider-pin-child-conflict",
            Some(conflicting),
        ))
        .await
        .expect_err("a create request naming a different provider must be refused");
    let message = error.to_string();
    assert!(
        message.contains("embed-test") && message.contains("other-embed-test"),
        "the refusal names both the recorded pin and the requested provider: {message}"
    );
    assert_eq!(
        factory.session_ids(),
        vec![
            SessionId::from("provider-pin-root"),
            SessionId::from("provider-pin-child"),
        ],
        "the refused create never reached the store"
    );
    Ok(())
}
