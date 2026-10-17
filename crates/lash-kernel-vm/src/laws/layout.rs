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
fn worker(tag, counter) {
  let total = 0
  for step in [1, 2] {
    try {
      let got = perform echo(tag) as Any
      set total = num.add(total, step)
      do apply counter()
    } finally {
      print (tag, step)
    }
  }
  return (tag, total)
}
fn make_counter(counts) {
  let n = 0
  let bump = fn() {
    set n = num.add(n, 1)
    set counts["calls"] = n
    return n
  }
  return bump
}
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

type Observed = (End, Vec<Datum>, Vec<Request>, u64);

fn observe(setup: Setup) -> Observed {
    let mut embedder = Embedder::with(PROGRAM, setup);
    let end = embedder.run_to_end(&["c", "a", "b", "b", "c", "a"]);
    (
        end,
        embedder.world.printed,
        embedder.requests,
        embedder.machine.meters().charged,
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

/// PLAN-INDEP for field caches: warm a read site, fail its position guard
/// on a different record, then delete, reinsert and mutate its field.
/// Errors, effects and the memory refusal retain their order and charge,
/// also with every safepoint reopened under a different, cold plan.
#[test]
fn field_hints_are_exact_across_guard_failures_and_cold_plans() {
    const READS: &str = r#"
fn fetch(obj) { return obj.value }
fn fetch_key(obj, key) { return obj[key] }
fn sibling() { let reply = perform echo("peer") as Any return reply }
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
  let held = (num.add(call fetch(a), 1), perform echo("read") as Any)
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
                break match end {
                    End::Finished(finished) => format!("finished {:?}", finished.result),
                    other => format!("{other:?}"),
                };
            }
            if matches!(step, Step::Parked(_)) {
                continuations.push(embedder.machine.canonical_continuation_bytes());
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
            embedder.machine.meters().charged,
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
    for program in [
        READS.to_owned(),
        READS.replace("return call fetch(b)", "return work.fill(50000)"),
    ] {
        let bounds = Bounds {
            memory: 1 << 18,
            ..super::embedder::ROOMY
        };
        let reference = observe(&program, bounds, Layout(0), false);
        for layout in [
            Layout(0),
            Layout(1),
            Layout(2),
            Layout(0x5eed),
            Layout(u64::MAX),
        ] {
            if layout != Layout(0) {
                assert_eq!(
                    observe(&program, bounds, layout, false),
                    reference,
                    "layout {layout:?}"
                );
            }
            assert_eq!(
                observe(&program, bounds, layout, true),
                reference,
                "cold layout {layout:?}"
            );
        }
    }
}

/// `K-CHG-003`: the executable computes a charge formula as the formula
/// states it, in saturating unsigned arithmetic with an empty sum 0 and an
/// empty product 1, however it groups the terms, folds the constants or
/// reuses a measurement the formula takes twice.
#[test]
fn a_compiled_formula_charges_what_the_formula_states() {
    use lash_kernel_doc::{Formula, Measure, Name, Operand, Param, Type};

    use crate::compile::{Plan, Source};

    let params: Vec<Param> = ["a", "b"]
        .into_iter()
        .map(|name| Param {
            name: Name::new(name),
            ty: Type::Any,
            optional: false,
        })
        .collect();
    let param = |name: &str| Operand::Param(Name::new(name));
    let deep = |name: &str| Formula::DeepSize(param(name));
    let size = |name: &str| Formula::Size(param(name));
    let result = Formula::DeepSize(Operand::Result);
    let input = Formula::Sum(vec![deep("a"), deep("b")]);
    let near = u64::MAX - 1;
    let formulas = [
        Formula::Constant(7),
        Formula::CeilLog2(Box::new(Formula::Sum(vec![input.clone(), result.clone()]))),
        Formula::Product(vec![size("a"), Formula::CeilLog2(Box::new(deep("a")))]),
        Formula::CeilLog2(Box::new(Formula::CeilLog2(Box::new(deep("a"))))),
        Formula::Sum(vec![]),
        Formula::Product(vec![]),
        Formula::Max(vec![]),
        Formula::Min(vec![]),
        Formula::Sum(vec![Formula::Constant(1), input.clone(), result.clone()]),
        Formula::Sum(vec![
            Formula::Constant(1),
            Formula::Product(vec![input.clone(), input.clone()]),
            result.clone(),
        ]),
        Formula::Sum(vec![
            Formula::Constant(1),
            Formula::Min(vec![size("a"), size("b")]),
            Formula::NestedSize(param("a")),
            Formula::NestedSize(param("b")),
            result.clone(),
        ]),
        Formula::Sum(vec![
            Formula::Constant(near),
            Formula::Sum(vec![Formula::Constant(near), deep("a")]),
        ]),
        Formula::Product(vec![
            Formula::Constant(near),
            Formula::Constant(0),
            Formula::Magnitude(param("b")),
        ]),
        Formula::Product(vec![
            Formula::Product(vec![deep("a"), Formula::Constant(3)]),
            Formula::Constant(near),
        ]),
        Formula::Max(vec![
            deep("missing"),
            Formula::Sum(vec![deep("a"), deep("a")]),
            Formula::Min(vec![result.clone()]),
        ]),
        Formula::Sum(vec![deep("missing"), Formula::Magnitude(param("a"))]),
    ];
    let amounts: [(u64, u64, u64); 4] = [(0, 0, 0), (2, 5, 9), (u64::MAX, 1, 3), (1, u64::MAX, 0)];
    for formula in &formulas {
        let plan = Plan::new(formula, &params);
        let entry = lash_kernel_doc::PreparedNative {
            entry: |_| Some(lash_kernel_doc::Value::Bool(true)),
            operation: "law.scalar_formula",
        };
        if let Some(scalar) = crate::compile::ScalarCall::new(&entry, formula, &plan) {
            let stated = formula.evaluate(&mut |operand, measure| {
                if matches!(operand, Operand::Param(name) if !params.iter().any(|param| param.name == *name)) { return 0; }
                match measure { Measure::Size | Measure::DeepSize => 1, Measure::NestedSize => 0, Measure::Magnitude => unreachable!("value-dependent formulas stay generic") }
            });
            assert_eq!(scalar.units, stated, "scalar version of {formula:?}");
        }
        for (a, b, returned) in amounts {
            // Each operand and measure gives its own amount.
            let amount = |index: usize, measure: Measure| {
                let base = [a, b, returned][index];
                match measure {
                    Measure::Size => base / 2,
                    Measure::DeepSize => base,
                    Measure::NestedSize => base / 3,
                    Measure::Magnitude => base.saturating_add(4),
                }
            };
            let stated = formula.evaluate(&mut |operand, measure| match operand {
                Operand::Result => amount(2, measure),
                Operand::Param(name) => params
                    .iter()
                    .position(|param| param.name == *name)
                    .map_or(0, |index| amount(index, measure)),
            });
            let compiled = plan.evaluate(|source, measure| match source {
                Source::Arg(index) => amount(index, measure),
                Source::Result => amount(2, measure),
                Source::Nothing => unreachable!("a missing operand measures nothing"),
            });
            assert_eq!(compiled, stated, "{formula:?} over {a}, {b}, {returned}");
        }
    }
}

/// K-SES-001 / K-MACH-008: inline and digested binding keys name the same
/// state under every layout, including after rebuilding cold lookup caches.
#[test]
fn session_binding_images_are_identical_across_layouts_and_cold_caches() {
    use lash_kernel_doc::graph_tokens::encode_body;

    let long = "binding".repeat(100);
    let declarations = (0..70)
        .map(|i| format!("let v{i} = {i}\n"))
        .collect::<String>();
    let source = format!(
        "main {{ {declarations} let {long} = 7 \
         set v0 = {long} set v69 = v0 \
         let reply = perform echo(v69) as Any \
         set {long} = reply set v0 = {long} \
         let again = perform echo(v0) as Any \
         set {long} = again return {long} }}"
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
            images.push(encode_body(
                &embedder.reopen(Layout(layout ^ (hop + 1)), true),
            ));
            embedder.deliver("echo");
        }
        let End::Finished(finished) = embedder.run_to_end(&[]) else {
            panic!("the binding program must finish");
        };
        assert_eq!(finished.result, int(7));
        let carry = finished.carry.expect("a session cell carries bindings");
        let mut heap = lash_kernel_state::paged::PagedHeap::open_header(
            carry.heap,
            Box::new(embedder.source.clone()),
            lash_kernel_state::paged::EditEpoch(2),
        )
        .unwrap();
        lash_kernel_state::paged::HeapLifecycle::adopt(&mut heap, &carry).unwrap();
        for name in ["v0", "v69", long.as_str()] {
            assert!(
                heap.session_binding(&lash_kernel_doc::Name::new(name))
                    .unwrap()
                    .is_some(),
                "the prepared key must equal the canonical key for {name}"
            );
        }
        images.push(encode_body(&carry));
        (images, embedder.machine.meters().charged)
    };
    let reference = observe(0);
    for layout in [1, 42, u64::MAX] {
        assert_eq!(observe(layout), reference, "layout {layout}");
    }
}
