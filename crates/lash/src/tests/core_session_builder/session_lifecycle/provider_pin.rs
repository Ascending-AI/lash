//! FIG-1558: the durable provider pin is read and guarded at open and at
//! child/fork creation, never silently discarded.

use super::*;

/// FIG-1558: the recorded provider pin is a durable fact, so an open that
/// names a different provider is answered at open — typed and host-visible —
/// not silently discarded and deferred to the first turn.
#[tokio::test]
async fn conflicting_provider_at_open_is_refused_before_any_turn() -> Result<()> {
    let store: Arc<dyn lash_core::RuntimePersistence> = Arc::new(SnapshotStore::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend_serving(store.clone()).await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let pinning = core.session("provider-pin-conflict").open().await?;
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
    let reopened = core.session("provider-pin-conflict").open().await?;
    assert_eq!(
        reopened.policy_snapshot().recorded_provider_id(),
        "embed-test"
    );
    Ok(())
}

/// FIG-1558: a related session opened with `.parent(..)` is admitted through
/// the same boundary as any other open, so its store request carries the
/// core's recorded provider pin. A conflicting pin on reopen is refused by
/// `conflicting_provider_at_open_is_refused_before_any_turn` above.
#[tokio::test]
async fn related_session_open_records_the_provider_pin() -> Result<()> {
    let factory = Arc::new(RecordingStoreFactory::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend_with_catalog(factory.clone()).await,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session("provider-pin-root").open().await?;

    core.session("provider-pin-child")
        .parent("provider-pin-root")
        .open()
        .await?;
    assert_eq!(
        factory.provider_ids(),
        vec!["embed-test".to_string(), "embed-test".to_string()],
        "a related session opened through the ordinary path carries the \
         recorded provider pin"
    );
    Ok(())
}
