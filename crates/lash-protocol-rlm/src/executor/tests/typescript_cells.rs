use super::*;

fn approval_request_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:approval_request",
        "approval_request",
        "Request host approval",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
        ["approval"],
        "request",
    ))
}

struct PolicyDeniedToolProvider;

#[async_trait::async_trait]
impl lash_core::ToolProvider for PolicyDeniedToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![approval_request_definition().manifest()]
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:approval_request"))
            .then(|| approval_request_definition().manifest())
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "approval_request" || name == "tool:approval_request")
            .then(|| Arc::new(approval_request_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::failure(lash_core::ToolFailure {
                class: lash_core::ToolFailureClass::PermissionDenied,
                code: "approval_denied".to_string(),
                message: "approval was denied".to_string(),
                source: lash_core::ToolFailureSource::Policy,
                retry: lash_core::ToolRetryStatus::Never,
                raw: None,
            })
        })
        .await
        .into()
    }
}

#[test]
fn typescript_cell_can_branch_on_policy_tool_failure_fields() {
    block_on(async {
        let definition = approval_request_definition();
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            Arc::new(PolicyDeniedToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![definition]),
        );
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_channel_and_bounds(
            &mut state,
            context,
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                    const settled = await Promise.allSettled([
                        approval.request({ reason: "settled deploy" })
                    ]);
                    const settledReason = settled[0].reason;
                    try {
                        await approval.request({ reason: "deploy" });
                        finish({ caught: false });
                    } catch (error) {
                        finish({
                            caught: error instanceof Error,
                            name: error.name,
                            code: error.cause.code,
                            message: error.message,
                            class: error.cause.class,
                            source: error.cause.source,
                            retry: error.cause.retry.type,
                            settledCode: settledReason.cause.code,
                            settledMessage: settledReason.message,
                            settledSource: settledReason.cause.source,
                            settledRetry: settledReason.cause.retry.type
                        });
                    }
                "#
                .to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;

        assert_eq!(response.error, None);
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!({
                "caught": true,
                "name": "EffectError",
                "code": "approval_denied",
                "message": "approval was denied",
                "class": "permission_denied",
                "source": "policy",
                "retry": "never",
                "settledCode": "approval_denied",
                "settledMessage": "approval was denied",
                "settledSource": "policy",
                "settledRetry": "never"
            }))
        );
    });
}

#[test]
fn the_dialect_accepts_bounded_while_with_nested_for() {
    let source = r#"let pool_i = 0;
let final_ids = [];
const candidate_pools = [{ matches: ["a", "b"] }];
while (final_ids.length < 2 && pool_i < candidate_pools.length) {
  for (const m of candidate_pools[pool_i].matches) {
    final_ids = [...final_ids, m];
  }
  pool_i = pool_i + 1;
}
finish(final_ids);"#;

    let program = lash_typescript::parse(source).expect("while should parse");
    lashlang::compile_ast(&program).expect("while should compile");
}

/// Closure-bearing TypeScript cells, mirroring the closure shapes of
/// `lash-typescript`'s durability corpus (`tests/dialect.rs`): a recursive
/// function, a nested function, an arrow that captures and is returned, and
/// an inline arrow whose closure becomes garbage immediately.
const CLOSURE_BEARING_TYPESCRIPT_CELLS: &[&str] = &[
    "function fact(n: number): number { if (n <= 1) { return 1; } return fact(n - 1) * n; } const f5 = fact(5);",
    "const top = 9; function outerFn(): number { function innerFn(): number { return top; } return innerFn(); } const nested = outerFn();",
    "const base = 10; const outer = () => { const inner = () => base; return inner; }; const held = outer();",
    "const xs = [1].map(x => x + 1);",
];

/// The trivial next cell from the FIG-1562 report.
const TRIVIAL_NEXT_CELL: &str = "finish(6 * 7);";

