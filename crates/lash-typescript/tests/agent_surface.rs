use lashlang::{
    AbilityOp, AbilityResult, Declaration, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Expr, ResourceOperationBatchResult, ResourceOperationResult, State, Value, Vm, VmRunOutcome,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected agent-surface ability")),
        }
    }
}

fn finished(source: &str) -> Value {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .expect("TypeScript should execute")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn define_process_is_a_static_declaration_and_return_stays_a_function_return() {
    let program = lash_typescript::parse(
        r#"
        const worker = defineProcess({
          name: "worker",
          signals: { ready: null },
          run: async (input: unknown) => {
            try { return input; } finally { wake("completed"); }
          }
        });
        const handle = start(worker, { input: 3 });
        finish(handle);
        "#,
    )
    .expect("agent program should lower");

    let [Declaration::Process(process)] = program.declarations.as_slice() else {
        panic!("expected exactly one process declaration")
    };
    assert_eq!(process.name.as_str(), "worker");
    assert_eq!(process.params[0].name.as_str(), "input");
    assert_eq!(process.signals[0].name.as_str(), "ready");
    let Expr::Try(wrapper) = &process.body else {
        panic!("process wrapper should translate uncaught errors into failure")
    };
    let Expr::Finish(call) = wrapper.body.as_ref() else {
        panic!("process wrapper should finish the run function result")
    };
    let Expr::Call { function, .. } = call.as_ref() else {
        panic!("process wrapper should call the authored run function")
    };
    let Expr::Function(function) = function.as_ref() else {
        panic!("run remains a real function")
    };
    assert!(contains_return(&function.body));
    assert!(contains_wake(&function.body));
    assert!(matches!(
        wrapper.catch.as_ref().map(|catch| catch.body.as_ref()),
        Some(Expr::Fail(_))
    ));
    assert!(contains_start(&program.main));
}

#[test]
fn durable_process_agent_primitives_link_through_existing_effects() {
    let source = r#"
        const worker = defineProcess({
          name: "worker",
          signals: { ready: null },
          run: async (input: unknown) => {
            const signal = await waitSignal("ready");
            await sleep(5);
            wake(signal);
            return input;
          }
        });
        const handle = start(worker, { input: 3 });
        finish(handle);
    "#;
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash_typescript::link(source, &environment)
        .expect("all TypeScript agent primitives should link to shared effects");
    assert_eq!(linked.artifact.exports.processes.len(), 1);
    assert_eq!(
        linked.artifact.compilation_dialect,
        lashlang::CompilationDialect::Typescript
    );
    let artifact: lashlang::ModuleArtifact = serde_json::from_slice(
        &serde_json::to_vec(&linked.artifact).expect("encode TypeScript artifact"),
    )
    .expect("decode TypeScript artifact");
    assert_eq!(
        artifact.compilation_dialect,
        lashlang::CompilationDialect::Typescript
    );
}

#[test]
fn production_link_cache_preserves_typescript_artifact_identity() {
    let source = r#"
        const worker = defineProcess({
          name: "worker", signals: {},
          run: async (input: unknown) => { const alias = input; return alias; }
        });
        finish(start(worker, { input: [1] }));
    "#;
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let program = lash_typescript::parse(source).expect("TypeScript should lower");
    let mut cache = lashlang::LinkedProgramCache::new();
    let linked = cache
        .get_or_compile_ast(
            source,
            program,
            &environment,
            lashlang::CompilationDialect::Typescript,
        )
        .expect("production cache should link TypeScript");
    assert_eq!(
        linked.linked_module().artifact.compilation_dialect,
        lashlang::CompilationDialect::Typescript
    );
    assert!(
        linked
            .linked_module()
            .module_ref
            .as_str()
            .starts_with("lashlang:v2:blake3:")
    );
}

#[test]
fn wake_signals_runs_and_process_finish_is_rejected() {
    let program = lash_typescript::parse(
        r#"
        const worker = defineProcess({
          name: "worker", signals: { ready: null },
          run: async () => await waitSignal("ready")
        });
        const handle = start(worker);
        wake(handle, "ready", { ok: true });
        finish(await handle);
        "#,
    )
    .expect("wake(handle, signal, payload) should lower");
    assert!(contains_signal_run(&program.main));

    let error = lash_typescript::parse(
        r#"
        const worker = defineProcess({
          name: "worker", signals: {},
          run: async () => { try { finish(1); } finally { wake("cleanup"); } }
        });
        "#,
    )
    .expect_err("finish inside run must not bypass finally");
    assert_eq!(
        error.code,
        lash_typescript::DiagnosticCode::UnsupportedExpression
    );
    assert!(error.message.contains("cell-only"));
}

#[test]
fn process_membership_reaches_functions_nested_inside_run() {
    let error = lash_typescript::parse(
        r#"
        const worker = defineProcess({
          name: "worker", signals: {},
          run: async () => {
            function stop() { finish(1); }
            return stop();
          }
        });
        "#,
    )
    .expect_err("finish nested inside run must remain cell-only");
    assert_eq!(
        error.code,
        lash_typescript::DiagnosticCode::UnsupportedExpression
    );
    assert!(error.message.contains("cell-only"));
}

#[derive(Default)]
struct SignalHost {
    signal: std::sync::Mutex<Option<lashlang::ProcessSignal>>,
}

impl ExecutionHost for SignalHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::StartProcess(_) => Ok(AbilityResult::Value(lashlang::from_json(
                serde_json::json!({ "__handle__": "process", "id": "run-1" }),
            ))),
            AbilityOp::SignalRun(signal) => {
                *self.signal.lock().expect("signal lock") = Some(signal);
                Ok(AbilityResult::Value(Value::Null))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected signal ability")),
        }
    }
}

#[test]
fn foreground_wake_delivers_a_named_process_signal() {
    let source = r#"
        const worker = defineProcess({
          name: "worker", signals: { ready: null },
          run: async () => await waitSignal("ready")
        });
        const handle = start(worker);
        wake(handle, "ready", { ok: true });
        finish(handle);
    "#;
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash_typescript::link(source, &environment).expect("signal program links");
    let host = SignalHost::default();
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &host,
    ))
    .expect("signal program executes");
    assert!(matches!(outcome, ExecutionOutcome::Finished(_)));
    let signal = host
        .signal
        .lock()
        .expect("signal lock")
        .clone()
        .expect("signal delivered");
    assert_eq!(signal.name, "ready");
    assert_eq!(
        signal.payload,
        lashlang::from_json(serde_json::json!({ "ok": true }))
    );
}

struct StartHost;

impl ExecutionHost for StartHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::StartProcess(start) => {
                assert_eq!(start.process_name, "worker");
                assert_eq!(start.args.get("input"), Some(&Value::Number(3.0)));
                Ok(AbilityResult::Value(Value::String("run-handle".into())))
            }
            AbilityOp::Await(Value::String(handle)) if handle.as_str() == "run-handle" => {
                Ok(AbilityResult::Value(Value::Number(6.0)))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected start ability")),
        }
    }
}

#[test]
fn start_and_await_process_execute_through_shared_process_effects() {
    let source = r#"
        const worker = defineProcess({
          name: "worker", signals: {},
          run: async (input: unknown) => { return input * 2; }
        });
        finish(await start(worker, { input: 3 }));
    "#;
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::default().with_processes(),
    );
    let linked = lash_typescript::link(source, &environment).expect("start should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &StartHost,
    ))
    .expect("start should execute");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(6.0)));
}

#[derive(Default)]
struct ProcessHandleIdInspectionHost {
    status_checked_process_id: std::sync::Mutex<Option<String>>,
}

