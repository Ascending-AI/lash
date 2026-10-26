//! Guarded reads run their source instructions' path whole.

use super::*;

/// Loops read a plain record's field and a list's item behind
/// TypeScript's guards: a kind test, then a unary test of the record's
/// membership of the brand key or a bounds test of the list's length.
/// Branded, short, non-collection and refreshed operands fail a guard;
/// finite slices and charge limits end inside the guarded path; a park
/// separates the calls.
const GUARDED: &str = r#"
definition "function read(obj: Any, rounds: Any) -> Any\nkernel 1\nmetering statements\nbody { let sum = 0 let i = 0 while flat.lt(i, rounds) { let t = absent if same(kind(obj), \"record\") { if bool.not(record.contains(obj, \"__lash_brand\")) { set t = obj.a } else { set t = 100 } } else { set t = 200 } set sum = flat.add(sum, t) set i = flat.add(i, 1) } return sum }"
definition "function item(xs: Any, rounds: Any) -> Any\nkernel 1\nmetering statements\nbody { let sum = 0 let i = 0 while flat.lt(i, rounds) { let t = absent if same(kind(xs), \"list\") { if flat.lt(1, list.len(xs)) { set t = xs[1] if same(t, ()) { set t = 0 } } else { set t = 100 } } else { set t = 200 } set sum = flat.add(sum, t) set i = flat.add(i, 1) } return sum }"
main {
  let plain = {a: 1, b: 2}
  let branded = {__lash_brand: "x", a: 5}
  let xs = [3, 4]
  let first = call read(plain, 9)
  set plain.a = 4
  let second = call read(plain, 5)
  let marked = call read(branded, 4)
  let other = call read(7, 3)
  let items = call item(xs, 6)
  let short = call item([3], 3)
  let text = call item("ab", 2)
  do perform 0("park") as Any
  set xs[1] = 6
  let again = (call read(plain, 3), call item(xs, 3))
  return (first, second, marked, other, items, short, text, again)
}
"#;

fn guarded_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_vocabulary::names::FunctionId>,
) -> Vec<(&'static str, lash_vocabulary::names::FunctionId)> {
    use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};
    let mut probes = call_probes(registry, ids);
    struct Not;
    impl Not {
        fn answer(args: &[Value]) -> Option<Value> {
            match args {
                [Value::Bool(value)] => Some(Value::Bool(!value)),
                _ => None,
            }
        }
    }
    impl NativeFunction for Not {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            Self::answer(call.args.values())
                .ok_or_else(|| NativeError::refused("bool", "not a bool"))
        }
        fn prepare(&self) -> Option<PreparedNative> {
            Some(PreparedNative {
                pair: None,
                entry: Self::answer,
                charged_entry: None,
                operation: "law.not",
            })
        }
    }
    for (name, rest, native) in [
        (
            "bool.not",
            "(x: Any) -> Bool\nkernel 1\ncharge sum(2, size(x))",
            std::sync::Arc::new(Not) as std::sync::Arc<dyn NativeFunction>,
        ),
        (
            "record.contains",
            "(xs: Any, key: Any) -> Bool\nkernel 1\ncharge sum(1, size(xs), size(key))",
            lash_kernel_lib::native("record.contains").unwrap(),
        ),
        (
            "list.get",
            "(xs: List(Any), index: Number) -> Any\nkernel 1\ncharge sum(1, size(index))",
            lash_kernel_lib::native("list.get").unwrap(),
        ),
        (
            "tuple.get",
            "(xs: Any, index: Number) -> Any\nkernel 1\ncharge sum(1, size(index))",
            lash_kernel_lib::native("tuple.get").unwrap(),
        ),
    ] {
        let definition =
            lash_kernel_doc::parse_definition(&format!("function {name}{rest}\nnative\n")).unwrap();
        probes.push((name, registry.register(definition, Some(native)).unwrap()));
    }
    probes
}

