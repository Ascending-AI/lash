//! Guarded member paths preserve source semantics across execution modes.

use super::*;

/// Loops read a plain record's field and a list's item behind
/// TypeScript's guards: a kind test, then a unary test of the record's
/// membership of the brand key or a bounds test of the list's length.
/// Installing/removing a brand, changing record layouts and changing list
/// lengths change the source guards. Holes retain their source conversion.
/// Branded, short, non-collection and refreshed operands fail a guard;
/// finite slices and charge limits end inside the guarded path; a park
/// separates the calls. Earlier fused scalar writes and distinct field names
/// catch relocating a field-name index as though it were an instruction PC.
const GUARDED: &str = r#"
definition "function read(obj: Any, rounds: Any) -> Any\nkernel 1\nmetering statements\nbody { let certified = flat.identity(1) let names = {left: certified, right: 2} do names.left do names.right let sum = 0 let i = 0 while flat.lt(i, rounds) { let held = sum let t = absent if same(kind(obj), \"record\") { if bool.not(record.contains(obj, \"__lash_brand\")) { set t = obj.a } else { set t = 100 } } else { set t = 200 } set sum = flat.add(held, t) set i = flat.add(i, 1) } return sum }"
definition "function item(xs: Any, rounds: Any) -> Any\nkernel 1\nmetering statements\nbody { let sum = 0 let i = 0 while flat.lt(i, rounds) { let held = sum let t = absent if same(kind(xs), \"list\") { if flat.lt(1, list.len(xs)) { set t = xs[1] if same(t, ()) { set t = 0 } } else { set t = 100 } } else { set t = 200 } set sum = flat.add(held, t) set i = flat.add(i, 1) } return sum }"
main {
  let plain = {a: 1, b: 2}
  let branded = {__lash_brand: "x", a: 5}
  let xs = [3, 4]
  let first = call read(plain, 9)
  set plain.a = 4
  let second = call read(plain, 5)
  set plain.__lash_brand = "installed"
  let installed = call read(plain, 2)
  remove plain.__lash_brand
  let restored = call read(plain, 2)
  let shifted = call read({b: 90, a: 7}, 2)
  let marked = call read(branded, 4)
  let other = call read(7, 3)
  let items = call item(xs, 6)
  let short = call item([3], 3)
  let text = call item("ab", 2)
  do perform 0("park") as Any
  set xs[1] = ()
  let hole = call item(xs, 2)
  set xs[1] = 6
  set xs[2] = 9
  let grown = call item(xs, 2)
  remove xs[2]
  let again = (call read(plain, 3), call item(xs, 3))
  return (first, second, installed, restored, shifted, marked, other, items, short, text, hole, grown, again)
}
"#;

fn guarded_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
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
    Vec<(&'static str, crate::Meters, [u64; 3])>,
    Vec<lash_kernel_state::semantic::Checkpoint>,
    Vec<Request>,
);

/// A run of [`GUARDED`]; `record` retains the original charge tape.
fn guarded(record: bool, slice: u64, bounds: Bounds) -> Guarded {
    guarded_run(GUARDED, record, slice, bounds)
}

fn guarded_run(source: &str, record: bool, slice: u64, bounds: Bounds) -> Guarded {
    guarded_sealing(source, record, slice, bounds, false)
}