impl ExecutionHost for ProcessHandleIdInspectionHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::StartProcess(start) => {
                assert_eq!(start.process_name, "worker");
                assert_eq!(start.args.get("input"), Some(&Value::Number(42.0)));
                Ok(AbilityResult::Value(lashlang::from_json(
                    serde_json::json!({ "__handle__": "process", "id": "process-test-42" }),
                )))
            }
            AbilityOp::ResourceOperation(call) => {
                let alias = match &call.receiver {
                    Value::Resource(handle) => handle.alias.clone(),
                    other => format!("{other:?}"),
                };
                if alias == "inspection" && call.operation == "status" {
                    let [Value::Record(fields)] = call.args.as_slice() else {
                        return Err(ExecutionHostError::new("expected record args"));
                    };
                    let pid = fields
                        .iter()
                        .find(|(k, _)| *k == "process_id")
                        .and_then(|(_, v)| match v {
                            Value::String(s) => Some(s.to_string()),
                            _ => None,
                        })
                        .ok_or_else(|| {
                            ExecutionHostError::new("missing process_id in status args")
                        })?;
                    *self.status_checked_process_id.lock().unwrap() = Some(pid);
                    Ok(AbilityResult::Value(Value::String("status-ok".into())))
                } else {
                    Err(ExecutionHostError::new("unexpected resource operation"))
                }
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected ability")),
        }
    }
}

#[test]
fn process_handle_exposes_id_member_for_subsequent_operations() {
    let source = r#"
        const worker = defineProcess({
          name: "worker", signals: {},
          run: async (input: unknown) => { return input; }
        });
        const handle = start(worker, { input: 42 });
        const processId = handle.id;
        const result = await inspection.status({ process_id: processId });
        finish({ processId: processId, result: result });
    "#;
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_binding(
            ["inspection"],
            "InspectionModule",
            "status",
            "tool:inspection/status",
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Str,
                output_from_input: None,
            },
        )
        .expect("operation binding");
    let environment = lashlang::LashlangHostEnvironment::new(
        catalog,
        lashlang::LashlangAbilities::default().with_processes(),
    );
    let linked = lash_typescript::link(source, &environment).expect("TypeScript should link");
    let host = ProcessHandleIdInspectionHost::default();
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &host,
    ))
    .expect("execution should succeed");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!({
            "processId": "process-test-42",
            "result": "status-ok"
        })))
    );
    assert_eq!(
        *host.status_checked_process_id.lock().unwrap(),
        Some("process-test-42".to_string())
    );
}

#[test]
fn promise_aggregates_reuse_await_shape_and_tools_require_await() {
    let program = lash_typescript::parse(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
    )
    .expect("Promise.all should lower");
    assert!(contains_aggregate_await(&program.main, true));

    let settled = lash_typescript::parse(
        "const results = await Promise.allSettled([web.fetch({ url: 'a' })]); finish(results);",
    )
    .expect("Promise.allSettled should lower");
    assert!(contains_aggregate_await(&settled.main, false));

    let error = lash_typescript::parse("web.fetch({ url: 'a' });")
        .expect_err("a deferred tool call without await must reject");
    assert_eq!(error.code, lash_typescript::DiagnosticCode::AwaitRequired);
}

struct ToolCallRecordingHost {
    dispatched: std::sync::Mutex<Vec<(String, String)>>,
}

impl ExecutionHost for ToolCallRecordingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) => {
                let alias = match &call.receiver {
                    Value::Resource(handle) => handle.alias.clone(),
                    other => format!("{other:?}"),
                };
                self.dispatched
                    .lock()
                    .expect("dispatched lock")
                    .push((alias, call.operation));
                Ok(AbilityResult::Value(Value::String("tool-ok".into())))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected tool call ability")),
        }
    }
}

fn find_receiver_call(expr: &Expr) -> Option<(Vec<&str>, &str)> {
    match expr {
        Expr::ReceiverCall {
            receiver,
            operation,
            ..
        } => {
            if let Expr::ResourceRef(resource_ref) = receiver.as_ref() {
                Some((
                    resource_ref.path.iter().map(|s| s.as_str()).collect(),
                    operation.as_str(),
                ))
            } else {
                None
            }
        }
        _ => expr.children().find_map(find_receiver_call),
    }
}

#[test]
fn tool_operations_colliding_with_instance_stdlib_names_lower_and_dispatch() {
    let cases = [
        (
            r#"finish(await web.search({ query: "lash" }));"#,
            vec!["web"],
            "search",
        ),
        (
            r#"finish(await tools.search({ query: "lash" }));"#,
            vec!["tools"],
            "search",
        ),
        (
            r#"finish(await inbox.alpha.delete({ id: "msg_123" }));"#,
            vec!["inbox", "alpha"],
            "delete",
        ),
    ];

    for (source, expected_path, expected_op) in cases {
        let program = lash_typescript::parse(source)
            .unwrap_or_else(|error| panic!("failed to parse {source}: {error}"));
        let (path, op) = find_receiver_call(&program.main)
            .unwrap_or_else(|| panic!("expected ReceiverCall in {source}"));
        assert_eq!(path, expected_path);
        assert_eq!(op, expected_op);

        let compiled = lash_typescript::compile(source)
            .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
        let host = ToolCallRecordingHost {
            dispatched: std::sync::Mutex::new(Vec::new()),
        };
        let outcome =
            futures::executor::block_on(lashlang::execute(&compiled, &mut State::new(), &host))
                .expect("execution should succeed");
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(Value::String("tool-ok".into()))
        );
        let dispatched = host.dispatched.lock().expect("dispatched lock").clone();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].1, expected_op);

        // Also verify linked dispatch through host catalog
        let mut catalog = lashlang::LashlangHostCatalog::new();
        catalog
            .add_module_operation_binding(
                expected_path.clone(),
                "ToolModule",
                expected_op,
                format!("tool:{}", expected_path.join("/")),
                lashlang::ResourceOperationBinding {
                    input_ty: lashlang::TypeExpr::Any,
                    output_ty: lashlang::TypeExpr::Any,
                    output_from_input: None,
                },
            )
            .expect("operation binding");
        let environment =
            lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
        let linked = lash_typescript::link(source, &environment).expect("TypeScript should link");
        let host_linked = ToolCallRecordingHost {
            dispatched: std::sync::Mutex::new(Vec::new()),
        };
        let linked_outcome = futures::executor::block_on(lashlang::execute(
            &lash_typescript::compile_linked(&linked),
            &mut State::new(),
            &host_linked,
        ))
        .expect("linked execution should succeed");
        assert_eq!(
            linked_outcome,
            ExecutionOutcome::Finished(Value::String("tool-ok".into()))
        );
        let expected_alias = expected_path.join(".");
        let linked_dispatched = host_linked
            .dispatched
            .lock()
            .expect("dispatched lock")
            .clone();
        assert_eq!(
            linked_dispatched,
            vec![(expected_alias, expected_op.to_string())]
        );
    }
}

#[test]
fn bound_instance_stdlib_methods_still_lower_to_stdlib() {
    let cases = [
        (
            r#"const s = "hello world"; finish(s.search(/world/));"#,
            Value::Number(6.0),
        ),
        (
            r#"const m = new Map([["k", 1]]); const removed = m.delete("k"); finish([removed, m.has("k")]);"#,
            Value::List(vec![Value::Bool(true), Value::Bool(false)].into()),
        ),
        (
            r#"const s = new Set([1, 2]); const removed = s.delete(1); finish([removed, s.has(1)]);"#,
            Value::List(vec![Value::Bool(true), Value::Bool(false)].into()),
        ),
        (
            r#"const arr = [1, 2, 3, 4]; finish(arr.filter((x: number) => x > 2));"#,
            Value::List(vec![Value::Number(3.0), Value::Number(4.0)].into()),
        ),
        (
            r#"const arr = [1, 2]; finish(arr.map((x: number) => x * 2));"#,
            Value::List(vec![Value::Number(2.0), Value::Number(4.0)].into()),
        ),
        (
            r#"const s = "abc"; finish(s.replace("b", "x"));"#,
            Value::String("axc".into()),
        ),
        (
            r#"const m = new Map([["a", 1]]); finish([...m.keys()]);"#,
            Value::List(vec![Value::String("a".into())].into()),
        ),
        (
            r#"const s = "hello"; finish(s.slice(1, 4));"#,
            Value::String("ell".into()),
        ),
    ];

    for (source, expected_value) in cases {
        assert_eq!(finished(source), expected_value, "failed for {source}");
    }
}

