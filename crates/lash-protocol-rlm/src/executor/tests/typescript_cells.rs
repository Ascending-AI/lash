use super::*;
use lash_lashlang_runtime::TraceLanguageExecutionFailure;

const SEED: u64 = 0x5_2c01;

#[test]
fn printed_cell_refuses_missing_or_mismatched_recorded_renderer() {
    block_on(async {
        for (offset, recorded) in [None, Some("another.renderer")].into_iter().enumerate() {
            let double = crate::testing::kernel_double(
                SEED + offset as u64,
                lash_restate_test::ServerConfig::default(),
            )
            .await;
            let handler = double
                .open_handler(crate::testing::default_cell_scope())
                .await
                .expect("open cell handler");
            let mut context = lash_core::testing::code_execution_context(
                crate::testing::double_ports(&double, &handler),
            );
            if let Some(id) = recorded {
                let mut render = crate::testing::recorded_test_render();
                render.renderer_id = id.to_string();
                context = context.with_recorded_render(render);
            }
            let response = crate::executor::execute_code_with_channel_and_bounds(
                &mut RlmExecutionState::for_engine("typescript"),
                context,
                ExecRequest {
                    code: "print('value');".to_string(),
                },
                crate::testing::memory_artifact_store().await,
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
                crate::render::CodeRendererSlot::default(),
            )
            .await;
            handler.close().await.expect("close cell handler");
            assert!(
                response
                    .error
                    .as_ref()
                    .is_some_and(|error| error.message.contains("recorded_renderer_unavailable")),
                "{response:?}"
            );
            assert!(response.observations.is_empty());
        }
    });
}

struct JournalPrintRenderer {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::render::CodeRenderer for JournalPrintRenderer {
    fn id(&self) -> &str {
        "law.journal-print"
    }

    fn print(
        &self,
        value: &lashlang::Value,
        params: &lash_render::RenderParams,
    ) -> lash_render::Rendered<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lash_render::render(value, params)
    }
}

