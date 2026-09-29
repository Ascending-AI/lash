use super::*;
use lash_sansio::SessionId;

const SEED: u64 = 0x5c_f103;

/// FIG-4099: generation is creation config. A reopen that states a
/// generation overlay runs with what the session recorded and writes nothing;
/// the overlay's merge, replace and clear are `update(SessionConfigPatch)`
/// changes, each durable.
#[tokio::test]
async fn generation_changes_are_patches_and_a_reopen_overlay_is_ignored() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("generation-merge")
        .session_spec(
            crate::SessionSpec::new().generation(lash_core::GenerationOptions {
                seed: Some(73),
                ..Default::default()
            }),
        )
        .open()
        .await?;
    let store =
        lash_core::store::SessionStore::new(factory.clone(), SessionId::from("generation-merge"))?;
    let recorded_generation = || async {
        store
            .load_session_head_meta()
            .await
            .expect("load the head")
            .expect("creation wrote the head")
            .config
            .generation
    };
    assert_eq!(
        recorded_generation().await.seed,
        Some(73),
        "creation bakes the stated generation into the head"
    );
    session
        .send(TurnInput::text("commit the initial generation seed"))
        .output()
        .await?;
    drop(session);

    let before = store.load_session_head_meta().await?.expect("head");
    let reopened = core
        .session("generation-merge")
        .session_spec(
            crate::SessionSpec::new().generation(lash_core::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(37),
                ..Default::default()
            }),
        )
        .open()
        .await?;
    assert_eq!(reopened.policy_snapshot().generation.seed, Some(73));
    assert_eq!(
        reopened.policy_snapshot().generation.output_token_cap,
        None,
        "a reopen's overlay is ignored"
    );
    let after = store.load_session_head_meta().await?.expect("head");
    assert_eq!(
        after.head_revision, before.head_revision,
        "the reopen wrote nothing"
    );
    assert_eq!(after.config, before.config, "the reopen wrote nothing");

    let config = reopened.admin().config();
    config
        .update(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Merge(
                lash_core::GenerationOptions {
                    output_token_cap: std::num::NonZeroUsize::new(37),
                    ..Default::default()
                },
            )),
            ..crate::SessionConfigPatch::default()
        })
        .await?;
    let generation = reopened.policy_snapshot().generation;
    assert_eq!(generation.seed, Some(73), "a merge keeps unstated options");
    assert_eq!(generation.output_token_cap, std::num::NonZeroUsize::new(37));
    assert_eq!(recorded_generation().await, generation);

    config
        .update(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(
                lash_core::GenerationOptions {
                    seed: Some(91),
                    ..Default::default()
                },
            )),
            ..crate::SessionConfigPatch::default()
        })
        .await?;
    assert_eq!(reopened.policy_snapshot().generation.seed, Some(91));
    assert_eq!(
        reopened.policy_snapshot().generation.output_token_cap,
        None,
        "a replace discards unstated options"
    );
    assert_eq!(recorded_generation().await.seed, Some(91));

    config
        .update(crate::SessionConfigPatch {
            generation: Some(crate::GenerationOverlay::Replace(Default::default())),
            ..crate::SessionConfigPatch::default()
        })
        .await?;
    assert_eq!(reopened.policy_snapshot().generation, Default::default());
    assert_eq!(recorded_generation().await, Default::default());
    Ok(())
}