/// Which methods a literal receiver carries used to be a hand-written table
/// beside the signature table, and the two disagreed: `valueOf` was listed for
/// string, number and the remaining literals but not for arrays, so
/// `[1].valueOf()` was refused as unavailable on this literal receiver while
/// the same call on a bound array lowered and ran (FIG-1718). Both spellings
/// are the same call and must answer the same.
#[test]
fn array_literal_value_of_matches_the_bound_array_path() {
    let expected = Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into());
    assert_eq!(finished("finish([1, 2].valueOf());"), expected);
    assert_eq!(
        finished("const items = [1, 2]; finish(items.valueOf());"),
        expected
    );
}

/// `Array.prototype.valueOf` hands back the receiver, not a copy of it. The
/// value is the weaker half of the claim — a detached copy satisfies the
/// assertion above — so pin the two properties only identity has: a write
/// through the result reaches the original, and `===` holds. `slice()` is the
/// control, a genuine copy that must fail both.
#[test]
fn array_value_of_returns_the_receiver_rather_than_a_copy() {
    assert_eq!(
        finished("const a = [1, 2]; const b = a.valueOf(); b.push(3); finish(a.length);"),
        Value::Number(3.0),
        "a write through valueOf() must reach the receiver"
    );
    assert_eq!(
        finished("const a = [1, 2]; finish(a.valueOf() === a);"),
        Value::Bool(true)
    );
    assert_eq!(
        finished("const a = [1, 2]; const b = a.slice(); b.push(3); finish(a.length);"),
        Value::Number(2.0),
        "slice() is a copy, so the control must not alias"
    );
    assert_eq!(
        finished("const a = [1, 2]; finish(a.slice() === a);"),
        Value::Bool(false)
    );
}

/// The ticket repro is a guest-level RegExp match: preserve its array-shaped
/// result fields while `valueOf()` returns the same match object.
#[test]
fn regexp_match_value_of_preserves_guest_shape_and_identity() {
    assert_eq!(
        finished(
            r#"
            const matched = "abc".match(/b/).valueOf();
            finish({ index: matched.index, first: matched[0], same: matched.valueOf() === matched });
            "#,
        ),
        lashlang::from_json(serde_json::json!({
            "index": 1,
            "first": "b",
            "same": true
        }))
    );
}

#[test]
fn instance_stdlib_collision_matrix_guard_sweeps_all_stdlib_methods() {
    let methods = lash_typescript::accepted_instance_methods();
    assert_eq!(
        methods.len(),
        89,
        "the collision matrix must sweep every accepted instance method"
    );

    for method in methods {
        // 1. Single-segment module authority: `await tools.<method>({})`
        let source_single = format!(r#"finish(await tools.{method}({{ payload: 1 }}));"#);
        let program_single = lash_typescript::parse(&source_single)
            .unwrap_or_else(|error| panic!("tools.{method} should parse: {error}"));
        let (path_single, op_single) = find_receiver_call(&program_single.main)
            .unwrap_or_else(|| panic!("expected ReceiverCall for tools.{method}"));
        assert_eq!(path_single, &["tools"], "tools.{method} path");
        assert_eq!(op_single, *method, "tools.{method} op");

        // 2. Dotted module authority: `await inbox.alpha.<method>({})`
        let source_dotted = format!(r#"finish(await inbox.alpha.{method}({{ payload: 1 }}));"#);
        let program_dotted = lash_typescript::parse(&source_dotted)
            .unwrap_or_else(|error| panic!("inbox.alpha.{method} should parse: {error}"));
        let (path_dotted, op_dotted) = find_receiver_call(&program_dotted.main)
            .unwrap_or_else(|| panic!("expected ReceiverCall for inbox.alpha.{method}"));
        assert_eq!(
            path_dotted,
            &["inbox", "alpha"],
            "inbox.alpha.{method} path"
        );
        assert_eq!(op_dotted, *method, "inbox.alpha.{method} op");
    }

    // Counter-case: an ECMA global namespace root with an instance-stdlib method
    // (globalThis.missing.get) must NOT lower as a tool call.
    let counter_source = "finish(globalThis.missing.get('k'));";
    let counter_program = lash_typescript::parse(counter_source)
        .expect("globalThis.missing.get should parse without requiring await");
    assert!(
        find_receiver_call(&counter_program.main).is_none(),
        "globalThis.missing.get must not lower as a tool call"
    );
}

#[test]
fn sibling_receiver_branches_pin_regexp_and_unsupported_checks() {
    // Branch :447 — RegExp methods on bound values lower to stdlib, while
    // unbound module authorities lower to tool calls.
    assert_eq!(
        finished(r#"const r = /abc/; finish(r.test("abcdef"));"#),
        Value::Bool(true)
    );
    assert_eq!(
        finished(r#"finish(/abc/.test("abcdef"));"#),
        Value::Bool(true)
    );
    let test_tool = lash_typescript::parse(r#"finish(await tools.test({ pattern: "abc" }));"#)
        .expect("tools.test should lower as tool call");
    let (path, op) = find_receiver_call(&test_tool.main).expect("ReceiverCall for tools.test");
    assert_eq!(path, &["tools"]);
    assert_eq!(op, "test");

    let exec_tool = lash_typescript::parse(r#"finish(await tools.exec({ command: "ls" }));"#)
        .expect("tools.exec should lower as tool call");
    let (path, op) = find_receiver_call(&exec_tool.main).expect("ReceiverCall for tools.exec");
    assert_eq!(path, &["tools"]);
    assert_eq!(op, "exec");

    // Branch :775 — Unbound ECMA globals and unsupported methods on bound
    // receivers refuse with TS_METHOD_UNSUPPORTED, while unawaited tool
    // operations refuse with TS_AWAIT_REQUIRED.
    let ecma_err = lash_typescript::compile("finish(Error.isError(new Error('x')));")
        .expect_err("ECMA static namespace method must refuse");
    assert_eq!(
        ecma_err.code,
        lash_typescript::DiagnosticCode::MethodUnsupported
    );

    let bound_err = lash_typescript::compile("const x = { a: 1 }; finish(x.nonExistentMethod());")
        .expect_err("unsupported method on bound receiver must refuse");
    assert_eq!(
        bound_err.code,
        lash_typescript::DiagnosticCode::MethodUnsupported
    );

    let unawaited_web = lash_typescript::parse("web.search({ query: 'x' });")
        .expect_err("unawaited web.search must require await");
    assert_eq!(
        unawaited_web.code,
        lash_typescript::DiagnosticCode::AwaitRequired
    );

    let unawaited_tools = lash_typescript::parse("tools.search({ query: 'x' });")
        .expect_err("unawaited tools.search must require await");
    assert_eq!(
        unawaited_tools.code,
        lash_typescript::DiagnosticCode::AwaitRequired
    );

    let unawaited_inbox = lash_typescript::parse("inbox.alpha.delete({ id: '1' });")
        .expect_err("unawaited inbox.alpha.delete must require await");
    assert_eq!(
        unawaited_inbox.code,
        lash_typescript::DiagnosticCode::AwaitRequired
    );
}

struct AggregateHost;

impl ExecutionHost for AggregateHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            ResourceOperationResult::Value(Value::Number(index as f64 + 1.0))
                        })
                        .collect(),
                ),
            )),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected aggregate ability")),
        }
    }
}

struct SettledHost;

