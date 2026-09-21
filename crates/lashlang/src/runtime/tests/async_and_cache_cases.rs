use super::*;

/// `process echo(value: str) { finish value }`
fn echo_process() -> Declaration {
    builders::process(
        "echo",
        vec![builders::param("value", TypeExpr::Str)],
        builders::block(vec![builders::finish(builders::var("value"))]),
    )
}

/// `process scan() { finish 1 }`
fn scan_process() -> Declaration {
    builders::process(
        "scan",
        Vec::new(),
        builders::block(vec![builders::finish(builders::num(1.0))]),
    )
}

/// `start echo(value: <value>)`
fn start_echo(value: &str) -> Expr {
    builders::start("echo", vec![("value", builders::string(value))])
}

/// `tools.echo({ value: <value> })`
fn tools_echo(value: &str) -> Expr {
    builders::receiver_call(
        builders::resource(&["tools"]),
        "echo",
        vec![builders::record(vec![("value", builders::string(value))])],
    )
}

/// `(results[<index>])?`
fn unwrap_result(index: usize) -> Expr {
    #[expect(
        clippy::cast_precision_loss,
        reason = "test indexes are small literals"
    )]
    builders::unwrap(builders::index(
        builders::var("results"),
        builders::num(index as f64),
    ))
}

/// ```text
/// process echo(value: str) { finish value }
/// handles = <handles>
/// results = await handles
/// finish results
/// ```
fn await_handles_program(handles: Expr) -> Program {
    builders::module(
        vec![echo_process()],
        vec![
            builders::assign("handles", handles),
            builders::assign("results", builders::await_expr(builders::var("handles"))),
            builders::finish(builders::var("results")),
        ],
    )
}

fn finish_seven() -> Program {
    builders::program(vec![builders::finish(builders::num(7.0))])
}

struct AsyncHost;

impl ExecutionHost for AsyncHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => match operation.operation.as_str() {
                // `processes.start` is the tool spelling of the retired `start`
                // form (FIG-2999): it answers with the process handle.
                "start" => {
                    // The start's own arguments ride in `args`, beside the
                    // `definition` slot that carries the process itself.
                    let args = operation
                        .args
                        .first()
                        .and_then(Value::as_record)
                        .and_then(|record| record.get("args"))
                        .and_then(Value::as_record)
                        .cloned()
                        .unwrap_or_default();
                    let mut record = Record::default();
                    record.insert(
                        lash_sansio::handle::HANDLE_FIELD.to_string(),
                        Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                    );
                    record.insert(
                        "id".to_string(),
                        Value::String(
                            lash_sansio::handle::HandleId::process("proc-1", 1)
                                .as_str()
                                .into(),
                        ),
                    );
                    record.insert(
                        "value".to_string(),
                        args.get("value").cloned().unwrap_or(Value::Null),
                    );
                    Ok(AbilityResult::Value(Value::Record(Arc::new(record))))
                }
                "cancel" | "signal" => Ok(AbilityResult::Value(Value::Null)),
                _ => Host.perform(AbilityOp::ResourceOperation(operation)).await,
            },
            AbilityOp::ResourceOperationBatch(batch) => {
                Host.perform(AbilityOp::ResourceOperationBatch(batch)).await
            }
            AbilityOp::Await(handle) => {
                let record = handle
                    .as_record()
                    .ok_or_else(|| ExecutionHostError::new("expected handle record"))?;
                let value = record.get("value").cloned().unwrap_or(Value::Null);
                if value == Value::String("fail".into()) {
                    return Err(ExecutionHostError::new("process failed"));
                }
                Ok(AbilityResult::Value(value))
            }
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

/// Awaits a real process handle whose `value` is `"fail"` as a host failure,
/// so a list can hold one settled handle next to one failed handle.
struct FailingAwaitHost;

