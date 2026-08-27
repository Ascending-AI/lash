struct AsyncHost;

impl ExecutionHost for AsyncHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                Host.perform(AbilityOp::ResourceOperation(operation)).await
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                Host.perform(AbilityOp::ResourceOperationBatch(batch)).await
            }
            AbilityOp::StartProcess(start) => {
                let mut record = Record::default();
                record.insert("__handle__".to_string(), Value::String("process".into()));
                record.insert(
                    "process".to_string(),
                    Value::String(start.process_name.into()),
                );
                record.insert(
                    "value".to_string(),
                    start.args.get("value").cloned().unwrap_or(Value::Null),
                );
                Ok(AbilityResult::Value(Value::Record(Arc::new(record))))
            }
            AbilityOp::Await(handle) => {
                let record = handle
                    .as_record()
                    .ok_or_else(|| ExecutionHostError::new("expected handle record"))?;
                Ok(AbilityResult::Value(
                    record.get("value").cloned().unwrap_or(Value::Null),
                ))
            }
            AbilityOp::Cancel(_) => Ok(AbilityResult::Value(Value::Null)),
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn linked_value_constructor_wraps_host_descriptor() {
    let mut resources = crate::LashlangHostCatalog::new();
    resources
        .add_value_constructor(
            ["timer", "Schedule"],
            crate::TypeExpr::Object(vec![crate::TypeField {
                name: "expr".into(),
                ty: crate::TypeExpr::Str,
                optional: false,
            }]),
            crate::TypeExpr::Ref("timer.Schedule".into()),
        )
        .expect("value constructor is unique");
    let surface = crate::LashlangHostEnvironment::new(resources, crate::LashlangAbilities::all());
    let program = crate::parse(
        r#"
        source = timer.Schedule({ expr: "0 8 * * *" })
        finish source
        "#,
    )
    .expect("program should parse");
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    let compiled = crate::compile_linked(&linked);
    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &Host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(Value::Record(record)) = outcome else {
        panic!("expected host descriptor record, got {outcome:?}");
    };
    assert_eq!(
        record.get(LASH_HOST_DESCRIPTOR_TYPE_KEY),
        Some(&Value::String("timer.Schedule".into()))
    );
    let Some(Value::Record(source)) = record.get(LASH_HOST_DESCRIPTOR_VALUE_KEY) else {
        panic!("expected wrapped source record");
    };
    assert_eq!(source.get("expr"), Some(&Value::String("0 8 * * *".into())));
}