impl ExecutionHost for SettledHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            if index == 0 {
                                ResourceOperationResult::Value(Value::String("ok".into()))
                            } else {
                                ResourceOperationResult::Error(ExecutionHostError::new("boom"))
                            }
                        })
                        .collect(),
                ),
            )),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected settled ability")),
        }
    }
}

struct SequentialAsyncMapHost {
    calls: AtomicUsize,
}

impl ExecutionHost for SequentialAsyncMapHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(AbilityResult::Value(Value::String("ok".into())))
                } else {
                    Err(ExecutionHostError::new("boom"))
                }
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected sequential async-map ability",
            )),
        }
    }
}

#[test]
fn promise_all_settled_async_map_catches_each_effect_failure_and_continues() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "finish(await Promise.allSettled(['a','b'].map(async url => await web.fetch({url}))));",
        &environment,
    )
    .expect("allSettled async map should link");
    let host = SequentialAsyncMapHost {
        calls: AtomicUsize::new(0),
    };
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &host,
    ))
    .expect("an individual callback rejection must not abort the async map");
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    let ExecutionOutcome::Finished(Value::List(items)) = outcome else {
        panic!("allSettled async map returns a list, got {outcome:?}");
    };
    assert_eq!(items.len(), 2);
    let rendered = format!("{items:?}");
    assert!(rendered.contains("fulfilled"), "{rendered}");
    assert!(rendered.contains("rejected"), "{rendered}");
    assert!(rendered.contains("boom"), "{rendered}");
}

#[test]
fn promise_all_executes_on_the_shared_aggregate_batch_machine() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_binding(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Any,
                output_from_input: None,
            },
        )
        .expect("test host binding");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all tool calls should link");
    let compiled = lash_typescript::compile_linked(&linked);
    let outcome = futures::executor::block_on(lashlang::execute(
        &compiled,
        &mut State::new(),
        &AggregateHost,
    ))
    .expect("aggregate should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
    );
}

#[test]
fn promise_all_settled_preserves_javascript_result_shape() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_binding(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Any,
                output_from_input: None,
            },
        )
        .expect("test host binding");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
    let linked = lash_typescript::link(
        "finish(await Promise.allSettled([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]));",
        &environment,
    )
    .expect("Promise.allSettled tool calls should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &SettledHost,
    ))
    .expect("settled aggregate should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            { "status": "fulfilled", "value": "ok" },
            { "status": "rejected", "reason": {
                "name": "EffectError",
                "message": "boom",
                "cause": {
                    "code": "ResourceOperationFailed",
                    "details": { "kind": "effect", "operation": "resource_batch" }
                }
            } }
        ])))
    );
}

/// A rejected `allSettled` leaf's reason is the same idiomatic error the awaited
/// form throws, so the discrimination a model writes against it works there too.
#[test]
fn promise_all_settled_rejection_reason_is_an_idiomatic_error() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.allSettled([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]);
         const reason = results[1].reason;
         finish([reason instanceof Error, String(reason), reason.name, reason.cause.code]);",
        &environment,
    )
    .expect("Promise.allSettled tool calls should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &SettledHost,
    ))
    .expect("settled aggregate should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            true,
            "EffectError: boom",
            "EffectError",
            "ResourceOperationFailed"
        ])))
    );
}

/// A host whose only tool operation fails, so a cell can catch the rejection the
/// substrate delivers for an ordinary awaited tool call.
struct RejectingToolHost;

impl ExecutionHost for RejectingToolHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => Err(ExecutionHostError::new("boom")),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected rejection ability")),
        }
    }
}

/// Runs a cell whose awaited tool call fails and finishes `probe`, evaluated
/// with the caught rejection bound to `error`.
fn caught_rejection(probe: &str) -> Value {
    let environment = two_leaf_web_environment();
    let source = format!(
        "try {{ await web.fetch({{ url: 'a' }}); finish('the tool call did not fail'); }}
         catch (error) {{ finish({probe}); }}"
    );
    let linked = lash_typescript::link(&source, &environment).expect("probe should link");
    match futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &RejectingToolHost,
    ))
    .expect("a failing tool call is catchable")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// FIG-1477's first observed failure: the delivered rejection was a plain
/// record, so the `instanceof Error` guard every model writes took the wrong
/// branch.
#[test]
fn a_tool_rejection_is_an_instance_of_error() {
    assert_eq!(
        caught_rejection("error instanceof Error"),
        Value::Bool(true)
    );
    assert_eq!(
        caught_rejection("[error instanceof TypeError, error instanceof RangeError]"),
        lashlang::from_json(serde_json::json!([false, false])),
        "the brand is an Error and nothing narrower"
    );
}

/// FIG-1477's second observed failure: `String(error)` rendered
/// `[object Object]`, so a model logging or reporting the rejection lost the
/// host's own text.
#[test]
fn a_tool_rejection_stringifies_informatively() {
    let Value::String(rendered) = caught_rejection("String(error)") else {
        panic!("String(error) is a string");
    };
    assert!(
        rendered.starts_with("EffectError: "),
        "String(error) names the brand: {rendered}"
    );
    assert!(
        rendered.contains("boom"),
        "String(error) carries the host's own text: {rendered}"
    );
}

/// FIG-1477's third observed failure: the standard try/catch discrimination —
/// read `message` off an `Error`, fall back to `String` otherwise — produced the
/// fallback branch and a wrong judged answer.
#[test]
fn a_tool_rejection_answers_the_standard_discrimination_pattern() {
    let Value::String(rendered) = caught_rejection(
        "error instanceof Error ? error.message : `not an error: ${String(error)}`",
    ) else {
        panic!("the discrimination probe finishes a string");
    };
    assert!(
        rendered.contains("boom"),
        "the Error branch reports the message: {rendered}"
    );
    assert_eq!(
        caught_rejection(
            "[error.name, typeof error.message, error.cause.code, error.cause.details.kind]"
        ),
        lashlang::from_json(serde_json::json!([
            "EffectError",
            "string",
            "UnwrappedModuleOperationFailed",
            "effect"
        ])),
        "the typed payload stays reachable on the documented `cause` property"
    );
}

#[test]
fn promise_aggregates_apply_promise_resolve_to_plain_values() {
    assert_eq!(
        finished("finish(await Promise.all([1, 2]));"),
        lashlang::from_json(serde_json::json!([1, 2]))
    );
    assert_eq!(
        finished("finish(await Promise.allSettled([1, 2]));"),
        lashlang::from_json(serde_json::json!([
            { "status": "fulfilled", "value": 1 },
            { "status": "fulfilled", "value": 2 }
        ]))
    );
}

struct RuntimeValueHost;

impl ExecutionHost for RuntimeValueHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                let Value::Resource(receiver) = operation.receiver else {
                    return Err(ExecutionHostError::new(
                        "runtime receiver is not a resource",
                    ));
                };
                assert_eq!(receiver.resource_type.as_str(), "typescript.Runtime");
                assert_eq!(receiver.alias.as_str(), "builtin");
                assert!(operation.args.is_empty());
                match operation.operation.as_str() {
                    "now" => Ok(AbilityResult::Value(Value::Number(1_723_456.0))),
                    "random" => Ok(AbilityResult::Value(Value::Number(0.25))),
                    other => Err(ExecutionHostError::new(format!(
                        "unexpected runtime operation {other}"
                    ))),
                }
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected runtime-value ability")),
        }
    }
}

#[test]
fn time_and_randomness_are_host_effects_instead_of_vm_nondeterminism() {
    let lowered = lash_typescript::parse("finish([Date.now(), Math.random()]);")
        .expect("runtime values should lower");
    assert!(
        lashlang::referenced_module_call_paths(&lowered).is_empty(),
        "resolved runtime intrinsics must not enter deferred tool discovery"
    );
    let program = lash_typescript::compile("finish({ now: Date.now(), random: Math.random() });")
        .expect("runtime values should compile");
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &RuntimeValueHost,
    ))
    .expect("runtime values should execute through the host");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!({
            "now": 1_723_456,
            "random": 0.25
        })))
    );
}

