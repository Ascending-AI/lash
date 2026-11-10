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
    Vec<(&'static str, crate::Meters, [u64; 3])>,
    Vec<lash_kernel_state::semantic::Checkpoint>,
    Vec<Request>,
    Vec<lash_kernel_exec::Weight>,
);

/// What a run shows its embedder whatever its slices: result, prints,
/// effects and charge events.
fn semantic(
    (end, printed, _, _, requests, events): Observation,
) -> (
    String,
    Vec<Datum>,
    Vec<Request>,
    Vec<lash_kernel_exec::Weight>,
) {
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
        let balance = embedder
            .machine
            .logical_balance()
            .expect("source owner balance");
        stops.push((
            kind,
            embedder.machine.meters(),
            [
                balance.entity_local_bytes,
                balance.root_and_control_bytes,
                balance.entities,
            ],
        ));
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
        lash_kernel_exec::Weight::expand(tape.steps()),
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
