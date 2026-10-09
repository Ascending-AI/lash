//! Laws of native repetition and guards, using real registered NativeFunction
//! calls. No machine double or alternative scheduler participates.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lash_kernel_doc::{
    Datum, Element, FunctionRegistry, NativeCall, NativeError, NativeFunction, NativeHeap, Object,
    ObjectId, Value, WorkCounter, parse_definition,
};

use crate::{HarnessError, NativeCase, NativeObservation, NativeOutcome, check_native_calls};

struct Count {
    calls: AtomicU64,
    cache_sensitive: bool,
}

impl NativeFunction for Count {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let work = if self.cache_sensitive {
            self.calls.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            1
        };
        call.counter.spend(work)?;
        call.counter
            .spend(u64::from(call.args[0] != Value::Int(0.into())))?;
        Ok(call.args[0].clone())
    }
}

/// These native cases hold only scalar arguments. Any heap operation is a
/// test failure, rather than an imitation of heap semantics.
struct ScalarHeap;

impl NativeHeap for ScalarHeap {
    fn len(&self, _: ObjectId) -> usize {
        panic!("scalar native read heap")
    }
    fn list_get(&self, _: ObjectId, _: usize) -> Option<Value> {
        panic!("scalar native read heap")
    }
    fn map_get(&self, _: ObjectId, _: &Value) -> Option<Value> {
        panic!("scalar native read heap")
    }
    fn set_contains(&self, _: ObjectId, _: &Value) -> bool {
        panic!("scalar native read heap")
    }
    fn record_get(&self, _: ObjectId, _: &str) -> Option<Value> {
        panic!("scalar native read heap")
    }
    fn visit(&self, _: ObjectId, _: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) {
        panic!("scalar native read heap")
    }
    fn allocate(&mut self, _: Object) -> Result<ObjectId, NativeError> {
        panic!("scalar native allocated heap")
    }
    fn reserve(&mut self, _: u64, _: u64) -> Result<(), NativeError> {
        panic!("scalar native reserved heap")
    }
}

fn registry(cache_sensitive: bool, limit: u64) -> Result<FunctionRegistry, HarnessError> {
    let definition = parse_definition(&format!(
        "function test.count(x: Int) -> Int\nkernel 1\ncharge 3\nguard \"steps\" {limit}\nnative\n"
    ))
    .map_err(|e| HarnessError(e.to_string()))?;
    let mut registry = FunctionRegistry::new();
    registry
        .register(
            definition,
            Some(Arc::new(Count {
                calls: AtomicU64::new(0),
                cache_sensitive,
            })),
        )
        .map_err(|e| HarnessError(e.to_string()))?;
    Ok(registry)
}

fn probe(
    registry: &FunctionRegistry,
    case: &NativeCase,
) -> Result<NativeObservation, HarnessError> {
    let registered = registry
        .get(&case.function)
        .ok_or_else(|| HarnessError("native missing".into()))?;
    let native = registered
        .native
        .as_ref()
        .ok_or_else(|| HarnessError("implementation missing".into()))?;
    let args = case
        .args
        .iter()
        .map(|arg| match arg {
            Datum::Int(value) => Ok(Value::Int(value.clone())),
            _ => Err(HarnessError("scalar law takes only Int".into())),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let lash_kernel_doc::Formula::Constant(charge) = registered.definition.charge else {
        return Err(HarnessError("scalar law pins a constant charge".into()));
    };
    let limit = registered
        .definition
        .guard
        .as_ref()
        .and_then(|guard| match guard.limit {
            lash_kernel_doc::Formula::Constant(limit) => Some(limit),
            _ => None,
        });
    let mut counter = WorkCounter::new(limit);
    let outcome = match native.call(NativeCall {
        args: &args,
        heap: &mut ScalarHeap,
        counter: &mut counter,
    }) {
        Ok(Value::Int(value)) => NativeOutcome::Returned(Datum::Int(value)),
        Err(NativeError::Guard(error)) => NativeOutcome::Guard { limit: error.limit },
        outcome => {
            return Err(HarnessError(format!(
                "unexpected scalar outcome {outcome:?}"
            )));
        }
    };
    let charged = if matches!(outcome, NativeOutcome::Guard { .. }) {
        0
    } else {
        charge
    };
    Ok(NativeObservation {
        outcome,
        charged,
        work: counter.spent(),
    })
}

fn native_case(limit: u64) -> NativeCase {
    let registry = registry(false, limit).expect("register native definition");
    let function = *registry.iter().next().expect("one native").0;
    NativeCase {
        name: "two work units are cache independent".into(),
        function,
        args: vec![Datum::Int(7.into())],
        expected: NativeObservation {
            outcome: if limit < 2 {
                NativeOutcome::Guard { limit }
            } else {
                NativeOutcome::Returned(Datum::Int(7.into()))
            },
            charged: if limit < 2 { 0 } else { 3 },
            work: limit.min(2),
        },
    }
}

#[test]
fn native_cold_and_warm_guards_have_identical_charge_and_work() {
    let case = native_case(1);
    let mut success = case.clone();
    success.name = "success has the same charge cold and warm".into();
    success.args = vec![Datum::Int(0.into())];
    success.expected = NativeObservation {
        outcome: NativeOutcome::Returned(Datum::Int(0.into())),
        charged: 3,
        work: 1,
    };
    assert_eq!(
        check_native_calls(|| registry(false, 1), probe, &[case, success])
            .expect("eight equal native calls"),
        8
    );
}

#[test]
fn native_harness_refuses_missing_registered_function_cases() {
    let error = check_native_calls(|| registry(false, 1), probe, &[])
        .expect_err("all registered natives require cases");
    assert!(error.0.contains("lacks result or guard cases"));
}

#[test]
fn native_harness_detects_cache_sensitive_work() {
    let case = native_case(1);
    // A cache-sensitive native guards at work 0 on its second warm call,
    // instead of work 1. The harness must catch the changed failure point.
    let error = check_native_calls(|| registry(true, 1), probe, &[case])
        .expect_err("warm-cache work differs");
    assert!(error.0.contains("observed"));
}
