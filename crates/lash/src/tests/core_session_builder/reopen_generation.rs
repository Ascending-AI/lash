use super::*;

#[tokio::test]
async fn reopen_generation_merges_durable_options_and_allows_explicit_clear() -> Result<()> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let factory = lash_core::facade_support::InMemorySessionStoreFactory::new();
    let mut policy = core.policy.clone();
    policy.session_id = Some("generation-merge".to_string());
    let store = lash_core::SessionStoreFactory::create_store(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            session_id: "generation-merge".to_string(),
            relation: lash_core::SessionRelation::Root,
            policy,
            pending_observer_intents: Vec::new(),
        },
    )
    .await?;
    let session = core
        .session("generation-merge")
        .store(store.clone())
        .session_spec(
            crate::SessionSpec::new().generation(lash_core::GenerationOptions {
                seed: Some(73),
                ..Default::default()
            }),
        )
        .open()
        .await?;
    session
        .turn(TurnInput::text("commit the initial generation seed"))
        .run()
        .await?;
    assert_eq!(
        store
            .load_session_head_meta()
            .await?
            .unwrap()
            .config
            .generation
            .seed,
        Some(73)
    );
    drop(session);
    let reopened = core
        .session("generation-merge")
        .store(store.clone())
        .session_spec(crate::SessionSpec::new().generation(Default::default()))
        .open()
        .await?;
    assert_eq!(reopened.policy_snapshot().generation.seed, Some(73));
    drop(reopened);
    let merged = core
        .session("generation-merge")
        .store(store.clone())
        .session_spec(
            crate::SessionSpec::new().generation(lash_core::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(37),
                ..Default::default()
            }),
        )
        .open()
        .await?;
    let generation = merged.policy_snapshot().generation;
    assert_eq!(generation.seed, Some(73));
    assert_eq!(generation.output_token_cap, std::num::NonZeroUsize::new(37));
    assert_eq!(
        store
            .load_session_head_meta()
            .await?
            .unwrap()
            .config
            .generation,
        generation
    );
    drop(merged);
    let replaced = core
        .session("generation-merge")
        .store(store.clone())
        .session_spec(
            crate::SessionSpec::new().replace_generation(lash_core::GenerationOptions {
                seed: Some(91),
                ..Default::default()
            }),
        )
        .open()
        .await?;
    assert_eq!(replaced.policy_snapshot().generation.seed, Some(91));
    assert_eq!(replaced.policy_snapshot().generation.output_token_cap, None);
    drop(replaced);
    let cleared = core
        .session("generation-merge")
        .store(store.clone())
        .session_spec(crate::SessionSpec::new().clear_generation())
        .open()
        .await?;
    assert_eq!(cleared.policy_snapshot().generation, Default::default());
    assert_eq!(
        store
            .load_session_head_meta()
            .await?
            .unwrap()
            .config
            .generation,
        Default::default()
    );
    Ok(())
}
