//! Fan-out witnesses and aggregate guards, re-asked of the runtime design.
//!
//! The compile-time fan-out branch that FIG-2766 abandoned lowered a mapped
//! tool await by hoisting it out of the callback, and its review found six
//! shapes the hoist silently mis-executed. Every one of the six returned a
//! plausible value while issuing the wrong host calls, so the witnesses assert
//! on what the host was *asked to do* — call counts, argument order, and
//! whether a batch was formed — never on the value that came back. That
//! property is design-independent, which is why they are carried here (ADR
//! 0095, FIG-2996): the runtime design runs one callback body to completion
//! before the next (`javascript_substrate.rs::execute_async_map`) and never
//! hoists, so each witness is a pin on that policy rather than on a repair.
//!
//! The two aggregate-acceptance guards are re-asked here too. On the old
//! design the acceptance rule was a *syntactic* one, so a scalar bound to a
//! name was read as an array and an array of process handles bound to a name
//! took a different path from the same array written inline.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ResourceOperation, ResourceOperationResult, State, Value,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct OrderRecordingHost {
    batches: AtomicUsize,
    batch_sizes: Mutex<Vec<usize>>,
    urls: Mutex<Vec<f64>>,
    prints: AtomicUsize,
    fail_every_call: bool,
}

impl OrderRecordingHost {
    fn failing() -> Self {
        Self {
            fail_every_call: true,
            ..Self::default()
        }
    }

    fn url_of(operation: &ResourceOperation) -> Result<f64, ExecutionHostError> {
        let [Value::Record(fields)] = operation.args.as_slice() else {
            return Err(ExecutionHostError::new("expected one record argument"));
        };
        match fields.get("url") {
            Some(Value::Number(number)) => Ok(*number),
            _ => Err(ExecutionHostError::new("expected a numeric url argument")),
        }
    }

    fn record(&self, operation: &ResourceOperation) -> Result<f64, ExecutionHostError> {
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
                    .push(batch.leaves.len());
                let mut results = Vec::new();
                for operation in batch
                    .leaves
                    .iter()
                    .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                {
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
                Ok(AbilityResult::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
            AbilityOp::Print(_) => {
                self.prints.fetch_add(1, Ordering::SeqCst);
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

fn execute(
    source: &str,
    host: &impl ExecutionHost,
) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let compiled = lash_typescript::compile(source).expect(source);
    futures::executor::block_on(lashlang::execute(&compiled, &mut State::new(), host))
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
fn a_conditional_await_issues_one_call() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => id === 0 ? 99 : await web.fetch({ url: id }))));",
        &host,
    )
    .expect("a conditionally awaited callback should run");

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
fn a_local_binding_before_the_await_links_and_runs() {
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
fn a_nested_dependent_await_in_tool_arguments_keeps_its_order() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => await web.fetch({ url: await web.fetch({ url: id }) }))));",
        &host,
    )
    .expect("a dependent await inside tool arguments should run in order");

    assert_eq!(outcome, numbers(&[20.0, 21.0]));
    assert_eq!(host.recorded_urls(), vec![0.0, 10.0, 1.0, 11.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
}

#[test]
fn a_projection_closing_over_the_callback_parameter_links_and_runs() {
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
fn a_callback_effect_after_the_await_links_and_runs() {
    let host = OrderRecordingHost::default();
    let outcome = execute(
        "finish(await Promise.all([0, 1].map(async id => { const r = await web.fetch({ url: id }); console.log(id); return r; })));",
        &host,
    )
    .expect("a callback that logs should link and run");

    assert_eq!(outcome, numbers(&[10.0, 11.0]));
    assert_eq!(host.recorded_urls(), vec![0.0, 1.0]);
    assert_eq!(host.batches.load(Ordering::SeqCst), 0);
    assert_eq!(host.prints.load(Ordering::SeqCst), 2);
}

/// Answers a process start with a handle and counts every durable wait, so a
/// re-check can assert that a refused aggregate reached no wait at all.
#[derive(Default)]
struct ProcessHost {
    starts: AtomicUsize,
    awaits: AtomicUsize,
}

/// The handle record a real host mints for a started process.
fn process_handle(id: &str) -> Value {
    let mut handle = lashlang::Record::new();
    handle.insert("__handle__".to_string(), Value::String("lash".into()));
    handle.insert("id".to_string(), Value::String(format!("p.1.{id}").into()));
    handle.insert("process_id".to_string(), Value::String(id.into()));
    Value::Record(std::sync::Arc::new(handle))
}

impl ProcessHost {
    fn wait(&self) -> Value {
        self.awaits.fetch_add(1, Ordering::SeqCst);
        Value::Number(7.0)
    }
}

impl ExecutionHost for ProcessHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            // The typed `await handle` seam, and `processes.await`, the tool
            // that parks on the same durable wait (ADR 0095).
            AbilityOp::Await(_) => Ok(AbilityResult::Value(self.wait())),
            // FIG-2999: starting is a leaf tool, so a start arrives as a
            // resource operation beside `processes.await`.
            AbilityOp::ResourceOperation(operation) if operation.operation == "start" => {
                let index = self.starts.fetch_add(1, Ordering::SeqCst);
                Ok(AbilityResult::Value(process_handle(&format!(
                    "run-{index}"
                ))))
            }
            AbilityOp::ResourceOperation(operation) => {
                assert_eq!(operation.operation, "await");
                Ok(AbilityResult::Value(self.wait()))
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = batch
                    .leaves
                    .iter()
                    .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                    .map(|operation| {
                        assert_eq!(operation.operation, "await");
                        ResourceOperationResult::Value(self.wait())
                    })
                    .collect();
                Ok(AbilityResult::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

/// `processes.await` is a host tool, not a dialect builtin, so the re-check
/// binds it in the catalog the way the process-controls plugin does.
fn process_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["processes"],
            "Processes",
            "await",
            "processes.await",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("processes.await binding");
    // FIG-2999: `start` is a leaf tool, and its `definition` slot is typed as a
    // process — that expected type is what lifts the process literal the
    // fixture starts.
    catalog
        .add_module_operation(
            ["processes"],
            "Processes",
            "start",
            "processes.start",
            lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                name: "definition".into(),
                ty: lashlang::TypeExpr::Process(lashlang::ProcessType::unknown()),
                optional: false,
            }]),
            lashlang::TypeExpr::Any,
        )
        .expect("processes.start binding");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
}

