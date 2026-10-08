//! A session's config transactions through the facade, on its own node
//! (FIG-4379): a resubmission under one id is the same transaction, and
//! every applied transaction is announced to the session's plugins.

use super::*;

/// A session created on `core` and opened there.
async fn opened(core: &LashCore, id: &str) -> Result<crate::LashSession> {
    let id = crate::SessionId::parse(id).expect("nonblank host identity");
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    core.session(id).open().await
}

fn budget(turns: usize) -> crate::config::ConfigTransaction {
    crate::config::ConfigTransaction::of(crate::config::SetTurnBudget {
        turn_budget: crate::TurnBudget::bounded(turns),
    })
}

/// A resubmission under one id with the same content is the same command:
/// it names the first one's receipt and settles as it did. The same id with
/// other content, or written against another revision, is refused
/// `ChangedContent` and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resubmitted_transaction_reuses_its_receipt_and_refuses_changed_content() -> Result<()> {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session = opened(&core, "config-resubmitted").await?;
    let config = session.admin().config();
    let base = config.revision().await?;
    let write = || crate::config::ConfigWrite::new("resubmitted", base);

    let crate::config::ConfigSettlement::Pending(first) = config.submit(write(), budget(7)).await?
    else {
        panic!("a store-backed session queues its transaction");
    };
    let crate::config::ConfigSettlement::Pending(again) = config.submit(write(), budget(7)).await?
    else {
        panic!("the resubmission names a queued command");
    };
    assert_eq!(again, first, "the resubmission names the same command");
    for (what, changed) in [
        ("other content", config.submit(write(), budget(9)).await),
        (
            "another expected revision",
            config
                .submit(
                    crate::config::ConfigWrite::new("resubmitted", base + 1),
                    budget(7),
                )
                .await,
        ),
    ] {
        assert!(
            matches!(
                &changed,
                Err(crate::EmbedError::ConfigSubmit(
                    lash_core::ConfigSubmitError::ChangedContent { id }
                )) if id == "resubmitted"
            ),
            "{what}: {changed:?}"
        );
    }
    let settled = config.settle(first).await?;
    assert_eq!(
        settled,
        crate::config::ConfigSettlement::Settled(
            crate::config::ConfigTransactionOutcome::Applied {
                base_revision: base,
                revision: base + 1,
                outputs: vec![serde_json::Value::Null],
            }
        ),
        "the one command applies once"
    );
    assert_eq!(config.revision().await?, base + 1);
    assert_eq!(
        session.policy_snapshot().turn_budget,
        crate::TurnBudget::bounded(7)
    );
    core.shutdown().await?;
    Ok(())
}

/// What a plugin's `SessionConfigChanged` observer was handed: the policy
/// before and after each applied transaction.
type Changes = Arc<StdMutex<Vec<(lash_core::SessionPolicy, lash_core::SessionPolicy)>>>;

/// Every applied core command announces `SessionConfigChanged` to the
/// session's plugins with the policy before and after it, whatever it
/// changed: the model binding (to another key, or another transport for one
/// wire model), generation and the turn budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5317: the durable session actor applies a config transaction without building its plugins, so no SessionConfigChanged observer runs"]
async fn every_applied_config_transaction_emits_a_lifecycle_event() -> Result<()> {
    let changes: Changes = Arc::default();
    let observer = {
        let changes = Arc::clone(&changes);
        StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("config-observer"),
            lash_core::facade_support::PluginSpec::new().with_runtime_event(
                crate::hook_key!("config-changed"),
                Arc::new(move |event| {
                    let changes = Arc::clone(&changes);
                    Box::pin(async move {
                        if let lash_core::plugin::PluginLifecycleEvent::SessionConfigChanged(ctx) =
                            event
                        {
                            changes
                                .lock_recover()
                                .push((ctx.previous.clone(), ctx.current.clone()));
                        }
                        Ok(())
                    })
                }),
            ),
        )
    };
    let alt = lash_core::LlmProfileMetadata::builder("alt-model")
        .context_window_tokens(123_456)
        .build()
        .expect("valid model metadata");
    let unwired = |kind: &'static str| {
        crate::testing::TestProvider::builder()
            .kind(kind)
            .complete_error("not wired")
            .build()
            .into_handle()
    };
    // Two keys share one wire model on different transports: a model change
    // moves the transport only through the key the registry minted.
    let models = lash_core::LlmProfileRegistry::new()
        .register(
            mock_llm_profile_spec().wire_model.clone(),
            lash_core::RegisteredLlmProfile::new(mock_llm_profile_spec(), mock_provider()),
        )
        .expect("register the mock model")
        .register(
            "alt-model",
            lash_core::RegisteredLlmProfile::new(alt.clone(), unwired("alt")),
        )
        .expect("register alt-model")
        .register(
            "alt-model-on-alt",
            lash_core::RegisteredLlmProfile::new(alt, unwired("alt-on-alt")),
        )
        .expect("register alt-model-on-alt");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .llm_profiles(Arc::new(models))
    .plugin(Arc::new(observer))
    .build(crate::testing::runtime_lease_owner())?;
    let session = opened(&core, "config-lifecycle").await?;
    let config = session.admin().config();
    let generation = lash_core::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    let transactions = [
        crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
            model: crate::LlmProfileKey::new("alt-model"),
        }),
        crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
            model: crate::LlmProfileKey::new("alt-model-on-alt"),
        }),
        crate::config::ConfigTransaction::of(crate::config::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(generation.clone()),
        }),
        budget(9),
    ];
    for (index, transaction) in transactions.into_iter().enumerate() {
        let revision = config.revision().await?;
        let outcome = config
            .apply(
                crate::config::ConfigWrite::new(format!("lifecycle-{index}"), revision),
                transaction,
            )
            .await?
            .await_outcome(&config)
            .await?;
        assert!(
            matches!(
                outcome,
                crate::config::ConfigTransactionOutcome::Applied { .. }
            ),
            "{index}: {outcome:?}"
        );
    }

    let changes = changes.lock_recover().clone();
    assert_eq!(
        changes.len(),
        4,
        "every applied transaction is announced once"
    );
    let key = |policy: &lash_core::SessionPolicy| {
        policy
            .profile_key()
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    let (previous, current) = &changes[0];
    assert_eq!(key(previous), mock_llm_profile_spec().wire_model);
    assert_eq!(key(current), "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(key(previous), "alt-model");
    assert_eq!(key(current), "alt-model-on-alt");
    assert_eq!(previous.wire_model(), current.wire_model());
    let (_, current) = &changes[2];
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[3];
    assert_eq!(previous.generation, generation);
    assert_eq!(current.turn_budget, crate::TurnBudget::bounded(9));
    core.shutdown().await?;
    Ok(())
}
