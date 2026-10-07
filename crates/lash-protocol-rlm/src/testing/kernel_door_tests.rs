//! The cell door's laws on a claimed session actor over the durable store:
//! a context claims exactly the scope its lent controller admits, and an
//! empty aggregate is the program's defect.

use lash_lashlang_runtime::ToolDefinitionBindingExt as _;

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

async fn run_cell(
    host: &super::DurableHost,
    context: lash_core::RuntimeExecutionContext<'_>,
    code: &str,
) -> lash_core::ExecResponse {
    super::execute_code_with_channel_and_bounds(
        &mut crate::executor::RlmExecutionState::for_engine("typescript"),
        context.with_recorded_render(super::recorded_test_render()),
        lash_core::ExecRequest {
            code: code.to_string(),
        },
        host.artifacts(),
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
        let host = super::DurableHost::open(super::default_cell_scope()).await;
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            host.ports(),
            std::sync::Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
        );
        let response = run_cell(&host, context, cell).await;
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
    }
}

/// A context that claims a scope other than the one its lent controller
/// admits is refused when it is built, before any effect runs.
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "the lent controller admits a scope the context does not claim")]
async fn a_context_claiming_another_scope_than_its_lent_controller_is_refused() {
    let host = super::DurableHost::open(lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("other-session"),
        lash_core::TurnId::from("other-turn"),
    ))
    .await;
    let _context = lash_core::testing::code_execution_context(host.ports());
}