impl ExecutionHost for FailingAwaitHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Await(handle) => {
                let record = handle
                    .as_record()
                    .ok_or_else(|| ExecutionHostError::new("expected handle record"))?;
                match record.get("value") {
                    Some(Value::String(value)) if value.as_str() == "fail" => {
                        Err(ExecutionHostError::new("process failed: fail"))
                    }
                    Some(value) => Ok(AbilityResult::Value(value.clone())),
                    None => Ok(AbilityResult::Value(Value::Null)),
                }
            }
            other => AsyncHost.perform(other).await,
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
    // source = timer.Schedule({ expr: "0 8 * * *" })
    // finish source
    let program = builders::program(vec![
        builders::assign(
            "source",
            builders::receiver_call(
                builders::resource(&["timer"]),
                "Schedule",
                vec![builders::record(vec![(
                    "expr",
                    builders::string("0 8 * * *"),
                )])],
            ),
        ),
        builders::finish(builders::var("source")),
    ]);
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
    // process echo(value: str) { finish value }
    // handle = start echo(value: "done")
    // result = await handle
    // cancel handle
    // finish result
    let program = builders::module(
        vec![echo_process()],
        vec![
            builders::assign("handle", start_echo("done")),
            builders::assign("result", builders::await_expr(builders::var("handle"))),
            builders::cancel(builders::var("handle")),
            builders::finish(builders::var("result")),
        ],
    );
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