/// Each stop point's step and meters, the parks' checkpoints and the end.
type Guarded = (
    String,
    Vec<Datum>,
    Vec<(&'static str, crate::Meters)>,
    Vec<crate::conformance::Checkpoint>,
);

/// A run of [`GUARDED`]; `record` keeps a charge tape, which no guarded
/// read runs under: the reference.
fn guarded(record: bool, slice: u64, bounds: Bounds) -> Guarded {
    guarded_run(GUARDED, record, slice, bounds)
}

fn guarded_run(source: &str, record: bool, slice: u64, bounds: Bounds) -> Guarded {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(guarded_probes),
            bounds,
            slice,
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

/// K-CHG-001 / K-MACH-008: a guarded read's path gives the values,
/// charges, stop points, held bytes and checkpoints its source
/// instructions give, at every finite slice and at charge limits that end
/// inside it; a guard that fails (a branded record, a short list, another
/// kind, an unfilled kind memo, a slice or limit ending in the path)
/// leaves its instructions to run.
#[test]
fn guarded_reads_agree() {
    let reads = || crate::machine::islands::GUARDED_READS.with(|n| n.replace(0));
    reads();
    let reference = guarded(true, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(reads(), 0, "a taped run takes no guarded read");
    assert_eq!(
        reference.0,
        format!(
            "finished {:?}",
            Datum::Tuple(vec![
                int(9),
                int(20),
                int(400),
                int(600),
                int(24),
                int(300),
                int(400),
                Datum::Tuple(vec![int(12), int(18)]),
            ])
        )
    );
    crate::machine::islands::DIRECT_FIELDS.with(|n| n.set(0));
    let natural = guarded(false, u64::MAX, super::super::embedder::ROOMY);
    assert!(crate::machine::islands::DIRECT_FIELDS.with(|n| n.get()) > 0);
    assert!(reads() > 0, "the plain run takes guarded reads");
    // The list path alone: its length test and item read run whole too.
    let items = GUARDED
        .lines()
        .find(|line| line.contains("function item"))
        .map(|item| format!("{item}\nmain {{ return call item([3, 4], 6) }}\n"))
        .unwrap();
    crate::machine::islands::DIRECT_INDICES.with(|n| n.set(0));
    let listed = guarded_run(&items, false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(listed.0, format!("finished {:?}", int(24)));
    assert!(reads() > 0, "the plain run takes guarded item reads");
    assert!(crate::machine::islands::DIRECT_INDICES.with(|n| n.get()) > 0);
    assert_eq!(natural, reference);
    let charged = reference.2.last().unwrap().1.charged.unwrap();
    let mut comparisons = 1;
    for slice in [1, 2, 3, 5, 7, 11, 13, 17, 23, 31] {
        assert_eq!(
            guarded(false, slice, super::super::embedder::ROOMY),
            guarded(true, slice, super::super::embedder::ROOMY),
            "slice {slice}"
        );
        comparisons += 1;
    }
    for limit in (charged / 2..charged).step_by(7) {
        let bounds = Bounds {
            charge_limit: limit,
            ..super::super::embedder::ROOMY
        };
        let ended = guarded(false, u64::MAX, bounds);
        assert!(ended.0.contains("Bound"), "limit {limit}: {}", ended.0);
        assert_eq!(ended, guarded(true, u64::MAX, bounds), "limit {limit}");
        comparisons += 1;
    }
    println!("guarded reads: {comparisons} paired runs");
}

/// K-CHG-001 / K-MACH-008: guarded member writes, including append at
/// length, commit once with the source charge and stop coordinate. A
/// branded record, wrong kind or out-of-range index leaves the source
/// instructions in charge; slices and limits can stop before the write.
#[test]
fn guarded_writes_agree() {
    const WRITES: &str = r#"
definition "function write(obj: Any, xs: Any, rounds: Any) -> Any\nkernel 1\nmetering statements\nbody { let i = 0 while flat.lt(i, rounds) { if same(kind(obj), \"record\") { if bool.not(record.contains(obj, \"__lash_brand\")) { set obj.a = i } } if same(kind(xs), \"list\") { if flat.lt(i, list.len(xs)) { set xs[i] = i } } set i = flat.add(i, 1) } return (obj, xs) }"
main {
  let obj = {a: 9}
  let xs = [8, 8, 8]
  let first = call write(obj, xs, 5)
  let branded = call write({__lash_brand: true, a: 7}, [], 3)
  let wrong = call write(42, 42, 3)
  do perform 0("park") as Any
  set xs[list.len(xs)] = {a: 4}
  let again = call write(obj, xs, 3)
  return (first, branded, wrong, again)
}
"#;
    let reads = || crate::machine::islands::GUARDED_READS.with(|n| n.replace(0));
    reads();
    let reference = guarded_run(WRITES, true, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(reads(), 0, "a taped run takes no guarded write");
    assert!(reference.0.starts_with("finished"), "{}", reference.0);
    let natural = guarded_run(WRITES, false, u64::MAX, super::super::embedder::ROOMY);
    assert!(reads() > 0, "the plain run takes guarded writes");
    assert_eq!(natural, reference);
    for slice in [1, 2, 3, 5, 7, 11, 13, 17, 23, 31] {
        assert_eq!(
            guarded_run(WRITES, false, slice, super::super::embedder::ROOMY),
            guarded_run(WRITES, true, slice, super::super::embedder::ROOMY),
            "slice {slice}"
        );
    }
    let charged = reference.2.last().unwrap().1.charged.unwrap();
    for limit in (charged / 2..charged).step_by(7) {
        let bounds = Bounds {
            charge_limit: limit,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            guarded_run(WRITES, false, u64::MAX, bounds),
            guarded_run(WRITES, true, u64::MAX, bounds),
            "limit {limit}"
        );
    }
}

/// K-MACH-008 / PLAN-INDEP: resident construction, field overwrite and
/// append use the semantic handlers' allocations and room. Aliases and
/// parks expose each write, and finite slices and limits can interrupt
/// any operand prefix or refuse construction or growth.
#[test]
fn resident_construction_and_stores_agree() {
    const BUILD: &str = r#"
main {
  let xs = []
  let alias = xs
  let i = 0
  while flat.lt(i, 19) {
    let item = {id: i, key: flat.add(i, 1)}
    set xs[list.len(xs)] = item
    set item.key = i
    set i = flat.add(i, 1)
  }
  do perform 0("park") as Any
  set xs[0] = {id: 99, key: 100}
  try { set xs[99] = 42 } catch err { print err.kind }
  try { set xs[-1] = 42 } catch err { print err.kind }
  return (alias, xs[0], (1, 2))
}
"#;
    let direct = || crate::machine::islands::DIRECT_COLLECTIONS.with(|n| n.replace(0));
    direct();
    let reference = guarded_run(BUILD, true, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(direct(), 0, "the tape retains the semantic heap context");
    assert!(reference.0.starts_with("finished"), "{}", reference.0);
    let natural = guarded_run(BUILD, false, u64::MAX, super::super::embedder::ROOMY);
    assert!(direct() > 0, "the plain run uses resident collections");
    assert_eq!(natural, reference);
    for slice in [1, 2, 3, 5, 7, 11, 13, 17, 23, 31] {
        assert_eq!(
            guarded_run(BUILD, false, slice, super::super::embedder::ROOMY),
            guarded_run(BUILD, true, slice, super::super::embedder::ROOMY),
            "slice {slice}"
        );
    }
    let charged = reference.2.last().unwrap().1.charged.unwrap();
    for limit in (charged / 2..charged).step_by(7) {
        let bounds = Bounds {
            charge_limit: limit,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            guarded_run(BUILD, false, u64::MAX, bounds),
            guarded_run(BUILD, true, u64::MAX, bounds),
            "limit {limit}"
        );
    }
    for memory in [0, 4096, 24 << 10, 64 << 10, 1 << 20] {
        let bounds = Bounds {
            memory,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            guarded_run(BUILD, false, 23, bounds),
            guarded_run(BUILD, true, 23, bounds),
            "memory {memory}"
        );
    }
}

/// K-COL / PLAN-INDEP: native item reads charge the native formula and
/// preserve its errors for missing, negative, fractional and wrong-kind
/// positions. Present absent values and tuple reads remain distinct.
#[test]
fn resident_native_items_agree() {
    for expression in [
        "list.get(xs, 1)",
        "list.get(xs, 0)",
        "list.get(xs, 1.0)",
        "tuple.get((3, 4), 1)",
        "list.get(xs, -1)",
        "list.get(xs, 2)",
        "list.get(xs, 0.5)",
        "list.get(xs, true)",
        "list.get((3, 4), 0)",
        "list.get(xs, 999999999999999999999999999999)",
    ] {
        let source = format!(
            "main {{ let xs = [absent, 4] let i = 0 let selected = absent while flat.lt(i, 5) {{ set selected = {expression} set i = flat.add(i, 1) }} do perform 0(\"park\") as Any return selected }}"
        );
        crate::machine::islands::DIRECT_ITEMS.with(|n| n.set(0));
        let reference = guarded_run(&source, true, u64::MAX, super::super::embedder::ROOMY);
        assert_eq!(crate::machine::islands::DIRECT_ITEMS.with(|n| n.get()), 0);
        let natural = guarded_run(&source, false, u64::MAX, super::super::embedder::ROOMY);
        if reference.0.starts_with("finished") {
            assert!(
                crate::machine::islands::DIRECT_ITEMS.with(|n| n.get()) > 0,
                "{expression}"
            );
        }
        assert_eq!(natural, reference, "{expression}");
        for slice in [1, 3, 7, 13, 31] {
            assert_eq!(
                guarded_run(&source, false, slice, super::super::embedder::ROOMY),
                guarded_run(&source, true, slice, super::super::embedder::ROOMY),
                "{expression}, slice {slice}"
            );
        }
        let charged = reference.2.last().unwrap().1.charged.unwrap();
        for limit in (0..charged).step_by(3) {
            let bounds = Bounds {
                charge_limit: limit,
                ..super::super::embedder::ROOMY
            };
            assert_eq!(
                guarded_run(&source, false, u64::MAX, bounds),
                guarded_run(&source, true, u64::MAX, bounds),
                "{expression}, limit {limit}"
            );
        }
    }
}
