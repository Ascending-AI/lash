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

fn run(source: &str, record: bool, slice: u64, bounds: Bounds, layout: Layout) -> Observation {
    run_selected(
        source,
        record,
        slice,
        bounds,
        layout,
        (super::ExecutionMode::Specialized, false),
        false,
    )
}

fn run_selected(
    source: &str,
    record: bool,
    slice: u64,
    bounds: Bounds,
    layout: Layout,
    (mode, cold): (super::ExecutionMode, bool),
    native: bool,
) -> Observation {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(if native { native_probes } else { probes }),
            bounds,
            slice,
            layout,
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if record {
        embedder.machine.record_charges(tape.clone());
    }
    let (mut stops, mut checkpoints) = (Vec::new(), Vec::new());
    let end = loop {
        match mode {
            super::ExecutionMode::Source => embedder.machine.force_source_execution(),
            super::ExecutionMode::Generic => embedder.machine.force_generic_regions(),
            super::ExecutionMode::Specialized => {}
        }
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
            embedder.reopen(layout, true);
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

/// K-CHG-001 / K-CHG-003 / K-MACH-008: accumulation retains every value, stop,
/// charge and held byte when its scalar or text guard answers or fails.
#[test]
fn accumulations_agree() {
    original_format_failure();
    original_text_body_prefixes();

    let source = scalar_document(
        r#"let s = "" let sum = 0 let i = 0 while flat.lt(i, 40) { set s = law.append(s, "é😀") set sum = flat.add(sum, 3) set i = flat.add(i, 1) } do perform 0("middle") as Any let old = s let j = 0 while flat.lt(j, 3) { set s = text.concat(s, "!") set j = flat.add(j, 1) } let huge = 9223372036854775807 try { set huge = flat.add(huge, 1) } catch err { print huge } let f = -0.0 set f = flat.add(f, 0.0) return (s, old, sum, huge, f)"#,
    );
    let without_tape = |(a, b, c, d, e, _): Observation| (a, b, c, d, e);

    let reference_body = run(
        &source,
        true,
        u64::MAX,
        super::super::embedder::ROOMY,
        Layout(1),
    );
    let run = |record, slice, bounds| run(&source, record, slice, bounds, Layout(0));

    let reference = run(true, u64::MAX, super::super::embedder::ROOMY);

    let natural = run(false, u64::MAX, super::super::embedder::ROOMY);

    assert_eq!(
        without_tape(natural.clone()),
        without_tape(reference.clone())
    );
    assert_eq!(without_tape(natural), without_tape(reference_body));

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
    for slice in [u64::MAX, 1, 7, 31] {
        let cold = slice != u64::MAX;
        let expected = run_selected(
            &source,
            false,
            slice,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, cold),
            false,
        );
        for mode in [
            super::ExecutionMode::Generic,
            super::ExecutionMode::Specialized,
        ] {
            let actual = run_selected(
                &source,
                false,
                slice,
                super::super::embedder::ROOMY,
                Layout(0),
                (mode, cold),
                false,
            );
            assert_eq!(
                actual.0, expected.0,
                "owned append result {mode:?}, slice={slice}"
            );
            assert_eq!(
                actual.1, expected.1,
                "owned append prints {mode:?}, slice={slice}"
            );
            assert_eq!(
                actual.4, expected.4,
                "owned append requests {mode:?}, slice={slice}"
            );
            assert_eq!(actual.5, expected.5, "owned append canonical charge tape");
            assert_eq!(actual.2.len(), expected.2.len(), "owned append stop count");
            for (at, (actual, expected)) in actual.2.iter().zip(&expected.2).enumerate() {
                assert_eq!(
                    actual, expected,
                    "owned append stop {at}, {mode:?}, slice={slice}"
                );
            }
            assert_eq!(
                actual.3.len(),
                expected.3.len(),
                "owned append cold boundary count"
            );
            for (at, (actual, expected)) in actual.3.iter().zip(&expected.3).enumerate() {
                assert!(
                    actual == expected,
                    "owned append cold state {at}, {mode:?}, slice={slice}"
                );
            }
            assert_eq!(
                actual, expected,
                "owned append {mode:?}, cold={cold}, slice={slice}"
            );
        }
    }
    let formatted = scalar_document(
        r#"let s = "abcdefghijklmnopé😀" let old = s let i = 0 while flat.lt(i, 3) { set s = text.concat(s, text.join(["é😀", num.to_text(-0.0), format.fixed(1.25, 2), format.radix(255, 16)], "|")) set i = flat.add(i, 1) } do perform 0(s) as Any return (s, old)"#,
    );

    for slice in [u64::MAX, 1, 7] {
        let cold = slice != u64::MAX;
        let expected = run_selected(
            &formatted,
            false,
            slice,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, cold),
            false,
        );
        for mode in [
            super::ExecutionMode::Generic,
            super::ExecutionMode::Specialized,
        ] {
            assert_eq!(
                run_selected(
                    &formatted,
                    false,
                    slice,
                    super::super::embedder::ROOMY,
                    Layout(0),
                    (mode, cold),
                    false,
                ),
                expected,
                "canonical formatted pieces {mode:?}, cold={cold}, slice={slice}"
            );
        }
    }
    // A real kind miss still takes the exact body. Accepted original text
    // prefixes now succeed, so they no longer supply this guard witness.
    let guarded = scalar_document(
        r#"try { do law.append(1, "text") } catch error { print error } return null"#,
    );
    let expected = run_selected(
        &guarded,
        false,
        u64::MAX,
        super::super::embedder::ROOMY,
        Layout(0),
        (super::ExecutionMode::Source, false),
        false,
    );
    for mode in [
        super::ExecutionMode::Generic,
        super::ExecutionMode::Specialized,
    ] {
        assert_eq!(
            run_selected(
                &guarded,
                false,
                u64::MAX,
                super::super::embedder::ROOMY,
                Layout(0),
                (mode, false),
                false,
            ),
            expected,
            "text kind guard {mode:?}"
        );
    }

    let native_formatted = scalar_document(
        r#"let s = "abcdefghijklmnopé😀" let old = s let parts = ["é😀", "tag"] let separator = "|" let i = 0 while flat.lt(i, 3) { let joined = text.join(parts, separator) set s = law.native_concat(s, joined) set i = flat.add(i, 1) } do perform 0(s) as Any return (s, old)"#,
    );

    for record in [false, true] {
        for slice in [u64::MAX, 1, 7] {
            let cold = slice != u64::MAX;
            let expected = run_selected(
                &native_formatted,
                record,
                slice,
                super::super::embedder::ROOMY,
                Layout(0),
                (super::ExecutionMode::Source, cold),
                true,
            );
            for mode in [
                super::ExecutionMode::Generic,
                super::ExecutionMode::Specialized,
            ] {
                let actual = run_selected(
                    &native_formatted,
                    record,
                    slice,
                    super::super::embedder::ROOMY,
                    Layout(0),
                    (mode, cold),
                    true,
                );

                assert_eq!(
                    actual, expected,
                    "native text source/region/cold agreement {mode:?}, slice={slice}"
                );
            }
        }
    }

    // K-CHG-001: a text completion consumes the same accepted constructor
    // prefix. Its native fee and only the remaining fixed tape settle once.
    let prefix_text = scalar_document(
        r#"let parts = ["a", "b"] let joined = text.join(parts, "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyzé😀") do perform 0(joined) as Any return joined"#,
    );
    for record in [false, true] {
        let reference = run_selected(
            &prefix_text,
            record,
            u64::MAX,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, false),
            true,
        );
        let charged = reference.2.last().unwrap().1.charged.unwrap();
        let limits = [
            super::super::embedder::ROOMY,
            Bounds {
                charge_limit: charged - 1,
                ..super::super::embedder::ROOMY
            },
            Bounds {
                charge_limit: charged,
                ..super::super::embedder::ROOMY
            },
            Bounds {
                memory: 512,
                ..super::super::embedder::ROOMY
            },
            Bounds {
                memory: 1024,
                ..super::super::embedder::ROOMY
            },
        ];
        for (limit, bounds) in limits.into_iter().enumerate() {
            for slice in [u64::MAX, 1, 7] {
                let cold = slice != u64::MAX;
                let expected = run_selected(
                    &prefix_text,
                    record,
                    slice,
                    bounds,
                    Layout(0),
                    (super::ExecutionMode::Source, cold),
                    true,
                );
                for mode in [
                    super::ExecutionMode::Generic,
                    super::ExecutionMode::Specialized,
                ] {
                    let actual = run_selected(
                        &prefix_text,
                        record,
                        slice,
                        bounds,
                        Layout(0),
                        (mode, cold),
                        true,
                    );

                    assert_eq!(
                        actual, expected,
                        "accepted text prefix {mode:?}, record={record}, slice={slice}, limit={limit}"
                    );
                }
            }
        }
    }
}

