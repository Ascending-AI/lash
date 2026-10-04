//! This crate's twins on the Restate server double and the storage-only
//! store set (D1 F8, PR-S2).

use lash_lashlang_runtime::ToolDefinitionBindingExt as _;

const SEED: u64 = 0x5_2d30;

fn echo_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo",
        "echo",
        "Echo the text back",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["echo"], "say"))
}

struct EchoToolProvider;

#[async_trait::async_trait]
impl lash_core::ToolProvider for EchoToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![echo_definition().manifest()]
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:echo")).then(|| echo_definition().manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<std::sync::Arc<lash_core::ToolContract>> {
        (name == "echo" || name == "tool:echo")
            .then(|| std::sync::Arc::new(echo_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let text = call
            .args
            .get("text")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(text))
    }
}

/// A TypeScript cell that awaits one echo call and then two in one
/// aggregate, and finishes with all three answers.
const ECHO_CELL: &str = r#"
    const single = await echo.say({ text: "one" });
    const pair = await Promise.all([
        echo.say({ text: "a" }),
        echo.say({ text: "b" })
    ]);
    finish({ single, pair });
"#;

async fn run_echo_cell(context: lash_core::RuntimeExecutionContext<'_>) -> lash_core::ExecResponse {
    run_cell(context, ECHO_CELL).await
}

async fn run_cell(
    context: lash_core::RuntimeExecutionContext<'_>,
    code: &str,
) -> lash_core::ExecResponse {
    crate::testing::execute_code_with_channel_and_bounds(
        &mut crate::executor::RlmExecutionState::for_engine("typescript"),
        context.with_recorded_render(super::recorded_test_render()),
        lash_core::ExecRequest {
            code: code.to_string(),
        },
        super::sqlite_memory_artifact_store().await,
        lash_lashlang_runtime::LashlangSurface::default(),
        None,
        crate::projection::RlmProjectedBindings::default(),
        None,
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
        crate::render::CodeRendererSlot::default(),
    )
    .await
}

fn assert_echo_cell_answered(response: &lash_core::ExecResponse) {
    assert_eq!(response.error, None, "the cell runs to its finish");
    assert_eq!(
        response.terminal_finish,
        Some(serde_json::json!({ "single": "one", "pair": ["a", "b"] })),
        "every tool call answers through the handler's controller"
    );
}

/// A cell whose context installs its own parent invocation claims that
/// invocation's scope, so the handler it runs in is opened for that scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_under_an_installed_invocation_runs_in_a_handler_for_its_scope() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("door-session"),
            lash_core::TurnId::from("door-turn"),
        ))
        .await
        .expect("open the invocation's handler");
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            super::double_ports(&double, &handler),
            std::sync::Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
            lash_core::testing::exec_code_invocation(
                "door-session",
                "door-turn",
                0,
                0,
                "door-exec",
                "exec:door",
            ),
        );
    let response = run_echo_cell(context).await;
    assert_echo_cell_answered(&response);
    handler
        .close()
        .await
        .expect("close the invocation's handler");
}

/// FIG-4547: an aggregate with no members is the program's defect, not a host
/// failure — the identical retry fails the same way, so host-retry guidance
/// would waste a turn. `Promise.race([])` ends the cell on the uncatchable
/// `AggregateAwaitUnsettled` terminal (ADR 0099 §11 clause 5 — the `catch`
/// never runs) and `Promise.any([])` rejects uncaught; both classify as
/// program failures.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_aggregate_fails_the_cell_as_a_program_defect() {
    for (aggregate, cell) in [
        (
            "Promise.race",
            r#"try {
  finish(await Promise.race([]));
} catch (error) {
  finish("caught");
}"#,
        ),
        ("Promise.any", r#"finish(await Promise.any([]));"#),
    ] {
        let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(super::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            super::double_ports(&double, &handler),
            std::sync::Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
        );
        let response = run_cell(context, cell).await;
        let failure = response
            .error
            .as_ref()
            .unwrap_or_else(|| panic!("{aggregate}: the empty aggregate fails the cell"));
        assert_eq!(
            failure.kind,
            lash_core::CellFailureKind::Program,
            "{aggregate}: an aggregate with no members is the program's defect, \
             not a host failure to retry: {failure:?}"
        );
        assert_ne!(
            response.terminal_finish,
            Some(serde_json::json!("caught")),
            "{aggregate}: the empty-aggregate failure stays uncatchable"
        );
        if aggregate == "Promise.race" {
            assert!(
                failure.message.contains("no members to settle")
                    && failure.message.contains("guard the empty case"),
                "{aggregate}: the feedback names the empty aggregate and its \
                 guard: {failure:?}"
            );
        }
        handler.close().await.expect("close the cell's handler");
    }
}

/// A context that claims a scope other than the one its lent controller
/// admits is refused when it is built, before any effect runs.
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "the lent controller admits a scope the context does not claim")]
async fn a_context_claiming_another_scope_than_its_lent_controller_is_refused() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("other-session"),
            lash_core::TurnId::from("other-turn"),
        ))
        .await
        .expect("open a handler for another scope");
    let _context =
        lash_core::testing::code_execution_context(super::double_ports(&double, &handler));
}
