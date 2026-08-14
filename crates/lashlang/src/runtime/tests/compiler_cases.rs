#[test]
fn label_on_await_assignment_attaches_to_await_instruction() {
    let program = crate::parse(
        r#"
        @label(title: "Wait for child")
        result = await handle
        finish result
        "#,
    )
    .expect("program should parse");
    let surface = runtime_test_environment()
        .with_language_features(crate::LashlangLanguageFeatures::default().with_label_annotations())
        .with_globals(["handle"]);
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    let compiled = crate::compile_linked(&linked);
    let await_instruction = compiled
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::AwaitHandle))
        .expect("await handle instruction");

    assert!(
        compiled
            .chunk
            .lashlang_execution_sites
            .get(await_instruction)
            .and_then(Option::as_ref)
            .is_some(),
        "label should attach to the awaited effect instruction"
    );
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ObserveStep)),
        "label on awaited assignment should not emit a standalone observe step"
    );
}

#[test]
fn aggregate_await_record_of_resource_calls_emits_batch_instruction() {
    let compiled = compile_source(
        r#"
        results = await {
          first: tools.echo({ value: "a" }),
          second: tools.echo({ value: "b" })
        }
        finish results
        "#,
    )
    .expect("program should compile");
    let listing = compiled_instruction_listing(&compiled);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceOperationBatch(_))),
        "aggregate await should compile to one batch instruction:\n{listing}"
    );
    assert!(
        !compiled.chunk.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::ResourceCall { .. } | Instruction::ResourceCallUnwrap { .. }
        )),
        "aggregate await should not emit sequential resource calls:\n{listing}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_nested_resource_calls_reconstructs_shape() {
    let value = exec(
        r#"
        result = await {
          outer: [
            tools.echo({ value: "a" })?,
            { inner: tools.echo({ value: "b" })? }
          ]
        }
        finish result
        "#,
    )
    .await
    .expect("program should run");

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    let Value::List(outer) = &record["outer"] else {
        panic!("expected outer list");
    };
    assert_eq!(outer[0], Value::String("a".into()));
    assert_eq!(
        outer[1].as_record().unwrap()["inner"],
        Value::String("b".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_tuple_of_resource_calls_batches_and_reconstructs_tuple() {
    let source = r#"
        result = await (tools.echo({ value: "left" })?, tools.echo({ value: "right" })?)
        finish result
        "#;
    let compiled = compile_source(source).expect("program should compile");
    let listing = compiled_instruction_listing(&compiled);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceOperationBatch(_))),
        "aggregate tuple await should compile to one batch instruction:\n{listing}"
    );

    let value = exec(source).await.expect("program should run");
    let Value::Tuple(items) = value else {
        panic!("expected tuple result");
    };
    assert_eq!(
        &items[..],
        [Value::String("left".into()), Value::String("right".into())]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_mixed_pure_values_batch_resource_leaves_and_reconstructs_shape() {
    struct MixedHost {
        batch_len: std::sync::atomic::AtomicUsize,
    }

    impl ExecutionHost for MixedHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperationBatch(batch) => {
                    self.batch_len
                        .store(batch.operations.len(), std::sync::atomic::Ordering::SeqCst);
                    Ok(AbilityResult::ResourceOperationBatch(
                        ResourceOperationBatchResult::settled_in_input_order(batch
                                .operations
                                .into_iter()
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
                                .collect()),
                    ))
                }
                AbilityOp::ResourceOperation(_) => Err(ExecutionHostError::new(
                    "mixed aggregate should use the batch host ability",
                )),
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    let host = MixedHost {
        batch_len: std::sync::atomic::AtomicUsize::new(0),
    };
    let program = crate::parse(
        r#"
        label = "cache-miss"
        result = await {
          first: tools.echo({ value: "a" })?,
          source: label,
          nested: [
            3,
            tools.echo({ value: "b" })?,
            { ok: true, total: len([1, 2, 3]) }
          ]
        }
        finish result
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("program should finish");
    };

    assert_eq!(host.batch_len.load(std::sync::atomic::Ordering::SeqCst), 2);
    let record = value.as_record().expect("result record");
    assert_eq!(record["first"], Value::String("a".into()));
    assert_eq!(record["source"], Value::String("cache-miss".into()));
    let Value::List(nested) = &record["nested"] else {
        panic!("nested list");
    };
    assert_eq!(nested[0], Value::Number(3.0));
    assert_eq!(nested[1], Value::String("b".into()));
    let nested_record = nested[2].as_record().expect("nested record");
    assert_eq!(nested_record["ok"], Value::Bool(true));
    assert_eq!(nested_record["total"], Value::Number(3.0));
}

#[test]
fn aggregate_await_effectful_non_tool_leaf_keeps_existing_path() {
    let compiled = compile_source(
        r#"
        process child() { finish "done" }
        result = await {
          child: start child(),
          tool: tools.echo({ value: "x" })?
        }
        finish result
        "#,
    )
    .expect("program should compile");
    let listing = compiled_instruction_listing(&compiled);
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceOperationBatch(_))),
        "effectful non-tool leaves should keep the existing await path:\n{listing}"
    );
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::StartProcess { .. })),
        "test should cover a non-tool effect leaf:\n{listing}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_evaluates_arguments_once_in_source_order_before_batch() {
    #[derive(Default)]
    struct OrderHost {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl OrderHost {
        fn echo_value(operation: &ResourceOperation) -> Value {
            operation
                .args
                .first()
                .and_then(Value::as_record)
                .and_then(|record| record.get("value"))
                .cloned()
                .unwrap_or(Value::Null)
        }
    }

    impl ExecutionHost for OrderHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    let value = Self::echo_value(&operation);
                    self.events.lock_recover().push(format!("single:{value}"));
                    Ok(AbilityResult::Value(value))
                }
                AbilityOp::ResourceOperationBatch(batch) => {
                    let values = batch
                        .operations
                        .iter()
                        .map(Self::echo_value)
                        .collect::<Vec<_>>();
                    self.events.lock_recover().push(format!(
                        "batch:{}",
                        values
                            .iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    ));
                    Ok(AbilityResult::ResourceOperationBatch(
                        ResourceOperationBatchResult::settled_in_input_order(values
                                .into_iter()
                                .map(ResourceOperationResult::Value)
                                .collect()),
                    ))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    let host = OrderHost::default();
    let program = crate::parse(
        r#"
        result = await {
          first: tools.echo({ value: tools.echo({ value: "arg-a" })? })?,
          second: tools.echo({ value: tools.echo({ value: "arg-b" })? })?
        }
        finish result
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("program should run");
    assert!(matches!(outcome, ExecutionOutcome::Finished(_)));
    assert_eq!(
        host.events.lock_recover().as_slice(),
        ["single:arg-a", "single:arg-b", "batch:arg-a,arg-b"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_leaf_unwrap_waits_for_all_siblings_then_reports_first_error() {
    struct CountingBatchHost {
        batch_len: std::sync::atomic::AtomicUsize,
    }

    impl ExecutionHost for CountingBatchHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperationBatch(batch) => {
                    self.batch_len
                        .store(batch.operations.len(), std::sync::atomic::Ordering::SeqCst);
                    Ok(AbilityResult::ResourceOperationBatch(
                        ResourceOperationBatchResult::settled_in_input_order(batch
                                .operations
                                .into_iter()
                                .map(|operation| {
                                    if operation.operation == "err" {
                                        ResourceOperationResult::Error(ExecutionHostError::new(
                                            "boom",
                                        ))
                                    } else {
                                        ResourceOperationResult::Value(Value::String("ok".into()))
                                    }
                                })
                                .collect()),
                    ))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unexpected non-batch host ability")),
            }
        }
    }

    let host = CountingBatchHost {
        batch_len: std::sync::atomic::AtomicUsize::new(0),
    };
    let program = crate::parse(
        r#"
        result = await {
          bad: tools.err()?,
          good: tools.echo({ value: "ok" })?
        }
        finish result
        "#,
    )
    .expect("program should parse");
    let mut state = State::new();
    let err = execute_program(&program, &mut state, &host)
        .await
        .expect_err("program should fail after batch completes");
    assert_eq!(host.batch_len.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        err.to_string()
            .contains("`?` unwrapped failed module operation: boom"),
        "{err}"
    );
}

#[test]
fn labeled_process_resource_operation_site_correlates_to_workflow_node() {
    let source = r#"
        process search_test() {
          @label(title: "Spawn subagent with web search")
          result = await tools.echo({ value: { ok: true } })?
          wake result
          finish result
        }
        "#;
    let program = crate::parse(source).expect("program should parse");
    let surface = runtime_test_environment().with_language_features(
        crate::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    let compiled =
        crate::compile_linked_process(&linked, "search_test").expect("process should compile");
    let resource_call = compiled
        .chunk
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::ResourceCallUnwrap { .. }))
        .expect("resource call unwrap instruction");
    let site = compiled
        .chunk
        .lashlang_execution_sites
        .get(resource_call)
        .and_then(Option::as_ref)
        .expect("resource call execution site");

    let graph = crate::workflow_graph_from_source(source).expect("workflow graph");
    let graph_node_id = crate::node_id_for_execution_site(&graph, site)
        .expect("runtime site should correlate to a workflow node");
    let graph_node = graph
        .nodes()
        .find(|node| node.id == graph_node_id)
        .expect("correlated graph node");

    assert_eq!(site.node_kind, "resource_operation");
    assert_eq!(graph_node.name, "Spawn subagent with web search");
    assert!(
        !compiled
            .chunk
            .lashlang_execution_sites
            .iter()
            .flatten()
            .any(|site| {
                site.node_kind == "step" && site.label == "Spawn subagent with web search"
            }),
        "labeled resource operation should not emit a parallel step site"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn real_runs_correlate_every_execution_site_to_the_selected_workflow_path() {
    #[derive(Default)]
    struct CorrelationHost {
        observations: Mutex<Vec<crate::LashlangExecutionObservation>>,
    }

    impl ExecutionHost for CorrelationHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            Host.perform(op).await
        }

        fn observe_lashlang_execution(&self, observation: crate::LashlangExecutionObservation) {
            self.observations.lock_recover().push(observation);
        }
    }

    let source = r#"
        @label(title: "First call")
        first = await tools.echo({ value: "first" })?
        if true {
          @label(title: "Selected call")
          selected = await tools.echo({ value: first })?
        } else {
          @label(title: "Skipped call")
          selected = await tools.echo({ value: "skipped" })?
        }
        @label(title: "Finish selected")
        finish selected
        "#;
    let graph = crate::workflow_graph_from_source(source).expect("workflow graph");
    let compiled = compile_labeled_source(source);

    let mut invocation_paths = Vec::new();
    for _ in 0..2 {
        let host = CorrelationHost::default();
        let outcome = execute_compiled(&compiled, &mut State::new(), &host)
            .await
            .expect("workflow invocation should run");
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(Value::String("first".into()))
        );

        let observations = host.observations.into_inner().recover();
        let correlated = observations
            .iter()
            .map(|observation| {
                let (site, occurrence) = match observation {
                    crate::LashlangExecutionObservation::NodeStarted { site, occurrence }
                    | crate::LashlangExecutionObservation::NodeCompleted { site, occurrence }
                    | crate::LashlangExecutionObservation::NodeFailed {
                        site, occurrence, ..
                    }
                    | crate::LashlangExecutionObservation::BranchSelected {
                        site,
                        occurrence,
                        ..
                    }
                    | crate::LashlangExecutionObservation::ChildStarted {
                        site, occurrence, ..
                    } => (site, *occurrence),
                };
                let node_id = crate::node_id_for_execution_site(&graph, site)
                    .expect("every observed runtime site should resolve to the workflow graph");
                assert!(
                    graph.nodes().any(|node| node.id == node_id),
                    "correlated node id must belong to the projected graph"
                );
                (observation, node_id, occurrence)
            })
            .collect::<Vec<_>>();

        let selected_path = correlated
            .iter()
            .filter(|(observation, _, _)| {
                matches!(
                    observation,
                    crate::LashlangExecutionObservation::NodeStarted { .. }
                        | crate::LashlangExecutionObservation::BranchSelected { .. }
                )
            })
            .map(|(_, node_id, occurrence)| {
                let node = graph
                    .nodes()
                    .find(|node| node.id == *node_id)
                    .expect("correlated graph node");
                (node.name.clone(), *occurrence)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            selected_path,
            vec![
                ("First call".to_string(), 1),
                ("if".to_string(), 1),
                ("Selected call".to_string(), 1),
                ("Finish selected".to_string(), 1),
            ],
            "correlated nodes should follow only the executed branch"
        );
        invocation_paths.push(selected_path);
    }

    assert_eq!(
        invocation_paths[0], invocation_paths[1],
        "each invocation must start with an independent correlation sequence"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn generic_iterator_loops_cover_range_list_keys_nested_control_and_mutation() {
    let source = r#"
        counts = {}
        items = ["a", "b", "a"]
        seen = []
        total = 0
        for i in range(0, 5) {
          if i == 1 {
            continue
          }
          if i == 4 {
            break
          }
          total = total + i
        }
        for item in items {
          counts[item] = counts[item] + 1
          seen = push(seen, format("{}:{}", item, counts[item]))
        }
        pairs = []
        for key in keys(counts) {
          for n in range(0, counts[key]) {
            pairs = pairs + [format("{}{}", key, n)]
          }
        }
        finish { total: total, counts: counts, seen: seen, pairs: pairs }
    "#;
    let compiled = compile_source(source).expect("program should compile");
    let begin_iterators = compiled
        .chunk
        .code
        .iter()
        .filter(|instruction| {
            matches!(
                instruction,
                Instruction::BeginIter(_) | Instruction::BeginRangeIter { .. }
            )
        })
        .count();
    assert!(
        begin_iterators >= 4,
        "every `for` loop should compile to the generic iterator bytecode, got {begin_iterators}"
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &Host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(Value::Record(record)) = outcome else {
        panic!("expected record result");
    };
    assert_eq!(record["total"], Value::Number(5.0));
    let counts = record["counts"].as_record().expect("counts record");
    assert_eq!(counts["a"], Value::Number(2.0));
    assert_eq!(counts["b"], Value::Number(1.0));
    let Value::List(seen) = &record["seen"] else {
        panic!("seen should be a list");
    };
    assert_eq!(seen.len(), 3);
    let Value::List(pairs) = &record["pairs"] else {
        panic!("pairs should be a list");
    };
    assert_eq!(pairs.len(), 3);
}

#[test]
fn list_comprehension_compiles_to_iterator_and_append_bytecode() {
    let compiled = compile_source(
        r#"
        finish [n * 2 for n in range(0, 4) if n % 2 == 0]
        "#,
    )
    .expect("program should compile");
    let listing = compiled_instruction_listing(&compiled);

    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::BeginRangeIter { .. })),
        "range comprehension should use iterator bytecode:\n{listing}"
    );
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ListAppend)),
        "comprehension should append into the result list directly:\n{listing}"
    );
}

