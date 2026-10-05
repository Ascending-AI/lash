use super::*;

const SEED: u64 = 0x5c_f102;

#[tokio::test]
async fn settled_config_survives_park_without_pending_graph_nodes() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
        .llm_profiles(test_catalog(
            mock_provider(),
            [
                mock_llm_profile_spec(),
                llm_profile_spec("settled-model", Some("settled-variant".to_string()), 64_000),
            ],
        ))
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session(crate::SessionId::parse("parked-config").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("establish head"))
        .output()
        .await?;
    let settled_reasoning = lash_core::ReasoningSelection::Effort("settled-variant".to_string());
    let expected_model = recorded_llm_profile(llm_profile_spec(
        "settled-model",
        Some("settled-variant".to_string()),
        64_000,
    ))
    .with_reasoning(settled_reasoning.clone());
    let expected_generation = lash_core::GenerationOptions {
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.35).expect("temperature")),
        output_token_cap: std::num::NonZeroUsize::new(777),
        ..lash_core::GenerationOptions::default()
    };
    session
        .admin()
        .config()
        .configure(
            crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
                model: lash_core::LlmProfileKey::new("settled-model"),
            })
            .then(crate::config::SetReasoning {
                reasoning: settled_reasoning,
            })
            .then(crate::config::SetGeneration {
                generation: lash_core::facade_support::GenerationOverlay::Replace(
                    expected_generation.clone(),
                ),
            }),
        )
        .await?;

    let parked = Box::pin(session.park()).await?;
    let resumed = Box::pin(core.resume(parked)).await?;
    let policy = resumed.policy_snapshot();
    assert_eq!(policy.model, Some(expected_model));
    assert_eq!(policy.generation, expected_generation);
    Ok(())
}