/// K-CHG-002: a declined join keeps the constructor's accepted original
/// prefix for precise source, including its charge and saved caller operands.
fn original_format_failure() {
    let source = scalar_document(
        r#"let build = fn() { let text = "old" set text = text.concat(text, text.join(["field=", 1, "; suffix"], "")) return text } return apply build()"#,
    );
    for record in [false, true] {
        let expected = run_selected(
            &source,
            record,
            u64::MAX,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, false),
            false,
        );
        for mode in [
            super::ExecutionMode::Generic,
            super::ExecutionMode::Specialized,
        ] {
            assert_eq!(
                run_selected(
                    &source,
                    record,
                    u64::MAX,
                    super::super::embedder::ROOMY,
                    Layout(0),
                    (mode, false),
                    false,
                ),
                expected,
                "original format failure, {mode:?}, taped={record}"
            );
        }
    }
}

/// K-CHG-001 / K-MACH-008: a deferred literal's accepted original close
/// continues into the actual forwarding body, with its frame and native peak.
fn original_text_body_prefixes() {
    let source = scalar_document(
        r#"let text = law.append("abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyzé😀", "é😀") do perform 0(text) as Any return text"#,
    );
    for record in [false, true] {
        let reference = run_selected(
            &source,
            record,
            u64::MAX,
            super::super::embedder::ROOMY,
            Layout(0),
            (super::ExecutionMode::Source, false),
            false,
        );
        let charged = reference.2.last().unwrap().1.charged.unwrap();
        let bounds = [
            super::super::embedder::ROOMY,
            Bounds {
                charge_limit: charged - 1,
                ..super::super::embedder::ROOMY
            },
            Bounds {
                charge_limit: charged,
                ..super::super::embedder::ROOMY
            },
            Bounds {
                memory: 512,
                ..super::super::embedder::ROOMY
            },
            Bounds {
                memory: 1024,
                ..super::super::embedder::ROOMY
            },
        ];
        for layout in [Layout(0), Layout(42)] {
            for (limit, bounds) in bounds.into_iter().enumerate() {
                for slice in [u64::MAX, 1, 7] {
                    let cold = slice != u64::MAX;
                    let expected = run_selected(
                        &source,
                        record,
                        slice,
                        bounds,
                        layout,
                        (super::ExecutionMode::Source, cold),
                        false,
                    );
                    for mode in [
                        super::ExecutionMode::Generic,
                        super::ExecutionMode::Specialized,
                    ] {
                        let actual = run_selected(
                            &source,
                            record,
                            slice,
                            bounds,
                            layout,
                            (mode, cold),
                            false,
                        );

                        assert_eq!(
                            actual, expected,
                            "original body text {mode:?}, record={record}, slice={slice}, limit={limit}, layout={}",
                            layout.0
                        );
                        // Layout 42 deliberately splits original charge regions;
                        // its deferred literal may end before the call. Default
                        // placement must execute this compound original prefix.
                    }
                }
            }
        }
    }
}