async fn execute_typescript_test_cell(
    mut state: RlmExecutionState,
    code: &str,
) -> (RlmExecutionState, ExecResponse) {
    let response = execute_code_with_channel_and_bounds(
        &mut state,
        lash_core::testing::code_execution_context(),
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    (state, response)
}

/// A `console.log` observation has to describe the value the cell inspected.
///
/// The prompt tells the model to inspect values with `console.log`, and on
/// the pre-FIG-2767 path every one of these cells wrote `[object Object]`
/// into the observation, so 17 toolbench attempts learned nothing from the
/// step they were told to take. This is the end-to-end witness: the text
/// asserted here is what reaches the model.
#[test]
fn typescript_console_observations_describe_the_value() {
    block_on(async {
        for (cell, expected) in [
            (
                "const record = { id: 7, tags: [\"a\", \"b\"] }; console.log(record);",
                r#"{"id":7,"tags":["a","b"]}"#,
            ),
            (
                "console.log(\"record:\", { id: 7 });",
                r#"record: {"id":7}"#,
            ),
            (
                "console.log([{ id: 1 }, { id: 2 }]);",
                r#"[{"id":1},{"id":2}]"#,
            ),
        ] {
            let state = RlmExecutionState::for_engine("typescript");
            let (_, response) = execute_typescript_test_cell(state, cell).await;
            assert!(
                response.error.is_none(),
                "cell `{cell}`: {:?}",
                response.error
            );
            let observations = response
                .observations
                .iter()
                .map(|observation| observation.text.as_str())
                .collect::<Vec<_>>();
            assert_eq!(observations, vec![expected], "cell `{cell}`");
            assert!(
                !observations
                    .iter()
                    .any(|text| text.contains("[object Object]")),
                "cell `{cell}` still renders an opaque object"
            );
        }
    });
}

/// A closure allocated by one cell must not fail validation of the next
/// cell's program.
///
/// Each RLM cell compiles its own `CompiledProgram` while the heap survives
/// the cell boundary, so a closure from cell N is re-validated against cell
/// N+1's function table. See FIG-1562.
#[test]
fn a_closure_from_one_typescript_cell_does_not_poison_the_next_cell() {
    block_on(async {
        for cell in CLOSURE_BEARING_TYPESCRIPT_CELLS {
            let state = RlmExecutionState::for_engine("typescript");
            let (state, first) = execute_typescript_test_cell(state, cell).await;
            assert!(first.error.is_none(), "cell `{cell}`: {:?}", first.error);

            let (_, second) = execute_typescript_test_cell(state, TRIVIAL_NEXT_CELL).await;
            assert!(
                second.error.is_none(),
                "the trivial cell after `{cell}` failed: {:?}",
                second.error
            );
        }
    });
}

/// The same composition across the durability boundary: snapshot a state
/// holding a real closure, restore it into a fresh engine, then run a
/// *different* cell against it.
///
/// This one passes today, and the sibling test above is why it is worth
/// keeping: the closure demonstrably survives the cell boundary in memory,
/// so the fact that a *restored* state accepts the next cell is a real
/// property of the RLM persistence path (closure-valued globals do not
/// reach the snapshot), not an artefact of an empty heap. It guards that
/// property once the cell boundary itself is fixed. See FIG-1562.
#[test]
fn a_restored_typescript_closure_does_not_poison_a_different_cell() {
    block_on(async {
        for cell in CLOSURE_BEARING_TYPESCRIPT_CELLS {
            let state = RlmExecutionState::for_engine("typescript");
            let (mut state, first) = execute_typescript_test_cell(state, cell).await;
            assert!(first.error.is_none(), "cell `{cell}`: {:?}", first.error);

            let snapshot = hydrate_snapshot(
                state
                    .snapshot_execution_state()
                    .expect("snapshot components"),
            );
            let mut restored = RlmExecutionState::for_engine("typescript");
            restored
                .restore_execution_state(&snapshot)
                .expect("restore TypeScript execution state");

            let (_, response) = execute_typescript_test_cell(restored, TRIVIAL_NEXT_CELL).await;
            assert!(
                response.error.is_none(),
                "the trivial cell after restoring `{cell}` failed: {:?}",
                response.error
            );
        }
    });
}

/// `finish` is the only statement that ends the turn; every other terminal
/// statement leaves it open.
///
/// The RLM loop asks the provider again for as long as the executed cell
/// reports no terminal finish, and the agent-workbench immutable-deployment
/// gate depends on exactly that: its first cell registers a process and must
/// leave the turn open so the second provider call (the gate) happens at all.
/// Re-authoring that cell's trailing report as `finish(...)` during the
/// single-language cutover terminated the turn after the registration and the
/// gate became unreachable, which is the shape FIG-3074 reports. Both arms are
/// asserted here: the reporting cell stays open, the finishing cell closes with
/// its value.
#[test]
fn only_finish_closes_a_typescript_cells_turn() {
    block_on(async {
        let (_, reporting) = execute_typescript_test_cell(
            RlmExecutionState::for_engine("typescript"),
            "print(\"journal prefix committed\");",
        )
        .await;
        assert!(
            reporting.error.is_none(),
            "reporting cell failed: {:?}",
            reporting.error
        );
        assert_eq!(
            reporting.terminal_finish, None,
            "a cell that only reports must leave the turn open for the next provider call"
        );

        let (_, finishing) = execute_typescript_test_cell(
            RlmExecutionState::for_engine("typescript"),
            "finish(\"journal prefix committed\");",
        )
        .await;
        assert!(
            finishing.error.is_none(),
            "finishing cell failed: {:?}",
            finishing.error
        );
        assert_eq!(
            finishing.terminal_finish,
            Some(serde_json::json!("journal prefix committed")),
            "a cell that calls finish must close the turn with its value"
        );
    });
}

fn echo_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo",
        "echo",
        "Echo the text back",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object" }),
    )
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

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "echo" || name == "tool:echo").then(|| Arc::new(echo_definition().contract()))
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

