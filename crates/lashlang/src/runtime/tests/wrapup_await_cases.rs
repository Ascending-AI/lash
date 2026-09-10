use super::*;

/// Records every host ability a comprehension-await program performs: one
/// entry per batch (its operations, in order) and a count of single calls.
#[derive(Default)]
struct ComprehensionBatchHost {
    batches: Mutex<Vec<Vec<String>>>,
    singles: AtomicUsize,
}

impl ComprehensionBatchHost {
    fn perform_operation(operation: ResourceOperation) -> Result<Value, ExecutionHostError> {
        match operation.operation.as_str() {
            "order" => {
                let id = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut record = Record::new();
                record.insert("id".to_string(), id);
                record.insert("status".to_string(), Value::String("shipped".into()));
                Ok(Value::Record(Arc::new(record)))
            }
            "err" => Err(ExecutionHostError::new(format!(
                "boom {}",
                Self::describe(&operation)
            ))),
            "maybe_err" => {
                if Self::describe(&operation) == "maybe_err:b" {
                    Err(ExecutionHostError::new("failed order b"))
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Host::perform_resource_operation(operation),
        }
    }

    fn describe(operation: &ResourceOperation) -> String {
        let arg = operation
            .args
            .first()
            .and_then(Value::as_record)
            .and_then(|record| record.get("value").or_else(|| record.get("id")))
            .map(|value| value.to_string())
            .unwrap_or_default();
        format!("{}:{arg}", operation.operation)
    }

    fn batches(&self) -> Vec<Vec<String>> {
        self.batches.lock_recover().clone()
    }
}

impl ExecutionHost for ComprehensionBatchHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                self.singles.fetch_add(1, Ordering::SeqCst);
                Self::perform_operation(operation).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                self.batches
                    .lock_recover()
                    .push(batch.operations.iter().map(Self::describe).collect());
                // Deliberately settle in reverse order: Lashlang must still
                // select its rejection in written order.
                let mut results =
                    vec![ResourceOperationResult::Value(Value::Null); batch.operations.len()];
                let mut settlement_order = Vec::with_capacity(results.len());
                for (index, operation) in batch.operations.into_iter().enumerate().rev() {
                    results[index] =
                        ResourceOperationResult::from_result(Self::perform_operation(operation));
                    settlement_order.push(index);
                }
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult {
                        results,
                        settlement_order,
                    },
                ))
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected host ability in comprehension await test: {other:?}"
            ))),
        }
    }
}

#[derive(Default)]
struct AggregateProcessHost {
    awaits: AtomicUsize,
    reject_await: bool,
}

impl ExecutionHost for AggregateProcessHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .iter()
                        .map(|operation| {
                            if operation.operation == "err" {
                                ResourceOperationResult::Error(ExecutionHostError::new(
                                    "tool failed",
                                ))
                            } else {
                                ResourceOperationResult::Value(Value::Number(7.0))
                            }
                        })
                        .collect(),
                ),
            )),
            AbilityOp::StartProcess(_) => {
                let mut handle = Record::new();
                handle.insert("handle".to_string(), Value::String("h".into()));
                Ok(AbilityResult::Value(Value::Record(Arc::new(handle))))
            }
            AbilityOp::Await(_) => {
                self.awaits.fetch_add(1, Ordering::SeqCst);
                if self.reject_await {
                    Err(ExecutionHostError::new("process failed"))
                } else {
                    Ok(AbilityResult::Value(Value::Number(42.0)))
                }
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected host ability in aggregate process await test: {other:?}"
            ))),
        }
    }
}

fn comprehension_compile(source: &str) -> CompiledProgram {
    let mut catalog = crate::LashlangHostCatalog::new();
    for (module, operation) in [
        ("tools", "echo"),
        ("tools", "err"),
        ("tools", "maybe_err"),
        ("retail", "order"),
    ] {
        catalog
            .add_module_operation(
                [module],
                module,
                operation,
                operation,
                crate::TypeExpr::Any,
                crate::TypeExpr::Any,
            )
            .unwrap();
    }
    let linked = crate::LinkedModule::link(
        crate::parse(source).expect("program should parse"),
        crate::LashlangHostEnvironment::new(catalog, crate::LashlangAbilities::all()),
    )
    .expect("program should link");
    crate::compile_linked(&linked)
}