fn execute_linked(
    source: &str,
    host: &impl ExecutionHost,
) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let linked = lash_typescript::link(source, &process_environment()).expect(source);
    futures::executor::block_on(lashlang::execute(
        &lash_typescript::compile_linked(&linked),
        &mut State::new(),
        host,
    ))
}

/// Two live process handles, `a` and `b`, for the re-check below to aggregate.
const WORKER: &str = r#"
    const worker = async (input: unknown) => { return input; };
    const a = await processes.start({ definition: worker, args: { input: 1 } });
    const b = await processes.start({ definition: worker, args: { input: 2 } });
"#;

fn refusal(outcome: Result<ExecutionOutcome, lashlang::RuntimeError>, label: &str) -> String {
    match outcome {
        Ok(outcome) => panic!("{label}: expected a refusal, got {outcome:?}"),
        Err(lashlang::RuntimeError::PendingTool { problem }) => problem,
        Err(error) => panic!("{label}: expected the typed pending-tool refusal, got {error}"),
    }
}

/// Re-check A: an aggregate whose operand is not an array is refused, and the
/// refusal does not depend on how the operand was spelled.
///
/// The compile-time design decided this syntactically — it admitted any
/// non-literal expression — so `Promise.all(n)` over a number bound to a name
/// read the scalar as an array and handed it straight back. Acceptance is a
/// runtime question here (`Vm::await_pending_array`), asked of the value and
/// not of the source, so a name cannot be a way past it; these cases are pins
/// on that, not repairs.
#[test]
fn an_aggregate_over_a_non_array_is_refused_however_the_operand_is_spelled() {
    for method in ["all", "allSettled"] {
        let mut problems = Vec::new();
        for (label, source) in [
            (
                "a literal scalar",
                format!("finish(await Promise.{method}(3));"),
            ),
            (
                "a bound number",
                format!("const n = 3; finish(await Promise.{method}(n));"),
            ),
            (
                "a bound string",
                format!("const s = 'ab'; finish(await Promise.{method}(s));"),
            ),
            (
                "a bound record",
                format!("const o = {{ a: 1 }}; finish(await Promise.{method}(o));"),
            ),
            (
                "a returned scalar",
                format!("function f() {{ return 3; }} finish(await Promise.{method}(f()));"),
            ),
        ] {
            let host = OrderRecordingHost::default();
            let problem = refusal(execute(&source, &host), label);
            assert_eq!(problem, "Promise aggregate requires an array", "{label}");
            assert!(host.recorded_urls().is_empty(), "{label}");
            problems.push(problem);
        }
        // The spelling cannot change the diagnostic, which is the property the
        // syntactic rule could not hold.
        assert_eq!(
            problems
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            1
        );
    }
}

/// Re-check B: an array of process handles behaves identically inline and
/// bound to a name, in both directions.
///
/// The compile-time guard reached only the inline literal, so binding the
/// array to a name was a way past it and the handles were never awaited. The
/// runtime aggregate classifies element *values*, so all three spellings — and
/// the accepted `processes.await` form — agree, and a refused aggregate never
/// reaches the durable wait.
#[test]
fn an_array_of_process_handles_behaves_the_same_inline_and_bound() {
    let mut refusals = Vec::new();
    for (label, body) in [
        ("inline", "finish(await Promise.all([a, b]));"),
        (
            "bound to a name",
            "const hs = [a, b]; finish(await Promise.all(hs));",
        ),
        (
            "built by push",
            "const hs = []; hs.push(a); hs.push(b); finish(await Promise.all(hs));",
        ),
    ] {
        let host = ProcessHost::default();
        let problem = refusal(execute_linked(&format!("{WORKER}{body}"), &host), label);
        assert!(
            problem.contains("processes.await(handle)"),
            "{label}: {problem}"
        );
        assert_eq!(host.starts.load(Ordering::SeqCst), 2, "{label}");
        // Refused before the seam: no handle is left half-settled.
        assert_eq!(host.awaits.load(Ordering::SeqCst), 0, "{label}");
        refusals.push(problem);
    }
    assert_eq!(
        refusals
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1
    );

    // And the accepted spelling agrees the same way: both forms aggregate the
    // two durable waits into one batch.
    for (label, body) in [
        (
            "inline",
            "finish(await Promise.all([processes.await({ handle: a }), processes.await({ handle: b })]));",
        ),
        (
            "bound to a name",
            "const ws = [processes.await({ handle: a }), processes.await({ handle: b })]; finish(await Promise.all(ws));",
        ),
    ] {
        let host = ProcessHost::default();
        let outcome = execute_linked(&format!("{WORKER}{body}"), &host).unwrap_or_else(|error| {
            panic!("{label}: an aggregate of durable waits must run: {error}")
        });
        assert_eq!(outcome, numbers(&[7.0, 7.0]), "{label}");
        assert_eq!(host.awaits.load(Ordering::SeqCst), 2, "{label}");
    }
}
