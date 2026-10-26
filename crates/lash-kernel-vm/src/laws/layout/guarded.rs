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
    let natural = guarded(false, u64::MAX, super::super::embedder::ROOMY);
    assert!(reads() > 0, "the plain run takes guarded reads");
    // The list path alone: its length test and item read run whole too.
    let items = GUARDED
        .lines()
        .find(|line| line.contains("function item"))
        .map(|item| format!("{item}\nmain {{ return call item([3, 4], 6) }}\n"))
        .unwrap();
    let listed = guarded_run(&items, false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(listed.0, format!("finished {:?}", int(24)));
    assert!(reads() > 0, "the plain run takes guarded item reads");
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
