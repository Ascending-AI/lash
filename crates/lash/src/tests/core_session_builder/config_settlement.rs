use super::*;

const SEED: u64 = 0x5c_f102;

#[tokio::test]
async fn settled_config_survives_park_without_pending_graph_nodes() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .models(test_catalog(
        mock_provider(),
        [
            mock_model_spec(),
            model_spec("settled-model", Some("settled-variant".to_string()), 64_000),
        ],
    ))
    .model("mock-model")
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("parked-config").created().await.open().await?;
    session
        .send(TurnInput::text("establish head"))
        .output()
        .await?;
    let settled_reasoning = lash_core::ReasoningSelection::Effort("settled-variant".to_string());
    let expected_model = recorded_model(model_spec(
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
            crate::config::ConfigTransaction::of(crate::config::SetModel {
                model: lash_core::ModelKey::new("settled-model"),
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

/// A commanded config change is durable across an incidental reopen: a host
/// that reopens the session with a default spec (no model, no generation)
/// keeps the settled durable values instead of reseeding core defaults over
/// them. This pins the failure class where a cold observer open reverted a
/// mid-run model change (FIG-1875 seed-then-write + presence-aware
/// reconciliation).
#[tokio::test]
async fn commanded_model_survives_an_incidental_default_spec_reopen() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .models(test_catalog(
        mock_provider(),
        [
            mock_model_spec(),
            model_spec("commanded-model", None, 64_000),
        ],
    ))
    .model("mock-model")
    .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("incidental-reopen")
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("establish head"))
        .output()
        .await?;
    let commanded_model = recorded_model(model_spec("commanded-model", None, 64_000));
    let commanded_generation = lash_core::GenerationOptions {
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.55).expect("temperature")),
        ..lash_core::GenerationOptions::default()
    };
    session
        .admin()
        .config()
        .configure(
            crate::config::ConfigTransaction::of(crate::config::SetModel {
                model: lash_core::ModelKey::new("commanded-model"),
            })
            .then(crate::config::SetGeneration {
                generation: lash_core::facade_support::GenerationOverlay::Replace(
                    commanded_generation.clone(),
                ),
            }),
        )
        .await?;
    Box::pin(session.close()).await?;

    let reopened = core
        .session("incidental-reopen")
        .created()
        .await
        .open()
        .await?;
    let policy = reopened.policy_snapshot();
    assert_eq!(
        policy.model,
        Some(commanded_model),
        "a default-spec reopen keeps the commanded durable model"
    );
    assert_eq!(
        policy.generation, commanded_generation,
        "a default-spec reopen keeps the commanded durable generation"
    );
    Ok(())
}
