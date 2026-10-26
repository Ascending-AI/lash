//! Call-owned frame entry preserves source operands and continuations.

use super::*;

#[derive(Debug, PartialEq)]
struct Observed {
    end: String,
    printed: Vec<Datum>,
    requests: Vec<Request>,
    stops: Vec<(&'static str, crate::Meters)>,
    checkpoints: Vec<crate::conformance::Checkpoint>,
    events: Vec<lash_kernel_exec::Weight>,
}

fn observe(source: &str, layout: Layout, slice: u64, bounds: Bounds, taped: bool) -> Observed {
    let mut embedder = Embedder::with(
        source,
        Setup {
            layout,
            slice,
            bounds,
            probes: Some(scalar_probes),
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if taped {
        embedder.machine.record_charges(tape.clone());
    }
    let (mut stops, mut checkpoints) = (Vec::new(), Vec::new());
    let mut register_statements = 0;
    let end = loop {
        if !taped {
            embedder.machine.count_execution();
        }
        let step = embedder.run();
        if !taped {
            register_statements += embedder
                .machine
                .take_execution_census()
                .statements
                .get(&crate::perf_witness::execution::Owner::RegisterProgram)
                .copied()
                .unwrap_or(0);
        }
        stops.push((
            match &step {
                Step::Ended(_) => "ended",
                Step::Slice => "slice",
                Step::Parked(_) => "parked",
            },
            embedder.machine.meters(),
        ));
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("finished {:?}", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                embedder.reopen(layout, true);
                if taped {
                    embedder.machine.record_charges(tape.clone());
                }
                embedder.deliver_next();
            }
        }
    };
    if !taped {
        assert!(
            register_statements > 0,
            "paired call cases must run register statements: {source}"
        );
    }
    Observed {
        end,
        printed: embedder.world.printed,
        requests: embedder.requests,
        stops,
        checkpoints,
        events: lash_kernel_exec::Weight::expand(tape.steps()),
    }
}

/// K-EVAL-008 / K-CHG-001 / K-MACH-008: pushing a frame inside its call
/// instruction preserves every completed operand across an admission
/// refusal or parked callee. Effectful operands keep their source evaluation
/// order; alternating closures retain their own cells. Values, charge events,
/// finite stop points, held bytes and parked images agree with the general
/// layout, including arity, type, depth, memory and missing-binding exits.
#[test]
fn direct_call_entry_agrees() {
    let definition = |signature: &str, body: &str| {
        let function = format!(
            "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        );
        format!("definition {}\n", serde_json::to_string(&function).unwrap())
    };
    let definitions = format!(
        "{}{}{}{}",
        definition("literal(x: Any)", "return 1"),
        definition("pair(a: Any, b: Any)", "return (a, b)"),
        definition("pause(x: Any)", "do perform 0(x) as Any return x"),
        definition("typed(x: Int)", "return x"),
    );
    let mut comparisons = 0;
    for body in [
        "let f = fn(x) { return 1 } return apply f(7)",
        "let n = 0 let f = fn(x) { set n = flat.add(n, 1) let g = fn(y) { return call pair(y, n) } return apply g(x) } return invoke each.map(f, [1, 2, 3])",
        "let n = 0 let f = fn(x) { set n = flat.add(n, 1) let answer = call pause(x) return call pair(answer, n) } return invoke each.map(f, [1, 2])",
        "let f = fn(x) { try { return call pause(x) } finally { do flat.add(x, 1) } } return invoke each.map(f, [1, 2])",
        "let x = 3 return call pair(x, 4)",
        "let f = fn(a, b) { return (a, b) } return apply f(3, 4)",
        "let f = fn(a, b) { return (a, b) } return apply f(3)",
        "let f = fn(a, b) { return (a, b) } return apply f(1, 2, 3)",
        "let bad = 3 return apply bad(4)",
        "let f = fn(x) { return call typed(x) } return apply f(false)",
        "return call pair(1, call pause(2))",
        "return call pair(call pause(1), 2)",
        "let f = fn(a, b) { return (a, b) } let g = fn() { set f = fn(x) { return 1 } return 8 } return apply f(apply g(), 4)",
        "let f = fn(a, b) { return (a, b) } return apply f(1, missing)",
        "let recurse = fn(self, n) { if flat.lt(n, 1) { return call pause(n) } return flat.add(n, apply self(self, flat.add(n, -1))) } return apply recurse(recurse, 4)",
        "let a = 0 let b = 100 let f = fn(x) { set a = flat.add(a, x) do perform 0(a) as Any return a } let g = fn(x) { set b = flat.add(b, x) return b } let first = apply f(2) let second = apply g(3) return (first, second, apply f(4))",
    ] {
        let source = format!("{definitions}main {{ {body} }}");
        let bounds = super::super::embedder::ROOMY;
        let reference = observe(&source, Layout(1), u64::MAX, bounds, true);
        let natural = observe(&source, Layout(0), u64::MAX, bounds, true);
        assert_eq!(natural, reference, "{body}");
        if body == "let f = fn(x) { return 1 } return apply f(7)" {
            assert_eq!(natural.end, format!("finished {:?}", int(1)));
        }
        let plain = observe(&source, Layout(0), u64::MAX, bounds, false);
        assert_eq!(plain.end, reference.end, "untaped result: {body}");
        assert_eq!(
            plain,
            observe(&source, Layout(1), u64::MAX, bounds, false),
            "untaped: {body}"
        );
        for slice in [1, 2, 3, 5, 11, 31] {
            assert_eq!(
                observe(&source, Layout(0), slice, bounds, true),
                observe(&source, Layout(1), slice, bounds, true),
                "slice {slice}: {body}"
            );
            comparisons += 1;
        }
        let charged = reference.stops.last().unwrap().1.charged.unwrap();
        for limit in 0..charged {
            let bounds = Bounds {
                charge_limit: limit,
                ..bounds
            };
            assert_eq!(
                observe(&source, Layout(0), u64::MAX, bounds, true),
                observe(&source, Layout(1), u64::MAX, bounds, true),
                "limit {limit}: {body}"
            );
            comparisons += 1;
        }
        for memory in [8192, 16384] {
            let bounds = Bounds { memory, ..bounds };
            assert_eq!(
                observe(&source, Layout(0), u64::MAX, bounds, true),
                observe(&source, Layout(1), u64::MAX, bounds, true),
                "memory {memory}: {body}"
            );
            comparisons += 1;
        }
        let bounds = Bounds {
            call_depth: 3,
            ..bounds
        };
        assert_eq!(
            observe(&source, Layout(0), u64::MAX, bounds, true),
            observe(&source, Layout(1), u64::MAX, bounds, true),
            "depth: {body}"
        );
    }
    println!("direct call entry: {comparisons} paired runs");
}
