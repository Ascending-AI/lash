




#[tokio::test]
async fn catalog_refresh_timeout_reaps_the_service_and_retains_the_last_catalog() {
    let clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().expect("fixture directory");
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "stall_refresh",
            ..MockOptions::default()
        },
    )
    .await;
    let entry = entry(&pool);
    let mut lifecycle = scripted::Lifecycle::new();
    lifecycle.observe(&entry);
    let service = entry.service_snapshot().expect("initial service");
    entry.request_tool_refresh(service.generation);
    while received(root.path())
        .lines()
        .filter(|line| line.contains("tools/list"))
        .count()
        < 2
    {
        tokio::task::yield_now().await;
    }
    clock
        .expire(Instant::now() + entry.config.startup_timeout())
        .await;
    let reconnect = lifecycle.reconnect_scheduled().await;
    assert!(
        entry.service_snapshot().is_none(),
        "timeout destroys the service's unanswered SDK responders"
    );
    assert_eq!(pool.advertised_tools()[0].name(), mcp_name("mock", "work"));
    for _ in 0..100_000 {
        entry.request_tool_refresh(service.generation);
    }
    clock.expire(reconnect).await;
    published_generation(&entry, 2).await;
    entry
        .establish()
        .await
        .expect("new-generation control barrier");
    assert_eq!(
        received(root.path())
            .lines()
            .filter(|line| line.contains("tools/list"))
            .count(),
        3,
        "old-generation notifications do not refresh the replacement service"
    );
    drop(clock);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn catalog_page_and_item_boundaries_are_inclusive() {
    let clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().expect("fixture directory");
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "boundary",
            ..MockOptions::default()
        },
    )
    .await;
    assert_eq!(pool.advertised_tools().len(), 4096);
    assert_eq!(
        received(root.path())
            .lines()
            .filter(|line| line.contains("tools/list"))
            .count(),
        64
    );
    drop(clock);
    pool.shutdown_all().await;
}