#[test]
fn argless_date_uses_the_same_journaled_clock_effect_as_date_now() {
    let program = lash_typescript::compile(
        "const d=new Date(); finish(`${d.getTime()}|${Date.now()}|${d.toISOString()}`);",
    )
    .expect("argless Date should compile through the runtime clock");
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &RuntimeValueHost,
    ))
    .expect("argless Date should execute through the host");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String(
            "1723456|1723456|1970-01-01T00:28:43.456Z".into()
        ))
    );
}

#[test]
fn common_for_forms_and_standard_library_execute() {
    assert_eq!(
        finished(
            r#"
            let total = 0;
            for (let i = 0; i < 4; i++) { total = total + i; }
            for (const value of [4, 5]) { total = total + value; }
            finish(total);
            "#,
        ),
        Value::Number(15.0)
    );
    assert_eq!(
        finished(
            r#"
            const text = "  durable TypeScript  ".trim().toUpperCase();
            const parts = ["DURABLE", "TYPESCRIPT"];
            finish({
              text,
              parts,
              keys: Object.keys({ b: 2, a: 1 }),
              array: Array.isArray(parts),
              integer: Number.isSafeInteger(42),
              encoded: JSON.stringify({ ok: true }),
              root: Math.sqrt(81)
            });
            "#,
        ),
        lashlang::from_json(serde_json::json!({
            "text": "DURABLE TYPESCRIPT",
            "parts": ["DURABLE", "TYPESCRIPT"],
            "keys": ["b", "a"],
            "array": true,
            "integer": true,
            "encoded": "{\"ok\":true}",
            "root": 9
        }))
    );
}

struct ProcessDurabilityHost;

impl ExecutionHost for ProcessDurabilityHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .iter()
                        .map(|operation| {
                            ResourceOperationResult::Value(
                                operation
                                    .args
                                    .first()
                                    .and_then(Value::as_record)
                                    .and_then(|record| record.get("value"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                            )
                        })
                        .collect(),
                ),
            )),
            AbilityOp::WaitSignal { name } => {
                assert_eq!(name, "ready");
                Ok(AbilityResult::Value(Value::String("signalled".into())))
            }
            AbilityOp::Sleep(_) => Ok(AbilityResult::Value(Value::Null)),
            AbilityOp::ProcessEvent(event) => Ok(AbilityResult::Value(event.value)),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected durable-process ability",
            )),
        }
    }
}

fn suspend_and_resume_process(source: &str, globals: serde_json::Value) -> ExecutionOutcome {
    futures::executor::block_on(async {
        let mut catalog = lashlang::LashlangHostCatalog::new();
        catalog
            .add_module_operation_binding(
                ["web"],
                "Web",
                "fetch",
                "tool:web/fetch",
                lashlang::ResourceOperationBinding {
                    input_ty: lashlang::TypeExpr::Any,
                    output_ty: lashlang::TypeExpr::Any,
                    output_from_input: None,
                },
            )
            .expect("test host binding");
        let environment =
            lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all());
        let linked = lash_typescript::link(source, &environment).expect("process should link");
        let compiled =
            lashlang::compile_linked_process(&linked, "worker").expect("process should compile");
        let mut state = State::from_snapshot(lashlang::Snapshot::new(
            lashlang::from_json(globals)
                .as_record()
                .expect("process globals must be a record")
                .clone(),
        ));
        let host = ProcessDurabilityHost;
        let execution_environment = lashlang::ExecutionEnvironment::new(&host).process();
        let mut vm = Vm::from_state(&compiled, &mut state, &execution_environment)
            .expect("install process VM");
        assert_eq!(
            vm.run_process_until_effect()
                .await
                .expect("run to durable effect"),
            VmRunOutcome::EffectCompleted
        );
        let encoded = serde_json::to_vec(&vm.suspend().expect("capture continuation"))
            .expect("encode continuation");
        let continuation = serde_json::from_slice(&encoded).expect("decode continuation");
        let mut resumed = Vm::resume_from(continuation, &compiled, &execution_environment)
            .expect("resume process VM");
        loop {
            match resumed
                .run_process_until_effect()
                .await
                .expect("complete resumed process")
            {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::Complete(outcome) => break outcome,
            }
        }
    })
}

#[test]
fn durable_processes_resume_across_await_signal_sleep_and_pending_finally() {
    let cases = [
        (
            r#"
            const worker = defineProcess({
              name: "worker", signals: { ready: null },
              run: async () => await waitSignal("ready")
            });
            "#,
            serde_json::json!({}),
            Value::String("signalled".into()),
        ),
        (
            r#"
            const worker = defineProcess({
              name: "worker", signals: {},
              run: async (input: unknown) => { await sleep(5); return input; }
            });
            "#,
            serde_json::json!({ "input": 7 }),
            Value::Number(7.0),
        ),
        (
            r#"
            const worker = defineProcess({
              name: "worker", signals: {},
              run: async (input: unknown) => {
                try { return input; } finally { await sleep(5); }
              }
            });
            "#,
            serde_json::json!({ "input": 9 }),
            Value::Number(9.0),
        ),
        (
            r#"
            const worker = defineProcess({
              name: "worker", signals: {},
              run: async (input: unknown) => { wake(input); return input; }
            });
            "#,
            serde_json::json!({ "input": 11 }),
            Value::Number(11.0),
        ),
    ];
    for (source, globals, expected) in cases {
        assert_eq!(
            suspend_and_resume_process(source, globals),
            ExecutionOutcome::Finished(expected)
        );
    }
}

#[test]
fn uncaught_throw_fails_a_durable_process() {
    futures::executor::block_on(async {
        let source = r#"
            const worker = defineProcess({
              name: "worker", signals: {},
              run: async () => { throw "broken"; }
            });
        "#;
        let program = lash_typescript::parse(source).expect("process should lower");
        let compiled =
            lash_typescript::compile_process(&program, "worker").expect("process compiles");
        let mut state = State::new();
        let host = ProcessDurabilityHost;
        let execution_environment = lashlang::ExecutionEnvironment::new(&host).process();
        let mut vm = Vm::from_state(&compiled, &mut state, &execution_environment)
            .expect("install process VM");
        let outcome = match vm
            .run_process_until_effect()
            .await
            .expect("uncaught throw should become a process outcome")
        {
            VmRunOutcome::Complete(outcome) => outcome,
            VmRunOutcome::EffectCompleted => panic!("process failure should be terminal"),
        };
        assert_eq!(
            outcome,
            ExecutionOutcome::Failed(Value::String("broken".into()))
        );
    });
}

#[test]
fn durable_process_resumes_after_shared_promise_batch() {
    let source = r#"
        const worker = defineProcess({
          name: "worker", signals: {},
          run: async () => await Promise.all([
            web.fetch({ value: 1 }), web.fetch({ value: 2 })
          ])
        });
    "#;
    assert_eq!(
        suspend_and_resume_process(source, serde_json::json!({})),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
    );
}

fn contains_return(expr: &Expr) -> bool {
    matches!(expr, Expr::Return(_)) || expr.children().any(contains_return)
}

fn contains_wake(expr: &Expr) -> bool {
    matches!(expr, Expr::Wake(_)) || expr.children().any(contains_wake)
}

fn contains_signal_run(expr: &Expr) -> bool {
    matches!(expr, Expr::SignalRun { name, .. } if name.as_str() == "ready")
        || expr.children().any(contains_signal_run)
}

fn contains_start(expr: &Expr) -> bool {
    matches!(expr, Expr::StartProcess(_)) || expr.children().any(contains_start)
}

