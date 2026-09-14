//! The TypeScript spellings that build a list, and what each of them costs the
//! heap.
//!
//! FIG-3063: `xs.push(item)` and `xs[xs.length] = item` are the two spellings a
//! loop uses to accumulate results, and both of them used to rebuild the whole
//! backing array on every iteration. They now grow the array the heap already
//! owns. The observable contract is unchanged, and that is what these tests
//! pin: what each spelling stores, what it returns, and which spellings are
//! still copies because JavaScript says they are.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, RuntimeError,
    State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unsupported array-append ability")),
        }
    }
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
}

fn finished(source: &str) -> Value {
    match execute(source)
        .unwrap_or_else(|error| panic!("TypeScript should execute: {source}: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

fn numbers(values: &[f64]) -> Value {
    Value::List(values.iter().copied().map(Value::Number).collect())
}

#[test]
fn push_appends_and_answers_the_new_length() {
    assert_eq!(
        finished("const xs = [1]; xs.push(2); finish(xs);"),
        numbers(&[1.0, 2.0])
    );
    assert_eq!(
        finished("const xs = [1]; finish(xs.push(2));"),
        Value::Number(2.0)
    );
    // Multi-argument push appends left to right and answers the length after
    // all of them landed.
    assert_eq!(
        finished("const xs = [1]; xs.push(2, 3, 4); finish(xs);"),
        numbers(&[1.0, 2.0, 3.0, 4.0])
    );
    assert_eq!(
        finished("const xs = [1]; finish(xs.push(2, 3, 4));"),
        Value::Number(4.0)
    );
    // A zero-argument push is a no-op that still reports the length.
    assert_eq!(
        finished("const xs = [1, 2]; finish(xs.push());"),
        Value::Number(2.0)
    );
}

#[test]
fn push_builds_a_list_in_a_loop() {
    assert_eq!(
        finished(
            "const xs: number[] = [];
             for (let i = 0; i < 4; i++) { xs.push(i * 2); }
             finish(xs);"
        ),
        numbers(&[0.0, 2.0, 4.0, 6.0])
    );
}

#[test]
fn terminal_index_assignment_appends() {
    assert_eq!(
        finished(
            "const xs: number[] = [];
             for (let i = 0; i < 4; i++) { xs[xs.length] = i * 2; }
             finish(xs);"
        ),
        numbers(&[0.0, 2.0, 4.0, 6.0])
    );
    // The same write spelled with a literal index at the end of the array.
    assert_eq!(
        finished("const xs = [1]; xs[1] = 2; finish(xs);"),
        numbers(&[1.0, 2.0])
    );
}

#[test]
fn interior_index_assignment_still_replaces() {
    assert_eq!(
        finished("const xs = [1, 2, 3]; xs[1] = 9; finish(xs);"),
        numbers(&[1.0, 9.0, 3.0])
    );
    // A write past the end is still the refusal it was: the fast path is the
    // terminal index only.
    let error =
        execute("const xs = [1]; xs[3] = 9; finish(xs);").expect_err("a hole is not representable");
    assert!(
        error.to_string().contains("TS_SPARSE_ARRAY_UNSUPPORTED"),
        "{error}"
    );
}

/// ADR 0096: an array is a reference, and every name that reaches the object
/// observes an append through any of them. Making the append in-place must not
/// move that line in either direction — it was already the law, and an in-place
/// append is exactly as visible as a rebuild that keeps the same identity.
#[test]
fn an_append_is_visible_through_every_name_that_reaches_the_array() {
    let value = finished(
        "const xs = [1];
         const ys = xs;
         const holder = { list: xs };
         xs.push(2);
         ys[ys.length] = 3;
         finish({ xs, ys, held: holder.list, same: xs === ys });",
    );
    let Value::Record(record) = &value else {
        panic!("expected a record, got {value:?}");
    };
    assert_eq!(record.get("xs"), Some(&numbers(&[1.0, 2.0, 3.0])));
    assert_eq!(record.get("ys"), Some(&numbers(&[1.0, 2.0, 3.0])));
    assert_eq!(record.get("held"), Some(&numbers(&[1.0, 2.0, 3.0])));
    assert_eq!(record.get("same"), Some(&Value::Bool(true)));
}

/// `concat` and spread build a new array in JavaScript, so they stay copies
/// here. They are the spellings a program reaches for when it wants the old
/// list left alone, and nothing about the append path may quietly fuse them
/// into the accumulator.
#[test]
fn concat_and_spread_leave_the_source_alone() {
    let value = finished(
        "const xs = [1];
         const joined = xs.concat([2]);
         const spread = [...xs, 3];
         xs.push(4);
         finish({ xs, joined, spread, aliased: joined === xs });",
    );
    let Value::Record(record) = &value else {
        panic!("expected a record, got {value:?}");
    };
    assert_eq!(record.get("xs"), Some(&numbers(&[1.0, 4.0])));
    assert_eq!(record.get("joined"), Some(&numbers(&[1.0, 2.0])));
    assert_eq!(record.get("spread"), Some(&numbers(&[1.0, 3.0])));
    assert_eq!(record.get("aliased"), Some(&Value::Bool(false)));
}

/// Appending a reference keeps it a reference: the array holds the same object
/// the program pushed, so a later mutation through either name is seen through
/// the other. The rebuild path established this by copying the member into the
/// new vector; the in-place append has to register the same parent edge.
#[test]
fn appending_a_reference_keeps_it_shared() {
    let value = finished(
        "const inner: number[] = [1];
         const xs: number[][] = [];
         xs.push(inner);
         xs[xs.length] = inner;
         inner.push(2);
         finish({ first: xs[0], second: xs[1], length: xs.length });",
    );
    let Value::Record(record) = &value else {
        panic!("expected a record, got {value:?}");
    };
    assert_eq!(record.get("first"), Some(&numbers(&[1.0, 2.0])));
    assert_eq!(record.get("second"), Some(&numbers(&[1.0, 2.0])));
    assert_eq!(record.get("length"), Some(&Value::Number(2.0)));
}

/// The other array mutators keep working beside the append fast path.
#[test]
fn the_other_end_mutators_are_unchanged() {
    assert_eq!(
        finished("const xs = [1, 2]; xs.unshift(0); finish(xs);"),
        numbers(&[0.0, 1.0, 2.0])
    );
    assert_eq!(
        finished("const xs = [1, 2]; finish(xs.pop());"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("const xs = [1, 2]; xs.shift(); finish(xs);"),
        numbers(&[2.0])
    );
    assert_eq!(
        finished("const xs = [1, 2, 3]; xs.splice(1, 1); finish(xs);"),
        numbers(&[1.0, 3.0])
    );
    assert_eq!(
        finished("const xs = [1, 2, 3]; xs.length = 1; finish(xs);"),
        numbers(&[1.0])
    );
}