#[test]
fn bounded_test_entry_uses_the_recorded_params_and_supplied_renderer() {
    block_on(async {
        let double =
            crate::testing::kernel_double(SEED + 11, lash_restate_test::ServerConfig::default())
                .await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open cell handler");
        let mut params = crate::render::ResolvedRlmRender::default();
        params.print.max_chars = 3;
        let context = lash_core::testing::code_execution_context(crate::testing::double_ports(
            &double, &handler,
        ))
        .with_recorded_render(lash_core::RecordedRender {
            renderer_id: "law.journal-print".into(),
            params: serde_json::to_value(params).expect("render params"),
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let response = crate::executor::execute_code_with_channel_and_bounds(
            &mut RlmExecutionState::for_engine("typescript"),
            context,
            ExecRequest {
                code: "print('abcdefgh');".into(),
            },
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
            crate::render::CodeRendererSlot(Arc::new(JournalPrintRenderer {
                calls: Arc::clone(&calls),
            })),
        )
        .await;
        handler.close().await.expect("close cell handler");
        assert_eq!(response.error, None, "{response:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let [print] = response.observations.as_slice() else {
            panic!("expected one print: {response:?}");
        };
        assert_eq!(print.projection.limit_chars, 3);
        assert!(print.text.contains("rendered within 3"), "{}", print.text);
        assert!(print.text.contains("\nabc"), "{}", print.text);
    });
}

#[test]
fn journaled_prints_replay_without_calling_the_renderer() {
    block_on(async {
        let double =
            crate::testing::kernel_double(SEED + 20, lash_restate_test::ServerConfig::default())
                .await;
        let backend = double.lash_backend();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observations = Arc::new(std::sync::Mutex::new(None));
        let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
            let backend = backend.clone();
            let calls = Arc::clone(&calls);
            let observations = Arc::clone(&observations);
            Arc::new(move |scoped| {
                let backend = backend.clone();
                let calls = Arc::clone(&calls);
                let observations = Arc::clone(&observations);
                Box::pin(async move {
                    let context = lash_core::testing::code_execution_context_with_invocation(
                        crate::testing::attempt_ports(&backend, scoped),
                        lash_core::testing::exec_code_invocation(
                            "print-replay-session",
                            "print-replay-turn",
                            0,
                            0,
                            "exec-code:print-replay",
                            "exec-code:print-replay",
                        ),
                    )
                    .with_recorded_render(lash_core::RecordedRender {
                        renderer_id: "law.journal-print".into(),
                        params: serde_json::to_value(crate::render::ResolvedRlmRender::default())
                            .expect("render params"),
                    });
                    let response = crate::executor::execute_code_with_channel_and_bounds(
                        &mut RlmExecutionState::for_engine("typescript"),
                        context,
                        ExecRequest {
                            code: "print('journaled');".into(),
                        },
                        crate::testing::memory_artifact_store().await,
                        LashlangSurface::default(),
                        None,
                        RlmProjectedBindings::default(),
                        RlmLashlangExecutionTraceConfig::default(),
                        lashlang::ExecutionBounds::unbounded(),
                        crate::plugin::RlmChannel::Cell,
                        crate::render::CodeRendererSlot(Arc::new(JournalPrintRenderer {
                            calls: Arc::clone(&calls),
                        })),
                    )
                    .await;
                    assert_eq!(response.error, None, "{response:?}");
                    if crash {
                        assert_eq!(calls.load(Ordering::SeqCst), 1);
                        *observations.lock().expect("observations") =
                            Some(response.observations.clone());
                        panic!("crash after the prints step");
                    }
                    assert_eq!(
                        response.observations,
                        observations
                            .lock()
                            .expect("observations")
                            .clone()
                            .expect("first pass")
                    );
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                })
            })
        };
        double
            .run_crashed_then_redriven(
                lash_core::AdmittedScope::turn("print-replay-session", "print-replay-turn"),
                attempt(true),
                attempt(false),
            )
            .await
            .expect("crashed attempt and replay");
    });
}

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
    .with_retry_policy(lash_core::ToolRetryPolicy::safe(3, 10, 100))
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            crate::testing::double_ports(&double, &handler),
            Arc::new(PolicyDeniedToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![definition]),
        );
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_test_render(
            &mut state,
            context,
            ExecRequest {
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
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        handler.close().await.expect("close the cell's handler");

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

#[derive(Default)]
struct FailureTraceSink(std::sync::Mutex<Vec<lash_core::facade_support::TraceRecord>>);

impl lash_core::facade_support::TraceSink for FailureTraceSink {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        self.0.lock().expect("trace sink lock").push(record.clone());
        Ok(())
    }
}

#[test]
fn scalar_and_batch_tool_failures_keep_recorded_provenance_on_node_failed() {
    block_on(async {
        for code in [
            "await approval.request({ reason: 'scalar' });",
            "await Promise.all([approval.request({ reason: 'batch' })]);",
        ] {
            let sink = Arc::new(FailureTraceSink::default());
            let double =
                crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default())
                    .await;
            let handler = double
                .open_handler(lash_core::AdmittedScope::turn(
                    lash_core::SessionId::from("failure-session"),
                    lash_core::TurnId::from("failure-turn"),
                ))
                .await
                .expect("open the cell's handler");
            let response = execute_code_with_test_render(
                &mut RlmExecutionState::for_engine("typescript"),
                lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(crate::testing::double_ports(&double, &handler), Arc::new(PolicyDeniedToolProvider), lash_core::ToolCatalog::from_tool_definitions(vec![approval_request_definition()]), lash_core::testing::exec_code_invocation(
                        "failure-session", "failure-turn", 0, 0, "failure-exec", "exec:failure",
                    )),
                ExecRequest { code: code.into() },
                crate::testing::memory_artifact_store().await,
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig {
                    sink: Some(sink.clone()),
                    trace_context: TraceContext::default(),
                },
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
            ).await;
            handler.close().await.expect("close the cell's handler");
            assert!(response.error.is_some(), "the effect must fail: {code}");
            let records = sink.0.lock().expect("trace sink lock");
            let failed = records
                .iter()
                .find_map(|record| match &record.event {
                    lash_core::TraceEvent::LanguageExecution { event, .. } => {
                        match &event.payload {
                            TraceLanguageExecutionPayload::NodeFailed {
                                call_id, failure, ..
                            } => Some((call_id, failure)),
                            _ => None,
                        }
                    }
                    _ => None,
                })
                .expect("a failed effect node must be observed");
            let TraceLanguageExecutionFailure::Effect {
                class,
                code: failure_code,
                message,
                replay_key,
                source,
                retry,
            } = failed.1
            else {
                panic!("failed effect lost its typed provenance: {:?}", failed.1);
            };
            assert_eq!(*class, lash_core::ToolFailureClass::PermissionDenied);
            assert_eq!(failure_code, "approval_denied");
            assert_eq!(message, "approval was denied");
            assert_eq!(*source, lash_core::ToolFailureSource::Policy);
            assert_eq!(*retry, lash_core::ToolRetryStatus::Never);
            // The recorded effect's key names the issue ordinal under the
            // cell's `lk2` namespace (FIG-3586); the node names its call by
            // the `ToolCallId` derived from that ordinal (ADR 0117).
            assert!(
                replay_key.contains(":lk2:"),
                "an issue-ordinal key: {replay_key}"
            );
            assert!(failed.0.is_some(), "a failed node names its call");
            assert!(
                !replay_key.contains(":attempt:"),
                "telemetry attempt entered effect identity"
            );
        }
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
    lashlang::testing::harness::try_compile_program(&program).expect("while should compile");
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
    let double =
        crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let handler = double
        .open_handler(crate::testing::default_cell_scope())
        .await
        .expect("open the cell's handler");
    let response = execute_code_with_test_render(
        &mut state,
        lash_core::testing::code_execution_context(crate::testing::double_ports(&double, &handler)),
        ExecRequest {
            code: code.to_string(),
        },
        crate::testing::memory_artifact_store().await,
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    handler.close().await.expect("close the cell's handler");
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
                    .snapshot_execution_state(lash_core::FleetFormat::current())
                    .expect("snapshot components"),
            );
            let mut restored = RlmExecutionState::for_engine("typescript");
            restored
                .restore_execution_state(&snapshot, lash_core::FleetFormat::current())
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
        lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(
                lash_core::ToolCallOutput::success(text).with_view(lash_core::ToolView {
                    blocks: vec![lash_core::ToolViewBlock::Text {
                        text: "model-only view".to_string(),
                        meta: Default::default(),
                    }],
                }),
            ),
        )
    }
}

