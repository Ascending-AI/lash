use super::*;

const SEED: u64 = 0x5c_f102;

#[tokio::test]
async fn settled_config_survives_park_without_pending_graph_nodes() -> Result<()> {
    let double = restate_double(SEED).await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("parked-config").open().await?;
    session
        .send(TurnInput::text("establish head"))
        .output()
        .await?;
    let expected_model = model_spec("settled-model", Some("settled-variant".to_string()), 64_000);
    let expected_generation = lash_core::GenerationOptions {
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.35).expect("temperature")),
        output_token_cap: std::num::NonZeroUsize::new(777),
        ..lash_core::GenerationOptions::default()
    };
    session
        .admin()
        .config()
        .update(SessionConfigPatch {
            model: Some(expected_model.clone()),
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                expected_generation.clone(),
            )),
            ..SessionConfigPatch::default()
        })
        .await?;

    let parked = Box::pin(session.park()).await?;
    let resumed = Box::pin(core.resume(parked)).await?;
    let policy = resumed.policy_snapshot();
    assert_eq!(policy.model, expected_model);
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
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("incidental-reopen").open().await?;
    session
        .send(TurnInput::text("establish head"))
        .output()
        .await?;
    let commanded_model = model_spec("commanded-model", None, 64_000);
    let commanded_generation = lash_core::GenerationOptions {
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.55).expect("temperature")),
        ..lash_core::GenerationOptions::default()
    };
    session
        .admin()
        .config()
        .update(SessionConfigPatch {
            model: Some(commanded_model.clone()),
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                commanded_generation.clone(),
            )),
            ..SessionConfigPatch::default()
        })
        .await?;
    Box::pin(session.close()).await?;

    let reopened = core.session("incidental-reopen").open().await?;
    let policy = reopened.policy_snapshot();
    assert_eq!(
        policy.model, commanded_model,
        "a default-spec reopen keeps the commanded durable model"
    );
    assert_eq!(
        policy.generation, commanded_generation,
        "a default-spec reopen keeps the commanded durable generation"
    );
    Ok(())
}

/// FIG-1896 seed-then-write: a host-supplied open value wins at open AND is
/// durable immediately after open — a crash/reload right after open yields
/// the host value, not the pre-open head. The seed write settles before the
/// session handle is observable, so the durable head is true again by the
/// time `open()` returns.
#[tokio::test]
async fn host_supplied_reopen_value_is_durable_immediately_after_open() -> Result<()> {
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
    let mut policy = core.policy.clone();
    policy.session_id = Some(lash_core::SessionId::from("seeded-reopen"));
    let store = lash_core::SessionStoreFactory::create_store(
        factory.as_ref(),
        &lash_core::SessionStoreCreateRequest {
            session_id: lash_core::SessionId::from("seeded-reopen"),
            relation: lash_core::SessionRelation::Root,
            policy,
            pending_observer_intents: Vec::new(),
        },
    )
    .await?;

    // Establish a durable head carrying the original model.
    let session = core.session("seeded-reopen").open().await?;
    session
        .send(TurnInput::text("establish head"))
        .output()
        .await?;
    drop(session);

    // Reopen with an explicit host model: the seed wins at open...
    let host_model = model_spec("host-seeded-model", None, 64_000);
    let reopened = core
        .session("seeded-reopen")
        .session_spec(crate::SessionSpec::new().model(host_model.clone()))
        .open()
        .await?;
    assert_eq!(reopened.policy_snapshot().model, host_model);

    // ...and the head already carries it before any turn runs.
    let head = store
        .load_session_head_meta()
        .await?
        .expect("a durable head exists after reopen");
    assert_eq!(
        head.config.model, host_model,
        "the host seed is guard-written before open returns"
    );
    drop(reopened);

    // A subsequent incidental reopen inherits it from the head, proving the
    // value is durable rather than resident-only.
    let reloaded = core.session("seeded-reopen").open().await?;
    assert_eq!(
        reloaded.policy_snapshot().model,
        host_model,
        "a crash/reload after the seeded open yields the host value"
    );
    Ok(())
}