#[test]
fn every_container_insertion_lowering_emits_value_isolation() {
    let compiled = compile_source(
        r#"
        source = []
        target = {}
        target.plain = source
        target.effect = await tools.echo({ value: source })?
        copied = [item for item in [source]]
        acc = []
        acc = push(acc, source)
        finish copied
        "#,
    )
    .expect("container insertion program should compile");
    let code = &compiled.chunk.code;
    let listing = compiled_instruction_listing(&compiled);

    let isolated_paths = code
        .windows(2)
        .filter(|pair| {
            matches!(
                pair,
                [Instruction::DeepCopy, Instruction::PathAssign { .. }]
            )
        })
        .count();
    assert_eq!(
        isolated_paths, 2,
        "plain and forced-effect paths must both copy:\n{listing}"
    );
    assert!(
        code.windows(2)
            .any(|pair| matches!(pair, [Instruction::DeepCopy, Instruction::ListAppend])),
        "comprehension accumulation must copy:\n{listing}"
    );
    assert!(
        code.windows(2).any(|pair| matches!(
            pair,
            [
                Instruction::DeepCopy,
                Instruction::Intrinsic(IntrinsicOp::PushAssign(_))
            ]
        )),
        "push insertion must copy:\n{listing}"
    );
    assert!(
        code.windows(2).any(|pair| matches!(
            pair,
            [
                Instruction::IterNext { .. },
                Instruction::DeepCopyLoopBinding(_)
            ]
        )),
        "iterator bindings must copy:\n{listing}"
    );
}

