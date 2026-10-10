//! The executable is a cache: how it is laid out, and which of a
//! function's implementations it runs, changes nothing a run can observe
//! (`K-CHG-001`, `K-MACH-008`).

use lash_kernel_doc::Datum;

use super::embedder::{Embedder, Setup, int, result, text};
use crate::{End, Layout, Machine, Request};

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
