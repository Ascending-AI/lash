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
                entry: |heap, args, _| heap.call(0, args),
                depth: 1,
                writes: false,
                element_write: false,
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

type Observation = (
    String,
    Vec<Datum>,
    Vec<(&'static str, crate::Meters)>,
    Vec<lash_kernel_state::semantic::Checkpoint>,
);

fn run(source: &str, record: bool, slice: u64, bounds: Bounds, layout: Layout) -> Observation {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(probes),
            bounds,
            slice,
            layout,
            ..Setup::default()
        },
    );
    if record {
        embedder.machine.record_charges(crate::ChargeTape::new());
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
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("finished {:?}", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                embedder.deliver_next();
            }
        }
    };
    (end, embedder.world.printed, stops, checkpoints)
}

/// K-CHG-001 / K-CHG-003 / K-MACH-008: accumulation retains every value, stop,
/// charge and held byte when its scalar or text guard answers or fails.
#[test]
fn accumulations_agree() {
    let source = scalar_document(
        r#"let s = "" let sum = 0 let i = 0 while flat.lt(i, 40) { set s = law.append(s, "é😀") set sum = flat.add(sum, 3) set i = flat.add(i, 1) } do perform 0("middle") as Any let old = s let j = 0 while flat.lt(j, 3) { set s = text.concat(s, "!") set j = flat.add(j, 1) } let huge = 9223372036854775807 try { set huge = flat.add(huge, 1) } catch err { print huge } let f = -0.0 set f = flat.add(f, 0.0) return (s, old, sum, huge, f)"#,
    );
    let taken = || crate::machine::islands::ACCUMULATED.with(|n| n.replace(0));
    let reference_body = run(
        &source,
        true,
        u64::MAX,
        super::super::embedder::ROOMY,
        Layout(1),
    );
    let run = |record, slice, bounds| run(&source, record, slice, bounds, Layout(0));
    taken();
    let reference = run(true, u64::MAX, super::super::embedder::ROOMY);
    crate::machine::islands::TEXT_APPENDED.with(|n| n.set(0));
    let natural = run(false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(natural, reference);
    assert_eq!(natural, reference_body);
    assert!(
        taken() > 0,
        "accumulation must run as one register instruction"
    );
    assert!(
        crate::machine::islands::TEXT_APPENDED.with(|n| n.get()) > 0,
        "the helper append runs directly"
    );
    for slice in [1, 2, 3, 5, 11, 31, 67] {
        assert_eq!(
            run(false, slice, super::super::embedder::ROOMY),
            run(true, slice, super::super::embedder::ROOMY),
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
            run(false, u64::MAX, bounds),
            run(true, u64::MAX, bounds),
            "charge {limit}"
        );
    }
    for memory in [4096, 16384, 65536, 131072] {
        let bounds = Bounds {
            memory,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            run(false, u64::MAX, bounds),
            run(true, u64::MAX, bounds),
            "memory {memory}"
        );
    }
}