async fn comprehension_finish(host: &ComprehensionBatchHost, source: &str) -> Value {
    let compiled = comprehension_compile(source);
    let mut state = State::new();
    match execute_compiled(&compiled, &mut state, host)
        .await
        .expect("program should run")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

async fn aggregate_process_finish(host: &AggregateProcessHost, source: &str) -> Value {
    let compiled = comprehension_compile(source);
    let mut state = State::new();
    match execute_compiled(&compiled, &mut state, host)
        .await
        .expect("program should run")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bound_process_containers_use_process_await_seam() {
    for (source, expected) in [
        (
            "process echo() { finish null }; h=start echo(); finish await [[h], tools.echo({})?]",
            r#"[[{"ok":true,"value":42}],7]"#,
        ),
        (
            "process echo() { finish null }; h=start echo(); hs=[h]; finish await [hs, tools.echo({})?]",
            r#"[[{"ok":true,"value":42}],7]"#,
        ),
        (
            "process echo() { finish null }; h=start echo(); hr={child:h}; finish await [hr, tools.echo({})?]",
            r#"[{"child":{"ok":true,"value":42}},7]"#,
        ),
        (
            "process echo() { finish null }; h=start echo(); hs=[[[h]]]; finish await [hs, tools.echo({})?]",
            r#"[[[[{"ok":true,"value":42}]]],7]"#,
        ),
        (
            r#"process echo() { finish null }; h=start echo(); hs=[1,h,{note:"kept"}]; finish await [hs, tools.echo({})?]"#,
            r#"[[1,{"ok":true,"value":42},{"note":"kept"}],7]"#,
        ),
        (
            "process echo() { finish null }; h=start echo(); hs=[h]; finish await hs",
            r#"[{"ok":true,"value":42}]"#,
        ),
    ] {
        let host = AggregateProcessHost::default();
        let value = aggregate_process_finish(&host, source).await;
        assert_eq!(value.to_string(), expected, "{source}");
        assert_eq!(host.awaits.load(Ordering::SeqCst), 1, "{source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bound_failing_process_waits_until_module_leaves_settle() {
    let host = AggregateProcessHost {
        reject_await: true,
        ..AggregateProcessHost::default()
    };
    let compiled = comprehension_compile(
        "process echo() { finish null }; h=start echo(); hs=[h]; finish await [hs, tools.err({})?]",
    );
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .expect_err("the module rejection must win");
    assert!(error.to_string().contains("tool failed"), "{error}");
    assert_eq!(host.awaits.load(Ordering::SeqCst), 0);
}

#[test]
fn awaiting_a_settled_literal_is_a_link_diagnostic() {
    for (source, kind) in [
        ("finish await [1, 2]", "list"),
        ("finish await { a: 1 }", "record"),
        ("finish await [x for x in [1, 2]]", "list"),
        ("finish await 1", "number"),
        ("finish await (1 + 2)", "number"),
        ("finish await [1 + 2]", "list"),
        ("finish await { a: 1 + 2 }", "record"),
    ] {
        let diagnostic = link_diagnostic(source);
        assert!(
            diagnostic.contains(&format!("`await` of a settled {kind}:")),
            "{source}: {diagnostic}"
        );
        assert!(
            diagnostic.contains("await [m.op({ id: x })? for x in xs]"),
            "{source}: {diagnostic}"
        );
    }
}

#[test]
fn awaiting_a_handle_record_shape_is_not_rejected_as_settled() {
    for source in [
        r#"finish await { handle: "h" }"#,
        r#"finish await { __handle__: "h" }"#,
    ] {
        let program = crate::parse(source).unwrap();
        crate::LinkedModule::link(program, runtime_test_environment())
            .expect("handle shape should link");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nested_comprehension_aggregates_match_literal_expansion() {
    for (nested, literal) in [
        (
            r#"finish await ([tools.echo({ value: x })? for x in [1,2]], 7)"#,
            r#"finish await ([tools.echo({ value: 1 })?, tools.echo({ value: 2 })?], 7)"#,
        ),
        (
            r#"finish await { orders: [tools.echo({ value: x })? for x in [1,2]] }"#,
            r#"finish await { orders: [tools.echo({ value: 1 })?, tools.echo({ value: 2 })?] }"#,
        ),
        (
            r#"finish await [([ { orders: [tools.echo({ value: x })? for x in [1,2]] } ], tools.echo({ value: 3 })?)]"#,
            r#"finish await [([ { orders: [tools.echo({ value: 1 })?, tools.echo({ value: 2 })?] } ], tools.echo({ value: 3 })?)]"#,
        ),
        (
            r#"rows = [[1,2], [], [3]]; finish await [[tools.echo({ value: x })? for x in row] for row in rows]"#,
            r#"finish await [[tools.echo({ value: 1 })?, tools.echo({ value: 2 })?], [], [tools.echo({ value: 3 })?]]"#,
        ),
        (
            r#"finish await { before: tools.echo({ value: 0 })?, orders: [tools.echo({ value: x })? for x in []], after: tools.echo({ value: 3 })? }"#,
            r#"finish await { before: tools.echo({ value: 0 })?, orders: [], after: tools.echo({ value: 3 })? }"#,
        ),
    ] {
        let nested_host = ComprehensionBatchHost::default();
        let literal_host = ComprehensionBatchHost::default();
        let expected = comprehension_finish(&literal_host, literal).await;
        assert_eq!(comprehension_finish(&nested_host, nested).await, expected);
        assert_eq!(nested_host.batches(), literal_host.batches());
        assert_eq!(nested_host.batches().len(), 1);
        assert_eq!(nested_host.singles.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nested_comprehension_rejections_follow_written_order_after_settlement() {
    let host = ComprehensionBatchHost::default();
    let compiled = comprehension_compile(
        r#"finish await { orders: [[tools.err({ value: x })? for x in row] for row in [["first", "second"]]], last: tools.echo({ value: "last" })? }"#,
    );
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("first"), "{error}");
    assert_eq!(
        host.batches(),
        vec![vec![
            "err:first".to_string(),
            "err:second".to_string(),
            "echo:last".to_string()
        ]]
    );
    assert_eq!(host.singles.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_settled_await_names_the_value_and_nested_path() {
    let host = ComprehensionBatchHost::default();
    let compiled = comprehension_compile(
        "value = tools.echo({ value: { orders: [7] } })?; finish await value",
    );
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::AwaitExpectsHandle { .. }));
    assert!(
        error.to_string().contains("number at `orders[0]`"),
        "{error}"
    );
}
