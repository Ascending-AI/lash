#[derive(Clone, Copy)]
struct WorkbenchControlTools;

#[async_trait]
impl lash::tools::StaticToolExecute for WorkbenchControlTools {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolResult {
        match call.name {
            "workbench_cancel" => lash::tools::ToolResult::from_output(
                lash::tools::ToolCallOutput::cancelled(lash::tools::ToolCancellation::runtime(
                    "the operator cancelled the workbench action",
                )),
            ),
            "workbench_finish" => lash::tools::ToolResult::from_output(
                lash::tools::ToolCallOutput::success(json!({ "accepted": true })).with_control(
                    lash::tools::ToolControl::Finish {
                        value: lash::tools::ToolValue::from(json!({
                            "finished_by": "workbench_finish"
                        })),
                    },
                ),
            ),
            "workbench_fail" => lash::tools::ToolResult::ok(json!({ "accepted": false }))
                .with_control(lash::tools::ToolControl::Fail {
                    failure: lash::tools::ToolFailure::tool(
                        lash::tools::ToolFailureClass::Execution,
                        "workbench_action_rejected",
                        "the workbench action was rejected",
                    ),
                }),
            other => lash::tools::ToolResult::err_fmt(format_args!(
                "unknown workbench control tool `{other}`"
            )),
        }
    }
}

fn workbench_control_tools() -> Arc<dyn lash::tools::ToolProvider> {
    use lash::tools::ToolDefinitionLashlangExt as _;

    let empty_input = json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    });
    let definitions = vec![
        lash::tools::ToolDefinition::raw(
            "tool:workbench_cancel",
            "workbench_cancel",
            "Cancel the current workbench action with a typed cancellation.",
            empty_input.clone(),
            json!({ "type": "object" }),
        )
        .with_lashlang_binding(lash::tools::LashlangToolBinding::new(
            ["workbench_control"],
            "cancel",
        )),
        lash::tools::ToolDefinition::raw(
            "tool:workbench_finish",
            "workbench_finish",
            "Finish the turn directly from a workbench tool.",
            empty_input.clone(),
            json!({ "type": "object" }),
        )
        .with_lashlang_binding(lash::tools::LashlangToolBinding::new(
            ["workbench_control"],
            "finish",
        )),
        lash::tools::ToolDefinition::raw(
            "tool:workbench_fail",
            "workbench_fail",
            "Stop the turn with a typed workbench tool error.",
            empty_input,
            json!({ "type": "object" }),
        )
        .with_lashlang_binding(lash::tools::LashlangToolBinding::new(
            ["workbench_control"],
            "fail",
        )),
    ];
    Arc::new(lash::tools::StaticToolProvider::new(
        definitions,
        WorkbenchControlTools,
    ))
}

fn workbench_control_response(source: &'static str) -> lash::provider::LlmResponse {
    text_response(&format!("<lashlang>\n{source}\n</lashlang>"))
}

#[test]
fn workbench_tools_expose_typed_cancellation_and_turn_control() {
    run_async_test_on_stack_budget("workbench-tool-control-test", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-tool-control-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create tool control data dir");
        let responses = Arc::new(Mutex::new(std::collections::VecDeque::from([
            workbench_control_response(
                "cancelled = await workbench_control.cancel({})\nfinish \"cancellation observed\"",
            ),
            workbench_control_response("await workbench_control.finish({})?"),
            workbench_control_response("await workbench_control.fail({})?"),
        ])));
        let provider_responses = Arc::clone(&responses);
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-tool-control-provider")
            .complete(move |_| {
                let responses = Arc::clone(&provider_responses);
                async move {
                    responses
                        .lock()
                        .expect("workbench tool control responses lock")
                        .pop_front()
                        .ok_or_else(|| {
                            lash::provider::LlmTransportError::new(
                                "workbench tool control response queue exhausted",
                            )
                        })
                }
            })
            .build()
            .into_handle();
        let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        )) as Arc<dyn lash::persistence::SessionStoreFactory>;
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &data_dir.join("processes.db"),
                data_dir.join("lash-sessions"),
            )
            .await
            .expect("open tool control process registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(
                lash::ModelSpec::from_token_limits(
                    "workbench-tool-control-model",
                    Default::default(),
                    4_096,
                    None,
                )
                .expect("tool control model"),
            )
            .tools(workbench_control_tools())
            .plugin(Arc::new(WorkbenchPluginFactory::new("")))
            .store_factory(store_factory)
            .process_registry(Arc::clone(&process_registry))
            .disable_queued_work_driver()
            .build()
            .expect("build tool control workbench core");
        let session = core
            .session("workbench-tool-control-session")
            .open()
            .await
            .expect("open tool control session");

        let cancelled = session
            .turn(lash::TurnInput::text("cancel the action"))
            .run()
            .await
            .expect("run cancellation turn")
            .result;
        assert_eq!(cancelled.final_value(), Some(&json!("cancellation observed")));
        let cancellation_output = &cancelled.tool_calls[0].output;
        let cancellation_status =
            serde_json::to_value(cancellation_output.status()).expect("serialize tool status");
        assert_eq!(cancellation_status, json!("cancelled"));
        assert_eq!(
            cancellation_output.value_for_projection(),
            json!({
                "message": "the operator cancelled the workbench action",
                "source": "cancellation"
            })
        );

        let finished = session
            .turn(lash::TurnInput::text("finish from the tool"))
            .run()
            .await
            .expect("run tool finish turn")
            .result;
        assert!(matches!(
            &finished.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::ToolValue { tool_name, value })
                if tool_name == "workbench_finish"
                    && value == &json!({ "finished_by": "workbench_finish" })
        ));

        let failed = session
            .turn(lash::TurnInput::text("reject from the tool"))
            .run()
            .await
            .expect("run tool failure turn")
            .result;
        assert!(matches!(
            &failed.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::ToolError { tool_name, value })
                if tool_name == "workbench_fail"
                    && value["code"] == "workbench_action_rejected"
        ));

        session.close().await.expect("close tool control session");
        drop(core);
        drop(process_registry);
        std::fs::remove_dir_all(&data_dir).expect("remove tool control data dir");
    });
}