fn contains_aggregate_await(expr: &Expr, unwrap: bool) -> bool {
    match expr {
        Expr::Await(value) if matches!(value.as_ref(), Expr::List(_)) => {
            value
                .children()
                .any(|child| matches!(child, Expr::ResultUnwrap(_)))
                == unwrap
        }
        _ => expr
            .children()
            .any(|child| contains_aggregate_await(child, unwrap)),
    }
}

/// The decisive case from the FIG-1305 report.
///
/// Leaf 0 rejects late with `late-A`; leaf 1 rejects early with `early-B`. The
/// host reports that leaf 1 settled first. ECMA rejects `Promise.all` at the
/// first settled rejection, so the surfaced reason must be `early-B` — the
/// input-order scan surfaces `late-A`.
struct FirstSettledRejectionHost;

impl ExecutionHost for FirstSettledRejectionHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => {
                assert_eq!(
                    batch.operations.len(),
                    2,
                    "the decisive case has two leaves"
                );
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult::settled_in_order(
                        vec![
                            ResourceOperationResult::Error(ExecutionHostError::new("late-A")),
                            ResourceOperationResult::Error(ExecutionHostError::new("early-B")),
                        ],
                        // Leaf 1 settled first.
                        vec![1, 0],
                    ),
                ))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected first-settled ability")),
        }
    }
}

fn two_leaf_web_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_binding(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Any,
                output_from_input: None,
            },
        )
        .expect("test host binding");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
}

#[test]
fn promise_all_rejects_with_the_first_settled_rejection() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all should link");
    let error = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &FirstSettledRejectionHost,
    ))
    .expect_err("a rejected aggregate fails the program");
    let rendered = error.to_string();
    assert!(
        rendered.contains("early-B"),
        "Promise.all must surface the first-settled rejection: {rendered}"
    );
    assert!(
        !rendered.contains("late-A"),
        "Promise.all must not surface the later rejection: {rendered}"
    );
}

/// `Promise.allSettled` is specified to preserve *input* order regardless of
/// when each leaf settled, so the same out-of-order settlement metadata must
/// leave the result array alone.
#[test]
fn promise_all_settled_stays_input_ordered_under_out_of_order_settlement() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "finish(await Promise.allSettled([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]));",
        &environment,
    )
    .expect("Promise.allSettled should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &FirstSettledRejectionHost,
    ))
    .expect("allSettled reports rejections as records");
    let ExecutionOutcome::Finished(Value::List(items)) = outcome else {
        panic!("allSettled returns a list, got {outcome:?}");
    };
    assert_eq!(items.len(), 2);
    let rendered = format!("{items:?}");
    let late = rendered.find("late-A").expect("leaf 0's reason is present");
    let early = rendered
        .find("early-B")
        .expect("leaf 1's reason is present");
    assert!(
        late < early,
        "allSettled keeps input order even when leaf 1 settled first: {rendered}"
    );
}

/// A host that reports an order that is not an ordering of its own results
/// must be refused, not read as input order.
struct MalformedSettlementHost;

impl ExecutionHost for MalformedSettlementHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_order(
                    batch
                        .operations
                        .iter()
                        .map(|_| ResourceOperationResult::Error(ExecutionHostError::new("boom")))
                        .collect(),
                    // Two leaves, but the same one named twice.
                    vec![1, 1],
                ),
            )),
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected malformed ability")),
        }
    }
}

#[test]
fn a_settlement_order_that_is_not_a_permutation_fails_closed() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all should link");
    let error = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &MalformedSettlementHost,
    ))
    .expect_err("a malformed settlement order is refused");
    let rendered = error.to_string();
    assert!(
        rendered.contains("settled position 1 was reported twice"),
        "the refusal names the offending position, not just a length: {rendered}"
    );
}

/// Selecting by settlement order must be a pure function of the journaled
/// result: replaying the same recorded batch selects the same reason, with no
/// re-sampling of anything.
#[test]
fn the_selected_rejection_is_replay_deterministic() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all should link");
    let compiled = lash_typescript::compile_linked(&linked);
    let mut reasons = Vec::new();
    for _ in 0..8 {
        let error = futures::executor::block_on(lashlang::execute(
            &compiled,
            &mut State::new(),
            &FirstSettledRejectionHost,
        ))
        .expect_err("a rejected aggregate fails the program");
        reasons.push(error.to_string());
    }
    let first = &reasons[0];
    assert!(
        first.contains("early-B"),
        "the recorded order selects the early rejection: {first}"
    );
    assert!(
        reasons.iter().all(|reason| reason == first),
        "replaying the same journaled order selects the same reason every time: {reasons:?}"
    );
}

/// Lashlang's own aggregates keep selecting in input order: the settlement
/// metadata is present but the dialect never asked to be ordered by it.
#[test]
fn lashlang_aggregates_still_select_in_input_order() {
    let environment = two_leaf_web_environment();
    let program = lashlang::parse(
        "let results = await [web.fetch({ url: 'a' })?, web.fetch({ url: 'b' })?]\nfinish results",
    );
    let program = program.expect("PROBE: lashlang aggregate parses");
    let linked = lashlang::LinkedModule::link(program, &environment)
        .expect("PROBE: lashlang aggregate links");
    let compiled =
        lashlang::compile_linked_with_dialect(&linked, lashlang::CompilationDialect::Lashlang);
    let error = futures::executor::block_on(lashlang::execute(
        &compiled,
        &mut State::new(),
        &FirstSettledRejectionHost,
    ))
    .expect_err("a rejected aggregate fails the program");
    let rendered = error.to_string();
    assert!(
        rendered.contains("late-A"),
        "lashlang keeps input-order selection: {rendered}"
    );
}

/// Settlement order is consumed inside a single `perform` and never persisted.
/// Snapshot v7 is independently required by the substrate-minted error brands;
/// the aggregate rule still does not move the VM ABI.
#[test]
fn settlement_order_does_not_reach_the_continuation_format() {
    assert_eq!(
        lashlang::LASHLANG_SNAPSHOT_VERSION,
        7,
        "snapshot v7 carries the substrate-minted error brands"
    );
    assert_eq!(
        lashlang::LASHLANG_VM_ABI_VERSION,
        "lashlang-vm-abi-v6",
        "the compiled-batch selection rule moved the VM ABI"
    );
}

/// A stored artifact that does not name its dialect must not decode at all.
///
/// The salvaged review probe: with a serde default, an artifact whose JSON
/// predates the dialect field decoded as Lashlang and verified, which is the
/// one route by which a TypeScript artifact could be compiled with Lashlang
/// semantics — including input-order rejection selection.
#[test]
fn an_artifact_without_a_dialect_does_not_decode() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link("finish(1);", &environment).expect("links");
    let artifact = linked.artifact.clone();
    let mut json = serde_json::to_value(&artifact).expect("artifact encodes");
    assert!(
        json.get("compilation_dialect").is_some(),
        "the dialect is always written"
    );
    json.as_object_mut()
        .expect("artifact object")
        .remove("compilation_dialect");
    let decoded = serde_json::from_value::<lashlang::ModuleArtifact>(json);
    let error = decoded.expect_err("a dialect-less artifact must not decode");
    assert!(
        error.to_string().contains("compilation_dialect"),
        "the refusal names the missing dialect: {error}"
    );
}

