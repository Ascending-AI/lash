//! Host tools control a workbench turn with typed outcomes: a cancellation
//! is an uncatchable host terminal, a finish ends the turn with the tool's
//! value, and a typed failure stops it.

use super::*;

#[derive(Clone, Copy)]
struct WorkbenchControlTools;

#[async_trait]
impl lash::tools::StaticToolExecute for WorkbenchControlTools {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        (async {
            match call.name() {
                "workbench_cancel" => lash::tools::ToolOutcome::from_output(
                    lash::tools::ToolCallOutput::cancelled(lash::tools::ToolCancellation::runtime(
                        "the operator cancelled the workbench action",
                    )),
                ),
                "workbench_finish" => lash::tools::ToolOutcome::from_output(
                    lash::tools::ToolCallOutput::success(json!({ "accepted": true })).with_control(
                        lash::tools::ToolControl::Finish {
                            value: lash::tools::ToolValue::untrusted_json(json!({
                                "finished_by": "workbench_finish"
                            })),
                        },
                    ),
                ),
                "workbench_fail" => lash::tools::ToolOutcome::ok(json!({ "accepted": false }))
                    .with_control(lash::tools::ToolControl::Fail {
                        failure: lash::tools::ToolFailure::tool(
                            lash::tools::ToolFailureClass::Execution,
                            "workbench_action_rejected",
                            "the workbench action was rejected",
                        ),
                    }),
                other => lash::tools::ToolOutcome::err_fmt(format_args!(
                    "unknown workbench control tool `{other}`"
                )),
            }
        })
        .await
        .into()
    }
}

fn workbench_control_tools() -> Arc<dyn lash::tools::ToolProvider> {
    use lash::tools::ToolDefinitionBindingExt as _;

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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(lash::tools::ToolBinding::new(
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(lash::tools::ToolBinding::new(
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
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(lash::tools::ToolBinding::new(["workbench_control"], "fail")),
    ];
    Arc::new(lash::tools::StaticToolProvider::new(
        definitions,
        WorkbenchControlTools,
    ))
}

/// The output of every tool call a turn's activity reports completed.
#[derive(Default)]
struct CompletedToolCalls {
    outputs: Mutex<Vec<lash::tools::ToolCallOutput>>,
}

#[async_trait]
impl TurnActivitySink for CompletedToolCalls {
    async fn emit(&self, activity: lash::TurnActivity) {
        if let lash::TurnEvent::ToolCallCompleted { output, .. } = activity.event {
            self.outputs.lock_recover().push(output);
        }
    }
}

fn workbench_control_cell(source: &str) -> String {
    format!("<typescript>\n{source}\n</typescript>")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workbench_tools_expose_typed_cancellation_and_turn_control() {
    let workbench = Workbench::builder(scripted_cells_provider(vec![
        workbench_control_cell(
            "try {\n  await workbench_control.cancel({});\n} catch (error) {\n}\nfinish(\"cancellation observed\");",
        ),
        workbench_control_cell("await workbench_control.finish({});"),
        workbench_control_cell("await workbench_control.fail({});"),
    ]))
    .tool_provider(workbench_control_tools())
    .build()
    .await;
    let state = &workbench.state;
    let session = state
        .create_or_open_session(&state.current_session_id(), "test")
        .await
        .expect("open the tool control session");

    let completed = Arc::new(CompletedToolCalls::default());
    let cancelled = session
        .send(lash::TurnInput::text("cancel the action"))
        .output_into(completed.as_ref())
        .await
        .expect("run the cancellation turn");
    assert!(
        matches!(
            &cancelled.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled { .. })
        ),
        "a cancelled tool is an uncatchable host terminal, so the catch's \
         finish is never reached and the turn ends cancelled: {:?}",
        cancelled.outcome
    );
    // A durable report is thin and a cancelled turn commits nothing: the
    // call's output is read off the turn's activity.
    let outputs = completed.outputs.lock_recover().clone();
    let [cancellation] = outputs.as_slice() else {
        panic!("the cancellation turn completed one tool call: {outputs:?}");
    };
    assert_eq!(
        serde_json::to_value(cancellation.status()).expect("serialize the tool status"),
        json!("cancelled")
    );
    assert_eq!(
        cancellation.value_for_projection(),
        json!({
            "message": "the operator cancelled the workbench action",
            "source": "cancellation"
        })
    );

    let finished = session
        .send(lash::TurnInput::text("finish from the tool"))
        .output()
        .await
        .expect("run the tool finish turn")
        .result;
    assert!(
        matches!(
            &finished.outcome,
            lash::TurnOutcome::Finished(lash::TurnFinish::ToolValue { tool_name, value })
                if tool_name == "workbench_finish"
                    && value == &json!({ "finished_by": "workbench_finish" })
        ),
        "{:?}",
        finished.outcome
    );

    let failed = session
        .send(lash::TurnInput::text("reject from the tool"))
        .output()
        .await
        .expect("run the tool failure turn")
        .result;
    assert!(
        matches!(
            &failed.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::ToolError { tool_name, value })
                if tool_name == "workbench_fail"
                    && value["code"] == "workbench_action_rejected"
        ),
        "{:?}",
        failed.outcome
    );
    drop(session);
    workbench.shutdown().await;
}