#[test]
fn compiled_process_cache_reuses_process_ref_and_host_requirements_ref() {
    let linked = crate::LinkedModule::link(
        // process scan() { finish 1 }
        builders::module(vec![scan_process()], Vec::new()),
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

#[tokio::test(flavor = "current_thread")]
async fn receiver_module_operation_unwraps_result() {
    // `finish (await tools.echo({ value: "ok" })?)`
    let value = exec(builders::program(vec![builders::finish(
        builders::module_call(
            &["tools"],
            "echo",
            vec![builders::record(vec![("value", builders::string("ok"))])],
        ),
    )]))
    .await
    .expect("module operation should run");
    assert_eq!(value, Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn receiver_module_operation_errors_are_sanitized() {
    // `finish (await tools.err({ value: "nope" })?)`
    let err = exec(builders::program(vec![builders::finish(
        builders::module_call(
            &["tools"],
            "err",
            vec![builders::record(vec![("value", builders::string("nope"))])],
        ),
    )]))
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
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, ProcessEventKind::Yield);
    assert_eq!(events[0].value, Value::String("checkpoint".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn while_runs_inside_process_body() {
    // process count_to(limit: int) {
    //   n = 0
    //   while n < limit { n = n + 1 }
    //   finish n
    // }
    let program = builders::module(
        vec![builders::process(
            "count_to",
            vec![builders::param("limit", TypeExpr::Int)],
            builders::block(vec![
                builders::assign("n", builders::num(0.0)),
                builders::while_loop(
                    builders::binary(
                        builders::var("n"),
                        crate::ast::BinaryOp::Less,
                        builders::var("limit"),
                    ),
                    builders::block(vec![builders::assign(
                        "n",
                        builders::binary(
                            builders::var("n"),
                            crate::ast::BinaryOp::Add,
                            builders::num(1.0),
                        ),
                    )]),
                ),
                builders::finish(builders::var("n")),
            ]),
        )],
        Vec::new(),
    );
    let compiled = crate::compile_process(&program, "count_to").expect("process should compile");
    let mut state = State::new();
    state
        .insert_global("limit", Value::Number(4.0))
        .expect("seeding a global stays within the heap bound");

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
async fn process_lifecycle_controls_sleep_and_wait() {
    let host = RecordingProcessHost::default();
    let program = Program::block(vec![
        Expr::SleepFor(Box::new(Expr::Number(5.0))),
        Expr::Assign {
            target: crate::AssignTarget::variable("payload".into()),
            expr: Box::new(Expr::WaitSignal {
                name: "ready".into(),
            }),
        },
        Expr::Finish(Box::new(Expr::Variable("payload".into()))),
    ]);
    let mut state = State::new();
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
    // The receiving side, `wait_signal`, plus yield/fail are process-only.
    // `finish` is valid in foreground code and process code.
    for (keyword, stmt) in [
        ("yield", Expr::Yield(Box::new(Expr::String("event".into())))),
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
    // `before = 20 + 2` / `echoed = await tools.echo({ value: before })?`
    // `after = echoed + 1` / `finish [before, echoed, after]`
    let mut expressions = echo_round_trip_prefix();
    expressions.push(builders::finish(builders::list(vec![
        builders::var("before"),
        builders::var("echoed"),
        builders::var("after"),
    ])));
    let value = exec(builders::program(expressions))
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
    // `before = 1` / `value = await tools.err({})?` / `finish value`. The
    // statement table carries the three top-level spans; the expression table
    // carries the failing `tools.err({})` call, which is the span the caret run
    // below pins. FIG-3065: nothing in production supplies these offsets any
    // more, so the test states them.
    let source = r#"
        before = 1
        value = await tools.err({})?
        finish value
        "#;
    let compiled = compile_program_for_tests(builders::with_source_spans(
        builders::with_expression_spans(
            builders::program(vec![
                builders::assign("before", builders::num(1.0)),
                builders::assign(
                    "value",
                    builders::await_expr(builders::unwrap(builders::receiver_call(
                        builders::resource(&["tools"]),
                        "err",
                        vec![builders::record(Vec::new())],
                    ))),
                ),
                builders::finish(builders::var("value")),
            ]),
            &[(9, 19), (28, 56), (65, 77)],
        ),
        &[(&[1, 0, 0, 0], 42, 55)],
    ));
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
    // `before = 20 + 2` / `echoed = await tools.echo({ value: before })?`
    // `after = echoed + 1` / `finish after`
    let mut expressions = echo_round_trip_prefix();
    expressions.push(builders::finish(builders::var("after")));
    let compiled = compile_program_for_tests(builders::program(expressions));
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
    // `values = [n * 2 for n in range(0, 6) if n > 1]` / `finish values`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "values",
            builders::comprehension(
                builders::binary(builders::var("n"), BinaryOp::Multiply, builders::num(2.0)),
                vec![
                    builders::comprehension_for(
                        "n",
                        builders::builtin("range", vec![builders::num(0.0), builders::num(6.0)]),
                    ),
                    builders::comprehension_if(builders::binary(
                        builders::var("n"),
                        BinaryOp::Greater,
                        builders::num(1.0),
                    )),
                ],
            ),
        ),
        builders::finish(builders::var("values")),
    ]));
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
    // result = await 1
    // finish result
    let program = builders::program(vec![
        builders::assign("result", builders::await_expr(builders::num(1.0))),
        builders::finish(builders::var("result")),
    ]);
    let mut state = State::new();
    let error = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect_err("awaiting a resolved value must fail loudly");
    assert!(
        matches!(&error, RuntimeError::AwaitExpectsHandle { .. }),
        "expected the typed await error, got {error:?}"
    );
    assert_eq!(
        error.to_string(),
        "`await` expects a process handle but found number; the value is already resolved"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_of_handles_returns_results_in_order() {
    // process echo(value: str) { finish value }
    // handles = [
    //   start echo(value: "first"),
    //   start echo(value: "second"),
    //   start echo(value: "third")
    // ]
    // results = await handles
    // finish results
    let program = await_handles_program(builders::list(vec![
        start_echo("first"),
        start_echo("second"),
        start_echo("third"),
    ]));
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
    // process echo(value: str) { finish value }
    // handles = [start echo(value: "done"), start echo(value: "fail")]
    // results = await handles
    // finish results
    let program =
        await_handles_program(builders::list(vec![start_echo("done"), start_echo("fail")]));
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &FailingAwaitHost)
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
    assert_eq!(err["error"], Value::String("process failed: fail".into()));
}

/// A record is neither a thenable nor an aggregate-await container, so `await`
/// hands it back untouched and its fields are still handles: settlement is
/// shallow over element positions (ADR 0096). The recursive walk that used to
/// reach into a bound record belonged to the retired surface dialect.
#[tokio::test(flavor = "current_thread")]
async fn await_of_a_record_leaves_its_handle_fields_unsettled() {
    // process echo(value: str) { finish value }
    // handles = {
    //   first: start echo(value: "one"),
    //   second: start echo(value: "two"),
    // }
    // results = await handles
    // finish results.first?
    let program = builders::module(
        vec![echo_process()],
        vec![
            builders::assign(
                "handles",
                builders::record(vec![
                    ("first", start_echo("one")),
                    ("second", start_echo("two")),
                ]),
            ),
            builders::assign("results", builders::await_expr(builders::var("handles"))),
            builders::finish(builders::unwrap(builders::field(
                builders::var("results"),
                "first",
            ))),
        ],
    );
    let mut state = State::new();
    let error = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect_err("an unsettled handle is not a result record");
    assert_eq!(
        error,
        RuntimeError::ToolResultExpected {
            actual: "heap_ref".to_string()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn result_unwrap_extracts_awaited_handles_and_joined_results() {
    // process echo(value: str) { finish value }
    // handle = start echo(value: "done")
    // result = (await handle)?
    // finish result
    let program = builders::module(
        vec![echo_process()],
        vec![
            builders::assign("handle", start_echo("done")),
            builders::assign(
                "result",
                builders::unwrap(builders::await_expr(builders::var("handle"))),
            ),
            builders::finish(builders::var("result")),
        ],
    );
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &AsyncHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    assert_eq!(value, Value::String("done".into()));

    // process echo(value: str) { finish value }
    // results = await [start echo(value: "left"), start echo(value: "right")]
    // finish [(results[0])?, (results[1])?]
    let program = builders::module(
        vec![echo_process()],
        vec![
            builders::assign(
                "results",
                builders::await_expr(builders::list(vec![
                    start_echo("left"),
                    start_echo("right"),
                ])),
            ),
            builders::finish(builders::list(vec![unwrap_result(0), unwrap_result(1)])),
        ],
    );
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

/// A process handle written at an element position of an awaited aggregate is
/// refused, wherever it is written and whatever is beside it.
///
/// This replaces ADR 0087's phase two, where such a handle settled after the
/// tool batch and a tool rejection therefore always won it. There is one
/// recorded settlement order now, so a wait that wants a place in it has to be
/// a leaf of the batch: `processes.await(handle)`, the tool that parks on it.
/// The repair says so, in both written positions, including beside a leaf that
/// would itself have rejected.
#[tokio::test(flavor = "current_thread")]
async fn a_process_handle_is_not_an_aggregate_leaf() {
    // process echo(value: str) { finish value }
    // h = start echo(value: "fail")
    // await [h, <leaf>]   /   await [<leaf>, h]
    let rejecting_leaf = || {
        builders::unwrap(builders::receiver_call(
            builders::resource(&["tools"]),
            "err",
            vec![builders::record(Vec::new())],
        ))
    };
    let aggregate = |leaves: Vec<Expr>| {
        builders::module(
            vec![echo_process()],
            vec![
                builders::assign("h", start_echo("fail")),
                builders::finish(builders::await_expr(builders::list(leaves))),
            ],
        )
    };
    for leaf in [tools_echo("right"), rejecting_leaf()] {
        for program in [
            aggregate(vec![builders::var("h"), leaf.clone()]),
            aggregate(vec![leaf.clone(), builders::var("h")]),
        ] {
            let mut state = State::new();
            let error = execute_program(&program, &mut state, &AsyncHost)
                .await
                .expect_err("a process handle is not a leaf of the batch");
            assert!(
                error.to_string().contains("processes.await(handle)"),
                "the refusal must name the tool that parks on the wait: {error}"
            );
        }
    }
}

// Which rejection an aggregate reports is the batch's recorded settlement
// order, and is pinned where that order is produced (`lash-core`'s
// `session::settlement_latency_tests`). ADR 0087's rule that a module
// rejection always beat a failing process is gone with the phase it described:
// both are leaves of one batch now, and a durable process wait reaches that
// batch as `processes.await`.

// ------------------------------------------------------------------
//  Type literals: syntactic signatures with enum, list, nested, ref,
//  optional fields. See the top-level README for the full grammar.
// ------------------------------------------------------------------

/// `finish Type { <fields> }`
fn finish_type_literal(fields: Vec<crate::ast::TypeField>) -> Program {
    builders::program(vec![builders::finish(builders::type_literal(
        TypeExpr::Object(fields),
    ))])
}

/// `before = 20 + 2` / `echoed = await tools.echo({ value: before })?`
/// `after = echoed + 1`
fn echo_round_trip_prefix() -> Vec<Expr> {
    vec![
        builders::assign(
            "before",
            builders::binary(builders::num(20.0), BinaryOp::Add, builders::num(2.0)),
        ),
        builders::assign(
            "echoed",
            builders::module_call(
                &["tools"],
                "echo",
                vec![builders::record(vec![("value", builders::var("before"))])],
            ),
        ),
        builders::assign(
            "after",
            builders::binary(builders::var("echoed"), BinaryOp::Add, builders::num(1.0)),
        ),
    ]
}

/// Extract the inner JSON Schema wrapped by a `$lash_type` value.
fn unwrap_schema(value: &Value) -> &Record {
    crate::runtime::unwrap_type_value(value)
        .and_then(Value::as_record)
        .expect("Type value must unwrap to a schema record")
}

#[tokio::test(flavor = "current_thread")]
async fn type_scalar_schemas_const_fold_to_json_schema() {
    for (src, ty, expected) in [
        ("finish Type { v: str }", TypeExpr::Str, "string"),
        ("finish Type { v: int }", TypeExpr::Int, "integer"),
        ("finish Type { v: float }", TypeExpr::Float, "number"),
        ("finish Type { v: bool }", TypeExpr::Bool, "boolean"),
        ("finish Type { v: dict }", TypeExpr::Dict, "object"),
    ] {
        let value = exec(finish_type_literal(vec![builders::type_field(
            "v", ty, false,
        )]))
        .await
        .expect("should succeed");
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
    // `finish Type { v: any }`
    let value = exec(finish_type_literal(vec![builders::type_field(
        "v",
        TypeExpr::Any,
        false,
    )]))
    .await
    .expect("should succeed");
    let schema = unwrap_schema(&value);
    let props = schema["properties"].as_record().expect("properties");
    let v = props["v"].as_record().expect("field schema");
    assert!(v.is_empty(), "any must be an empty JSON Schema");
}

#[tokio::test(flavor = "current_thread")]
async fn type_enum_produces_string_with_enum_array() {
    // `finish Type { status: enum["ok", "err", "pending"] }`
    let value = exec(finish_type_literal(vec![builders::type_field(
        "status",
        TypeExpr::Enum(vec!["ok".into(), "err".into(), "pending".into()]),
        false,
    )]))
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
    // `finish Type { tags: list[str] }`
    let value = exec(finish_type_literal(vec![builders::type_field(
        "tags",
        TypeExpr::List(Box::new(TypeExpr::Str)),
        false,
    )]))
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
    // `finish Type { labels: list[enum["a", "b"]] }`
    let value = exec(finish_type_literal(vec![builders::type_field(
        "labels",
        TypeExpr::List(Box::new(TypeExpr::Enum(vec!["a".into(), "b".into()]))),
        false,
    )]))
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
    // `finish Type { title: str, meta: Type { pages: int, published: int } }`
    let value = exec(finish_type_literal(vec![
        builders::type_field("title", TypeExpr::Str, false),
        builders::type_field(
            "meta",
            TypeExpr::Object(vec![
                builders::type_field("pages", TypeExpr::Int, false),
                builders::type_field("published", TypeExpr::Int, false),
            ]),
            false,
        ),
    ]))
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
    // `finish Type { a: str, b: int? }`
    let value = exec(finish_type_literal(vec![
        builders::type_field("a", TypeExpr::Str, false),
        builders::type_field("b", TypeExpr::Int, true),
    ]))
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
    // `Inner = Type { count: int }`
    // `Outer = Type { name: str, nested: Inner }` / `finish Outer`
    let value = exec(builders::program(vec![
        builders::assign(
            "Inner",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "count",
                TypeExpr::Int,
                false,
            )])),
        ),
        builders::assign(
            "Outer",
            builders::type_literal(TypeExpr::Object(vec![
                builders::type_field("name", TypeExpr::Str, false),
                builders::type_field("nested", TypeExpr::Ref("Inner".into()), false),
            ])),
        ),
        builders::finish(builders::var("Outer")),
    ]))
    .await
    .expect("should succeed");
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
    // `Inner = { count: 5 }` / `Outer = Type { nested: Inner }` / `finish Outer`
    let err = exec(builders::program(vec![
        builders::assign(
            "Inner",
            builders::record(vec![("count", builders::num(5.0))]),
        ),
        builders::assign(
            "Outer",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "nested",
                TypeExpr::Ref("Inner".into()),
                false,
            )])),
        ),
        builders::finish(builders::var("Outer")),
    ]))
    .await
    .expect_err("should fail: Inner is not a Type value");
    assert!(
        matches!(err, RuntimeError::NotTypeValue { .. }),
        "expected NotTypeValue, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn type_ref_with_undefined_name_is_undefined_variable() {
    // `finish Type { nested: MissingType }`
    let err = exec(finish_type_literal(vec![builders::type_field(
        "nested",
        TypeExpr::Ref("MissingType".into()),
        false,
    )]))
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
    // `Inner = Type { n: int }` / `A = Type { x: str }`
    // `B = Type { nested: Inner }` / `finish B`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "Inner",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "n",
                TypeExpr::Int,
                false,
            )])),
        ),
        builders::assign(
            "A",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "x",
                TypeExpr::Str,
                false,
            )])),
        ),
        builders::assign(
            "B",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "nested",
                TypeExpr::Ref("Inner".into()),
                false,
            )])),
        ),
        builders::finish(builders::var("B")),
    ]));
    let stats = compiled.compile_stats();
    assert_eq!(stats.type_literals_total, 3);
    // ADR 0096: the compile-time folder was Lashlang's value-semantics
    // optimization and went with the dialect, so a type literal that names an
    // earlier binding is emitted rather than folded. `Inner` and `A` still fold
    // because they close over nothing.
    assert_eq!(
        stats.type_literals_const_folded, 2,
        "Inner and A are constant"
    );
    assert_eq!(stats.type_literals_dynamic, 1);
    assert_eq!(stats.type_ref_sites, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn profile_report_shows_resolve_type_ref_counts() {
    // `Inner = await tools.echo({ value: Type { n: int } })?`
    // `Outer = Type { nested: Inner }`
    // `limit = await tools.echo({ value: 1 })?`
    // `numbers = push(range(limit), limit)`
    // `checked = validate({ nested: { n: numbers[0] } }, Outer)` / `finish checked`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "Inner",
            builders::module_call(
                &["tools"],
                "echo",
                vec![builders::record(vec![(
                    "value",
                    builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                        "n",
                        TypeExpr::Int,
                        false,
                    )])),
                )])],
            ),
        ),
        builders::assign(
            "Outer",
            builders::type_literal(TypeExpr::Object(vec![builders::type_field(
                "nested",
                TypeExpr::Ref("Inner".into()),
                false,
            )])),
        ),
        builders::assign(
            "limit",
            builders::module_call(
                &["tools"],
                "echo",
                vec![builders::record(vec![("value", builders::num(1.0))])],
            ),
        ),
        builders::assign(
            "numbers",
            builders::builtin(
                "push",
                vec![
                    builders::builtin("range", vec![builders::var("limit")]),
                    builders::var("limit"),
                ],
            ),
        ),
        builders::assign(
            "checked",
            builders::builtin(
                "validate",
                vec![
                    builders::record(vec![(
                        "nested",
                        builders::record(vec![(
                            "n",
                            builders::index(builders::var("numbers"), builders::num(0.0)),
                        )]),
                    )]),
                    builders::var("Outer"),
                ],
            ),
        ),
        builders::finish(builders::var("checked")),
    ]));
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
    // Shape = Type { name: str, tags: list[str] }
    // await tools.spawn({ output: Shape })
    // finish null
    let program = builders::program(vec![
        builders::assign(
            "Shape",
            builders::type_literal(TypeExpr::Object(vec![
                builders::type_field("name", TypeExpr::Str, false),
                builders::type_field("tags", TypeExpr::List(Box::new(TypeExpr::Str)), false),
            ])),
        ),
        builders::await_expr(builders::receiver_call(
            builders::resource(&["tools"]),
            "spawn",
            vec![builders::record(vec![("output", builders::var("Shape"))])],
        )),
        builders::finish(builders::null()),
    ]);
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
async fn lash_type_wrapper_survives_round_trip_through_json() {
    // `finish Type { n: int }`
    let value = exec(finish_type_literal(vec![builders::type_field(
        "n",
        TypeExpr::Int,
        false,
    )]))
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
// format (`{"__projected__": <tagged seed entry>}`) only needs a wrapper to survive the
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
    let (value, _) = exec_with_projected(
        builders::program(vec![builders::finish(builders::field(
            builders::var("input"),
            "prompt",
        ))]),
        &projected,
    )
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
    let (value, _) = exec_with_projected(
        // finish cfg.options.timeout
        builders::program(vec![builders::finish(builders::field(
            builders::field(builders::var("cfg"), "options"),
            "timeout",
        ))]),
        &projected,
    )
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
    let (value, _) = exec_with_projected(
        builders::program(vec![builders::finish(builders::index(
            builders::var("items"),
            builders::num(1.0),
        ))]),
        &projected,
    )
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
    let (value, _) = exec_with_projected(
        builders::program(vec![builders::finish(builders::binary(
            builders::field(builders::var("input"), "n"),
            crate::ast::BinaryOp::Add,
            builders::num(1.0),
        ))]),
        &projected,
    )
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
        // g = 42
        // finish { proj: input.prompt, glob: g, lit: 99 }
        builders::program(vec![
            builders::assign("g", builders::num(42.0)),
            builders::finish(builders::record(vec![
                ("proj", builders::field(builders::var("input"), "prompt")),
                ("glob", builders::var("g")),
                ("lit", builders::num(99.0)),
            ])),
        ]),
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
    program: Program,
    mode: TerminatorMode,
) -> (Result<ExecutionOutcome, RuntimeError>, Vec<AbilityOp>) {
    let host = TerminatorHost::new(mode);
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
    let (outcome, observed) =
        run_with_terminator_host(finish_seven(), TerminatorMode::Identity).await;
    assert_eq!(
        outcome.expect("finish should succeed"),
        ExecutionOutcome::Finished(Value::Number(7.0))
    );
    assert_eq!(observed.len(), 1, "host should observe one terminator op");
    assert!(matches!(observed[0], AbilityOp::Finish(Value::Number(n)) if n == 7.0));
}

#[tokio::test(flavor = "current_thread")]
async fn host_transforms_finish_value() {
    let (outcome, _) = run_with_terminator_host(finish_seven(), TerminatorMode::Transform).await;
    assert_eq!(
        outcome.expect("finish should succeed"),
        ExecutionOutcome::Finished(Value::Number(107.0)),
        "handler should transform the finish value before the VM unwinds"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn host_error_during_finish_propagates_as_runtime_error() {
    let (outcome, _) = run_with_terminator_host(finish_seven(), TerminatorMode::Err).await;
    let err = outcome.expect_err("host error should surface");
    let message = err.to_string();
    assert!(message.contains("finish failed"), "{message}");
    assert!(message.contains("handler refused"), "{message}");
}

#[tokio::test(flavor = "current_thread")]
async fn host_returning_unit_for_finish_errors_cleanly() {
    let (outcome, _) = run_with_terminator_host(finish_seven(), TerminatorMode::Unit).await;
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
        // process scan() { finish 1 }
        builders::module(vec![scan_process()], Vec::new()),
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
