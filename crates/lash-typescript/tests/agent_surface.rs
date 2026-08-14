use lashlang::{
    AbilityOp, AbilityResult, Declaration, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Expr, ResourceOperationBatchResult, ResourceOperationResult, State, Value, Vm, VmRunOutcome,
};

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
            .starts_with("lashlang:v1:sha256:")
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
                "code": "ResourceOperationFailed",
                "details": { "kind": "effect", "operation": "resource_batch" }
            } }
        ])))
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

/// Settlement order is consumed inside a single `perform` and never persisted,
/// so the durable continuation and snapshot formats must be untouched by this
/// change. If a later change does put batch state in a continuation, this pin
/// is the reminder that the format versions have to move with it.
#[test]
fn settlement_order_does_not_reach_the_continuation_format() {
    assert_eq!(
        lashlang::LASHLANG_SNAPSHOT_VERSION,
        4,
        "the snapshot format does not carry batch settlement state"
    );
    assert_eq!(
        lashlang::LASHLANG_VM_ABI_VERSION,
        "lashlang-vm-abi-v5",
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
