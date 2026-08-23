#[tokio::test]
async fn settled_config_survives_park_without_pending_graph_nodes() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("parked-config").open().await?;
    session.turn(TurnInput::text("establish head")).run().await?;
    let expected_model = model_spec("settled-model", Some("settled-variant".to_string()), 64_000);
    let expected_generation = lash_core::GenerationOptions {
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.35).expect("temperature")),
        output_token_cap: std::num::NonZeroUsize::new(777),
        ..lash_core::GenerationOptions::default()
    };
    session
        .configure(SessionConfigPatch {
            model: Some(expected_model.clone()),
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                expected_generation.clone(),
            )),
            ..SessionConfigPatch::default()
        })
        .await?;

    let parked = session.park().await?;
    let resumed = Box::pin(core.resume(parked)).await?;
    let policy = resumed.policy_snapshot();
    assert_eq!(policy.model, expected_model);
    assert_eq!(policy.generation, expected_generation);
    Ok(())
}
