//! Named text/JSON rules exercised on an arena bench, called as the machine
//! calls a native.
pub(crate) mod corpus;
mod json;
mod lab;
mod paged;
mod text;

use crate::testsupport::{Bench, datum, decode, heap, raised};
use lash_kernel_doc::{Datum, FunctionRegistry, Integer, Value, WorkCounter};

fn s(value: &str) -> Datum {
    Datum::Text(value.into())
}
fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}
fn args(heap: &mut Bench, values: Vec<Datum>) -> Vec<Value> {
    values.into_iter().map(|v| decode(heap, v)).collect()
}
fn call(
    heap: &mut Bench,
    name: &str,
    args: &[Value],
    room: u64,
) -> lash_kernel_state::arena::Called {
    let mut registry = FunctionRegistry::new();
    super::register_text_json(&mut registry).unwrap();
    let (_, native) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == name)
        .unwrap();
    heap.call(
        &**native.native().unwrap(),
        args,
        room,
        &mut WorkCounter::new(None),
    )
}

/// K-LJSON-005/006: syntax and depth refusal precede exhausted result space.
#[test]
fn a_refused_json_tree_preserves_its_syntax_and_depth_errors() {
    for (input, expected) in [
        (format!("[{}0", "0,".repeat(1_999)), "json_syntax"),
        (
            format!("[{}{}]", "0,".repeat(1_999), "[".repeat(100)),
            "json_depth",
        ),
    ] {
        let mut heap = heap();
        let input = args(&mut heap, vec![s(&input), s("int"), s("by_spelling")]);
        let answer = call(&mut heap, "json.parse", &input, 1 << 10).answer;
        let Some((kind, _)) = raised(&heap, &answer) else {
            panic!("JSON error: {answer:?}")
        };
        assert_eq!(kind, expected);
    }
}

/// K-LJSON-005: paged input preserves escaped Unicode, number tokens and
/// duplicate-key insertion order across payload-leaf boundaries.
#[test]
fn paged_json_preserves_unicode_numbers_and_first_key_positions() {
    let prefix = "x".repeat(4093);
    let source = format!(
        "{{\"first\":\"{prefix}😀\",\"first\":\"last\",\"num\":1e3,\"text\":\"{prefix}\\uD83D\\uDE00\"}}"
    );
    let mut heap = heap();
    let input = args(&mut heap, vec![s(&source), s("int"), s("by_spelling")]);
    let output = call(&mut heap, "json.parse", &input, u64::MAX)
        .answer
        .unwrap();
    assert_eq!(
        datum(&heap, &output),
        Datum::Record(vec![
            ("first".into(), s("last")),
            ("num".into(), int(1000)),
            ("text".into(), s(&format!("{prefix}😀")))
        ])
    );
}

/// K-LFMT-004: every out-of-range radix has the same error, even when its
/// magnitude is paged and there is no room to materialize it.
#[test]
fn a_paged_radix_keeps_the_range_error_before_reading_the_number() {
    for negative in [false, true] {
        let mut heap = heap();
        let magnitude = num_bigint::BigInt::from(10_u32).pow(1000);
        let radix = Integer::new(if negative { -magnitude } else { magnitude });
        let input = args(&mut heap, vec![Datum::Absent, Datum::Int(radix)]);
        let answer = call(&mut heap, "format.radix", &input, 0).answer;
        assert_eq!(
            raised(&heap, &answer),
            Some((
                "number_range".to_owned(),
                "radix must be between 2 and 36".to_owned()
            ))
        );
    }
}

/// K-VAL-009: constructing a timestamp preserves arbitrary integer nanoseconds exactly.
#[test]
fn timestamp_construction_preserves_nanoseconds() {
    for value in ["0", "-1", "123456789012345678901234567890"] {
        let nanoseconds = Integer::parse(value).unwrap();
        let mut heap = heap();
        let input = args(&mut heap, vec![Datum::Int(nanoseconds.clone())]);
        let answer = call(&mut heap, "timestamp.from_nanoseconds", &input, 4096)
            .answer
            .unwrap();
        assert_eq!(
            datum(&heap, &answer),
            Datum::Timestamp(lash_kernel_doc::Timestamp { nanoseconds })
        );
    }
}
