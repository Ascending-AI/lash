#[test]
fn commit_budget_is_explicit_host_policy_with_no_implicit_builder_fallback() {
    let budget = lash::CommitBudget::new(
        lash::CommitBudgetLimit::bounded(1024 * 1024),
        lash::CommitBudgetLimit::Unbounded,
    );
    let expected_bytes = std::num::NonZeroUsize::new(1024 * 1024).expect("non-zero byte budget");
    assert_eq!(budget.bytes, lash::CommitBudgetLimit::Bounded(expected_bytes));
    assert_eq!(budget.nodes, lash::CommitBudgetLimit::Unbounded);

    let bounded = lash::CommitBudget::bounded(1024 * 1024, 512);
    let host = lash::durability::RuntimeHostConfig::in_memory(bounded);
    assert_eq!(host.durability.commit_budget, bounded);

    let error = match lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(trigger_registration_provider())
        .model(test_model())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build()
    {
        Ok(_) => panic!("builder must not invent a commit budget"),
        Err(error) => error,
    };
    assert!(matches!(error, lash::EmbedError::MissingCommitBudget));

    let configured = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(trigger_registration_provider())
        .model(test_model())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(bounded)
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build();
    assert!(configured.is_ok(), "an explicit commit budget should build");
}
