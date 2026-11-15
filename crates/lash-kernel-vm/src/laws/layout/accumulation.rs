//! Accumulation agrees with the semantic operators, including refusing guards.

use super::*;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::plan as p;
use lash_kernel_value::FunctionId;
use std::collections::BTreeMap;

fn probes(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut probes = call_probes(registry, ids);
    for (definition, native) in lash_kernel_lib::numbers()
        .into_iter()
        .chain(lash_kernel_lib::text_json())
    {
        let name = match definition.name.as_str() {
            "num.to_text" => "num.to_text",
            "text.join" => "text.join",
            "format.fixed" => "format.fixed",
            "format.radix" => "format.radix",
            _ => continue,
        };
        probes.push((name, registry.register(definition, Some(native)).unwrap()));
    }
    let concat = ids["text.concat"];
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function law.append(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(7, deep(a), size(b), deep(result))\nnative\nuse text.concat = @{concat}\nbody {{ return text.concat(a, b) }}"
    )).unwrap();
    let function = registry.register(definition, None).unwrap();
    let plan = Box::leak(Box::new(p::HelperPlan {
        name: "law.append",
        function,
        callees: Box::leak(Box::new([concat])),
        literals: &[],
        fields: &[],
        records: &[],
        registers: 2,
        slots: &[],
        closures: &[],
        sites: &[],
        run: |_, _| unreachable!("a library body is the fallback"),
        template: Some(p::HelperTemplate {
            entry: None,
            metered: None,
            depth: 1,
            callees: Box::leak(Box::new([concat])),
            heap: Some(p::HeapTemplate {
                entry: Some(|heap, args, _| heap.call(0, args)),
                depth: 1,
                writes: false,
                copy: None,
                element: None,
                record_write: None,
                callees: Box::leak(Box::new([concat])),
            }),
            forwards: p::Forwards {
                texts: Some(0),
                ..Default::default()
            },
        }),
    }));
    registry.install_plans(&[plan]).unwrap();
    probes.push(("law.append", function));
    probes
}

fn native_probes(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut functions = probes(registry, ids);
    for (mut definition, native) in lash_kernel_lib::text_json() {
        if definition.name.as_str() != "text.concat" {
            continue;
        }
        definition.name = lash_kernel_value::FunctionName::new("law.native_concat").unwrap();
        functions.push((
            "law.native_concat",
            registry.register(definition, Some(native)).unwrap(),
        ));
    }
    functions
}

type Observation = (
    String,
    Vec<Datum>,
    Vec<(&'static str, crate::Meters)>,
    Vec<lash_kernel_state::semantic::Checkpoint>,
    Vec<Request>,
    Vec<crate::Weight>,
);

/// What a run shows its embedder whatever its slices: result, prints,
/// effects and charge events.
fn semantic(
    (end, printed, _, _, requests, events): Observation,
) -> (String, Vec<Datum>, Vec<Request>, Vec<crate::Weight>) {
    (end, printed, requests, events)
}

fn run(source: &str, record: bool, slice: u64, bounds: Bounds) -> Observation {
    run_selected(source, record, slice, bounds, false, false)
}

/// Runs `source`, sealing and reopening cold at every stop when `cold`,
/// with `law.native_concat` registered when `native`.
fn run_selected(
    source: &str,
    record: bool,
    slice: u64,
    bounds: Bounds,
    cold: bool,
    native: bool,
) -> Observation {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(if native { native_probes } else { probes }),
            bounds,
            slice,
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if record {
        embedder.machine.record_charges(tape.clone());
    }
    let (mut stops, mut checkpoints) = (Vec::new(), Vec::new());
    let end = loop {
        let step = embedder.run();
        let kind = match &step {
            Step::Ended(_) => "ended",
            Step::Slice => "slice",
            Step::Parked(_) => "parked",
        };
        stops.push((kind, embedder.machine.meters()));
        if cold && !matches!(step, Step::Ended(_)) {
            checkpoints.push(embedder.machine.semantic_checkpoint());
            embedder.reopen(true);
            if record {
                embedder.machine.record_charges(tape.clone());
            }
        }
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("finished {:?}", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                if !cold {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                }
                embedder.deliver_next();
            }
        }
    };
    (
        end,
        embedder.world.printed,
        stops,
        checkpoints,
        embedder.requests,
        crate::Weight::expand(tape.steps()),
    )
}

