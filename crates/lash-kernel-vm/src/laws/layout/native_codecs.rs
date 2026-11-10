//! K-LJSON-001/K-MACH-008: actual codec and keyed-sort graphs cross the ABI.
use super::*;
use crate::laws::embedder::{int, record, result, text};

const JSON_RECORD: &str = r#"main {
 let f = fn(input) {
  let first = json.decode(input, "number", "by_spelling", "record")
  let second = json.decode(input, "number", "by_spelling", "record")
  let alias = first
  set first.a[0] = 9
  let rendered = json.render(alias, set{}, absent, (-5, 21, true, false))
  do perform 0(rendered) as Any
  return (rendered, json.stringify(second))
 }
 return apply f("{\"a\":[1,2],\"a\":[3,\"a long child text\"],\"label\":\"é😀\"}")
}"#;
const JSON_MAP: &str = r#"main {
 let f = fn(input) {
  let decoded = json.decode(input, "int", "by_spelling", "map")
  do perform 0(decoded) as Any
  set decoded["x"]["k"][0] = 8
  return json.stringify(decoded)
 }
 return apply f("{\"x\":{\"k\":[1,2]},\"x\":{\"k\":[3]},\"n\":123456789012345678901234567890}")
}"#;
const KEYED_SORT: &str = r#"main {
 let f = fn(items, keys) {
  let old = items
  let sorted = list.sort_by_keys(items, keys, false)
  let reversed = list.sort_by_keys(items, keys, true)
  do perform 0(sorted) as Any
  set sorted[0].id = 7
  return (old, sorted, reversed)
 }
 return apply f([{id: 0}, {id: 1}, {id: 2}, {id: 3}], [(1,"b"), (0,"z"), (1,"b"), (0,"a")])
}"#;

type Observation = (
    End,
    Option<u64>,
    Vec<lash_kernel_exec::Weight>,
    Vec<(lash_kernel_state::semantic::Checkpoint, [u64; 3])>,
    Vec<Request>,
    [u64; 3],
);

fn same(actual: &Observation, expected: &Observation, context: &str) {
    match (&actual.0, &expected.0) {
        (End::Finished(actual), End::Finished(expected)) => {
            // Session roots retain storage rekeying. PLAN-INDEP compares their
            // reachable graph through the kernel's semantic equality domain.
            assert_eq!(actual.result, expected.result, "{context}");
            assert_eq!(actual.finish, expected.finish, "{context}");
            assert_eq!(actual.not_carried, expected.not_carried, "{context}");
            assert_eq!(
                actual.carry.is_some(),
                expected.carry.is_some(),
                "{context}"
            );
        }
        _ => assert_eq!(actual.0, expected.0, "{context}"),
    }
    assert_eq!(actual.1, expected.1, "canonical charge total: {context}");
    assert_eq!(actual.2, expected.2, "canonical charge tape: {context}");
    assert_eq!(
        actual.3, expected.3,
        "semantic continuation, graph and balance: {context}"
    );
    assert_eq!(
        actual.4, expected.4,
        "effect identity and operands: {context}"
    );
    assert_eq!(
        actual.5, expected.5,
        "final canonical source balance: {context}"
    );
}

/// Sealed and reopened cold, each codec and keyed sort ends, charges and
/// refuses as it does warm.
pub(super) fn agree(observe: &impl Fn(&str, bool, bool, Bounds) -> Observation) {
    let item = |id| record([("id", int(id))]);
    let examples = [
        (
            JSON_RECORD,
            Datum::Tuple(vec![
                text("{\"a\":[9,\"a long child text\"],\"label\":\"é😀\"}"),
                text("{\"a\":[3,\"a long child text\"],\"label\":\"é😀\"}"),
            ]),
            2,
        ),
        (
            JSON_MAP,
            text("{\"x\":{\"k\":[8]},\"n\":123456789012345678901234567890}"),
            1,
        ),
        (
            KEYED_SORT,
            Datum::Tuple(vec![
                Datum::List(vec![item(0), item(1), item(2), item(7)]),
                Datum::List(vec![item(7), item(1), item(0), item(2)]),
                Datum::List(vec![item(0), item(2), item(1), item(7)]),
            ]),
            2,
        ),
    ];
    let roomy = super::super::embedder::ROOMY;
    for taped in [false, true] {
        for (source, expected, _graphs) in &examples {
            let reference = observe(source, false, taped, roomy);
            assert_eq!(
                result(reference.0.clone()),
                *expected,
                "canonical native source result: {source}"
            );
            same(
                &observe(source, true, taped, roomy),
                &reference,
                &format!("native codec/sort cold, taped={taped}: {source}"),
            );
            let charge = reference.1.unwrap();
            for bounds in [
                Bounds {
                    charge_limit: charge - 1,
                    ..roomy
                },
                Bounds {
                    charge_limit: charge,
                    ..roomy
                },
                Bounds {
                    memory: 512,
                    ..roomy
                },
                Bounds {
                    memory: 1024,
                    ..roomy
                },
            ] {
                same(
                    &observe(source, true, taped, bounds),
                    &observe(source, false, taped, bounds),
                    &format!("native codec/sort first refusal cold, {bounds:?}: {source}"),
                );
            }
        }
    }
}
