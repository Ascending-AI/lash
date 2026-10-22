//! K-BND-001/003: amplifying natives reserve against the real S7 census.
use crate::testsupport::{Bench, decode, exhausted, heap};
use lash_kernel_doc::{Datum, FunctionRegistry, Integer, NativeError, Value, WorkCounter};

use num_bigint::BigInt;

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}
fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}
fn call(
    registry: &FunctionRegistry,
    heap: &mut Bench,
    name: &str,
    input: &[Value],
    room: u64,
) -> Result<Value, NativeError> {
    let (_, registered) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == name)
        .unwrap();
    let limit = registered
        .definition()
        .guard
        .as_ref()
        .map(|guard| guard.limit.evaluate(&mut |_, _| 0));
    crate::testsupport::call(
        heap,
        &**registered.native().unwrap(),
        input,
        room,
        &mut WorkCounter::new(limit),
    )
}
/// K-BND-001/K-LIB-007: powers and radix digits refuse before their output
/// outgrows the remaining logical space, including paged integer inputs.
#[test]
fn integer_amplifiers_refuse_under_the_real_census() {
    let mut registry = FunctionRegistry::new();
    crate::register_numbers(&mut registry).unwrap();
    for (name, input, extra) in [
        ("int.pow", vec![int(2), int(1 << 20)], 16 << 10),
        (
            "int.to_text",
            vec![Datum::Int(Integer::new(BigInt::from(1) << 16_000)), int(2)],
            4 << 10,
        ),
        (
            "num.to_text",
            vec![Datum::Int(Integer::new(BigInt::from(1) << 16_000))],
            4 << 10,
        ),
    ] {
        let mut heap = heap();
        let input: Vec<_> = input.into_iter().map(|v| decode(&mut heap, v)).collect();
        let result = call(&registry, &mut heap, name, &input, extra);
        assert!(exhausted(&result), "{name}: {result:?}");
    }
}
/// K-BND-003: text, formatting and JSON expansion cannot construct results
/// beyond the caller's remaining space; syntax precedence has its own law.
#[test]
fn text_and_json_amplifiers_refuse_under_the_real_census() {
    let mut registry = FunctionRegistry::new();
    crate::register_text_json(&mut registry).unwrap();
    for (name, input) in [
        ("text.repeat", vec![text("ab"), int(40_000)]),
        (
            "text.replace",
            vec![text(&"x".repeat(300)), text("x"), text(&"y".repeat(300))],
        ),
        ("text.split", vec![text(&"x".repeat(2_000)), text("")]),
        ("text.to_code_points", vec![text(&"x".repeat(2_000))]),
        ("text.to_utf16_units", vec![text(&"x".repeat(2_000))]),
        ("format.fixed", vec![int(1), int(40_000)]),
        (
            "json.parse",
            vec![text("1e40000"), text("int"), text("by_spelling")],
        ),
        (
            "json.parse",
            vec![
                text(&format!("[{}0]", "0,".repeat(1_999))),
                text("int"),
                text("by_spelling"),
            ],
        ),
    ] {
        let mut heap = heap();
        let input: Vec<_> = input.into_iter().map(|v| decode(&mut heap, v)).collect();
        let result = call(&registry, &mut heap, name, &input, 16 << 10);
        assert!(exhausted(&result), "{name}: {result:?}");
    }
}