#[test]
fn effectful_loop_bodies_compile_to_generic_iterator_bytecode() {
    let program = crate::parse(
        r#"
        items = [1, 2]
        for item in items {
          print item
        }
        finish null
        "#,
    )
    .expect("program should parse");
    let compiled = compile_program(&program);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::BeginIter(_))),
        "effectful loops should use the generic iterator bytecode"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn constant_propagation_does_not_cross_control_flow_boundaries() {
    let value = exec(
        r#"
        x = 1
        if false {
          x = 2
        }
        y = x + 1
        finish y
        "#,
    )
    .await
    .expect("program should succeed");

    assert_eq!(value, Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn reusable_execution_scratch_preserves_results_across_runs() {
    let program = crate::parse(
        r#"
        items = [1, 2, 3]
        total = 0
        for item in items {
          total = total + item
        }
        finish total
        "#,
    )
    .expect("program should parse");
    let compiled = compile_program(&program);
    let mut scratch = ExecutionScratch::new();

    for _ in 0..3 {
        let mut state = State::new();
        let outcome = execute_compiled_with_scratch(&compiled, &mut state, &Host, &mut scratch)
            .await
            .expect("program should run");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(6.0)));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn continuation_and_undefined_variable_are_reported() {
    let outcome = exec_outcome("x = 1")
        .await
        .expect("missing finish should continue");
    assert_eq!(outcome, ExecutionOutcome::Continued);

    let err = crate::parse("finish").expect_err("bare finish should fail");
    assert!(matches!(err, crate::ParseError::MissingFinishValue { .. }));

    let err = exec("finish x")
        .await
        .expect_err("undefined variable should fail");
    assert_eq!(
        err,
        RuntimeError::UndefinedVariable {
            name: "x".to_string()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn condition_and_iteration_errors_are_reported() {
    let value = exec("if 1 { finish 1 } else { finish 2 }")
        .await
        .expect("numeric truthiness should be accepted");
    assert_eq!(value, Value::Number(1.0));

    let value = exec("if \"\" { finish 1 } else { finish 2 }")
        .await
        .expect("empty string should be falsy");
    assert_eq!(value, Value::Number(2.0));

    let err = exec("for x in 1 { finish x }")
        .await
        .expect_err("non-list iteration should fail");
    assert_eq!(err, RuntimeError::NonListIteration);
}

#[tokio::test(flavor = "current_thread")]
async fn stmt_call_and_tool_results_cover_success_and_error() {
    exec("await tools.echo({ value: 1 }) finish 1")
        .await
        .expect("statement module operation should succeed");
    let missing = exec("bad = await tools.missing({}) finish bad")
        .await
        .expect("missing module operation should be wrapped");
    assert_eq!(
        missing.as_record().expect("result should be a record")["ok"],
        Value::Bool(false)
    );

    let value = exec(
        "ok = await tools.echo({ value: 7 }) bad = await tools.err({}) finish { ok: ok, bad: bad }",
    )
    .await
    .expect("module operation program should succeed");
    let record = value.as_record().expect("expected record");
    assert_eq!(record["ok"].as_record().unwrap()["ok"], Value::Bool(true));
    assert_eq!(record["bad"].as_record().unwrap()["ok"], Value::Bool(false));
}

#[tokio::test(flavor = "current_thread")]
async fn result_unwrap_extracts_success_and_preserves_manual_handling() {
    let value = exec("finish (await tools.echo({ value: 7 })?)")
        .await
        .expect("unwrap should succeed");
    assert_eq!(value, Value::Number(7.0));

    let value = exec(
        r#"
        result = await tools.err({})
        finish result.ok ? result.error : "unexpected"
        "#,
    )
    .await
    .expect("manual wrapper handling should still work");
    assert_eq!(value, Value::String("unexpected".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn unparenthesized_await_module_operation_unwrap_skips_handle_await() {
    let compiled = compile_source("value = await tools.echo({ value: 7 })?\nfinish value")
        .expect("program should compile");
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));
}

#[tokio::test(flavor = "current_thread")]
async fn parenthesized_await_module_operation_unwrap_skips_handle_await() {
    let compiled = compile_source("value = (await tools.echo({ value: 7 }))?\nfinish value")
        .expect("program should compile");
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_await_module_operation_unwrap_skips_handle_await() {
    let compiled = compile_labeled_source(
        r#"@label(title: "Echo")
value = await tools.echo({ value: { answer: "ok" } })?
finish value"#,
    );
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finished outcome");
    };
    let record = value
        .as_record()
        .expect("finished value should be a record");
    assert_eq!(record["answer"], Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_parenthesized_await_module_operation_unwrap_skips_handle_await() {
    let compiled = compile_labeled_source(
        r#"@label(title: "Echo")
value = (await tools.echo({ value: { answer: "ok" } }))?
finish value"#,
    );
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finished outcome");
    };
    let record = value
        .as_record()
        .expect("finished value should be a record");
    assert_eq!(record["answer"], Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_process_await_module_operation_unwrap_skips_handle_await() {
    let compiled = compile_labeled_process_source(
        r#"
process echo_from_process() {
  @label(title: "Echo")
  value = await tools.echo({ value: { answer: "ok" } })?
  finish value
}
"#,
        "echo_from_process",
    );
    assert_resource_call_unwrap_without_handle_await(&compiled);

    let mut state = State::new();
    let outcome = execute_compiled_process(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("process should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finished outcome");
    };
    let record = value
        .as_record()
        .expect("finished value should be a record");
    assert_eq!(record["answer"], Value::String("ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn direct_module_operation_unwrap_skips_observable_wrapper() {
    let compiled =
        compile_source("finish (await tools.echo({ value: 7 })?)").expect("program should compile");
    assert_resource_call_unwrap_without_handle_await(&compiled);
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResultUnwrap))
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &RejectingAwaitHost)
        .await
        .expect("program should run");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));

    let err = exec("finish (await tools.err({})?)")
        .await
        .expect_err("failed unwrap should abort");
    assert_eq!(
        err,
        RuntimeError::UnwrappedModuleOperationFailed {
            source: ExecutionHostError::new("boom"),
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn result_unwrap_reports_failed_and_malformed_wrappers() {
    let err = exec("finish (await tools.err({})?)")
        .await
        .expect_err("failed module operation unwrap should abort");
    assert_eq!(
        err,
        RuntimeError::UnwrappedModuleOperationFailed {
            source: ExecutionHostError::new("boom"),
        }
    );

    let err = exec("finish 1?")
        .await
        .expect_err("non-wrapper should fail");
    assert_eq!(
        err,
        RuntimeError::ToolResultExpected {
            actual: "number".to_string(),
        }
    );

    let err = exec("finish { ok: true }?")
        .await
        .expect_err("missing value should fail");
    assert_eq!(err, RuntimeError::ToolResultMissingValue);
}

#[tokio::test(flavor = "current_thread")]
async fn field_index_unary_and_boolean_paths_are_covered() {
    let value = exec(
        r#"
        rec = { nested: { name: "lash" } }
        xs = ["a", "b"]
        ok = false and missing
        alt = true or missing
        finish [rec.nested.name, xs[1], "abc"[2], -1, not false, !false, ok, alt]
        "#,
    )
    .await
    .expect("program should succeed");

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("lash".to_string().into()),
                Value::String("b".to_string().into()),
                Value::String("c".to_string().into()),
                Value::Number(-1.0),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Bool(true),
            ]
            .into()
        )
    );

    let value = exec("finish true and false")
        .await
        .expect("and path should succeed");
    assert_eq!(value, Value::Bool(false));

    let value = exec("finish false or true")
        .await
        .expect("or path should succeed");
    assert_eq!(value, Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn field_index_and_type_errors_are_covered() {
    let err = exec("n = 1 finish n.name")
        .await
        .expect_err("field access should fail");
    assert!(matches!(err, RuntimeError::CannotReadField { .. }));

    let value = exec("rec = {} finish rec.name")
        .await
        .expect("missing field should yield null");
    assert_eq!(value, Value::Null);

    let err = exec("finish 1[0]")
        .await
        .expect_err("bad index target should fail");
    assert!(matches!(err, RuntimeError::CannotIndex { .. }));

    let value = exec("finish [1][2]")
        .await
        .expect("list oob should yield null");
    assert_eq!(value, Value::Null);

    let value = exec("finish \"a\"[2]")
        .await
        .expect("string oob should yield null");
    assert_eq!(value, Value::Null);

    let err = exec("finish [1][1.5]")
        .await
        .expect_err("fractional index should fail");
    assert!(matches!(err, RuntimeError::InvalidIndex));

    let value = exec("finish [1][-1]")
        .await
        .expect("negative index should resolve from the end");
    assert_eq!(value, Value::Number(1.0));

    let value = exec("finish not 1")
        .await
        .expect("not should use truthiness");
    assert_eq!(value, Value::Bool(false));

    let value = exec("finish not 0").await.expect("zero should be falsy");
    assert_eq!(value, Value::Bool(true));

    let value = exec("rec = { ok: false } finish len(rec.value.items)")
        .await
        .expect("null chain should work");
    assert_eq!(value, Value::Number(0.0));
}

#[tokio::test(flavor = "current_thread")]
async fn arithmetic_and_compare_errors_are_covered() {
    assert_eq!(
        exec("finish 7 - 2").await.expect("subtract should succeed"),
        Value::Number(5.0)
    );
    assert_eq!(
        exec("finish 3 * 4").await.expect("multiply should succeed"),
        Value::Number(12.0)
    );
    assert_eq!(
        exec("finish 8 / 2").await.expect("divide should succeed"),
        Value::Number(4.0)
    );
    assert_eq!(
        exec("finish 8 % 3").await.expect("modulo should succeed"),
        Value::Number(2.0)
    );
    assert_eq!(
        exec("finish 1 != 2")
            .await
            .expect("not equal should succeed"),
        Value::Bool(true)
    );
    assert_eq!(
        exec("finish 1 <= 2")
            .await
            .expect("less-equal should succeed"),
        Value::Bool(true)
    );
    assert_eq!(
        exec("finish 2 > 1").await.expect("greater should succeed"),
        Value::Bool(true)
    );
    assert_eq!(
        exec("finish 2 >= 1")
            .await
            .expect("greater-equal should succeed"),
        Value::Bool(true)
    );

    let value = exec("finish [1,2] + [3]")
        .await
        .expect("list concat should succeed");
    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );

    let value = exec("finish \"a\" + \"b\"")
        .await
        .expect("string add should succeed");
    assert_eq!(value, Value::String("ab".to_string().into()));

    let value = exec("finish \"a\" + 1")
        .await
        .expect("string coercion should succeed");
    assert_eq!(value, Value::String("a1".to_string().into()));

    let value = exec("finish 1 + \"b\"")
        .await
        .expect("string coercion should succeed");
    assert_eq!(value, Value::String("1b".to_string().into()));

    let value = exec("finish 1 + true")
        .await
        .expect("bool should coerce for addition");
    assert_eq!(value, Value::Number(2.0));

    let value = exec("finish null + 2")
        .await
        .expect("null should coerce for addition");
    assert_eq!(value, Value::Number(2.0));

    let value = exec("finish \"2\" * 3")
        .await
        .expect("numeric strings should coerce");
    assert_eq!(value, Value::Number(6.0));

    let value = exec("finish \"2\" < 10")
        .await
        .expect("numeric strings should compare");
    assert_eq!(value, Value::Bool(true));

    let err = exec("finish {} + 1")
        .await
        .expect_err("records should still fail arithmetic");
    assert!(matches!(err, RuntimeError::ExpectedNumberType { .. }));
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_success_matrix_is_covered() {
    let value = exec(
        r#"
        rec = { a: 1, b: 2 }
        base = [1, 2]
        finish {
          len_s: len("ab"),
          len_l: len([1,2,3]),
          len_r: len(rec),
          len_n: len(null),
          empty_n: empty(null),
          empty_s: empty(""),
          empty_l: empty([]),
          empty_r: empty({}),
          keys_n: keys(null),
          values_n: values(null),
          keys: keys(rec),
          values: values(rec),
          contains_s: contains("abc", "b"),
          contains_num: contains("123", 2),
          contains_l: contains([1,2,3], 2),
          contains_r: contains({ foo: 1, bar: 2 }, "foo"),
          contains_n: contains(null, 2),
          find_hit: find("alpha beta", "beta"),
          find_from: find("banana", "na", 3),
          find_missing: find("alpha", "z"),
          starts: starts_with("lash", "la"),
          starts_num: starts_with(123, 12),
          ends: ends_with("lash", "sh"),
          split: split(101, 0),
          join: join(["a",2,true], "-"),
          trim: trim(101),
          slice_s: slice("abcd", 1, 3),
          slice_end_s: slice("abcd", 2, null),
          slice_back_s: slice("abcd", 3, 1),
          slice_from_start_s: slice("abcd", null, 2),
          slice_l: slice([1,2,3,4], 1, 3),
          slice_end_l: slice([1,2,3,4], 2, null),
          slice_back_l: slice([1,2,3,4], 3, 1),
          slice_from_start_l: slice([1,2,3,4], null, 2),
          to_s: to_string({ a: 1 }),
          to_i_n: to_int(3.9),
          to_i_s: to_int("4"),
          to_i_b: to_int(true),
          to_f_n: to_float(1),
          to_f_s: to_float("2.5"),
          to_f_nl: to_float(null),
          fmt: format("x={},y={}", 1, true),
          range_end: range(3),
          range_pair: range(-2, 2),
          range_step: range(0, 5, 2),
          range_step_down: range(5, 0, -2),
          range_empty: range(2, 2),
          ceil_div: ceil_div(10, 3),
          floor_div: floor_div(-10, 3),
          pushed: push(base, 3),
          base_after_push: base,
          valid: validate(
            {
              name: "pkg",
              version: "1.0.0",
              deps: [{ name: "dep", optional: true }],
              extra: "preserved"
            },
            Type {
              name: str,
              version: str,
              deps: list[Type { name: str, optional: bool? }]
            }
          )
        }
        "#,
    )
    .await
    .expect("builtins should succeed");

    let record = value.as_record().expect("expected record");
    assert_eq!(record["len_s"], Value::Number(2.0));
    assert_eq!(record["len_n"], Value::Number(0.0));
    assert_eq!(record["contains_num"], Value::Bool(true));
    assert_eq!(record["contains_l"], Value::Bool(true));
    assert_eq!(record["contains_r"], Value::Bool(true));
    assert_eq!(record["contains_n"], Value::Bool(false));
    assert_eq!(record["find_hit"], Value::Number(6.0));
    assert_eq!(record["find_from"], Value::Number(4.0));
    assert_eq!(record["find_missing"], Value::Null);
    assert_eq!(record["keys_n"], Value::List(Vec::new().into()));
    assert_eq!(record["values_n"], Value::List(Vec::new().into()));
    assert_eq!(record["starts_num"], Value::Bool(true));
    assert_eq!(
        record["split"],
        Value::List(
            vec![
                Value::String("1".to_string().into()),
                Value::String("1".to_string().into())
            ]
            .into()
        )
    );
    assert_eq!(record["join"], Value::String("a-2-true".to_string().into()));
    assert_eq!(record["trim"], Value::String("101".to_string().into()));
    assert_eq!(record["slice_s"], Value::String("bc".to_string().into()));
    assert_eq!(
        record["slice_end_s"],
        Value::String("cd".to_string().into())
    );
    assert_eq!(record["slice_back_s"], Value::String(String::new().into()));
    assert_eq!(
        record["slice_from_start_s"],
        Value::String("ab".to_string().into())
    );
    assert_eq!(
        record["slice_end_l"],
        Value::List(vec![Value::Number(3.0), Value::Number(4.0)].into())
    );
    assert_eq!(record["slice_back_l"], Value::List(Vec::new().into()));
    assert_eq!(
        record["slice_from_start_l"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(record["to_i_n"], Value::Number(3.0));
    assert_eq!(record["to_i_b"], Value::Number(1.0));
    assert_eq!(record["to_f_s"], Value::Number(2.5));
    assert_eq!(record["to_f_nl"], Value::Number(0.0));
    assert_eq!(
        record["fmt"],
        Value::String("x=1,y=true".to_string().into())
    );
    assert_eq!(
        record["range_end"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        record["range_pair"],
        Value::List(
            vec![
                Value::Number(-2.0),
                Value::Number(-1.0),
                Value::Number(0.0),
                Value::Number(1.0)
            ]
            .into()
        )
    );
    assert_eq!(record["range_empty"], Value::List(Vec::new().into()));
    assert_eq!(
        record["range_step"],
        Value::List(vec![Value::Number(0.0), Value::Number(2.0), Value::Number(4.0)].into())
    );
    assert_eq!(
        record["range_step_down"],
        Value::List(vec![Value::Number(5.0), Value::Number(3.0), Value::Number(1.0)].into())
    );
    assert_eq!(record["ceil_div"], Value::Number(4.0));
    assert_eq!(record["floor_div"], Value::Number(-4.0));
    assert_eq!(
        record["pushed"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );
    assert_eq!(
        record["base_after_push"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
    let valid = record["valid"].as_record().expect("validated record");
    assert_eq!(valid["name"], Value::String("pkg".to_string().into()));
    assert_eq!(
        valid["extra"],
        Value::String("preserved".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn grep_text_returns_documented_line_records() {
    let value = exec(
        r#"
        matches = grep_text("alpha\nbeta match\r\ngamma match\n", "match")
        finish matches
        "#,
    )
    .await
    .expect("grep_text should succeed");

    let Value::List(matches) = value else {
        panic!("expected list");
    };
    assert_eq!(matches.len(), 2);
    let first = matches[0].as_record().expect("first match record");
    assert_eq!(first["line"], Value::Number(2.0));
    assert_eq!(first["text"], Value::String("beta match".into()));
    assert_eq!(first["match"], Value::String("match".into()));
    assert_eq!(first["start"], Value::Number(5.0));
    assert_eq!(first["end"], Value::Number(10.0));
}

#[tokio::test(flavor = "current_thread")]
async fn find_uses_character_offsets() {
    let value = exec(
        r#"
        finish {
          first: find("éclair café", "café"),
          from_end: find("abc", "", 3),
          beyond_end: find("abc", "a", 4)
        }
        "#,
    )
    .await
    .expect("find should succeed");

    let record = value.as_record().expect("record");
    assert_eq!(record["first"], Value::Number(7.0));
    assert_eq!(record["from_end"], Value::Number(3.0));
    assert_eq!(record["beyond_end"], Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_error_matrix_is_covered() {
    let cases = [
        ("finish len(true)", "len"),
        ("finish empty(true)", "empty"),
        ("finish keys([])", "keys"),
        ("finish values([])", "values"),
        ("finish contains(1, 2)", "contains"),
        ("finish find(\"a\")", "find"),
        ("finish find(\"a\", \"a\", -1)", "find"),
        ("finish grep_text({}, \"a\")", "grep_text"),
        ("finish grep_text(\"a\", \"\")", "grep_text"),
        ("finish starts_with({}, \"a\")", "starts_with"),
        ("finish ends_with({}, \"a\")", "ends_with"),
        ("finish split({}, \",\")", "split"),
        ("finish join(1, \",\")", "join"),
        ("finish trim({})", "trim"),
        ("finish slice(1, 0, 1)", "slice"),
        ("finish to_int({})", "to_int"),
        ("finish to_int(\"x\")", "to_int"),
        ("finish to_float({})", "to_float"),
        ("finish to_float(\"x\")", "to_float"),
        ("finish json_parse(\"{\")", "json_parse"),
        ("finish format()", "format"),
        ("finish format({})", "format"),
        ("finish format(\"{1}\", \"x\")", "format"),
        ("finish format(\"{}\", \"x\", \"y\")", "format"),
        ("finish format(\"{} {1}\", \"x\", \"y\")", "format"),
        ("finish format(\"{x}\")", "format"),
        ("finish format(\"{\")", "format"),
        ("finish format(\"}\")", "format"),
        (
            "finish validate({ name: \"pkg\" }, { type: \"object\" })",
            "validate",
        ),
        ("finish range()", "range"),
        ("finish range(1, 2, 3, 4)", "range"),
        ("finish range(\"3\")", "range"),
        ("finish range(1.5)", "range"),
        ("finish range(0, 5, 0)", "range"),
        ("finish range(0, 1000001)", "range"),
        ("finish range(1000001, 0, -1)", "range"),
        ("finish ceil_div()", "ceil_div"),
        ("finish ceil_div(1.5, 1)", "ceil_div"),
        ("finish ceil_div(1, 0)", "ceil_div"),
        ("finish floor_div(\"1\", 1)", "floor_div"),
        ("finish push(1, 2)", "push"),
        ("finish push([1])", "push"),
        ("finish no_such_builtin()", "no_such_builtin"),
    ];

    for (source, _) in cases {
        let err = exec(source).await.expect_err("builtin should fail");
        assert!(matches!(
            err,
            RuntimeError::InvalidArgumentCount { .. }
                | RuntimeError::EmptyUnsupported
                | RuntimeError::KeysUnsupported
                | RuntimeError::ValuesUnsupported
                | RuntimeError::LenUnsupported
                | RuntimeError::ContainsUnsupported
                | RuntimeError::InUnsupported
                | RuntimeError::InvalidCharacterIndex { .. }
                | RuntimeError::ExpectedText { .. }
                | RuntimeError::EmptyGrepNeedle
                | RuntimeError::JoinUnsupported
                | RuntimeError::SliceUnsupported
                | RuntimeError::ExpectedNumber
                | RuntimeError::ExpectedNumberType { .. }
                | RuntimeError::InvalidJson { .. }
                | RuntimeError::FormatTemplateMissing
                | RuntimeError::FormatTemplateInvalid { .. }
                | RuntimeError::Format(_)
                | RuntimeError::ValidateTypeLiteralRequired
                | RuntimeError::InvalidRangeBound
                | RuntimeError::InvalidRangeBoundType { .. }
                | RuntimeError::ZeroRangeStep
                | RuntimeError::RangeTooLarge { .. }
                | RuntimeError::InvalidIntegerDivisionArgument { .. }
                | RuntimeError::InvalidIntegerDivisionArgumentType { .. }
                | RuntimeError::IntegerDivisionByZero { .. }
                | RuntimeError::PushUnsupported
                | RuntimeError::UnknownBuiltin { .. }
        ));
    }

    let err = exec("finish len()")
        .await
        .expect_err("arity error should fail");
    assert!(matches!(err, RuntimeError::InvalidArgumentCount { .. }));
}

#[tokio::test(flavor = "current_thread")]
async fn validate_reports_precise_shape_errors() {
    let cases = [
        (
            "finish validate({ name: \"pkg\" }, Type { name: str, version: str })",
            "validation failed: $: missing required field `version`",
        ),
        (
            r#"finish validate({ packages: [{ name: "pkg", version: 1 }] }, Type { packages: list[Type { name: str, version: str }] })"#,
            "validation failed: $.packages[0].version: expected string, got number",
        ),
        (
            r#"finish validate({ status: "maybe" }, Type { status: enum["ok", "err"] })"#,
            "validation failed: $.status: expected one of [ok, err], got maybe",
        ),
        (
            r#"finish validate({ count: 1.5 }, Type { count: int })"#,
            "validation failed: $.count: expected integer, got number",
        ),
    ];

    for (source, expected) in cases {
        let err = exec(source)
            .await
            .expect_err("validate should reject bad value");
        assert_eq!(
            err,
            RuntimeError::ValidationFailed {
                reason: expected
                    .strip_prefix("validation failed: ")
                    .expect("validation prefix")
                    .to_string()
            }
        );
    }

    let err = exec("finish validate({ name: \"pkg\" }, { type: \"object\" })")
        .await
        .expect_err("raw schema records should be rejected");
    assert_eq!(err, RuntimeError::ValidateTypeLiteralRequired);
}

#[tokio::test(flavor = "current_thread")]
async fn validate_union_accepts_any_variant() {
    // `str | null` must accept both a string and a null.
    let out = exec(r#"finish validate({ email: "a@b" }, Type { email: str | null })"#)
        .await
        .expect("string-branch validate should succeed");
    assert_eq!(
        out,
        Value::Record(Arc::new({
            let mut rec = record_with_capacity(1);
            rec.insert("email".into(), Value::String("a@b".into()));
            rec
        }))
    );

    let out = exec(r#"finish validate({ email: null }, Type { email: str | null })"#)
        .await
        .expect("null-branch validate should succeed");
    let Value::Record(rec) = &out else {
        panic!("expected record");
    };
    assert!(matches!(rec.get("email"), Some(Value::Null)));
}

#[tokio::test(flavor = "current_thread")]
async fn validate_union_rejects_value_matching_no_variant() {
    let err = exec(r#"finish validate({ email: 42 }, Type { email: str | null })"#)
        .await
        .expect_err("number should not match str | null");
    let RuntimeError::ValidationFailed { reason } = err else {
        panic!("expected ValidationFailed");
    };
    assert!(
        reason.contains("$.email"),
        "error should point at the failing field: {reason}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn validate_static_and_dynamic_type_paths_share_error_text() {
    let static_err = exec(r#"finish validate({ email: 42 }, Type { email: str | null })"#)
        .await
        .expect_err("static Type literal should reject number");
    let dynamic_err = exec(
        r#"
Schema = await tools.echo({ value: Type { email: str | null } })?
finish validate({ email: 42 }, Schema)
"#,
    )
    .await
    .expect_err("runtime Type value should reject number");

    assert_eq!(static_err, dynamic_err);
    assert_eq!(
        dynamic_err,
        RuntimeError::ValidationFailed {
            reason: "$.email: expected one of [string, null], got number".to_string()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn validate_object_type_accepts_image_descriptors() {
    let program = crate::parse(
        r#"
finish validate(img, Type {
  type: str,
  id: str,
  label: str,
  size: int,
  width: int | null,
  height: int | null
})
"#,
    )
    .expect("program should parse");
    let mut state = State::new();
    state.globals.insert_str(
        "img",
        Value::Image(Box::new(ImageValue::new(
            "img-1",
            crate::MediaType::parse("image/png").unwrap(),
            "chart.png",
            1234,
            Some(640),
            None,
        ))),
    );

    let outcome = execute_program(&program, &mut state, &Host)
        .await
        .expect("image descriptor validation should succeed");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::Image(Box::new(ImageValue::new(
            "img-1",
            crate::MediaType::parse("image/png").unwrap(),
            "chart.png",
            1234,
            Some(640),
            None
        ))))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn helper_functions_are_covered_directly() {
    assert!(expect_arg_count("x", &[Value::Null], 1).is_ok());
    assert!(expect_arg_count("x", &[], 1).is_err());
    assert_eq!(as_number(&Value::Number(1.0)).expect("number"), 1.0);
    assert_eq!(as_number(&Value::Bool(true)).expect("bool"), 1.0);
    assert_eq!(as_number(&Value::Null).expect("null"), 0.0);
    assert_eq!(
        as_number(&Value::String("2.5".to_string().into())).expect("numeric"),
        2.5
    );
    assert_eq!(
        coerce_string(&Value::String("x".to_string().into())).expect("string"),
        "x"
    );
    assert_eq!(coerce_string(&Value::Bool(true)).expect("bool"), "true");
    assert_eq!(as_offset(&Value::Number(-1.0)).expect("offset"), -1);
    assert_eq!(as_slice_bound(&Value::Null).expect("null bound"), None);
    assert_eq!(
        as_slice_bound(&Value::Number(2.0)).expect("numeric bound"),
        Some(2)
    );
    assert_eq!(
        as_slice_bound(&Value::Number(-2.0)).expect("negative numeric bound"),
        Some(-2)
    );
    assert_eq!(slice_string("héllo", Some(1), Some(4)), "éll");
    assert_eq!(slice_string("abcdef", Some(-2), None), "ef");
    assert_eq!(slice_string("abcdef", None, Some(-1)), "abcde");
    assert_eq!(slice_string("abcdef", Some(-5), Some(-2)), "bcd");
    assert_eq!(slice_string("abc", Some(1), None), "bc");
    assert_eq!(slice_string("abc", Some(3), Some(1)), "");
    assert_eq!(clamp_slice_bounds(Some(1), Some(3), 4), Some((1, 3)));
    assert_eq!(clamp_slice_bounds(Some(1), None, 4), Some((1, 4)));
    assert_eq!(clamp_slice_bounds(None, Some(2), 4), Some((0, 2)));
    assert_eq!(clamp_slice_bounds(Some(-2), None, 4), Some((2, 4)));
    assert_eq!(clamp_slice_bounds(None, Some(-1), 4), Some((0, 3)));
    assert_eq!(clamp_slice_bounds(Some(-10), Some(10), 4), Some((0, 4)));
    assert_eq!(clamp_slice_bounds(Some(3), Some(1), 4), None);
    assert_eq!(
        resolve_index(&Value::Number(-1.0), 3).expect("resolved"),
        Some(2)
    );
    assert_eq!(
        resolve_index(&Value::Number(-4.0), 3).expect("resolved"),
        None
    );

    assert_eq!(
        compare_numbers(Value::Number(1.0), Value::Number(2.0), |a, b| a < b).expect("compare"),
        Value::Bool(true)
    );
    assert_eq!(
        compare_ordered(
            Value::String("abc".to_string().into()),
            Value::String("def".to_string().into()),
            |a, b| a < b,
            |a, b| a < b,
        )
        .expect("string compare"),
        Value::Bool(true)
    );
    assert_eq!(
        add_values(Value::Number(1.0), Value::Number(2.0)).expect("add"),
        Value::Number(3.0)
    );
    assert_eq!(
        add_values(Value::String("a".to_string().into()), Value::Bool(true)).expect("concat"),
        Value::String("atrue".to_string().into())
    );
    assert_eq!(
        add_values(Value::Bool(true), Value::Number(2.0)).expect("numeric coercion"),
        Value::Number(3.0)
    );
    assert_eq!(
        success(Value::Number(1.0)).as_record().unwrap()["ok"],
        Value::Bool(true)
    );
    assert_eq!(
        error_value("x".to_string()).as_record().unwrap()["error"],
        Value::String("x".to_string().into())
    );
    assert_eq!(stringify_value(&Value::Null).expect("stringify"), "null");
    assert_eq!(
        stringify_value(&Value::Number(1.0)).expect("stringify"),
        "1"
    );
    assert_eq!(
        stringify_value(&Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
        .expect("stringify"),
        "[1,2]"
    );
    assert_eq!(
        stringify_value(&Value::Tuple(
            vec![Value::Number(1.0), Value::String("x".into())].into()
        ))
        .expect("stringify"),
        r#"(1, "x")"#
    );
    assert_eq!(
        stringify_value(&Value::Tuple(vec![Value::Number(1.0)].into())).expect("stringify"),
        "(1,)"
    );
    assert_eq!(
        stringify_value(&Value::Tuple(Vec::new().into())).expect("stringify"),
        "()"
    );
    assert_eq!(
        add_values(
            Value::Tuple(vec![Value::Number(1.0)].into()),
            Value::Tuple(vec![Value::Number(2.0)].into())
        )
        .expect("tuple concat"),
        Value::Tuple(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
    let mut appended = String::from("prefix:");
    append_stringified_value(&mut appended, &Value::Bool(true)).expect("append stringify");
    assert_eq!(appended, "prefix:true");
    assert_eq!(
        apply_format("a{}b", &[Value::Number(1.0)]).expect("format"),
        "a1b"
    );
    let compiled_one_arg = compile_format_template("a{}b", 1);
    let one_arg = compiled_one_arg
        .one_arg
        .as_ref()
        .expect("single placeholder template should keep its direct shape");
    assert_eq!(one_arg.prefix.as_deref(), Some("a"));
    assert_eq!(one_arg.suffix.as_deref(), Some("b"));
    assert_eq!(
        execute_compiled_format_one_number_compact_direct(&compiled_one_arg, 42.0)
            .expect("compiled one-number format")
            .as_str(),
        "a42b"
    );
    assert!(
        compile_format_template("{}:{}", 2).one_arg.is_none(),
        "multi-arg templates should keep the generic compiled format path"
    );
    assert_eq!(
        apply_format(
            "b={1} a={0}",
            &[Value::String("x".into()), Value::String("y".into())]
        )
        .expect("indexed format"),
        "b=y a=x"
    );
    assert_eq!(
        apply_format("{{{}}}", &[Value::Number(1.0)]).expect("escaped braces"),
        "{1}"
    );
    assert_eq!(
        apply_format("{999999999999999999999999999999999999}", &[])
            .expect_err("overflow slot should fail"),
        RuntimeError::Format(FormatError::InvalidSlot {
            slot: "999999999999999999999999999999999999".to_string()
        })
    );
    assert_eq!(
        apply_format("{x}", &[]).expect_err("invalid placeholder should fail"),
        RuntimeError::Format(FormatError::InvalidPlaceholder)
    );
    assert_eq!(
        apply_format(
            "{} {1}",
            &[Value::String("x".into()), Value::String("y".into())]
        )
        .expect_err("mixed placeholder styles should fail"),
        RuntimeError::Format(FormatError::MixedPlaceholderKinds)
    );
    assert_eq!(
        apply_format("{", &[]).expect_err("unmatched open brace should fail"),
        RuntimeError::Format(FormatError::UnmatchedOpenBrace)
    );
    assert_eq!(
        apply_format("}", &[]).expect_err("unmatched close brace should fail"),
        RuntimeError::Format(FormatError::UnmatchedCloseBrace)
    );
    assert_eq!(
        apply_format("plain", &[Value::Number(1.0)]).expect_err("unused arg should fail"),
        RuntimeError::Format(FormatError::UnusedArgument { index: 0 })
    );
    assert_eq!(
        value_type_name(&Value::Record(Record::default().into())),
        "record"
    );
    assert_eq!(value_type_name(&Value::Tuple(Vec::new().into())), "tuple");
    assert_eq!(
        RuntimeError::UndefinedVariable {
            name: "x".to_string()
        }
        .to_string(),
        "unknown name `x`"
    );
    assert_eq!(
        RuntimeError::CannotIndex {
            actual: "record".to_string()
        }
        .to_string(),
        "can't index record"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn json_helpers_cover_special_paths() {
    let json = to_json(&Value::Number(f64::NAN));
    assert_eq!(json, serde_json::Value::Null);
    assert_eq!(to_json(&Value::Null), serde_json::Value::Null);
    assert_eq!(to_json(&Value::Bool(true)), serde_json::Value::Bool(true));
    assert_eq!(
        to_json(&Value::String("x".to_string().into())),
        serde_json::Value::String("x".to_string())
    );
    assert_eq!(
        to_json(&Value::List(vec![Value::Number(1.0)].into())),
        serde_json::json!([1])
    );
    assert_eq!(
        to_json(&Value::Tuple(vec![Value::Number(1.0)].into())),
        serde_json::json!([1])
    );
    assert_eq!(
        to_json(&Value::Record({
            let mut record = Record::default();
            record.insert("a".to_string(), Value::Number(1.0));
            record.into()
        })),
        serde_json::json!({"a": 1})
    );

    let value = from_json(serde_json::json!({
        "a": [1, true, null, "x"]
    }));
    let record = value.as_record().expect("expected record");
    assert!(matches!(record["a"], Value::List(_)));
}