/// The canonical agent loop must compile.
///
/// The body filter used to reject every call and every member assignment, so
/// the most common loop in the language was a link-time rejection. Suspension
/// inside `for…of` is durable — the review proved it resumes across a
/// continuation round-trip — so the restriction was a conservative static
/// filter, not a durability requirement.
#[test]
fn for_of_bodies_accept_effects_and_unrelated_assignment() {
    let environment = two_leaf_web_environment();
    for source in [
        // The canonical agent loop.
        "const urls = ['a', 'b']; let out = ''; for (const url of urls) { const page = await web.fetch({ url }); out = out + page; } finish(out);",
        // A bare await on a tool, with no assignment at all.
        "const xs = ['a']; for (const x of xs) { await web.fetch({ url: x }); } finish('done');",
        // Assignment to something that is demonstrably not the iterable.
        "const xs = [1, 2]; const acc = { n: 0 }; for (const x of xs) { acc.n = acc.n + x; } finish(`${acc.n}`);",
        // A second array, written while iterating the first.
        "const xs = [1, 2]; const out = [0, 0]; for (const x of xs) { out[0] = x; } finish(`${out[0]}`);",
        // An aggregate inside the body.
        "const xs = ['a']; for (const x of xs) { await Promise.all([web.fetch({ url: x })]); } finish('done');",
        // The iterable is a call result, so nothing in the body can name it.
        "for (const c of 'ab'.concat('c')) { const page = await web.fetch({ url: c }); } finish('done');",
    ] {
        lash_typescript::link(source, &environment)
            .unwrap_or_else(|error| panic!("must link: {error}\n  source: {source}"));
    }
}

/// Genuine iterable mutation stays rejected, and says which shape reached it.
#[test]
fn for_of_bodies_still_reject_reaching_the_iterable() {
    let environment = two_leaf_web_environment();
    for (source, needle) in [
        (
            "const xs = [1, 2]; for (const x of xs) { xs[0] = 9; } finish('done');",
            "assigns through `xs`",
        ),
        (
            "const xs = [1, 2]; for (const x of xs) { xs.pop(); } finish('done');",
            "calls `xs.pop()`",
        ),
        (
            "const xs = [1, 2]; for (const x of xs) { await web.fetch({ url: xs }); } finish('done');",
            "passes `xs`",
        ),
    ] {
        let error = lash_typescript::link(source, &environment)
            .expect_err("iterable mutation stays rejected");
        assert_eq!(error.code.as_str(), "TS_FOR_OF_UNSUPPORTED", "{source}");
        assert!(
            error.to_string().contains(needle),
            "the rejection names the shape that reached the iterable: {error}"
        );
    }
}

#[test]
fn for_of_bodies_reject_mutation_in_patterns_without_rejecting_legal_patterns() {
    let environment = two_leaf_web_environment();
    for (shape, rejected, accepted) in [
        (
            "destructuring default",
            "const urls = ['a', 'b']; const xs = []; for (const u of urls) { const [a = urls.pop()] = xs; } finish('done');",
            "const urls = ['a', 'b']; const xs = []; for (const u of urls) { const [a = u] = xs; } finish('done');",
        ),
        (
            "parameter default",
            "const urls = ['a', 'b']; for (const u of urls) { function choose(a = urls.pop()) { return a; } } finish('done');",
            "const urls = ['a', 'b']; for (const u of urls) { function choose(a = u) { return a; } } finish('done');",
        ),
        (
            "computed pattern key",
            "const urls = ['a', 'b']; for (const u of urls) { const { [urls.pop()]: value } = {}; } finish('done');",
            "const urls = ['a', 'b']; for (const u of urls) { const { [u]: value } = {}; } finish('done');",
        ),
    ] {
        let error = lash_typescript::link(rejected, &environment)
            .expect_err("pattern-carried iterable mutation must reject");
        assert_eq!(error.code.as_str(), "TS_FOR_OF_UNSUPPORTED", "{shape}");
        assert!(
            error.to_string().contains("calls `urls.pop()`"),
            "{shape} names the newly reached mutating call: {error}"
        );

        lash_typescript::link(accepted, &environment)
            .unwrap_or_else(|error| panic!("legal {shape} must link: {error}"));
    }
}

/// A leaf that fails before the batch runs settles first.
///
/// This is the only reachable "leading" case in the host translation from
/// invocation positions to leaf positions — a journaled runtime value cannot be
/// a batch leaf, because an aggregate containing one is rejected before it ever
/// reaches a host. Without this the translation is unexercised.
struct PreparationFailureHost;

impl ExecutionHost for PreparationFailureHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => {
                assert_eq!(batch.operations.len(), 2);
                // Leaf 1 never entered the batch: it failed while being
                // prepared, so it had already settled when the batch started.
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult::settled_in_order(
                        vec![
                            ResourceOperationResult::Error(ExecutionHostError::new(
                                "ran-and-failed",
                            )),
                            ResourceOperationResult::Error(ExecutionHostError::new(
                                "never-prepared",
                            )),
                        ],
                        vec![1, 0],
                    ),
                ))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected preparation ability")),
        }
    }
}

#[test]
fn a_leaf_that_failed_before_the_batch_ran_settles_first() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all should link");
    let error = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &PreparationFailureHost,
    ))
    .expect_err("a rejected aggregate fails the program");
    let rendered = error.to_string();
    assert!(
        rendered.contains("never-prepared"),
        "the leaf that settled before the batch ran is the reported rejection: {rendered}"
    );
}

