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
    let batching = lash::QueuedWorkBatchingConfig::new(1)
        .with_max_rows(8)
        .with_max_pending_age(std::time::Duration::from_secs(5));
    let host = lash::durability::RuntimeHostConfig::in_memory(bounded, batching.clone());
    assert_eq!(host.durability.commit_budget, bounded);
    assert_eq!(host.durability.queued_work_batching, batching);
    assert_eq!(batching.action_token_reserve(), 1);
    assert_eq!(batching.max_rows(), 8);
    assert_eq!(batching.max_pending_age(), std::time::Duration::from_secs(5));
    assert_eq!(lash::QueuedWorkBatchingConfig::DEFAULT_MAX_ROWS, 64);
    let default_pending_age = lash::QueuedWorkBatchingConfig::DEFAULT_MAX_PENDING_AGE;
    assert_eq!(default_pending_age, std::time::Duration::from_secs(30));

    let error = match lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(trigger_registration_provider())
        .model(test_model())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(crate::test_core_owner())
    {
        Ok(_) => panic!("builder must not invent a commit budget"),
        Err(error) => error,
    };
    assert!(matches!(error, lash::EmbedError::MissingCommitBudget));

    let error = match lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(trigger_registration_provider())
        .model(test_model())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .commit_budget(bounded)
        .build(crate::test_core_owner())
    {
        Ok(_) => panic!("builder must not invent a queued-work action reserve"),
        Err(error) => error,
    };
    assert!(matches!(error, lash::EmbedError::MissingQueuedWorkBatching));

    let configured = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .without_queued_work()
        .provider(trigger_registration_provider())
        .model(test_model())
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .commit_budget(bounded)
        .queued_work_batching(batching)
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .build(crate::test_core_owner());
    assert!(configured.is_ok(), "an explicit commit budget should build");
}
