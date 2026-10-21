//! The executable is a cache: how it is laid out, and which of a
//! function's implementations it runs, changes nothing a run can observe
//! (`K-CHG-001`, `K-MACH-008`).

use lash_kernel_doc::Datum;

use super::embedder::{Embedder, Setup, int, result, text};
use crate::{Bounds, End, Layout, Machine, Request, Step};

/// A program that uses closures over loop variables, shared variables,
/// tasks, effects in loops, cleanup blocks, maps and library calls of
/// every kind.
const PROGRAM: &str = r#"
definition "function worker(tag: Any, counter: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  let total = 0\n  for step in [1, 2] {\n    try {\n      let got = perform 0(tag) as Any\n      set total = num.add(total, step)\n      do apply counter()\n    } finally {\n      print (tag, step)\n    }\n  }\n  return (tag, total)\n }"

definition "function make_counter(counts: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  let n = 0\n  let bump = fn() {\n    set n = num.add(n, 1)\n    set counts[\"calls\"] = n\n    return n\n  }\n  return bump\n }"

main {
  let counts = map{"calls": 0}
  let counter = call make_counter(counts)
  let hs = []
  let labels = []
  for tag in ["a", "b", "c"] {
    let h = spawn call worker(tag, counter)
    set hs[list.len(hs)] = h
    set labels[list.len(labels)] = fn() { return text.concat(tag, "!") }
  }
  let results = join all hs
  let shouted = []
  for label in labels {
    let s = apply label()
    set shouted[list.len(shouted)] = s
  }
  let doubled = invoke pair.twice(21)
  let add_one = fn(x) { return num.add(x, 1) }
  let eight = pair.twice(4)
  let stepped = invoke each.twice(add_one, eight)
  return (results, shouted, counts["calls"], doubled, stepped)
}
"#;

type Observed = (
    End,
    Vec<Datum>,
    Vec<Request>,
    u64,
    Vec<crate::conformance::Checkpoint>,
);

fn observe(setup: Setup) -> Observed {
    let mut embedder = Embedder::with(PROGRAM, setup);
    let mut order = ["c", "a", "b", "b", "c", "a"].into_iter();
    let mut checkpoints = Vec::new();
    let mut end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => (),
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                let next = order
                    .next()
                    .map(str::to_owned)
                    .unwrap_or_else(|| super::embedder::label(&embedder.pending[0]));
                embedder.deliver(&next);
            }
        }
    };
    if let End::Finished(finished) = &mut end
        && let Some(carry) = finished.carry.take()
    {
        checkpoints
            .push(crate::conformance::session(&carry, Box::new(embedder.source.clone())).unwrap());
    }
    (
        end,
        embedder.world.printed,
        embedder.requests,
        embedder.machine.meters().charged.expect("exact charge"),
        checkpoints,
    )
}

/// The same document compiled under different layouts gives the same
/// values, effect identities and charges.
#[test]
fn every_layout_runs_a_document_the_same_way() {
    let natural = observe(Setup::default());
    let row = |tag: &str| Datum::Tuple(vec![text(tag), int(3)]);
    assert_eq!(
        result(natural.0.clone()),
        Datum::Tuple(vec![
            Datum::List(vec![row("a"), row("b"), row("c")]),
            Datum::List(vec![text("a!"), text("b!"), text("c!")]),
            int(6),
            int(42),
            int(10),
        ])
    );
    assert_eq!(natural.2.len(), 6);
    for seed in [1, 2, 3, 0x5eed, u64::MAX] {
        let shuffled = observe(Setup {
            layout: Layout(seed),
            ..Setup::default()
        });
        assert_eq!(shuffled, natural, "layout {seed}");
    }
}

/// `K-CHG-001`, `K-CHG-007`: a function is charged its formula whether its
/// native implementation or its kernel body runs, and gives the same
/// values.
#[test]
fn a_native_implementation_and_a_kernel_body_run_the_same() {
    let native = observe(Setup::default());
    let body = observe(Setup {
        native_twice: false,
        ..Setup::default()
    });
    assert_eq!(body, native);
}