/// Two identical aggregates raised from one cell must not share identities.
///
/// The aggregate sits in a function declaration, which is where an aggregate
/// ordinarily sits once a cell factors its work into helpers, and which is the
/// case the compiler describes with no execution site at all:
/// `lashlang_execution_paths` walks `program.main`. With no site, every leaf
/// used to fall back to its position inside the batch, so the second call of
/// `pair` re-minted the first call's two identities and the batch re-minted the
/// first batch's content hash — one effect replay key for two aggregates, and
/// the second aggregate reading the first one's journalled outcome.
///
/// This is the defect FIG-3394 closes; it is red on the parent commit, where
/// the four calls mint two distinct identities instead of four.
#[test]
fn identical_aggregates_in_one_cell_mint_distinct_leaf_identities() {
    block_on(async {
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
        );
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_channel_and_bounds(
            &mut state,
            context,
            ExecRequest {
                language: "typescript".to_string(),
                code: r#"
                    async function pair() {
                        return await Promise.all([
                            echo.say({ text: "a" }),
                            echo.say({ text: "b" })
                        ]);
                    }
                    const first = await pair();
                    const second = await pair();
                    finish({ first, second });
                "#
                .to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;

        assert_eq!(response.error, None);
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!({ "first": ["a", "b"], "second": ["a", "b"] }))
        );

        let call_ids = response
            .calls
            .iter()
            .filter_map(|call| call.host_record.as_ref())
            .filter_map(|record| record.call_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(call_ids.len(), 4, "four leaves ran: {call_ids:?}");
        let distinct = call_ids.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            distinct.len(),
            4,
            "each leaf of each aggregate needs its own identity: {call_ids:?}"
        );

        // Both leaves belong to one structural workflow node. The leaf
        // position within the batch and that node's increasing occurrence
        // still give each call a distinct identity.
        let first_pass = call_ids
            .iter()
            .filter(|id| id.ends_with(":child:0"))
            .count();
        let second_pass = call_ids
            .iter()
            .filter(|id| id.ends_with(":child:1"))
            .count();
        assert_eq!(
            (first_pass, second_pass),
            (2, 2),
            "two leaf positions, reached twice: {call_ids:?}"
        );
        for occurrence in 1..=4 {
            assert_eq!(
                call_ids
                    .iter()
                    .filter(|id| id.contains(&format!(":{occurrence}:child:")))
                    .count(),
                1,
                "the shared node assigns one occurrence per leaf call: {call_ids:?}"
            );
        }
    });
}