#[test]
fn code_mode_receives_the_structured_tool_value_and_ignores_its_view() {
    block_on(async {
        let double =
            crate::testing::kernel_double(SEED + 10, lash_restate_test::ServerConfig::default())
                .await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open cell handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            crate::testing::double_ports(&double, &handler),
            Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
        );
        let response = execute_code_with_test_render(
            &mut RlmExecutionState::for_engine("typescript"),
            context,
            ExecRequest {
                code: "finish(await echo.say({ text: 'structured' }));".to_string(),
            },
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        handler.close().await.expect("close cell handler");
        assert_eq!(response.error, None, "{response:?}");
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!("structured"))
        );
    });
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
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let handler = double
            .open_handler(crate::testing::default_cell_scope())
            .await
            .expect("open the cell's handler");
        let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
            crate::testing::double_ports(&double, &handler),
            Arc::new(EchoToolProvider),
            lash_core::ToolCatalog::from_tool_definitions(vec![echo_definition()]),
        );
        let mut state = RlmExecutionState::for_engine("typescript");
        let response = execute_code_with_test_render(
            &mut state,
            context,
            ExecRequest {
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
            crate::testing::memory_artifact_store().await,
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        handler.close().await.expect("close the cell's handler");

        assert_eq!(response.error, None);
        assert_eq!(
            response.terminal_finish,
            Some(serde_json::json!({ "first": ["a", "b"], "second": ["a", "b"] }))
        );

        let call_ids = response
            .calls
            .iter()
            .filter_map(|call| call.host_record.as_ref())
            .map(|record| record.call_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(call_ids.len(), 4, "four leaves ran: {call_ids:?}");
        let distinct = call_ids.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            distinct.len(),
            4,
            "each leaf of each aggregate needs its own identity: {call_ids:?}"
        );

        // Each aggregate is one command with its own issue ordinal, and each
        // leaf's id is derived from that ordinal and its position in the
        // aggregate (FIG-3586, ADR 0117): the derivation is pinned by
        // `LashlangHostIdentities`' own laws.
    });
}

