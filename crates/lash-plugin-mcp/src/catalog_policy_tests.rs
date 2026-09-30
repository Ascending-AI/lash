#[tokio::test]
async fn catalog_storm_returns_promptly_and_shutdown_cancels_stalled_refresh() {
    use futures_util::FutureExt;
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
    let service = entry.service_snapshot().expect("connected service");
    let handler = McpToolListRefresh {
        entry: Arc::downgrade(&entry),
        service_generation: service.generation,
    };
    for notification in 0..100_000 {
        assert!(
            handler
                .refresh_tools(service.peer.clone())
                .now_or_never()
                .is_some(),
            "notification {notification} retained an independent stalled discovery"
        );
        if notification == 0 {
            while received(root.path())
                .lines()
                .filter(|line| line.contains("tools/list"))
                .count()
                < 2
            {
                tokio::task::yield_now().await;
            }
        }
    }
    // An establish reply is a control barrier, even while discovery is stalled.
    entry
        .establish()
        .await
        .expect("control progresses during storm");
    assert_eq!(
        received(root.path())
            .lines()
            .filter(|line| line.contains("tools/list"))
            .count(),
        2,
        "startup plus exactly one active refresh"
    );
    drop(clock);
    pool.shutdown_all().await;
    assert!(entry.actor_handle.lock_recover().is_none());
    assert!(entry.service_snapshot().is_none());
}

async fn assert_catalog_refused(behavior: &'static str, reason: &str) {
    let root = tempfile::tempdir().expect("fixture directory");
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior,
                startup_timeout_ms: 10_000,
                ..MockOptions::default()
            },
        ),
    )]))
    .await
    .expect("pool retains refused server");
    let status = pool.server_statuses().remove(0);
    let error = status
        .last_error
        .as_ref()
        .map(McpServerFault::message)
        .unwrap_or("")
        .to_string();
    pool.shutdown_all().await;
    assert!(!status.connected, "oversized catalog was installed");
    assert!(error.contains(reason), "expected {reason}, got {error}");
}

#[tokio::test]
async fn catalog_cursor_cycle_is_refused() {
    assert_catalog_refused("cursor_cycle", "cursor cycle").await;
}
#[tokio::test]
async fn catalog_page_limit_plus_one_is_refused() {
    assert_catalog_refused("page_limit", "page limit").await;
}
#[tokio::test]
async fn catalog_item_limit_plus_one_is_refused() {
    assert_catalog_refused("item_limit", "item limit").await;
}
#[tokio::test]
async fn catalog_byte_limit_plus_one_is_refused() {
    assert_catalog_refused("byte_limit", "byte limit").await;
}

#[tokio::test]
async fn catalog_storm_keeps_one_repeat_and_installs_each_valid_refresh_once() {
    use futures_util::FutureExt;
    let clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().expect("fixture directory");
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "refresh_version",
            ..MockOptions::default()
        },
    )
    .await;
    let entry = entry(&pool);
    let service = entry.service_snapshot().expect("connected service");
    let handler = McpToolListRefresh {
        entry: Arc::downgrade(&entry),
        service_generation: service.generation,
    };
    let hook = Arc::new(ActorPauseHook::default());
    *entry.refresh_install_hook.write_recover() = Some(Arc::clone(&hook));
    assert!(
        handler
            .refresh_tools(service.peer.clone())
            .now_or_never()
            .is_some(),
        "the first notification returns before discovery finishes"
    );
    hook.reached.notified().await;
    for notification in 1..100_000 {
        assert!(
            handler
                .refresh_tools(service.peer.clone())
                .now_or_never()
                .is_some(),
            "notification {notification} retained work"
        );
    }
    entry
        .establish()
        .await
        .expect("control barrier during refresh");
    assert_eq!(
        received(root.path())
            .lines()
            .filter(|line| line.contains("tools/list"))
            .count(),
        2
    );
    hook.release.notify_one();
    hook.reached.notified().await;
    assert_eq!(
        pool.advertised_tools()[0].name(),
        mcp_name("mock", "work-2")
    );
    assert_eq!(
        received(root.path())
            .lines()
            .filter(|line| line.contains("tools/list"))
            .count(),
        3
    );
    hook.release.notify_one();
    while pool.advertised_tools()[0].name() != mcp_name("mock", "work-3") {
        tokio::task::yield_now().await;
    }
    entry
        .establish()
        .await
        .expect("control barrier after repeat");
    assert_eq!(
        received(root.path())
            .lines()
            .filter(|line| line.contains("tools/list"))
            .count(),
        3,
        "the storm retained exactly one repeat"
    );
    drop(clock);
    pool.shutdown_all().await;
}

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