#[tokio::test(flavor = "current_thread")]
async fn process_handles_can_be_started_awaited_and_cancelled() {
    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        handle = start echo(value: "done")
        result = await handle
        cancel handle
        finish result
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let record = value
        .as_record()
        .expect("await should return wrapped result");
    assert_eq!(record["ok"], Value::Bool(true));
    assert_eq!(record["value"], Value::String("done".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn start_process_returns_raw_handle_and_passes_explicit_input() {
    let host = RecordingProcessHost::default();
    let program = crate::parse(
        r#"
        process scan(root: str) -> str {
          finish root
        }
        handle = start scan(root: ".")
        finish handle
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let handle = value.as_record().expect("start should return a handle");
    assert_eq!(handle["__handle__"], Value::String("process".into()));
    assert_eq!(handle["id"], Value::String("proc-1".into()));

    let starts = host.starts.lock_recover();
    assert_eq!(starts.len(), 1);
    let start = &starts[0];
    assert_eq!(start.process_name, "scan");
    assert_eq!(start.args["root"], Value::String(".".into()));
    assert!(start.module_ref.as_str().starts_with("lashlang:v2:blake3:"));
    assert_eq!(start.start_site.site.node_kind, "child_process");
    assert_eq!(start.start_site.site.label, "start scan");
    assert_eq!(start.start_site.occurrence, 1);
    assert!(start.start_site.site.node_id.starts_with("child_process:"));
}

#[tokio::test(flavor = "current_thread")]
async fn unlinked_compiled_program_rejects_unsited_process_starts() {
    let program = crate::parse(
        r#"
        process scan() { finish 1 }
        finish start scan()
        "#,
    )
    .expect("program should parse");
    let compiled = compile_program(&program);
    let mut state = State::new();

    let err = execute_compiled(&compiled, &mut state, &RecordingProcessHost::default())
        .await
        .expect_err("unsited start should fail");

    assert!(
        err.to_string()
            .contains("requires a deterministic lashlang execution site")
    );
}

#[test]
fn compiled_process_cache_reuses_process_ref_and_host_requirements_ref() {
    let linked = crate::LinkedModule::link(
        crate::parse("process scan() { finish 1 }").expect("parse module"),
        runtime_test_environment(),
    )
    .expect("link module");
    let process_ref = linked
        .artifact
        .process_ref("scan")
        .expect("scan process ref")
        .clone();
    let mut cache = CompiledProcessCache::with_capacity(2);

    let first = cache
        .get_or_compile(
            &linked.artifact,
            &process_ref,
            &linked.host_requirements_ref,
        )
        .expect("compile first");
    let second = cache
        .get_or_compile(
            &linked.artifact,
            &process_ref,
            &linked.host_requirements_ref,
        )
        .expect("compile second");

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1);
}

#[test]
fn compiled_process_cache_separates_source_dialects() {
    let program =
        crate::parse("process scan() { value = [1] alias = value alias[0] = 2 finish value[0] }")
            .expect("parse module");
    let lashlang = crate::LinkedModule::link_with_dialect(
        program.clone(),
        runtime_test_environment(),
        crate::CompilationDialect::Lashlang,
    )
    .expect("link Lashlang module");
    let typescript = crate::LinkedModule::link_with_dialect(
        program,
        runtime_test_environment(),
        crate::CompilationDialect::Typescript,
    )
    .expect("link TypeScript module");
    assert_ne!(lashlang.module_ref, typescript.module_ref);
    let process_ref = lashlang
        .artifact
        .process_ref("scan")
        .expect("scan process ref");
    assert_eq!(
        Some(process_ref),
        typescript.artifact.process_ref("scan"),
        "process identity intentionally remains source-dialect independent"
    );

    let mut cache = CompiledProcessCache::with_capacity(2);
    let lashlang_compiled = cache
        .get_or_compile(
            &lashlang.artifact,
            process_ref,
            &lashlang.host_requirements_ref,
        )
        .expect("compile Lashlang process");
    let typescript_compiled = cache
        .get_or_compile(
            &typescript.artifact,
            process_ref,
            &typescript.host_requirements_ref,
        )
        .expect("compile TypeScript process");

    assert!(!Arc::ptr_eq(&lashlang_compiled, &typescript_compiled));
    assert_eq!(cache.stats().hits, 0);
    assert_eq!(cache.stats().misses, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_module_operation_unwraps_result() {
    let value = exec(r#"finish (await tools.echo({ value: "ok" })?)"#)
        .await
        .expect("module operation should run");
    assert_eq!(value, Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_module_operation_errors_are_sanitized() {
    let err = exec(r#"finish (await tools.err({ value: "nope" })?)"#)
        .await
        .expect_err("module operation should fail");
    assert!(matches!(
        err,
        RuntimeError::UnwrappedModuleOperationFailed { .. }
    ));
    assert!(err.to_string().contains("module operation"));
}

#[tokio::test(flavor = "current_thread")]
async fn processes_emit_events_and_terminal_outcomes() {
    let host = RecordingProcessHost::default();
    let program = Program::block(vec![
        Expr::Yield(Box::new(Expr::String("checkpoint".into()))),
        Expr::Wake(Box::new(Expr::String("ready".into()))),
        Expr::Finish(Box::new(Expr::String("done".into()))),
    ]);
    let mut state = State::new();
    let compiled = compile_program(&program);
    let outcome = execute_compiled_process(&compiled, &mut state, &host)
        .await
        .expect("process admins should run");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("done".into()))
    );
    let events = host.events.lock_recover();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, ProcessEventKind::Yield);
    assert_eq!(events[0].value, Value::String("checkpoint".into()));
    assert_eq!(events[1].kind, ProcessEventKind::Wake);
    assert_eq!(events[1].value, Value::String("ready".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn while_runs_inside_process_body() {
    let program = crate::parse(
        r#"
        process count_to(limit: int) {
          n = 0
          while n < limit {
            n = n + 1
          }
          finish n
        }
        "#,
    )
    .expect("process with while should parse");
    let compiled = crate::compile_process(&program, "count_to").expect("process should compile");
    let mut state = State::new();
    state
        .globals
        .insert("limit".to_string(), Value::Number(4.0));

    let outcome = execute_compiled_process(&compiled, &mut state, &RecordingProcessHost::default())
        .await
        .expect("process while should run");

    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(4.0)));
}

#[tokio::test(flavor = "current_thread")]
async fn value_position_while_leaves_null() {
    let program = Program::block(vec![Expr::Finish(Box::new(Expr::While {
        condition: Box::new(Expr::Bool(false)),
        body: Box::new(Expr::Block(Vec::new())),
    }))]);
    let mut state = State::new();

    let outcome = execute_program(&program, &mut state, &Host)
        .await
        .expect("value-position while should run");

    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Null));
}

#[tokio::test(flavor = "current_thread")]
async fn process_lifecycle_controls_sleep_wait_and_signal() {
    let host = RecordingProcessHost::default();
    let mut handle = Record::new();
    handle.insert("__handle__".to_string(), Value::String("process".into()));
    handle.insert("id".to_string(), Value::String("target".into()));
    let program = Program::block(vec![
        Expr::SleepFor(Box::new(Expr::Number(5.0))),
        Expr::Assign {
            target: crate::AssignTarget::variable("payload".into()),
            expr: Box::new(Expr::WaitSignal {
                name: "ready".into(),
            }),
        },
        Expr::SignalRun {
            run: Box::new(Expr::Variable("run".into())),
            name: "ready".into(),
            payload: Box::new(Expr::Variable("payload".into())),
        },
        Expr::Finish(Box::new(Expr::Variable("payload".into()))),
    ]);
    let mut globals = Record::new();
    globals.insert("run".to_string(), Value::Record(Arc::new(handle)));
    let mut state = State::from_snapshot(Snapshot::new(globals));
    let compiled = compile_program(&program);

    let outcome = execute_compiled_process(&compiled, &mut state, &host)
        .await
        .expect("process lifecycle controls should run");

    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("signal-payload".into()))
    );
    let sleeps = host.sleeps.lock_recover();
    assert_eq!(sleeps.len(), 1);
    assert_eq!(sleeps[0].kind, SleepKind::For);
    assert_eq!(sleeps[0].value, Value::Number(5.0));
    let signals = host.signals.lock_recover();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].payload, Value::String("signal-payload".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn process_fail_returns_terminal_failure_outcome() {
    let host = RecordingProcessHost::default();
    let program = Program::block(vec![Expr::Fail(Box::new(Expr::Record(vec![(
        "reason".into(),
        Expr::String("bad".into()),
    )])))]);
    let mut state = State::new();
    let compiled = compile_program(&program);
    let outcome = execute_compiled_process(&compiled, &mut state, &host)
        .await
        .expect("process fail should run");
    let ExecutionOutcome::Failed(value) = outcome else {
        panic!("expected process failure");
    };
    let failure = value
        .as_record()
        .expect("failure should preserve raw value");
    assert_eq!(failure["reason"], Value::String("bad".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn process_mode_falling_off_end_finishes_null() {
    let host = RecordingProcessHost::default();
    let program = Program::block(vec![Expr::String("ignored".into())]);
    let compiled = compile_program(&program);
    let mut state = State::new();

    let outcome = execute_compiled_process(&compiled, &mut state, &host)
        .await
        .expect("process should run");

    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Null));
}

#[tokio::test(flavor = "current_thread")]
async fn foreground_rejects_programmatic_processes() {
    // `signal_run` (sending) is intentionally NOT in this list: it is allowed
    // from the foreground turn, like `await` / `cancel`. Only the receiving
    // side, `wait_signal`, plus yield/wake/fail are process-only. `finish`
    // is valid in foreground code and process code.
    for (keyword, stmt) in [
        ("yield", Expr::Yield(Box::new(Expr::String("event".into())))),
        ("wake", Expr::Wake(Box::new(Expr::String("event".into())))),
        (
            "wait_signal",
            Expr::WaitSignal {
                name: "ready".into(),
            },
        ),
        ("fail", Expr::Fail(Box::new(Expr::String("bad".into())))),
    ] {
        let program = Program::block(vec![stmt]);
        let mut state = State::new();
        let host = RecordingProcessHost::default();
        let err = execute_program(&program, &mut state, &host)
            .await
            .expect_err("foreground mode should reject process admins");
        assert_eq!(
            err,
            RuntimeError::SessionProcessAdminOutsideProcess {
                keyword: keyword.into()
            }
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn foreground_allows_signal_run() {
    let program = Program::block(vec![Expr::SignalRun {
        run: Box::new(Expr::String("handle".into())),
        name: "ready".into(),
        payload: Box::new(Expr::String("ping".into())),
    }]);
    let mut state = State::new();
    let host = RecordingProcessHost::default();
    execute_program(&program, &mut state, &host)
        .await
        .expect("foreground signal_run should be allowed");
    let signals = host.signals.lock_recover();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].name, "ready");
    assert_eq!(signals[0].payload, Value::String("ping".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn foreground_sleep_runs_as_regular_effect() {
    let host = RecordingProcessHost::default();
    let program = Program::block(vec![Expr::SleepFor(Box::new(Expr::Number(1.0)))]);
    let mut state = State::new();

    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("foreground sleep should run");

    assert_eq!(outcome, ExecutionOutcome::Continued);
    let sleeps = host.sleeps.lock_recover();
    assert_eq!(sleeps.len(), 1);
    assert_eq!(sleeps[0].kind, SleepKind::For);
}

#[tokio::test(flavor = "current_thread")]
async fn process_mode_rejects_programmatic_foreground_controls() {
    for (keyword, stmt) in [("print", Expr::Print(Box::new(Expr::String("debug".into()))))] {
        let program = Program::block(vec![stmt]);
        let compiled = compile_program(&program);
        let mut state = State::new();
        let host = RecordingProcessHost::default();
        let err = execute_compiled_process(&compiled, &mut state, &host)
            .await
            .expect_err("process mode should reject foreground controls");
        assert_eq!(
            err,
            RuntimeError::ForegroundControlInsideProcess {
                keyword: keyword.into()
            }
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sync_steps_resume_correctly_after_tool_effects() {
    let value = exec(
        r#"
        before = 20 + 2
        echoed = await tools.echo({ value: before })?
        after = echoed + 1
        finish [before, echoed, after]
        "#,
    )
    .await
    .expect("program should run");

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::Number(22.0),
                Value::Number(22.0),
                Value::Number(23.0)
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn traced_started_tool_errors_point_at_failing_tool_expression() {
    let source = r#"
        before = 1
        value = await tools.err({})?
        finish value
        "#;
    let compiled = compile_source(source).expect("program should compile");
    let mut state = State::new();
    let failure = execute_compiled_traced(&compiled, &mut state, &Host)
        .await
        .expect_err("unwrapped module operation error should fail");
    let message = crate::format_runtime_diagnostic(source, &failure.error, failure.span);

    assert!(
        message.contains("`?` unwrapped failed module operation: boom"),
        "{message}"
    );
    assert!(message.contains("--> line 3, column 23"), "{message}");
    assert!(
        message.contains("value = await tools.err({})?"),
        "{message}"
    );
    assert!(
        message.contains("                      ^~~~~~~~~~~~~"),
        "{message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn profiled_tool_effect_keeps_sync_instruction_counts() {
    let source = r#"
        before = 20 + 2
        echoed = await tools.echo({ value: before })?
        after = echoed + 1
        finish after
        "#;
    let compiled = compile_source(source).expect("program should compile");
    let mut state = State::new();
    let (_outcome, report) = profile_compiled(&compiled, &mut state, &Host)
        .await
        .expect("profile should succeed");
    let count = |name| {
        report
            .instruction_stats()
            .iter()
            .find(|stat| stat.name == name)
            .map_or(0, |stat| stat.count)
    };

    assert!(
        count("resource_call") > 0,
        "{:?}",
        report.instruction_stats()
    );
    assert!(count("binary") > 0, "{:?}", report.instruction_stats());
    assert!(count("load_name") > 0, "{:?}", report.instruction_stats());
    assert!(count("store_name") >= 3, "{:?}", report.instruction_stats());
}

#[tokio::test(flavor = "current_thread")]
async fn profile_report_tracks_list_comprehension_append_and_iteration() {
    let source = r#"
        values = [n * 2 for n in range(0, 6) if n > 1]
        finish values
        "#;
    let compiled = compile_source(source).expect("program should compile");
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ListAppend)),
        "comprehension should compile to ListAppend"
    );

    let mut state = State::new();
    let (outcome, report) = profile_compiled(&compiled, &mut state, &Host)
        .await
        .expect("profile should succeed");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::Number(4.0),
                Value::Number(6.0),
                Value::Number(8.0),
                Value::Number(10.0),
            ]
            .into()
        ))
    );

    let count = |name| {
        report
            .instruction_stats()
            .iter()
            .find(|stat| stat.name == name)
            .map_or(0, |stat| stat.count)
    };
    assert_eq!(
        count("append_assign"),
        4,
        "{:?}",
        report.instruction_stats()
    );
    assert!(count("begin_iter") > 0, "{:?}", report.instruction_stats());
    assert!(count("iter_next") > 0, "{:?}", report.instruction_stats());
}

#[tokio::test(flavor = "current_thread")]
async fn await_unknown_handle_reports_runtime_error() {
    let program = crate::parse(
        r#"
        result = await 1
        finish result
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let record = value
        .as_record()
        .expect("await should return wrapped error");
    assert_eq!(record["ok"], Value::Bool(false));
    assert_eq!(
        record["error"],
        Value::String("expected handle record".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_of_handles_returns_results_in_order() {
    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        handles = [
          start echo(value: "first"),
          start echo(value: "second"),
          start echo(value: "third")
        ]
        results = await handles
        finish results
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let Value::List(results) = value else {
        panic!("await list should return a list");
    };
    assert_eq!(results.len(), 3);
    for (result, expected) in results.iter().zip(["first", "second", "third"]) {
        let record = result
            .as_record()
            .expect("await should return wrapped result");
        assert_eq!(record["ok"], Value::Bool(true));
        assert_eq!(record["value"], Value::String(expected.into()));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_preserves_per_item_errors() {
    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        handles = [start echo(value: "done"), 1]
        results = await handles
        finish results
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let Value::List(results) = value else {
        panic!("await list should return a list");
    };
    let ok = results[0]
        .as_record()
        .expect("first result should be wrapped");
    assert_eq!(ok["ok"], Value::Bool(true));
    assert_eq!(ok["value"], Value::String("done".into()));

    let err = results[1]
        .as_record()
        .expect("second result should be wrapped");
    assert_eq!(err["ok"], Value::Bool(false));
    assert_eq!(err["error"], Value::String("expected handle record".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn await_record_of_handles_returns_record_of_wrappers() {
    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        handles = {
          first: start echo(value: "one"),
          second: start echo(value: "two"),
        }
        results = await handles
        finish [results.first?, results.second?]
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    assert_eq!(
        value,
        Value::List(vec![Value::String("one".into()), Value::String("two".into())].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn result_unwrap_extracts_awaited_handles_and_joined_results() {
    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        handle = start echo(value: "done")
        result = (await handle)?
        finish result
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    assert_eq!(value, Value::String("done".into()));

    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        results = await [
          start echo(value: "left"),
          start echo(value: "right")
        ]
        finish [(results[0])?, (results[1])?]
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    assert_eq!(
        value,
        Value::List(vec![Value::String("left".into()), Value::String("right".into()),].into())
    );
}

// ------------------------------------------------------------------
//  Type literals: syntactic signatures with enum, list, nested, ref,
//  optional fields. See the top-level README for the full grammar.
// ------------------------------------------------------------------

/// Extract the inner JSON Schema wrapped by a `$lash_type` value.
fn unwrap_schema(value: &Value) -> &Record {
    crate::runtime::unwrap_type_value(value)
        .and_then(Value::as_record)
        .expect("Type value must unwrap to a schema record")
}

#[tokio::test(flavor = "current_thread")]
async fn type_scalar_schemas_const_fold_to_json_schema() {
    for (src, expected) in [
        ("finish Type { v: str }", "string"),
        ("finish Type { v: int }", "integer"),
        ("finish Type { v: float }", "number"),
        ("finish Type { v: bool }", "boolean"),
        ("finish Type { v: dict }", "object"),
    ] {
        let value = exec(src).await.expect("should succeed");
        let schema = unwrap_schema(&value);
        assert_eq!(schema["type"], Value::String("object".into()));
        let props = schema["properties"]
            .as_record()
            .expect("properties must be record");
        let v = props["v"].as_record().expect("field schema");
        assert_eq!(v["type"], Value::String(expected.into()));
        assert_eq!(
            schema["additionalProperties"],
            Value::Bool(false),
            "additionalProperties must be false for {src}",
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn type_any_is_empty_schema() {
    let value = exec("finish Type { v: any }")
        .await
        .expect("should succeed");
    let schema = unwrap_schema(&value);
    let props = schema["properties"].as_record().expect("properties");
    let v = props["v"].as_record().expect("field schema");
    assert!(v.is_empty(), "any must be an empty JSON Schema");
}

#[tokio::test(flavor = "current_thread")]
async fn type_enum_produces_string_with_enum_array() {
    let value = exec(r#"finish Type { status: enum["ok", "err", "pending"] }"#)
        .await
        .expect("should succeed");
    let schema = unwrap_schema(&value);
    let status = schema["properties"].as_record().unwrap()["status"]
        .as_record()
        .expect("enum field schema");
    assert_eq!(status["type"], Value::String("string".into()));
    let Value::List(values) = &status["enum"] else {
        panic!("enum must be a list");
    };
    let strings: Vec<_> = values.iter().collect();
    assert_eq!(strings.len(), 3);
    assert_eq!(strings[0], &Value::String("ok".into()));
    assert_eq!(strings[2], &Value::String("pending".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn type_list_schema_wraps_inner_type_as_items() {
    let value = exec("finish Type { tags: list[str] }")
        .await
        .expect("should succeed");
    let schema = unwrap_schema(&value);
    let tags = schema["properties"].as_record().unwrap()["tags"]
        .as_record()
        .expect("list field schema");
    assert_eq!(tags["type"], Value::String("array".into()));
    let items = tags["items"].as_record().expect("items schema");
    assert_eq!(items["type"], Value::String("string".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn type_list_of_enum_preserves_nested_shape() {
    let value = exec(r#"finish Type { labels: list[enum["a", "b"]] }"#)
        .await
        .expect("should succeed");
    let schema = unwrap_schema(&value);
    let labels = schema["properties"].as_record().unwrap()["labels"]
        .as_record()
        .expect("list schema");
    let items = labels["items"].as_record().expect("enum item schema");
    assert_eq!(items["type"], Value::String("string".into()));
    assert!(matches!(items["enum"], Value::List(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn type_nested_object_is_full_subschema() {
    let value = exec(
        r#"
        finish Type {
          title: str,
          meta: Type {
            pages: int,
            published: int
          }
        }
        "#,
    )
    .await
    .expect("should succeed");
    let schema = unwrap_schema(&value);
    let meta = schema["properties"].as_record().unwrap()["meta"]
        .as_record()
        .expect("nested object schema");
    assert_eq!(meta["type"], Value::String("object".into()));
    let sub_props = meta["properties"].as_record().unwrap();
    assert_eq!(
        sub_props["pages"].as_record().unwrap()["type"],
        Value::String("integer".into())
    );
    let required = match &meta["required"] {
        Value::List(items) => items,
        _ => panic!("required must be list"),
    };
    assert_eq!(required.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn type_optional_field_drops_from_required() {
    let value = exec("finish Type { a: str, b: int? }")
        .await
        .expect("should succeed");
    let schema = unwrap_schema(&value);
    let required = match &schema["required"] {
        Value::List(items) => items,
        _ => panic!("required must be list"),
    };
    assert_eq!(required.len(), 1);
    assert_eq!(required[0], Value::String("a".into()));
    // Optional field still appears in properties (just not required).
    let props = schema["properties"].as_record().unwrap();
    assert!(props.get("b").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn type_ref_resolves_previously_defined_type() {
    let src = r#"
        Inner = Type { count: int }
        Outer = Type { name: str, nested: Inner }
        finish Outer
    "#;
    let value = exec(src).await.expect("should succeed");
    let schema = unwrap_schema(&value);
    let nested = schema["properties"].as_record().unwrap()["nested"]
        .as_record()
        .expect("nested resolved schema");
    assert_eq!(nested["type"], Value::String("object".into()));
    let nested_props = nested["properties"].as_record().unwrap();
    assert_eq!(
        nested_props["count"].as_record().unwrap()["type"],
        Value::String("integer".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn type_ref_to_non_type_value_is_type_error() {
    let err = exec(
        r#"
        Inner = { count: 5 }
        Outer = Type { nested: Inner }
        finish Outer
        "#,
    )
    .await
    .expect_err("should fail: Inner is not a Type value");
    assert!(
        matches!(err, RuntimeError::NotTypeValue { .. }),
        "expected NotTypeValue, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn type_ref_with_undefined_name_is_undefined_variable() {
    let err = exec("finish Type { nested: MissingType }")
        .await
        .expect_err("unknown ref should fail");
    assert_eq!(
        err,
        RuntimeError::UndefinedVariable {
            name: "MissingType".to_string()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn compile_stats_count_const_folded_and_dynamic_literals() {
    let src = r#"
        Inner = Type { n: int }
        A = Type { x: str }
        B = Type { nested: Inner }
        finish B
    "#;
    let compiled = compile_source(src).expect("should compile");
    let stats = compiled.compile_stats();
    assert_eq!(stats.type_literals_total, 3);
    assert_eq!(
        stats.type_literals_const_folded, 3,
        "Inner, A, and B are constant"
    );
    assert_eq!(stats.type_literals_dynamic, 0);
    assert_eq!(stats.type_ref_sites, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn profile_report_shows_resolve_type_ref_counts() {
    let src = r#"
        Inner = await tools.echo({ value: Type { n: int } })?
        Outer = Type { nested: Inner }
        limit = await tools.echo({ value: 1 })?
        numbers = push(range(limit), limit)
        checked = validate({ nested: { n: numbers[0] } }, Outer)
        finish checked
    "#;
    let compiled = compile_source(src).expect("should compile");
    let mut state = State::new();
    let (_outcome, report) = profile_compiled(&compiled, &mut state, &Host)
        .await
        .expect("profile should succeed");
    let names: Vec<_> = report.instruction_stats().iter().map(|s| s.name).collect();
    assert!(
        names.contains(&"resolve_type_ref"),
        "profile should track resolve_type_ref: {names:?}"
    );
    assert!(
        names.contains(&"wrap_type_literal"),
        "profile should track wrap_type_literal: {names:?}"
    );
    let builtin_names: Vec<_> = report.builtin_stats().iter().map(|s| s.name).collect();
    assert!(
        builtin_names.contains(&"validate"),
        "profile should track validate: {builtin_names:?}"
    );
    assert!(
        builtin_names.contains(&"range"),
        "profile should track range: {builtin_names:?}"
    );
    assert!(
        builtin_names.contains(&"push"),
        "profile should track push: {builtin_names:?}"
    );
    assert_eq!(report.compile_stats().type_literals_total, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn type_literal_inside_resource_operation_args_passes_through_as_record() {
    struct CaptureHost {
        captured: std::sync::Mutex<Option<Value>>,
    }
    impl ExecutionHost for CaptureHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    if operation.operation == "spawn" {
                        let schema = operation
                            .args
                            .first()
                            .and_then(Value::as_record)
                            .and_then(|record| record.get("output"))
                            .cloned()
                            .expect("output arg must be present");
                        *self.captured.lock_recover() = Some(schema);
                        return Ok(AbilityResult::Value(Value::Null));
                    }
                    Err(ExecutionHostError::new(format!(
                        "unknown: {}",
                        operation.operation
                    )))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }
    let host = CaptureHost {
        captured: std::sync::Mutex::new(None),
    };
    let program = crate::parse(
        r#"
        Shape = Type { name: str, tags: list[str] }
        await tools.spawn({ output: Shape })
        finish null
        "#,
    )
    .expect("should parse");
    let mut state = State::new();
    execute_program(&program, &mut state, &host)
        .await
        .expect("should run");

    let captured = host.captured.lock_recover().clone().expect("captured");
    let inner = crate::runtime::unwrap_type_value(&captured).expect("has $lash_type");
    let schema = inner.as_record().expect("schema record");
    assert_eq!(schema["type"], Value::String("object".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_field_name_is_parse_error() {
    let err = crate::parse("x = Type { a: str, a: int }").expect_err("duplicate field");
    let message = format!("{err}");
    assert!(message.contains("duplicate field"), "{message}");
}

#[tokio::test(flavor = "current_thread")]
async fn empty_enum_is_parse_error() {
    let err = crate::parse("x = Type { status: enum[] }").expect_err("empty enum");
    let message = format!("{err}");
    assert!(message.contains("enum"), "{message}");
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_type_constructor_becomes_ref_not_error_at_parse() {
    // Unknown identifiers in type position are treated as refs; runtime
    // resolution is what errors out.
    let program = crate::parse("finish Type { x: Unknown }").expect("should parse as ref");
    let Expr::Block(expressions) = program.main else {
        panic!("program should be a block");
    };
    assert!(matches!(expressions.last(), Some(Expr::Finish(_))));
}

#[tokio::test(flavor = "current_thread")]
async fn lash_type_wrapper_survives_round_trip_through_json() {
    let value = exec("finish Type { n: int }")
        .await
        .expect("should succeed");
    // to_json + from_json must preserve the Type-ness.
    let json = crate::runtime::to_json(&value);
    let recovered = crate::runtime::from_json(json);
    let schema = crate::runtime::unwrap_type_value(&recovered)
        .and_then(Value::as_record)
        .expect("round-trip must preserve wrapper");
    assert_eq!(schema["type"], Value::String("object".into()));
}

// ----------------------------------------------------------------------------
// Projection propagation: `Value::Projected` carries through path expressions
// (Field / Index) but is stripped by computation. This is the lashlang side
// of the unified `seed:` channel for spawn_agent / continue_as: the host wire
// format (`{"__projected__": <inner>}`) only needs a wrapper to survive the
// JSON boundary, but path-rooted entry-values must already be projected at
// runtime so they serialize that way.
// ----------------------------------------------------------------------------

fn projected_record_bindings(name: &str, record: serde_json::Value) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(
        name,
        ProjectedValue::scalar(name.to_string(), crate::runtime::from_json(record)),
    );
    projected
}

#[tokio::test(flavor = "current_thread")]
async fn field_access_on_projected_record_returns_projected() {
    let projected = projected_record_bindings(
        "input",
        serde_json::json!({ "prompt": "hello", "depth": 3 }),
    );
    let (value, _) = exec_with_projected("finish input.prompt", &projected)
        .await
        .expect("projected field read");
    assert!(
        matches!(value, Value::Projected(_)),
        "expected `input.prompt` to stay projected, got {value:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_field_access_keeps_projection() {
    let projected =
        projected_record_bindings("cfg", serde_json::json!({ "options": { "timeout": 30 } }));
    let (value, _) = exec_with_projected("finish cfg.options.timeout", &projected)
        .await
        .expect("nested projected field read");
    assert!(
        matches!(value, Value::Projected(_)),
        "expected nested field to stay projected, got {value:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn index_on_projected_list_returns_projected() {
    let projected =
        projected_record_bindings("items", serde_json::json!(["alpha", "beta", "gamma"]));
    let (value, _) = exec_with_projected("finish items[1]", &projected)
        .await
        .expect("projected index read");
    assert!(
        matches!(value, Value::Projected(_)),
        "expected `items[1]` to stay projected, got {value:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn computation_strips_projection() {
    let projected = projected_record_bindings("input", serde_json::json!({ "n": 7 }));
    let (value, _) = exec_with_projected("finish input.n + 1", &projected)
        .await
        .expect("computed value");
    assert!(
        !matches!(value, Value::Projected(_)),
        "computation should strip projection, got {value:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn record_literal_preserves_per_entry_projection() {
    let projected = projected_record_bindings("input", serde_json::json!({ "prompt": "hello" }));
    let (value, _) = exec_with_projected(
        "g = 42\nfinish { proj: input.prompt, glob: g, lit: 99 }",
        &projected,
    )
    .await
    .expect("record literal");
    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert!(
        matches!(record.get("proj"), Some(Value::Projected(_))),
        "expected `proj` entry to stay projected, got {:?}",
        record.get("proj")
    );
    assert!(
        !matches!(record.get("glob"), Some(Value::Projected(_))),
        "global `glob` should not be projected, got {:?}",
        record.get("glob")
    );
    assert!(
        !matches!(record.get("lit"), Some(Value::Projected(_))),
        "literal `lit` should not be projected, got {:?}",
        record.get("lit")
    );
}

// ---------------------------------------------------------------------------
// Terminator-op routing through the handler.
//
// `finish`, `finish`, `fail` go through `host.perform` as `AbilityOp::Finish`,
// `Finish`, `Fail`. Default behavior is identity pass-through (the host returns
// the value unchanged and the VM unwinds with that value). The handler may
// transform the value or refuse with an `Err`; it cannot prevent unwind.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum TerminatorMode {
    Identity,
    Transform,
    Err,
    Unit,
}

struct TerminatorHost {
    mode: TerminatorMode,
    observed: Mutex<Vec<AbilityOp>>,
}

impl TerminatorHost {
    fn new(mode: TerminatorMode) -> Self {
        Self {
            mode,
            observed: Mutex::new(Vec::new()),
        }
    }
}

impl ExecutionHost for TerminatorHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                let observed = match &value {
                    Value::Number(n) => AbilityOp::Finish(Value::Number(*n)),
                    other => AbilityOp::Finish(other.clone()),
                };
                self.observed.lock_recover().push(observed);
                match self.mode {
                    TerminatorMode::Identity => Ok(AbilityResult::Value(value)),
                    TerminatorMode::Transform => match value {
                        Value::Number(n) => Ok(AbilityResult::Value(Value::Number(n + 100.0))),
                        other => Ok(AbilityResult::Value(other)),
                    },
                    TerminatorMode::Err => Err(ExecutionHostError::new("handler refused")),
                    TerminatorMode::Unit => Ok(AbilityResult::Unit),
                }
            }
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

async fn run_with_terminator_host(
    source: &str,
    mode: TerminatorMode,
) -> (Result<ExecutionOutcome, RuntimeError>, Vec<AbilityOp>) {
    let host = TerminatorHost::new(mode);
    let program = crate::parse(source).expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host).await;
    let observed = host.observed.lock_recover().clone();
    (outcome, observed)
}

async fn run_process_with_terminator_host(
    program: Program,
    mode: TerminatorMode,
) -> (Result<ExecutionOutcome, RuntimeError>, Vec<AbilityOp>) {
    let host = TerminatorHost::new(mode);
    let compiled = compile_program(&program);
    let mut state = State::new();
    let outcome = execute_compiled_process(&compiled, &mut state, &host).await;
    let observed = host.observed.lock_recover().clone();
    (outcome, observed)
}

#[tokio::test(flavor = "current_thread")]
async fn finish_routes_through_host() {
    let (outcome, observed) = run_with_terminator_host("finish 7", TerminatorMode::Identity).await;
    assert_eq!(
        outcome.expect("finish should succeed"),
        ExecutionOutcome::Finished(Value::Number(7.0))
    );
    assert_eq!(observed.len(), 1, "host should observe one terminator op");
    assert!(matches!(observed[0], AbilityOp::Finish(Value::Number(n)) if n == 7.0));
}

#[tokio::test(flavor = "current_thread")]
async fn host_transforms_finish_value() {
    let (outcome, _) = run_with_terminator_host("finish 7", TerminatorMode::Transform).await;
    assert_eq!(
        outcome.expect("finish should succeed"),
        ExecutionOutcome::Finished(Value::Number(107.0)),
        "handler should transform the finish value before the VM unwinds"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn host_error_during_finish_propagates_as_runtime_error() {
    let (outcome, _) = run_with_terminator_host("finish 7", TerminatorMode::Err).await;
    let err = outcome.expect_err("host error should surface");
    let message = err.to_string();
    assert!(message.contains("finish failed"), "{message}");
    assert!(message.contains("handler refused"), "{message}");
}

#[tokio::test(flavor = "current_thread")]
async fn host_returning_unit_for_finish_errors_cleanly() {
    let (outcome, _) = run_with_terminator_host("finish 7", TerminatorMode::Unit).await;
    let err = outcome.expect_err("unit result should error");
    let message = err.to_string();
    assert!(message.contains("finish failed"), "{message}");
    assert!(message.contains("returned no value"), "{message}");
}

#[tokio::test(flavor = "current_thread")]
async fn finish_routes_through_host_in_process_mode() {
    let program = Program::block(vec![Expr::Finish(Box::new(Expr::Number(7.0)))]);
    let (outcome, observed) =
        run_process_with_terminator_host(program, TerminatorMode::Transform).await;
    assert_eq!(
        outcome.expect("finish should succeed"),
        ExecutionOutcome::Finished(Value::Number(107.0))
    );
    assert_eq!(observed.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn fail_routes_through_host_and_carries_failed_outcome() {
    let program = Program::block(vec![Expr::Fail(Box::new(Expr::String("boom".into())))]);
    let (outcome, observed) =
        run_process_with_terminator_host(program, TerminatorMode::Identity).await;
    assert_eq!(
        outcome.expect("fail should produce an outcome"),
        ExecutionOutcome::Failed(Value::String("boom".into()))
    );
    assert_eq!(observed.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn host_can_transform_fail_value_while_keeping_failure_path() {
    let program = Program::block(vec![Expr::Fail(Box::new(Expr::Number(7.0)))]);
    let (outcome, _) = run_process_with_terminator_host(program, TerminatorMode::Transform).await;
    assert_eq!(
        outcome.expect("fail should produce an outcome"),
        ExecutionOutcome::Failed(Value::Number(107.0)),
        "transformed value should still arrive on the failure path"
    );
}

/// A compiled-process cache hit must not build the key it only compares.
///
/// The key owns three cloned strings, so constructing it before the lookup
/// charged every hit an allocation and 83 bytes for a value it dropped
/// immediately — the same shape as the parse-on-hit defect, on the hot path.
#[test]
fn a_compiled_process_cache_hit_builds_no_key() {
    use std::sync::atomic::Ordering;

    let linked = crate::LinkedModule::link(
        crate::parse("process scan() { finish 1 }").expect("parse module"),
        runtime_test_environment(),
    )
    .expect("link module");
    let process_ref = linked
        .artifact
        .process_ref("scan")
        .expect("scan process ref")
        .clone();
    let mut cache = CompiledProcessCache::with_capacity(2);

    let before = crate::runtime::cache::COMPILED_PROCESS_KEYS_BUILT.load(Ordering::Relaxed);
    cache
        .get_or_compile(
            &linked.artifact,
            &process_ref,
            &linked.host_requirements_ref,
        )
        .expect("first compile misses");
    let after_miss = crate::runtime::cache::COMPILED_PROCESS_KEYS_BUILT.load(Ordering::Relaxed);
    assert_eq!(after_miss - before, 1, "a miss stores one owned key");

    for _ in 0..8 {
        cache
            .get_or_compile(
                &linked.artifact,
                &process_ref,
                &linked.host_requirements_ref,
            )
            .expect("subsequent lookups hit");
    }
    let after_hits = crate::runtime::cache::COMPILED_PROCESS_KEYS_BUILT.load(Ordering::Relaxed);
    assert_eq!(
        after_hits, after_miss,
        "eight cache hits must build no keys at all"
    );
    assert_eq!(cache.stats().hits, 8);
    assert_eq!(cache.stats().misses, 1);
}
