use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ResourceOperationBatchResult, ResourceOperationResult, State, Value,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn web_environment() -> lashlang::LashlangHostEnvironment {
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

fn execute(
    source: &str,
    host: &impl ExecutionHost,
) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let linked = lash_typescript::link(source, &web_environment()).expect("source should link");
    futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        host,
    ))
}

#[derive(Default)]
struct RecordingBatchHost {
    batches: AtomicUsize,
    individual_calls: AtomicUsize,
    batch_sizes: Mutex<Vec<usize>>,
    results: Mutex<Option<Vec<ResourceOperationResult>>>,
    settlement: Mutex<Option<Vec<usize>>>,
}

impl RecordingBatchHost {
    fn successful(count: usize) -> Self {
        Self {
            results: Mutex::new(Some(
                (0..count)
                    .map(|index| ResourceOperationResult::Value(Value::Number(index as f64)))
                    .collect(),
            )),
            settlement: Mutex::new(Some((0..count).collect())),
            ..Self::default()
        }
    }
}

impl ExecutionHost for RecordingBatchHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => {
                self.batches.fetch_add(1, Ordering::SeqCst);
                self.batch_sizes
                    .lock()
                    .expect("batch sizes lock")
                    .push(batch.operations.len());
                let results = self
                    .results
                    .lock()
                    .expect("results lock")
                    .take()
                    .expect("one configured batch");
                let settlement = self
                    .settlement
                    .lock()
                    .expect("settlement lock")
                    .take()
                    .expect("one configured settlement order");
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult::settled_in_order(results, settlement),
                ))
            }
            AbilityOp::ResourceOperation(_) => {
                self.individual_calls.fetch_add(1, Ordering::SeqCst);
                Err(ExecutionHostError::new("expected one aggregate batch"))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected promise-map ability")),
        }
    }
}

#[test]
fn sync_map_tool_calls_fan_out_in_one_runtime_sized_batch() {
    let host = RecordingBatchHost::successful(3);
    let outcome = execute(
        "const ids = ['a', 'b', 'c']; finish(await Promise.all(ids.map(id => web.fetch({ url: id }))));",
        &host,
    )
    .expect("sync map aggregate should execute");

    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into()
        ))
    );
    assert_eq!(host.batches.load(Ordering::SeqCst), 1);
    assert_eq!(*host.batch_sizes.lock().unwrap(), [3]);
    assert_eq!(host.individual_calls.load(Ordering::SeqCst), 0);

    let block_host = RecordingBatchHost::successful(2);
    execute(
        "finish(await Promise.all(['a', 'b'].map(id => { return web.fetch({ url: id }); })));",
        &block_host,
    )
    .expect("a block with one returned tool call should use the same batch");
    assert_eq!(block_host.batches.load(Ordering::SeqCst), 1);
    assert_eq!(*block_host.batch_sizes.lock().unwrap(), [2]);
}

#[test]
fn async_map_leading_tool_await_fans_out_before_projection() {
    let host = RecordingBatchHost {
        results: Mutex::new(Some(vec![
            ResourceOperationResult::Value(lashlang::from_json(
                serde_json::json!({ "name": "Ada" }),
            )),
            ResourceOperationResult::Value(lashlang::from_json(
                serde_json::json!({ "name": "Grace" }),
            )),
        ])),
        settlement: Mutex::new(Some(vec![1, 0])),
        ..RecordingBatchHost::default()
    };
    let outcome = execute(
        "const ids = ['a', 'b']; finish(await Promise.all(ids.map(async id => ({ id, name: (await web.fetch({ url: id })).name }))));",
        &host,
    )
    .expect("leading awaits should fan out before callback projections");

    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            { "id": "a", "name": "Ada" },
            { "id": "b", "name": "Grace" }
        ])))
    );
    assert_eq!(host.batches.load(Ordering::SeqCst), 1);
    assert_eq!(*host.batch_sizes.lock().unwrap(), [2]);
    assert_eq!(host.individual_calls.load(Ordering::SeqCst), 0);

    let block_host = RecordingBatchHost {
        results: Mutex::new(Some(vec![
            ResourceOperationResult::Value(lashlang::from_json(
                serde_json::json!({ "name": "Ada" }),
            )),
            ResourceOperationResult::Value(lashlang::from_json(
                serde_json::json!({ "name": "Grace" }),
            )),
        ])),
        settlement: Mutex::new(Some(vec![0, 1])),
        ..RecordingBatchHost::default()
    };
    let block_outcome = execute(
        "finish(await Promise.all(['a', 'b'].map(async id => { const r = await web.fetch({ url: id }); return r.name; })));",
        &block_host,
    )
    .expect("a leading awaited binding should batch before its return projection");
    assert_eq!(
        block_outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::String("Ada".into()), Value::String("Grace".into())].into()
        ))
    );
    assert_eq!(block_host.batches.load(Ordering::SeqCst), 1);
}