#[test]
fn ambient_effects_remain_unavailable_after_restore() {
    block_on(async {
        let (mut state, seeded) = execute_typescript_test_cell(
            RlmExecutionState::for_engine("typescript"),
            "const kept = { answer: 42 }; const sparse = [,2];",
        )
        .await;
        assert!(seeded.error.is_none(), "{seeded:?}");
        let snapshot = hydrate_snapshot(
            state
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .expect("snapshot components"),
        );
        for restore in [false, true] {
            for code in [
                "eval('kept.answer = 0');",
                "new Function('return 0');",
                "require('node:fs');",
                "finish(process.env);",
                "await fetch('https://example.invalid');",
                "import fs from 'node:fs';",
            ] {
                let candidate = if restore {
                    let mut candidate = RlmExecutionState::for_engine("typescript");
                    candidate
                        .restore_execution_state(&snapshot, lash_core::FleetFormat::current())
                        .expect("restore");
                    candidate
                } else {
                    let (candidate, first) = execute_typescript_test_cell(
                        RlmExecutionState::for_engine("typescript"),
                        "const kept = { answer: 42 }; const sparse = [,2];",
                    )
                    .await;
                    assert!(first.error.is_none());
                    candidate
                };
                let (candidate, response) = execute_typescript_test_cell(candidate, code).await;
                assert!(
                    response.error.is_some(),
                    "ambient capability admitted: {restore}/{code}"
                );
                if code.starts_with("eval(") {
                    assert!(
                        format!("{response:?}").contains("TS_EVAL_UNSUPPORTED"),
                        "the eval boundary remains explicit: {response:?}"
                    );
                }
                assert_eq!(response.terminal_finish, None);
                assert!(
                    response.observations.is_empty(),
                    "rejected code emits nothing"
                );
                let (_, control) = execute_typescript_test_cell(
                    candidate,
                    "finish({ answer: kept.answer, hole: 0 in sparse });",
                )
                .await;
                assert_eq!(
                    control.terminal_finish,
                    Some(serde_json::json!({"answer":42,"hole":false})),
                    "{restore}/{code}: {control:?}"
                );
            }
        }
    });
}

fn widened_contract_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw("tool:widened", "widened", "Return a validated string", serde_json::json!({
        "allOf": [
            { "type":"object", "properties": {"text":{"type":"string","minLength":4}}, "required":["text"] },
            { "type":"object", "properties": {"text":{}}, "additionalProperties":false }
        ]
    }), serde_json::json!({"type":"string"}))
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["bounded"], "say"))
}

struct WidenedContractProvider(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl lash_core::ToolProvider for WidenedContractProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![widened_contract_definition().manifest()]
    }
    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        (id == &lash_core::ToolId::from("tool:widened"))
            .then(|| widened_contract_definition().manifest())
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "widened" || name == "tool:widened")
            .then(|| Arc::new(widened_contract_definition().contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(lash_core::ToolCallOutput::success(
                call.args["text"].clone(),
            )),
        )
    }
}

#[test]
fn runtime_schema_validation_uses_declared_contract_after_inference_widens() {
    block_on(async {
        let catalog =
            lash_core::ToolCatalog::from_tool_definitions(vec![widened_contract_definition()]);
        let environment = LashlangSurface::default()
            .host_environment(&catalog)
            .expect("bridge contract");
        for (offset, arguments, expected, dispatches) in [
            (0, "{text:42}", "refused", 0),
            (1, "{text:'abc'}", "refused", 0),
            (2, "{text:'valid', extra:1}", "refused", 0),
            (3, "{text:'valid'}", "valid", 1),
        ] {
            let call = format!("await bounded.say({arguments})");
            let graph = lash_typescript::workflow_graph::workflow_graph_from_source_with_facets(
                &format!("const result = {call};"),
                Some(&environment),
            )
            .expect("project");
            assert!(
                graph.source_identity.is_some(),
                "inference admits even invalid runtime values"
            );
            let facets = graph
                .nodes()
                .next()
                .expect("call node")
                .type_facets
                .as_ref()
                .expect("facets");
            assert!(
                facets
                    .expected_arguments
                    .iter()
                    .any(|slot| slot.slot.to_string() == "arg[0]"
                        && slot.ty == lashlang::TypeExpr::Any),
                "the multi-allOf schema widens: {facets:?}"
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let double = crate::testing::kernel_double(
                SEED + 100 + offset,
                lash_restate_test::ServerConfig::default(),
            )
            .await;
            let handler = double
                .open_handler(crate::testing::default_cell_scope())
                .await
                .expect("handler");
            let context = lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
                crate::testing::double_ports(&double, &handler),
                Arc::new(WidenedContractProvider(Arc::clone(&calls))),
                catalog.clone(),
            );
            let code = format!("try {{ finish({call}); }} catch (error) {{ finish('refused'); }}");
            let response = execute_code_with_test_render(
                &mut RlmExecutionState::for_engine("typescript"),
                context,
                ExecRequest { code },
                crate::testing::memory_artifact_store().await,
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                crate::plugin::RlmChannel::Cell,
            )
            .await;
            handler.close().await.expect("close");
            assert_eq!(response.error, None, "{arguments}: {response:?}");
            assert_eq!(
                response.terminal_finish,
                Some(serde_json::json!(expected)),
                "{arguments}: {response:?}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                dispatches,
                "{arguments}: invalid request must never reach provider execution"
            );
        }
    });
}
