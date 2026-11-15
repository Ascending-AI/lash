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
    Vec<crate::Weight>,
    Vec<(lash_kernel_state::semantic::Checkpoint, u64)>,
    Vec<Request>,
    u64,
);

fn memory_refused(end: &End) -> bool {
    matches!(
        end,
        End::Error(crate::RunError::Bound(crate::BoundExceeded {
            bound: crate::Bound::Memory,
            ..
        }))
    )
}

/// Contract §2 (R10): a run reopened cold counts a paged payload only once
/// it is read, so at every park and at its end it holds no more than the
/// warm run, and it refuses for memory no earlier. Its checkpoints, charges,
/// effects and end are the warm run's, except that where warm refused for
/// memory, cold may run on past warm's refusal.
pub(super) fn cold_continues(cold: &Observation, warm: &Observation, context: &str) {
    for (park, ((_, cold_held), (_, warm_held))) in cold.3.iter().zip(&warm.3).enumerate() {
        assert!(
            cold_held <= warm_held,
            "held at park {park}: cold {cold_held} > warm {warm_held}: {context}"
        );
    }
    let checkpoints = |o: &Observation| {
        o.3.iter()
            .map(|(checkpoint, _)| checkpoint)
            .cloned()
            .collect::<Vec<_>>()
    };
    let (cold_parks, warm_parks) = (checkpoints(cold), checkpoints(warm));
    if memory_refused(&warm.0) {
        assert!(
            cold.1 >= warm.1,
            "cold charged past warm's memory refusal: {context}"
        );
        assert!(
            cold.2.starts_with(&warm.2),
            "warm charge tape is a prefix of cold's: {context}"
        );
        assert!(
            cold_parks.starts_with(&warm_parks),
            "warm continuations prefix cold's: {context}"
        );
        assert!(
            cold.4.starts_with(&warm.4),
            "warm effects prefix cold's: {context}"
        );
        return;
    }
    match (&cold.0, &warm.0) {
        (End::Finished(cold), End::Finished(warm)) => {
            // Session roots retain storage rekeying. PLAN-INDEP compares their
            // reachable graph through the kernel's semantic equality domain.
            assert_eq!(cold.result, warm.result, "{context}");
            assert_eq!(cold.finish, warm.finish, "{context}");
            assert_eq!(cold.not_carried, warm.not_carried, "{context}");
            assert_eq!(cold.carry.is_some(), warm.carry.is_some(), "{context}");
        }
        _ => assert_eq!(cold.0, warm.0, "{context}"),
    }
    assert_eq!(cold.1, warm.1, "canonical charge total: {context}");
    assert_eq!(cold.2, warm.2, "canonical charge tape: {context}");
    assert_eq!(
        cold_parks, warm_parks,
        "semantic continuation and graph: {context}"
    );
    assert_eq!(cold.4, warm.4, "effect identity and operands: {context}");
    assert!(
        cold.5 <= warm.5,
        "final held: cold {} > warm {}: {context}",
        cold.5,
        warm.5
    );
}

/// Sealed and reopened cold, each codec and keyed sort ends, charges and
/// refuses as it does warm, holding no more (contract §2).
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
            cold_continues(
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
                cold_continues(
                    &observe(source, true, taped, bounds),
                    &observe(source, false, taped, bounds),
                    &format!("native codec/sort first refusal cold, {bounds:?}: {source}"),
                );
            }
        }
    }
}
