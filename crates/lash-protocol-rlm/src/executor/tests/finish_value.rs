//! A finish value settles under the value schema its tool declares, in a
//! cell of either dialect.

use super::*;

/// `answer.submit`: a host finish tool that takes any record and declares
/// an integer value. Its body answers the record's `value` as the turn's
/// value, so settlement alone stands between a text value and the turn.
fn submit() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::control(
        "tool:answer_submit",
        "answer_submit",
        "End the turn with an integer",
        lash_core::ToolDefinition::default_input_schema(),
        lash_core::TurnControls::finish(
            lash_core::JsonSchema::admit(serde_json::json!({ "type": "integer" }))
                .expect("an integer schema is admitted"),
        ),
    )
    .expect("a valid control tool")
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(["answer"], "submit"))
}

struct SubmitTool;

#[async_trait::async_trait]
impl lash_core::ToolProvider for SubmitTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![submit().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        matches!(name, "answer_submit" | "tool:answer_submit")
            .then(|| Arc::new(submit().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let value = call.args.get("value").cloned().unwrap_or_default();
        lash_core::ToolOutcome::finish(value).into()
    }
}

/// Runs `code` as one cell of `dialect` with `answer.submit` offered.
async fn submit_cell(dialect: &CellDialect, code: &str) -> lash_core::ExecResponse {
    let host = open_host().await;
    let services = if dialect.name() == CellDialect::python().name() {
        cell_services(dialect, python_workers(), None)
    } else {
        cell_services(dialect, lash_vm_client::service::Service::default(), None)
    };
    run_cell(
        &mut CodeModeExecutionState::new(dialect.name(), dialect.numbers()),
        cell_context(&host, SESSION, TURN, "exec-code:0", Arc::new(SubmitTool)),
        &services,
        code,
    )
    .await
}

/// FIG-5823 law 1, inline route: a finish value the tool's declared value
/// schema refuses fails the call. The cell catches the failure and goes
/// on; no finish takes effect, and the call spent the cell's one control
/// attempt, so a second finish is refused before it runs. The same cell
/// with a value the schema admits finishes with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_finish_value_its_schema_refuses_fails_the_call_inside_the_cell() {
    let cases = [
        (
            CellDialect::typescript(),
            "let first = 'none';\ntry { await answer.submit({ value: 'seven' }); } catch (error) { first = String(error); }\nlet second = 'none';\ntry { await answer.submit({ value: 7 }); } catch (error) { second = 'refused'; }\nconsole.log(first);\nconsole.log(second);",
            "await answer.submit({ value: 7 });",
        ),
        (
            CellDialect::python(),
            "first = 'none'\ntry:\n    await answer_submit({'value': 'seven'})\nexcept Exception as error:\n    first = str(error)\nsecond = 'none'\ntry:\n    await answer_submit({'value': 7})\nexcept Exception:\n    second = 'refused'\nprint(first)\nprint(second)",
            "await answer_submit({'value': 7})",
        ),
    ];
    for (dialect, refused, admitted) in cases {
        let response = submit_cell(&dialect, refused).await;
        assert!(response.error().is_none(), "{:?}", response.error());
        assert_eq!(response.finish_value(), None, "no finish took effect");
        let printed: Vec<_> = response.prints.iter().map(|print| &print.value).collect();
        let [first, second] = printed.as_slice() else {
            panic!("two prints: {printed:?}");
        };
        let first = first.as_str().expect("the caught failure prints as text");
        assert!(first.contains("declared value schema"), "{first}");
        assert_eq!(second, &&serde_json::json!("refused"));

        let response = submit_cell(&dialect, admitted).await;
        assert_eq!(finish_of(&response), serde_json::json!(7));
    }
}