/// PLAN-INDEP for per-site property caches: warm a read site, fail its guard
/// on a different record, then delete, reinsert and mutate its field.
/// Errors and effects retain their order and charge,
/// also with every safepoint reopened under a different, cold plan.
#[test]
fn property_caches_are_exact_across_guard_failures_and_cold_plans() {
    const READS: &str = r#"
definition "function fetch(obj: Any) -> Any\nkernel 1\nmetering statements\nbody {  return obj.value  }"

definition "function fetch_key(obj: Any, key: Any) -> Any\nkernel 1\nmetering statements\nbody {  return obj[key]  }"

definition "function sibling() -> Any\nkernel 1\nmetering statements\nbody {  let reply = perform 0(\"peer\") as Any return reply  }"

main {
  let peer = spawn call sibling()
  let a = {value: 10, other: 90}
  let b = {other: 80, value: 20}
  print call fetch(a)
  print call fetch(b)
  print call fetch_key(a, "other")
  print call fetch_key(b, "other")
  print call fetch_key(b, "value")
  print call fetch_key([40, 50], 0)
  let held = (num.add(call fetch(a), 1), perform 0("read") as Any)
  print held[0]
  let reply = held[1]
  remove a.value
  print call fetch(a)
  set a.value = reply
  print call fetch(a)
  set a.value = 30
  print call fetch(a)
  try { do call fetch(null) } catch err { print err.kind }
  let joined = join peer
  return call fetch(b)
}
"#;
    let observe = |program: &str, bounds: Bounds, layout: Layout, cold: bool| {
        let mut embedder = Embedder::with(
            program,
            Setup {
                bounds,
                layout,
                slice: if cold { 1 } else { u64::MAX },
                ..Setup::default()
            },
        );
        let mut plans = [Layout(0), Layout(2), Layout(1), Layout(0x5eed)]
            .into_iter()
            .cycle();
        let mut continuations = Vec::new();
        let end = loop {
            let step = embedder.run();
            if let Step::Ended(end) = step {
                if let End::Finished(finished) = &end
                    && let Some(carry) = &finished.carry
                {
                    continuations.push(
                        crate::conformance::session(carry, Box::new(embedder.source.clone()))
                            .unwrap(),
                    );
                }
                break match end {
                    End::Finished(finished) => format!("finished {:?}", finished.result),
                    other => format!("{other:?}"),
                };
            }
            if matches!(step, Step::Parked(_)) {
                continuations.push(embedder.machine.semantic_checkpoint());
            }
            if cold {
                embedder.reopen(plans.next().unwrap(), true);
            }
            if matches!(step, Step::Parked(_)) {
                embedder.deliver_next();
            }
        };
        (
            end,
            embedder.world.printed,
            embedder.requests,
            embedder.machine.meters().charged.expect("exact charge"),
            continuations,
        )
    };
    let natural = observe(READS, super::embedder::ROOMY, Layout(0), false);
    assert_eq!(natural.0, format!("finished {:?}", int(20)));
    assert_eq!(
        natural.1,
        vec![
            int(10),
            int(20),
            int(90),
            int(80),
            int(20),
            int(40),
            int(11),
            Datum::Absent,
            text("read"),
            int(30),
            text("type_error")
        ]
    );
    {
        let program = READS;
        let bounds = Bounds {
            memory: 1 << 18,
            ..super::embedder::ROOMY
        };
        let reference = observe(program, bounds, Layout(0), false);
        for layout in [
            Layout(0),
            Layout(1),
            Layout(2),
            Layout(0x5eed),
            Layout(u64::MAX),
        ] {
            if layout != Layout(0) {
                assert_eq!(
                    observe(program, bounds, layout, false),
                    reference,
                    "layout {layout:?}"
                );
            }
            assert_eq!(
                observe(program, bounds, layout, true),
                reference,
                "cold layout {layout:?}"
            );
        }
    }
}

/// K-SES-001 / K-MACH-008: inline and digested binding keys name the same
/// state under every layout, including after rebuilding cold lookup caches.
#[test]
fn session_binding_images_are_identical_across_layouts_and_cold_caches() {
    let long = "binding".repeat(100);
    let declarations = (0..70)
        .map(|i| format!("let v{i} = {i}\n"))
        .collect::<String>();
    let source = format!(
        "main {{ {declarations} let {long} = 7 set v0 = {long} set v69 = v0 let reply = perform 0(v69) as Any set {long} = reply set v0 = {long} let again = perform 0(v0) as Any set {long} = again return {long} }}"
    );
    let observe = |layout| {
        let mut embedder = Embedder::with(
            &source,
            Setup {
                layout: Layout(layout),
                ..Setup::default()
            },
        );
        let mut images = Vec::new();
        for hop in 0..2 {
            assert!(matches!(embedder.run(), Step::Parked(_)));
            embedder.reopen(Layout(layout ^ (hop + 1)), true);
            images.push(embedder.machine.semantic_checkpoint());
            embedder.deliver("echo");
        }
        let End::Finished(finished) = embedder.run_to_end(&[]) else {
            panic!("the binding program must finish");
        };
        assert_eq!(finished.result, int(7));
        let carry = finished.carry.expect("a session cell carries bindings");
        let mut heap = lash_kernel_state::kernel_internals::PagedHeap::open_header(
            lash_kernel_state::kernel_internals::session_heap(&carry),
            Box::new(embedder.source.clone()),
            lash_kernel_state::paged::EditEpoch(2),
        )
        .unwrap();
        lash_kernel_state::paged::HeapLifecycle::adopt(&mut heap, &carry).unwrap();
        for name in ["v0", "v69", long.as_str()] {
            assert!(
                heap.session_binding(&lash_vocabulary::legacy_names::Name::new(name))
                    .unwrap()
                    .is_some(),
                "the prepared key must equal the canonical key for {name}"
            );
        }
        images
            .push(crate::conformance::session(&carry, Box::new(embedder.source.clone())).unwrap());
        (
            images,
            embedder.machine.meters().charged.expect("exact charge"),
        )
    };
    let reference = observe(0);
    for layout in [1, 42, u64::MAX] {
        assert_eq!(observe(layout), reference, "layout {layout}");
    }
}