#[test]
fn identifier_bound_tool_map_is_already_settled_for_both_aggregates() {
    #[derive(Default)]
    struct SequentialHost {
        calls: AtomicUsize,
    }
    impl ExecutionHost for SequentialHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(_) => Ok(AbilityResult::Value(Value::Number(
                    self.calls.fetch_add(1, Ordering::SeqCst) as f64,
                ))),
                AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
                _ => Err(ExecutionHostError::new("identifier maps stay sequential")),
            }
        }
    }

    let host = SequentialHost::default();
    let outcome = execute(
        "const ids = ['a', 'b']; const ps = ids.map(id => web.fetch({ url: id })); finish([await Promise.all(ps), await Promise.allSettled(ps)]);",
        &host,
    )
    .expect("identifier-bound arrays should be accepted as resolved values");
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            [0, 1],
            [
                { "status": "fulfilled", "value": 0 },
                { "status": "fulfilled", "value": 1 }
            ]
        ])))
    );
}

#[test]
fn all_settled_sync_map_keeps_one_rejection_in_input_order() {
    let host = RecordingBatchHost {
        results: Mutex::new(Some(vec![
            ResourceOperationResult::Value(Value::String("first".into())),
            ResourceOperationResult::Error(ExecutionHostError::new("middle failed")),
            ResourceOperationResult::Value(Value::String("last".into())),
        ])),
        settlement: Mutex::new(Some(vec![2, 1, 0])),
        ..RecordingBatchHost::default()
    };
    let outcome = execute(
        "finish(await Promise.allSettled(['a', 'b', 'c'].map(id => web.fetch({ url: id }))));",
        &host,
    )
    .expect("allSettled should retain a rejected mapped leaf");
    assert_eq!(host.batches.load(Ordering::SeqCst), 1);
    let rendered = format!("{outcome:?}");
    assert!(rendered.contains("first"), "{rendered}");
    assert!(rendered.contains("middle failed"), "{rendered}");
    assert!(rendered.contains("last"), "{rendered}");
    assert!(rendered.find("first").unwrap() < rendered.find("middle failed").unwrap());
    assert!(rendered.find("middle failed").unwrap() < rendered.find("last").unwrap());
}

#[test]
fn all_sync_map_settles_every_leaf_then_reports_first_settled_rejection() {
    let host = RecordingBatchHost {
        results: Mutex::new(Some(vec![
            ResourceOperationResult::Error(ExecutionHostError::new("late input zero")),
            ResourceOperationResult::Value(Value::String("settled too".into())),
            ResourceOperationResult::Error(ExecutionHostError::new("early input two")),
        ])),
        settlement: Mutex::new(Some(vec![2, 1, 0])),
        ..RecordingBatchHost::default()
    };
    let error = execute(
        "finish(await Promise.all(['a', 'b', 'c'].map(id => web.fetch({ url: id }))));",
        &host,
    )
    .expect_err("Promise.all should reject after the whole batch settles");
    assert_eq!(host.batches.load(Ordering::SeqCst), 1);
    assert_eq!(*host.batch_sizes.lock().unwrap(), [3]);
    assert!(error.to_string().contains("early input two"), "{error}");
    assert!(!error.to_string().contains("late input zero"), "{error}");
}

#[test]
fn mapped_batch_preserves_five_element_input_order_under_shuffled_completion() {
    let host = RecordingBatchHost {
        results: Mutex::new(Some(
            (0..5)
                .map(|index| ResourceOperationResult::Value(Value::Number(index as f64)))
                .collect(),
        )),
        settlement: Mutex::new(Some(vec![4, 1, 3, 0, 2])),
        ..RecordingBatchHost::default()
    };
    let outcome = execute(
        "finish(await Promise.all(['a', 'b', 'c', 'd', 'e'].map(id => web.fetch({ url: id }))));",
        &host,
    )
    .expect("shuffled completion should not shuffle values");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            (0..5)
                .map(|index| Value::Number(index as f64))
                .collect::<Vec<_>>()
                .into()
        ))
    );
}

#[test]
fn a_second_dependent_await_keeps_the_async_map_sequential() {
    #[derive(Default)]
    struct SequentialHost {
        calls: AtomicUsize,
        batches: AtomicUsize,
    }
    impl ExecutionHost for SequentialHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(_) => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    Ok(AbilityResult::Value(Value::String("ok".into())))
                }
                AbilityOp::ResourceOperationBatch(_) => {
                    self.batches.fetch_add(1, Ordering::SeqCst);
                    Err(ExecutionHostError::new(
                        "dependent callbacks must not batch",
                    ))
                }
                AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
                _ => Err(ExecutionHostError::new("unexpected sequential-map ability")),
            }
        }
    }

    let host = SequentialHost::default();
    execute(
        "finish(await Promise.all(['a', 'b'].map(async id => { const first = await web.fetch({ url: id }); return await web.fetch({ url: first }); })));",
        &host,
    )
    .expect("dependent awaits should use the existing sequential async-map path");
    assert_eq!(host.calls.load(Ordering::SeqCst), 4);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