/// Sealed and reopened cold at every stop of each slice, `source` shows
/// its embedder what it does run whole under `bounds`.
fn cold_agrees(
    source: &str,
    record: bool,
    bounds: Bounds,
    native: bool,
    slices: &[u64],
    what: &str,
) {
    let warm = semantic(run_selected(
        source,
        record,
        u64::MAX,
        bounds,
        false,
        native,
    ));
    for &slice in slices {
        assert_eq!(
            semantic(run_selected(source, record, slice, bounds, true, native)),
            warm,
            "{what}: cold at slice {slice}, record={record}, {bounds:?}"
        );
    }
}

/// K-CHG-001 / K-CHG-003 / K-MACH-008: accumulation retains every value,
/// effect and charge when its scalar or text guard answers or fails,
/// with or without a tape, warm or sealed cold.
#[test]
fn accumulations_agree() {
    original_text_body_prefixes();

    let source = scalar_document(
        r#"let s = "" let sum = 0 let i = 0 while flat.lt(i, 40) { set s = law.append(s, "é😀") set sum = flat.add(sum, 3) set i = flat.add(i, 1) } do perform 0("middle") as Any let old = s let j = 0 while flat.lt(j, 3) { set s = text.concat(s, "!") set j = flat.add(j, 1) } let huge = 9223372036854775807 try { set huge = flat.add(huge, 1) } catch err { print huge } let f = -0.0 set f = flat.add(f, 0.0) return (s, old, sum, huge, f)"#,
    );
    let without_tape = |(a, b, c, d, e, _): Observation| (a, b, c, d, e);
    let run = |record, slice, bounds| run(&source, record, slice, bounds);

    let reference = run(true, u64::MAX, super::super::embedder::ROOMY);
    let natural = run(false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(without_tape(natural), without_tape(reference.clone()));

    for slice in [1, 2, 3, 5, 11, 31, 67] {
        assert_eq!(
            without_tape(run(false, slice, super::super::embedder::ROOMY)),
            without_tape(run(true, slice, super::super::embedder::ROOMY)),
            "slice {slice}"
        );
    }
    let charge = reference.2.last().unwrap().1.charged.unwrap();
    for limit in (0..charge).step_by(97) {
        let bounds = Bounds {
            charge_limit: limit,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            without_tape(run(false, u64::MAX, bounds)),
            without_tape(run(true, u64::MAX, bounds)),
            "charge {limit}"
        );
    }
    for memory in [4096, 16384, 65536, 131072] {
        let bounds = Bounds {
            memory,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            without_tape(run(false, u64::MAX, bounds)),
            without_tape(run(true, u64::MAX, bounds)),
            "memory {memory}"
        );
    }
    cold_agrees(
        &source,
        true,
        super::super::embedder::ROOMY,
        false,
        &[1, 7, 31],
        "owned append",
    );

    let formatted = scalar_document(
        r#"let s = "abcdefghijklmnopé😀" let old = s let i = 0 while flat.lt(i, 3) { set s = text.concat(s, text.join(["é😀", num.to_text(-0.0), format.fixed(1.25, 2), format.radix(255, 16)], "|")) set i = flat.add(i, 1) } do perform 0(s) as Any return (s, old)"#,
    );
    cold_agrees(
        &formatted,
        true,
        super::super::embedder::ROOMY,
        false,
        &[1, 7],
        "canonical formatted pieces",
    );

    let native_formatted = scalar_document(
        r#"let s = "abcdefghijklmnopé😀" let old = s let parts = ["é😀", "tag"] let separator = "|" let i = 0 while flat.lt(i, 3) { let joined = text.join(parts, separator) set s = law.native_concat(s, joined) set i = flat.add(i, 1) } do perform 0(s) as Any return (s, old)"#,
    );
    for record in [false, true] {
        cold_agrees(
            &native_formatted,
            record,
            super::super::embedder::ROOMY,
            true,
            &[1, 7],
            "native text",
        );
    }

    // K-CHG-001: a text completion consumes the same accepted constructor
    // prefix. Its native fee and only the remaining fixed tape settle once.
    let prefix_text = scalar_document(
        r#"let parts = ["a", "b"] let joined = text.join(parts, "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyzé😀") do perform 0(joined) as Any return joined"#,
    );
    for record in [false, true] {
        for bounds in refusing_bounds(&prefix_text, true) {
            cold_agrees(
                &prefix_text,
                record,
                bounds,
                true,
                &[1, 7],
                "accepted text prefix",
            );
        }
    }
}

/// Roomy bounds, and the charge and memory limits around `source`'s
/// whole run.
fn refusing_bounds(source: &str, native: bool) -> [Bounds; 5] {
    let roomy = super::super::embedder::ROOMY;
    let reference = run_selected(source, false, u64::MAX, roomy, false, native);
    let charged = reference.2.last().unwrap().1.charged.unwrap();
    [
        roomy,
        Bounds {
            charge_limit: charged - 1,
            ..roomy
        },
        Bounds {
            charge_limit: charged,
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
    ]
}

/// K-CHG-001 / K-MACH-008: a deferred literal's accepted original close
/// continues into the actual forwarding body, with its frame and native
/// peak, warm or sealed cold.
fn original_text_body_prefixes() {
    let source = scalar_document(
        r#"let text = law.append("abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyzé😀", "é😀") do perform 0(text) as Any return text"#,
    );
    for record in [false, true] {
        for bounds in refusing_bounds(&source, false) {
            cold_agrees(
                &source,
                record,
                bounds,
                false,
                &[1, 7],
                "original body text",
            );
        }
    }
}

/// K-LIB-007 (FIG-7688): a text appended at the shared tip of its left
/// operand's buffer leaves every other text its bytes. An alias of the
/// left, a second append from the same left, a closure's capture, the
/// right operand a native borrows from the left's own buffer, and an
/// append after a park from the version its checkpoint retains each read
/// what a copy would give them, warm, at every slice, and sealed and
/// reopened cold at every stop.
#[test]
fn a_shared_tip_append_leaves_every_other_text_its_bytes() {
    let source = scalar_document(
        r#"let s = law.native_concat("abcdefghijklmnop", "q") set s = law.native_concat(s, "r") let alias = s let pair = law.native_concat(s, s) let a = law.native_concat(alias, "1") let b = law.native_concat(alias, "2") let keep = fn() { return alias } set s = law.native_concat(pair, "!") do perform 0("middle") as Any let c = law.native_concat(alias, "3") set s = law.native_concat(s, "?") print (alias, a, b, apply keep()) return (s, pair, c)"#,
    );
    let alias = "abcdefghijklmnopqr";
    let pair = format!("{alias}{alias}");
    let texts = |texts: &[&str]| {
        Datum::Tuple(
            texts
                .iter()
                .map(|text| Datum::Text((*text).into()))
                .collect(),
        )
    };
    let printed = vec![texts(&[
        alias,
        &format!("{alias}1"),
        &format!("{alias}2"),
        alias,
    ])];
    let end = format!(
        "finished {:?}",
        texts(&[&format!("{pair}!?"), &pair, &format!("{alias}3")])
    );
    let roomy = super::super::embedder::ROOMY;
    for slice in [u64::MAX, 1, 2, 5] {
        let (got, prints, ..) = run_selected(&source, true, slice, roomy, false, true);
        assert_eq!((&got, &prints), (&end, &printed), "warm at slice {slice}");
    }
    for slice in [u64::MAX, 1, 3] {
        let (got, prints, ..) = run_selected(&source, true, slice, roomy, true, true);
        assert_eq!((&got, &prints), (&end, &printed), "cold at slice {slice}");
    }
}

/// K-LIB-007 / K-CHG-001 (FIG-7688): a chain of appends at the shared tip
/// holds and charges what the copying path does. Prepending to the same
/// texts never finds a tip, so it copies at every step; statement by
/// statement both hold the same memory and record the same charge, and
/// every memory bound refuses both at the same stop and total.
#[test]
fn a_shared_tip_chain_holds_and_refuses_as_a_copying_one() {
    let chain = |step: &str| {
        scalar_document(&format!(
            r#"let s = "abcdefghijklmnop" let i = 0 while flat.lt(i, 40) {{ set s = {step} set i = flat.add(i, 1) }} do perform 0("built") as Any return i"#
        ))
    };
    let (append, prepend) = (
        chain(r#"law.native_concat(s, "é😀")"#),
        chain(r#"law.native_concat("é😀", s)"#),
    );
    let observed = |source: &str, slice, bounds| {
        let (end, _, stops, _, _, events) = run_selected(source, true, slice, bounds, false, true);
        (end, stops, events)
    };
    let roomy = super::super::embedder::ROOMY;
    let whole = observed(&append, 1, roomy);
    assert_eq!(
        whole,
        observed(&prepend, 1, roomy),
        "statement by statement"
    );
    let peak = whole
        .1
        .iter()
        .map(|(_, meters)| meters.memory)
        .max()
        .unwrap();
    for memory in (peak / 2..=peak + 64).step_by(16) {
        let bounds = Bounds { memory, ..roomy };
        assert_eq!(
            observed(&append, u64::MAX, bounds),
            observed(&prepend, u64::MAX, bounds),
            "memory {memory}"
        );
    }
}

/// K-BND-003 / K-CHG-002..004 / K-LIB-007 (FIG-7688, the shared-tip
/// ruling's condition 7): under every memory bound, a chain of appends
/// refuses at the concatenation and the total charge derived by hand for
/// the copying path, and finishes from the bound that path needs.
///
/// The `k`th concatenation makes a text of `L(k) = 16 + 6k` bytes, held
/// as an object of `24 + L(k)` (the meter's schedule: `meter::OBJECT` and one
/// per byte) while its left operand, the `k-1`th, is still held; every
/// earlier text is unreachable and reclaimed before a refusal. So with
/// `base` held before any text, the `k`th needs `base + 24 + L(1)` for
/// `k = 1` and `base + 48 + L(k-1) + L(k)` after. A shared buffer's spare
/// capacity is never in it. A refused allocation reports the charge
/// before its statement: the `k+1`th's is the `k`th's plus the statement
/// `set s = law.native_concat(s, "é😀")` (1, and 3 nodes), the formula
/// `1 + deep(a) + deep(b) + deep(result) = 10 + L(k-1) + L(k)`, then the
/// counter's `set i = flat.add(i, 1)` (1, 3 nodes, and `3 + size(i) +
/// size(1) + size(i + 1)`) and the loop's test `flat.lt(i, 40)` (1, 3
/// nodes, and `3 + size(i) + size(40) + size(bool)`); before the loop,
/// `let i = 0` (2) and the `while` statement (1) stand in place of the
/// counter's step. A word's size is 2, 1 for zero (K-CHG-004).
#[test]
fn a_shared_tip_chain_refuses_at_the_hand_derived_copying_event() {
    let source = scalar_document(
        r#"let s = law.native_concat("abcdefghijklmnop", "é😀") let i = 0 while flat.lt(i, 40) { set s = law.native_concat(s, "é😀") set i = flat.add(i, 1) } return i"#,
    );
    let roomy = super::super::embedder::ROOMY;
    let run = |memory| {
        let (end, _, stops, ..) = run_selected(
            &source,
            true,
            u64::MAX,
            Bounds { memory, ..roomy },
            false,
            true,
        );
        (end, stops.last().unwrap().1.charged.unwrap())
    };
    // What the run holds before any text, and its charge when the first
    // concatenation starts: the two figures the oracle builds on.
    let base = run_selected(&source, true, 1, roomy, false, true).2[0]
        .1
        .memory;
    let first = run(base).1;
    let len = |k: u64| 16 + 6 * k;
    let size = |word: u64| if word == 0 { 1 } else { 2 };
    let need = |k: u64| match k {
        1 => base + 24 + len(1),
        _ => base + 48 + len(k - 1) + len(k),
    };
    let mut charge = vec![0, first];
    for k in 1..=40 {
        let concat = 4 + 10 + len(k - 1) + len(k);
        let counter = match k {
            1 => 2 + 1,
            _ => 4 + 3 + size(k - 2) + 2 + size(k - 1),
        };
        let test = 4 + 3 + size(k - 1) + 2 + 1;
        charge.push(charge[k as usize] + concat + counter + test);
    }
    for memory in base..=need(41) {
        let (end, charged) = run(memory);
        match (1..=41).find(|&k| need(k) > memory) {
            Some(k) => {
                assert!(
                    end.contains("bound: Memory")
                        && end.contains("FunctionName(\"law.native_concat\")"),
                    "memory {memory}: {end}"
                );
                assert_eq!(
                    charged, charge[k as usize],
                    "memory {memory}: concatenation {k}"
                );
            }
            None => assert_eq!(end, "finished Int(Integer(40))", "memory {memory}"),
        }
    }
}
