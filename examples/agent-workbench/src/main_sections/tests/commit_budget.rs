use super::*;

/// A core's commit budget and queued-work batching are host policy the
/// builder never invents: a builder missing either refuses to build, typed,
/// and one stating both builds over a durable SQLite memory backend.
#[tokio::test]
async fn commit_budget_is_explicit_host_policy_with_no_implicit_builder_fallback() {
    let budget = lash::CommitBudget::new(
        lash::CommitBudgetLimit::bounded(1024 * 1024),
        lash::CommitBudgetLimit::Unbounded,
    );
    let expected_bytes = std::num::NonZeroUsize::new(1024 * 1024).expect("non-zero byte budget");
    assert_eq!(
        budget.bytes,
        lash::CommitBudgetLimit::Bounded(expected_bytes)
    );
    assert_eq!(budget.nodes, lash::CommitBudgetLimit::Unbounded);

    let bounded = lash::CommitBudget::bounded(1024 * 1024, 512);
    let batching = lash::QueuedWorkBatchingConfig::new(1)
        .with_max_rows(8)
        .with_max_pending_age(Duration::from_secs(5));
    assert_eq!(batching.action_token_reserve(), 1);
    assert_eq!(batching.max_rows(), 8);
    assert_eq!(batching.max_pending_age(), Duration::from_secs(5));
    assert_eq!(lash::QueuedWorkBatchingConfig::DEFAULT_MAX_ROWS, 64);
    assert_eq!(
        lash::QueuedWorkBatchingConfig::DEFAULT_MAX_PENDING_AGE,
        Duration::from_secs(30)
    );

    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let backend = || {
        lash::durable::DurableBackendBuilder::new(Arc::clone(&stores))
            .build()
            .expect("the durable backend builds")
    };
    let profiles = || {
        Arc::new(WorkbenchLlmProfiles {
            provider: silent_provider(),
        })
    };
    let owner = || {
        lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("agent-workbench-test"),
            lash::persistence::LeaseIncarnationId::new(uuid::Uuid::new_v4().to_string()),
        )
    };

    let stated = || {
        LashCore::standard_builder(backend())
            .llm_profiles(profiles())
            .data_retention(lash::DataRetention::standard())
            .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
    };
    let error = match stated()
        .queued_work_batching(batching.clone())
        .build(owner())
    {
        Ok(_) => panic!("the builder must not invent a commit budget"),
        Err(error) => error,
    };
    assert!(matches!(error, lash::EmbedError::MissingCommitBudget));

    let error = match stated().commit_budget(bounded).build(owner()) {
        Ok(_) => panic!("the builder must not invent a queued-work action reserve"),
        Err(error) => error,
    };
    assert!(matches!(error, lash::EmbedError::MissingQueuedWorkBatching));

    let core = stated()
        .commit_budget(bounded)
        .queued_work_batching(batching)
        .build(owner())
        .expect("an explicit commit budget and batching build");
    core.shutdown().await.expect("the core shuts down");
}