/// Records what the host was actually asked to do, so a witness can assert on
/// call counts and argument order rather than only on returned values: two of
/// the shapes below return plausible values while issuing the wrong calls.
#[derive(Default)]
struct OrderRecordingHost {
    batches: AtomicUsize,
    batch_sizes: Mutex<Vec<usize>>,
    urls: Mutex<Vec<f64>>,
    prints: Mutex<Vec<String>>,
    fail_every_call: bool,
}

impl OrderRecordingHost {
    fn failing() -> Self {
        Self {
            fail_every_call: true,
            ..Self::default()
        }
    }

    fn url_of(operation: &lashlang::ResourceOperation) -> Result<f64, ExecutionHostError> {
        let [Value::Record(fields)] = operation.args.as_slice() else {
            return Err(ExecutionHostError::new("expected one record argument"));
        };
        fields
            .iter()
            .find(|(key, _)| *key == "url")
            .and_then(|(_, value)| match value {
                Value::Number(number) => Some(*number),
                _ => None,
            })
            .ok_or_else(|| ExecutionHostError::new("expected a numeric url argument"))
    }

    fn record(&self, operation: &lashlang::ResourceOperation) -> Result<f64, ExecutionHostError> {
        let url = Self::url_of(operation)?;
        self.urls.lock().expect("urls lock").push(url);
        Ok(url + 10.0)
    }

    fn recorded_urls(&self) -> Vec<f64> {
        self.urls.lock().expect("urls lock").clone()
    }
}

impl ExecutionHost for OrderRecordingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                let value = self.record(&operation)?;
                if self.fail_every_call {
                    return Err(ExecutionHostError::new(format!("failure-{}", value - 10.0)));
                }
                Ok(AbilityResult::Value(Value::Number(value)))
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                self.batches.fetch_add(1, Ordering::SeqCst);
                self.batch_sizes
                    .lock()
                    .expect("batch sizes lock")
                    .push(batch.operations.len());
                let mut results = Vec::new();
                for operation in &batch.operations {
                    let value = self.record(operation)?;
                    results.push(if self.fail_every_call {
                        ResourceOperationResult::Error(ExecutionHostError::new(format!(
                            "failure-{}",
                            value - 10.0
                        )))
                    } else {
                        ResourceOperationResult::Value(Value::Number(value))
                    });
                }
                let settlement = (0..results.len()).collect();
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult::settled_in_order(results, settlement),
                ))
            }
            AbilityOp::Print(value) => {
                self.prints
                    .lock()
                    .expect("prints lock")
                    .push(format!("{value:?}"));
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

fn numbers(values: &[f64]) -> ExecutionOutcome {
    ExecutionOutcome::Finished(Value::List(
        values
            .iter()
            .copied()
            .map(Value::Number)
            .collect::<Vec<_>>()
            .into(),
    ))
}

#[test]
fn a_conditional_await_stays_sequential_and_issues_one_call() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => id === 0 ? 99 : await web.fetch({ url: id }))));",
        &host,
    )
    .expect("a conditionally awaited callback should run on the sequential driver");

    assert_eq!(outcome, numbers(&[99.0, 11.0]));
    assert_eq!(host.recorded_urls(), vec![1.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_caught_await_returns_the_caught_value_instead_of_failing_the_aggregate() {
    let host = OrderRecordingHost::failing();
    let outcome = execute(
        "finish(await Promise.all([0].map(async id => { try { return await web.fetch({ url: id }); } catch (e) { return 99; } })));",
        &host,
    )
    .expect("a caught leaf failure must not escape the aggregate");

    assert_eq!(outcome, numbers(&[99.0]));
    assert_eq!(host.recorded_urls(), vec![0.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_local_binding_before_the_await_stays_sequential() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => { const url = id; return await web.fetch({ url }); })));",
        &host,
    )
    .expect("a callback binding a local before awaiting should link and run");

    assert_eq!(outcome, numbers(&[10.0, 11.0]));
    assert_eq!(host.recorded_urls(), vec![0.0, 1.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_nested_dependent_await_in_tool_arguments_stays_sequential() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => await web.fetch({ url: await web.fetch({ url: id }) }))));",
        &host,
    )
    .expect("a dependent await inside tool arguments should run on the sequential driver");

    assert_eq!(outcome, numbers(&[20.0, 21.0]));
    assert_eq!(host.recorded_urls(), vec![0.0, 10.0, 1.0, 11.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_projection_closing_over_the_callback_parameter_stays_sequential() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => { const r = await web.fetch({ url: id }); return (() => id)(); })));",
        &host,
    )
    .expect("a projection closure over the callback parameter should link and run");

    assert_eq!(outcome, numbers(&[0.0, 1.0]));
    assert_eq!(host.recorded_urls(), vec![0.0, 1.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_callback_effect_after_the_await_stays_sequential() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => { const r = await web.fetch({ url: id }); console.log(id); return r; })));",
        &host,
    )
    .expect("a callback that logs should link and run");

    assert_eq!(outcome, numbers(&[10.0, 11.0]));
    assert_eq!(host.recorded_urls(), vec![0.0, 1.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
    assert_eq!(host.prints.lock().expect("prints lock").len(), 2);
}