/// A run of `source` that seals and reopens cold at every stop when `cold`.
fn guarded_sealing(source: &str, record: bool, slice: u64, bounds: Bounds, cold: bool) -> Guarded {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(guarded_probes),
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
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("finished {:?}", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {
                if cold {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                    embedder.reopen(true);
                    if record {
                        embedder.machine.record_charges(tape.clone());
                    }
                }
            }
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                if cold {
                    embedder.reopen(true);
                    if record {
                        embedder.machine.record_charges(tape.clone());
                    }
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
    )
}

/// K-CHG-001 / K-MACH-008: guarded member source instructions preserve
/// values, charges, stop points, held bytes and checkpoints across cold
/// continuations, finite slices and interior charge limits.
#[test]
fn guarded_reads_agree() {
    let reference = guarded(true, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(
        reference.0,
        format!(
            "finished {:?}",
            Datum::Tuple(vec![
                int(9),
                int(20),
                int(200),
                int(8),
                int(14),
                int(400),
                int(600),
                int(24),
                int(300),
                int(400),
                int(0),
                int(12),
                Datum::Tuple(vec![int(12), int(18)]),
            ])
        )
    );
    // K-CHG-001: an inline text result uses the installed native contract;
    // it cannot restore the removed kind/fee memo or qualify by source alone.
    let mut run = Embedder::with(
        "main { let value = 3 return kind(value) }",
        Setup {
            probes: Some(guarded_probes),
            ..Setup::default()
        },
    );
    assert_eq!(
        result(run.run_to_end(&[])),
        Datum::Text("integer".into()),
        "canonical inline native"
    );
    let natural = guarded(false, u64::MAX, super::super::embedder::ROOMY);
    // The list path alone retains its length test and item read.
    let items = GUARDED
        .lines()
        .find(|line| line.contains("function item"))
        .map(|item| format!("{item}\nmain {{ return call item([3, 4], 6) }}\n"))
        .unwrap();
    let listed = guarded_run(&items, false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(listed.0, format!("finished {:?}", int(24)));
    assert_eq!(natural, reference);
    for slice in [1, 7, 31] {
        let cold = guarded_sealing(GUARDED, false, slice, super::super::embedder::ROOMY, true);
        assert_eq!(
            (&cold.0, &cold.1, &cold.4),
            (&natural.0, &natural.1, &natural.4),
            "cold guarded reads at slice {slice}"
        );
    }
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
    let reference = guarded_run(WRITES, true, u64::MAX, super::super::embedder::ROOMY);
    assert!(reference.0.starts_with("finished"), "{}", reference.0);
    let natural = guarded_run(WRITES, false, u64::MAX, super::super::embedder::ROOMY);
    assert_eq!(natural, reference);
    // K-MACH-008: same-sized scalar overwrites still dirty based cards;
    // replacing an arena edge by a scalar still releases that edge.
    let floats = WRITES.replace(
        "set obj.a = i",
        "set obj.a = 0.0 set obj.a = -0.0 set obj.a = 2.5 set obj.a = xs set obj.a = 3.5",
    );
    assert_eq!(
        guarded_run(&floats, false, u64::MAX, super::super::embedder::ROOMY),
        guarded_run(&floats, true, u64::MAX, super::super::embedder::ROOMY),
    );
    for memory in [4096, 24 << 10, 64 << 10] {
        let bounds = Bounds {
            memory,
            ..super::super::embedder::ROOMY
        };
        assert_eq!(
            guarded_run(&floats, false, 23, bounds),
            guarded_run(&floats, true, 23, bounds),
            "scalar overwrite at memory {memory}",
        );
    }
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

    let reference = guarded_run(BUILD, true, u64::MAX, super::super::embedder::ROOMY);

    assert!(reference.0.starts_with("finished"), "{}", reference.0);
    let natural = guarded_run(BUILD, false, u64::MAX, super::super::embedder::ROOMY);

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

        let reference = guarded_run(&source, true, u64::MAX, super::super::embedder::ROOMY);

        let natural = guarded_run(&source, false, u64::MAX, super::super::embedder::ROOMY);

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

/// K-COL / K-MACH-008: finite collection preparations preserve real lists,
/// holes, saved callback values and exact source exits. Rejected, abandoned
/// and stale append transactions publish no list edit or allocation ordinal.
/// Accepted regions agree with Source at warm, cold and refused boundaries.
#[test]
fn collection_region_preparations_preserve_source_and_storage() {
    const BODY: &str = r#"
        let out = [0, 0] let i = 0 let n = resident.len(values) let list = true
        for value in values { set out[resident.len(out)] = value }
        let sum = 0
        for value in values { set sum = flat.add(sum, value) }
        for value in values { set out[i] = value set i = flat.add(i, 1) }
        for value in values {
            let in_place = false
            if list { set in_place = num.lt(i, resident.len(out)) }
            if in_place { set out[i] = value }
            set i = num.add(i, 1)
        }
        set i = 0
        for value in values {
            if same(i, n) { break }
            if same(value, ()) { set i = flat.add(i, 1) } else { set out[0] = value }
            set i = flat.add(i, 1)
        }
        set i = 0
        while num.lt(i, n) {
            let present = false let value = absent
            if list { if num.lt(i, resident.len(values)) {
                set value = values[i] set present = bool.not(same(value, ()))
            } }
            if present { set out[0] = value }
            set i = num.add(i, 1)
        }
        let sorted = values let width = 1
        let dest = 0 let end = 0 let mid = 0 let left = 0 let right = 0
        for unused in values {
            if same(dest, end) {
                set mid = num.min(num.add(dest, width), n)
                set end = num.min(num.add(mid, width), n)
                set left = dest set right = mid
            }
            let from_right = false
            if num.lt(left, mid) {
                if num.lt(right, end) {
                    let first = list.get(sorted, left)
                    let second = list.get(sorted, right)
                    let output = apply compare(first, second)
                    let order = output
                    if num.lt(0, order) { set from_right = true }
                }
            } else { set from_right = true }
            if from_right {
                set out[dest] = list.get(sorted, right) set right = num.add(right, 1)
            } else {
                set out[dest] = list.get(sorted, left) set left = num.add(left, 1)
            }
            set dest = num.add(dest, 1)
        }
        return out
    "#;
    fn probes(
        registry: &mut lash_kernel_doc::FunctionRegistry,
        ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
    ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
        let mut probes = guarded_probes(registry, ids);
        // This accepted-runtime fixture uses the installed production contract.
        // The general guarded-read fixture intentionally retains its unknown one.
        let definition = lash_kernel_doc::parse_definition(
            "function bool.not(value: Bool) -> Bool\nkernel 1\ncharge 1\nnative\n",
        )
        .unwrap();
        let boolean_not = registry
            .register(definition, lash_kernel_lib::native("bool.not"))
            .unwrap();
        probes
            .iter_mut()
            .find(|(name, _)| *name == "bool.not")
            .unwrap()
            .1 = boolean_not;
        for (name, native, signature) in [
            ("num.add", "num.add", "(a: Number, b: Number) -> Number"),
            ("num.lt", "num.lt", "(a: Number, b: Number) -> Bool"),
            ("resident.len", "list.len", "(values: List(Any)) -> Int"),
            ("num.min", "num.min", "(a: Number, b: Number) -> Number"),
        ] {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function {name}{signature}\nkernel 1\ncharge 1\nnative\n"
            ))
            .unwrap();
            probes.push((
                name,
                registry
                    .register(definition, lash_kernel_lib::native(native))
                    .unwrap(),
            ));
        }
        let references = ["num.add", "num.lt"]
            .map(|name| (name, probes.iter().find(|(key, _)| *key == name).unwrap().1))
            .into_iter()
            .chain(
                [
                    "flat.add",
                    "same",
                    "bool.not",
                    "num.min",
                    "list.get",
                    "resident.len",
                ]
                .map(|name| (name, probes.iter().find(|(key, _)| *key == name).unwrap().1)),
            )
            .map(|(name, id)| format!("use {name} = @{id}\n"))
            .collect::<String>();
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function helper.direct(values: Any, compare: Any) -> Any\nkernel 1\ncharge 1\n{references}body {{ {BODY} }}"
        )).unwrap();
        probes.push((
            "helper.direct",
            registry.register(definition, None).unwrap(),
        ));
        probes
    }
    let source = "main { return invoke helper.direct([3, 7], fn(a, b) { return 0 }) }";
    let observe = |bounds: Bounds, slice: u64, cold: bool| {
        let mut runtime = Embedder::with(
            source,
            Setup {
                probes: Some(probes),
                bounds,
                slice,
                ..Setup::default()
            },
        );

        let tape = crate::ChargeTape::new();
        runtime.machine.record_charges(tape.clone());
        let mut stops = Vec::new();
        let mut reopened = 0;
        let end = loop {
            let step = runtime.run();

            let balance = runtime.machine.logical_balance().unwrap();
            stops.push((
                match &step {
                    Step::Ended(_) => "ended",
                    Step::Slice => "slice",
                    Step::Parked(_) => "parked",
                },
                runtime.machine.meters(),
                [
                    balance.entity_local_bytes,
                    balance.root_and_control_bytes,
                    balance.entities,
                ],
                (!matches!(&step, Step::Ended(_))).then(|| runtime.machine.semantic_checkpoint()),
            ));
            match step {
                Step::Ended(end) => break end,
                Step::Slice | Step::Parked(_) => {
                    if cold {
                        // Only publications after the first cold reopen count
                        // toward the cold witness, never its initial warm prefix.

                        runtime.reopen(true);
                        runtime.machine.record_charges(tape.clone());
                        reopened += 1;
                    }
                    if matches!(step, Step::Parked(_)) {
                        runtime.deliver_next();
                    }
                }
            }
        };

        let finished = matches!(&end, End::Finished(_));
        (
            (
                format!("{end:?}"),
                runtime.machine.meters(),
                crate::Weight::expand(tape.steps()),
                stops,
                finished,
            ),
            reopened,
        )
    };
    // Sealed cold at every stop, a run ends, charges and records its
    // charge events as it does whole.
    let outcome = |observed: &(String, crate::Meters, Vec<crate::Weight>, _, bool)| {
        (
            observed.0.clone(),
            observed.1.charged,
            observed.2.clone(),
            observed.4,
        )
    };
    let (reference, reopened) = observe(super::super::embedder::ROOMY, u64::MAX, false);
    assert_eq!(reopened, 0);
    assert!(
        reference.4,
        "the collection corpus finishes under roomy bounds"
    );
    for slice in [1, 3] {
        let (cold, reopened) = observe(super::super::embedder::ROOMY, slice, true);
        assert!(reopened > 0, "the cold fixture must reopen");
        assert_eq!(
            outcome(&cold),
            outcome(&reference),
            "cold collection run at slice {slice}"
        );
    }

    let charged = reference.1.charged.unwrap();
    let mut refusals = 0;
    let tight = [0, charged / 2, charged - 1]
        .map(|limit| Bounds {
            charge_limit: limit,
            ..super::super::embedder::ROOMY
        })
        .into_iter()
        .chain([0, 4096, 24 << 10, 64 << 10].map(|memory| Bounds {
            memory,
            ..super::super::embedder::ROOMY
        }));
    for bounds in tight {
        let (warm, _) = observe(bounds, u64::MAX, false);
        if bounds.memory == 0 || bounds.charge_limit < charged {
            assert!(!warm.4, "the tight case must refuse: {bounds:?}");
        }
        refusals += usize::from(!warm.4);
        let (cold, _) = observe(bounds, 3, true);
        assert_eq!(
            outcome(&cold),
            outcome(&warm),
            "tight collection prefix, cold: {bounds:?}"
        );
    }
    assert!(
        refusals >= 4,
        "the matrix exercises actual resource refusals"
    );
}