fn run_typescript(source: &str) -> Value {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(source, &environment)
        .unwrap_or_else(|error| panic!("link `{source}`: {error}"));
    match futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &AggregateHost,
    ))
    .unwrap_or_else(|error| panic!("execute `{source}`: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// The overflow rewrite must not touch anything but the overflowing numbers.
///
/// It copied source bytes with `byte as char`, so every UTF-8 continuation byte
/// was reinterpreted as Latin-1 and re-encoded: one out-of-range number
/// mojibaked every non-ASCII character in the document. That replaced a typed
/// failure with silently wrong data, on exactly the host-data path the clamping
/// was justified by.
#[test]
fn json_overflow_rewriting_preserves_non_ascii_text() {
    for (source, expected) in [
        (r#"finish(JSON.parse('{"a":"café","n":1e400}').a);"#, "café"),
        (
            r#"finish(JSON.parse('{"a":"日本語","n":1e400}').a);"#,
            "日本語",
        ),
        (
            r#"finish(JSON.parse('{"a":"emoji 😀 tail","n":1e400}').a);"#,
            "emoji 😀 tail",
        ),
        // Multi-byte characters immediately either side of the rewritten token.
        (
            r#"finish(JSON.parse('{"a":"é","n":1e400,"b":"ü"}').b);"#,
            "ü",
        ),
        // No overflow at all: the untouched path must stay correct too.
        (r#"finish(JSON.parse('{"a":"café"}').a);"#, "café"),
    ] {
        assert_eq!(
            run_typescript(source),
            Value::String(expected.into()),
            "{source}"
        );
    }
}

/// Guest data shaped like the rewrite's own marker must never be reinterpreted.
#[test]
fn json_overflow_sentinel_does_not_collide_with_guest_data() {
    let value = run_typescript(
        r#"finish(JSON.stringify(JSON.parse('{"o":{"__lash_json_f64_overflow_sign__":1},"n":1e400}').o));"#,
    );
    assert_eq!(
        value,
        Value::String(r#"{"__lash_json_f64_overflow_sign__":1}"#.into()),
        "a guest object that looks like the marker survives unchanged"
    );
}

/// `map` must actually run: it was advertised, accepted, and then failed at
/// run time with a host-boundary error because the stdlib builtin exports
/// every argument across the boundary and a closure cannot cross it.
#[test]
fn array_map_runs_its_callback_in_the_vm() {
    assert_eq!(
        run_typescript("finish([1,2,3].map((x) => x * 2).join('-'));"),
        Value::String("2-4-6".into())
    );
    assert_eq!(
        run_typescript("finish(['a','b'].map((x, i) => x + i).join('-'));"),
        Value::String("a0-b1".into())
    );
    assert_eq!(
        run_typescript("const a = [1,2,3]; finish(a.map((x) => x + 1).join('-'));"),
        Value::String("2-3-4".into())
    );
    // The callback closes over its environment.
    assert_eq!(
        run_typescript("const k = 10; finish([1,2].map((x) => x + k).join('-'));"),
        Value::String("11-12".into())
    );
    assert_eq!(
        run_typescript("finish([].map((x) => x).join('-'));"),
        Value::String("".into())
    );
}

/// Callback arity is ordinary ECMAScript call arity: named functions and
/// three-argument callbacks are valid, while a missing callback rejects.
#[test]
fn array_map_accepts_ecma_callback_shapes_and_rejects_a_missing_callback() {
    assert_eq!(
        run_typescript(
            "function d(x: number): number { return x * 2; } finish([1,2].map(d).join('-'));"
        ),
        Value::String("2-4".into())
    );
    assert_eq!(
        run_typescript("finish([1,2].map((x, i, all) => x+i+all.length).join('-'));"),
        Value::String("3-5".into())
    );
    for source in [
        "finish([1,2].map().join('-'));",
        "finish([1,2].filter().join('-'));",
        "finish([1,2].reduce());",
    ] {
        let environment = two_leaf_web_environment();
        let error = lash_typescript::link(source, &environment)
            .expect_err("a missing callback must reject at link time");
        assert_eq!(error.code.as_str(), "TS_METHOD_UNSUPPORTED", "{source}");
    }
}

/// Giving the iterable a second name inside the body defeats root tracking:
/// the alias is written through, the snapshot iterator hides it, and the loop
/// diverges from ECMA. Both escapes the verification found reject by name.
#[test]
fn for_of_bodies_reject_aliasing_the_iterable() {
    let environment = two_leaf_web_environment();
    for source in [
        "const urls = ['a','b','c']; let out = ''; for (const u of urls) { const alias = urls; alias[1] = 'MUT'; out = out + u; } finish(out);",
        "const urls = ['a','b']; for (const u of urls) { const box = { inner: urls }; box.inner[0] = 'MUT'; } finish('done');",
        "const urls = ['a','b']; for (const u of urls) { const boxed = [urls]; boxed[0][0] = 'MUT'; } finish('done');",
    ] {
        let error = lash_typescript::link(source, &environment)
            .expect_err("aliasing the iterable must reject");
        assert_eq!(error.code.as_str(), "TS_FOR_OF_UNSUPPORTED", "{source}");
        assert!(
            error.to_string().contains("urls"),
            "the rejection names the iterable: {error}"
        );
    }
    // A member-rooted iterable is the same hazard: `data.items` roots at
    // `data`, and the mutation half of the filter already tracks that root, so
    // the aliasing half has to agree or the alias escapes through the gap.
    for source in [
        "const data = { items: ['a','b'] }; let out = ''; for (const u of data.items) { const alias = data.items; alias[1] = 'MUT'; out = out + u; } finish(out);",
        "const data = { items: ['a','b'] }; for (const u of data.items) { const b = { x: data.items }; b.x[1] = 'MUT'; } finish('done');",
        "const data = { items: ['a','b'] }; for (const u of data.items) { const b = [data.items]; b[0][1] = 'MUT'; } finish('done');",
        "const data = { items: ['a','b'] }; for (const u of data.items) { const alias = data; alias.items[1] = 'MUT'; } finish('done');",
    ] {
        let error = lash_typescript::link(source, &environment)
            .expect_err("aliasing a member-rooted iterable must reject");
        assert_eq!(error.code.as_str(), "TS_FOR_OF_UNSUPPORTED", "{source}");
        assert!(
            error.to_string().contains("data"),
            "the rejection names the root the loop is walking: {error}"
        );
    }
    // Binding something else entirely stays fine.
    for source in [
        "const urls = ['a']; const other = ['b']; for (const u of urls) { const alias = other; alias[0] = 'ok'; } finish('done');",
        "const data = { items: ['a'] }; const other = ['b']; for (const u of data.items) { const alias = other; alias[0] = 'ok'; } finish('done');",
        "const urls = ['a','b']; let out = ''; for (const u of urls) { const upper = u + '!'; out = out + upper; } finish(out);",
        "const data = { items: ['a','b'] }; let n = 0; for (const u of data.items) { n = n + 1; } finish('done');",
    ] {
        lash_typescript::link(source, &environment)
            .unwrap_or_else(|error| panic!("legal body must still compile: {source}: {error}"));
    }
}

/// The register states that a `map` callback cannot perform effects and that
/// there is therefore no suspension point inside `map` to make durable. Both
/// halves are claims about behaviour, so both are pinned here rather than
/// asserted in prose alone.
#[test]
fn map_callbacks_cannot_perform_effects() {
    let environment = two_leaf_web_environment();

    // An effect inside the callback terminates with the typed error the
    // register names — not an untyped host failure, and not silently working.
    let linked = lash_typescript::link(
        "const out = [1,2].map((x) => { console.log(x); return x; }); finish(out);",
        &environment,
    )
    .expect("an effect inside a callback is not a link-time rejection today");
    let error = futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        &AggregateHost,
    ))
    .expect_err("an effect inside a map callback must fail");
    assert!(
        error
            .to_string()
            .contains("effects are not supported inside builtin callbacks"),
        "the failure is the typed builtin-callback error: {error}"
    );

    // `await` inside the callback never reaches run time, so no suspension can
    // occur inside `map`.
    let rejected = lash_typescript::link(
        "const out = [1,2].map(async (x) => { return await fetchPage('a'); }); finish(out);",
        &environment,
    )
    .expect_err("an async callback must reject");
    assert!(
        rejected.code.as_str().starts_with("TS_"),
        "the rejection is named: {rejected}"
    );
}

/// Every `Math.random()` draw crosses the journal, in order, so a replayed turn
/// reproduces the sequence it drew the first time.
///
/// This is the one accepted operation with no oracle: pinning it against Node
/// is impossible by construction. The property that makes it admissible in a
/// durable program is not the distribution but the seam — the VM samples no
/// RNG of its own, so a host serving a recorded journal replays the run
/// exactly. If a draw were ever computed in-VM, the second run below would
/// still succeed while the host's draw count fell short.
#[test]
fn math_random_draws_replay_from_the_journal_in_order() {
    struct JournalHost {
        recorded: Vec<f64>,
        served: std::sync::Mutex<usize>,
    }

    impl ExecutionHost for JournalHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    assert_eq!(operation.operation.as_str(), "random");
                    let mut cursor = self.served.lock().expect("journal cursor");
                    let value = *self
                        .recorded
                        .get(*cursor)
                        .expect("the journal has a recorded draw for every call");
                    *cursor += 1;
                    Ok(AbilityResult::Value(Value::Number(value)))
                }
                AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
                _ => Err(ExecutionHostError::new("unexpected ability")),
            }
        }
    }

    let recorded = vec![0.125, 0.5, 0.875, 0.0];
    let program = lash_typescript::compile(
        "const out: number[] = []; for (let i = 0; i < 4; i++) { out[out.length] = Math.random(); } finish(out.join(','));",
    )
    .expect("a journaled random sequence should compile");

    let mut results = Vec::new();
    for _ in 0..2 {
        let host = JournalHost {
            recorded: recorded.clone(),
            served: std::sync::Mutex::new(0),
        };
        let outcome =
            futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &host))
                .expect("a journaled random sequence should execute");
        assert_eq!(
            *host.served.lock().expect("journal cursor"),
            recorded.len(),
            "every draw must reach the host"
        );
        results.push(outcome);
    }
    assert_eq!(results[0], results[1], "replay must reproduce the sequence");
    assert_eq!(
        results[0],
        ExecutionOutcome::Finished(Value::String("0.125,0.5,0.875,0".into()))
    );
}

#[test]
fn typescript_host_catalog_composition_refuses_duplicate_operations() {
    let mut resources = lashlang::LashlangHostCatalog::tool_default(["lookup"]);
    let incoming = lashlang::LashlangHostCatalog::tool_default(["lookup"]);

    assert!(matches!(
        resources.try_extend(incoming),
        Err(lashlang::LashlangHostCatalogError::ConflictingModuleOperation {
            module,
            operation,
            ..
        }) if module == "tools" && operation == "lookup"
    ));
}
