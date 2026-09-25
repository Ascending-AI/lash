use super::*;
use lash::rlm::RlmSendBuilderExt;

fn deferred_tools_test_core(
    data_dir: &std::path::Path,
    provider: ProviderHandle,
    deferred: deferred_tools::WorkbenchDeferredTools,
) -> LashCore {
    let backend = test_file_backend(data_dir);
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(workbench_lashlang_abilities()),
        &backend.clone().into(),
    )
    .with_deferred_tool_resolver(deferred.resolver());
    LashCore::rlm_builder(backend.into(), lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .session_spec(lash::SessionSpec::new().turn_budget(lash::TurnBudget::Unbounded))
        .model(test_model())
        // The `processes` module is catalogue presence, not an ability bit (ADR
        // 0095): the workbench's scripted sources author `processes.*`, so the
        // surface only exists when this factory is installed, as bootstrap does.
        .plugin(Arc::new(lash_plugin_process_controls::SessionProcessAdminPluginFactory::new()))
        .plugin(Arc::new(
            WorkbenchPluginFactory::new().with_deferred_tools(deferred),
        ))
        .without_queued_work()
        .build(crate::test_core_owner())
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
        let deferred =
            deferred_tools::WorkbenchDeferredTools::open(data_dir.join("deferred-tool-grants.db"))
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
                            r#"<typescript>
const matches = await tools.search({ query: "text checksum", limit: 1 });
print(matches);
</typescript>"#,
                        )),
                        1 => {
                            let request = serde_json::to_string(&request.messages)
                                .expect("serialize provider request");
                            assert!(request.contains("text.sha256"), "{request}");
                            Ok(text_response(
                                r#"<typescript>
const result = await text.sha256({ text: "restart proof" });
finish(result.digest);
</typescript>"#,
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
            .send(lash::TurnInput::text(
                "Find the checksum utility, then checksum restart proof.",
            ))
            .require_finish()
            .expect("require deferred finish")
            .output()
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
        let deferred =
            deferred_tools::WorkbenchDeferredTools::open(data_dir.join("deferred-tool-grants.db"))
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
                            r#"<typescript>
const matches = await tools.search({ query: "text checksum", limit: 1 });
const result = await text.sha256({ text: "too soon" });
finish(result.digest);
</typescript>"#,
                        )),
                        1 => {
                            let request = serde_json::to_string(&request.messages)
                                .expect("serialize same-block error request");
                            assert!(request.contains("text.sha256"), "{request}");
                            assert!(request.contains("link"), "{request}");
                            Ok(text_response(
                                r#"<typescript>
const result = await mystery.not_real({});
finish(result);
</typescript>"#,
                            ))
                        }
                        2 => {
                            let request = serde_json::to_string(&request.messages)
                                .expect("serialize unknown-path error request");
                            assert!(request.contains("mystery.not_real"), "{request}");
                            assert!(request.contains("link"), "{request}");
                            Ok(text_response(
                                r#"<typescript>
finish("typed link failures observed");
</typescript>"#,
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
            .send(lash::TurnInput::text("Exercise deferred link failures."))
            .require_finish()
            .expect("require link-error finish")
            .output()
            .await
            .expect("recover after typed link errors");
        assert_eq!(
            output.final_value(),
            Some(&json!("typed link failures observed"))
        );
        let _ = std::fs::remove_dir_all(data_dir);
    });
}
