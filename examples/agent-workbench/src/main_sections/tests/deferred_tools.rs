fn deferred_tools_test_core(
    data_dir: &std::path::Path,
    provider: ProviderHandle,
    deferred: deferred_tools::WorkbenchDeferredTools,
) -> LashCore {
    let artifact_store = Arc::new(sync_await({
        let path = data_dir.join("artifacts.db");
        async move {
            lash_sqlite_store::Store::open(&path)
                .await
                .expect("open deferred test artifact store")
        }
    })) as Arc<dyn lash::persistence::LashlangArtifactStore>;
    let process_env_store = Arc::new(sync_await({
        let path = data_dir.join("process-env.db");
        async move {
            lash_sqlite_store::Store::open(&path)
                .await
                .expect("open deferred test process env store")
        }
    })) as Arc<dyn lash::persistence::ProcessExecutionEnvStore>;
    let trigger_store = Arc::new(sync_await({
        let path = data_dir.join("triggers.db");
        async move {
            lash_sqlite_store::SqliteTriggerStore::open(&path)
                .await
                .expect("open deferred test trigger store")
        }
    }));
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(lash::rlm::ExecutionBound::instructions(1_000_000), lash::rlm::ExecutionBound::secs(30))
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        artifact_store,
    )
    .with_deferred_tool_resolver(deferred.resolver());
    LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(test_model())
        .store_factory(Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        ))
        .plugin(Arc::new(
            WorkbenchPluginFactory::new("").with_deferred_tools(deferred),
        ))
        .trigger_store(trigger_store)
        .disable_queued_work_driver()
        .advanced()
        .runtime_host_config(lash::durability::RuntimeHostConfig::new(
            Arc::new(lash::durability::InlineEffectHost::default()),
            test_attachment_store(),
            process_env_store,
            lash::CommitBudget::bounded(1024 * 1024, 512),
            lash::QueuedWorkBatchingConfig::new(1),
        ))
        .build()
        .expect("build deferred-tool test core")
}

#[test]
fn deferred_search_observation_enables_next_block_call() {
    run_async_test_on_stack_budget("workbench-deferred-round-trip", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-deferred-round-trip-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create deferred round-trip dir");
        let deferred = deferred_tools::WorkbenchDeferredTools::open(
            data_dir.join("deferred-tool-grants.db"),
        )
        .expect("open deferred grants");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-deferred-round-trip")
            .complete(move |request| {
                let calls = Arc::clone(&calls);
                async move {
                    let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match call {
                        0 => Ok(text_response(
                            r#"<lashlang>
matches = await tools.search({ query: "text checksum", limit: 1 })?
print(matches)
</lashlang>"#,
                        )),
                        1 => {
                            let request = serde_json::to_string(&request.messages)
                                .expect("serialize provider request");
                            assert!(request.contains("text.sha256"), "{request}");
                            Ok(text_response(
                                r#"<lashlang>
result = await text.sha256({ text: "restart proof" })?
finish result.digest
</lashlang>"#,
                            ))
                        }
                        other => panic!("unexpected deferred round-trip provider call {other}"),
                    }
                }
            })
            .build()
            .into_handle();
        let core = deferred_tools_test_core(&data_dir, provider, deferred);
        let session = core
            .session("workbench-deferred-round-trip")
            .open()
            .await
            .expect("open deferred round-trip session");
        let output = session
            .turn(lash::TurnInput::text(
                "Find the checksum utility, then checksum restart proof.",
            ))
            .require_finish()
            .expect("require deferred finish")
            .run()
            .await
            .expect("deferred search and call round trip");
        assert_eq!(
            output.final_value(),
            Some(&json!(
                "6aaa2c8b150bc016006f9d88df2e273adea1965bc4e8c66f87357b62a8e3afc9"
            ))
        );
        session.close().await.expect("close deferred session");
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

#[test]
fn same_block_discovery_cannot_relink_and_unknown_paths_report_link_errors() {
    run_async_test_on_stack_budget("workbench-deferred-link-errors", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-deferred-link-errors-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create deferred link-error dir");
        let deferred = deferred_tools::WorkbenchDeferredTools::open(
            data_dir.join("deferred-tool-grants.db"),
        )
        .expect("open deferred grants");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-deferred-link-errors")
            .complete(move |request| {
                let calls = Arc::clone(&calls);
                async move {
                    let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match call {
                        0 => Ok(text_response(
                            r#"<lashlang>
matches = await tools.search({ query: "text checksum", limit: 1 })?
result = await text.sha256({ text: "too soon" })?
finish result.digest
</lashlang>"#,
                        )),
                        1 => {
                            let request = serde_json::to_string(&request.messages)
                                .expect("serialize same-block error request");
                            assert!(request.contains("text.sha256"), "{request}");
                            assert!(request.contains("link"), "{request}");
                            Ok(text_response(
                                r#"<lashlang>
result = await mystery.not_real({})?
finish result
</lashlang>"#,
                            ))
                        }
                        2 => {
                            let request = serde_json::to_string(&request.messages)
                                .expect("serialize unknown-path error request");
                            assert!(request.contains("mystery.not_real"), "{request}");
                            assert!(request.contains("link"), "{request}");
                            Ok(text_response(
                                r#"<lashlang>
finish "typed link failures observed"
</lashlang>"#,
                            ))
                        }
                        other => panic!("unexpected deferred link-error provider call {other}"),
                    }
                }
            })
            .build()
            .into_handle();
        let core = deferred_tools_test_core(&data_dir, provider, deferred);
        let output = core
            .session("workbench-deferred-link-errors")
            .open()
            .await
            .expect("open deferred link-error session")
            .turn(lash::TurnInput::text("Exercise deferred link failures."))
            .require_finish()
            .expect("require link-error finish")
            .run()
            .await
            .expect("recover after typed link errors");
        assert_eq!(output.final_value(), Some(&json!("typed link failures observed")));
        let _ = std::fs::remove_dir_all(data_dir);
    });
}
