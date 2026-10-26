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

/// PLAN-INDEP for per-site property caches: warm a read site, fail its guard
/// on a different record, then delete, reinsert and mutate its field.
/// Errors and effects retain their order and charge,
/// also with every safepoint reopened under a different, cold plan.
#[test]
fn property_caches_are_exact_across_guard_failures_and_cold_plans() {
    const READS: &str = r#"
definition "function fetch(obj: Any) -> Any\nkernel 1\nmetering statements\nbody {  return obj.value  }"

definition "function fetch_key(obj: Any, key: Any) -> Any\nkernel 1\nmetering statements\nbody {  return obj[key]  }"

definition "function replace(obj: Any, key: Any, value: Any) -> Any\nkernel 1\nmetering statements\nbody {  set obj[key] = value\n return obj[key]  }"

definition "function sibling() -> Any\nkernel 1\nmetering statements\nbody {  let reply = perform 0(\"peer\") as Any return reply  }"

main {
  let peer = spawn call sibling()
  let a = {value: 10, other: 90}
  let b = {other: 80, value: 20}
  let alias = a
  print call fetch(a)
  print call fetch(b)
  print call fetch_key(a, "other")
  print call fetch_key(b, "other")
  print call fetch_key(b, "value")
  let items = [40, 50]
  let same_items = items
  print call fetch_key(items, 0)
  print call replace(same_items, 0, 41)
  print call fetch_key(items, 0)
  print call replace(alias, "value", 12)
  print call fetch(a)
  set alias.value = 10
  let held = (num.add(call fetch(a), 1), perform 0("read") as Any)
  print held[0]
  let reply = held[1]
  remove a.value
  print call fetch(a)
  do call replace(alias, "value", reply)
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
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
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
                embedder.machine.record_charges(tape.clone());
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
            tape.steps()
                .into_iter()
                .flat_map(|weight| match weight {
                    lash_kernel_exec::Weight::Units(n) => {
                        vec![lash_kernel_exec::Weight::Units(1); n as usize]
                    }
                    atomic => vec![atomic],
                })
                .collect::<Vec<_>>(),
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
            int(41),
            int(41),
            int(12),
            int(12),
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

/// K-VAL / K-CHG-001 / K-MACH-008: a linear accumulator may reuse only
/// its own new text. Aliases frozen before either loop retain their UTF-8
/// bytes, including across pressure, a park and slice-by-slice reopening.
#[test]
fn a_text_accumulator_preserves_frozen_aliases_and_charge() {
    let prefix = "α🙂".repeat(16);
    let part = "é🙂".repeat(32);
    let function = r#"function accumulate() -> Any
kernel 1
metering statements
body {
  let current = "$PREFIX"
  let old = current
  let i = 0
  while num.lt(i, 80) {
    set current = text.concat(current, "é🙂")
    set i = num.add(i, 1)
  }
  let frozen = current
  print (old, frozen)
  do perform 0("frozen") as Any
  let j = 0
  while num.lt(j, 80) {
    set current = text.concat(current, "$PART")
    set j = num.add(j, 1)
  }
  print (old, frozen, current)
  do perform 0("done") as Any
  return (old, frozen, current)
}"#
    .replace("$PREFIX", &prefix)
    .replace("$PART", &part);
    let source = format!(
        "definition {}\nmain {{ let answer = call accumulate() return answer }}",
        serde_json::to_string(&function).unwrap()
    );
    let observe = |layout: Layout, cold: bool, memory| {
        let mut embedder = Embedder::with(
            &source,
            Setup {
                layout,
                slice: if cold { 1 } else { u64::MAX },
                bounds: Bounds {
                    memory,
                    ..super::embedder::ROOMY
                },
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        let mut saved = Vec::new();
        let end = loop {
            let step = embedder.run();
            if let Step::Ended(mut end) = step {
                if let End::Finished(finished) = &mut end
                    && let Some(carry) = finished.carry.take()
                {
                    saved.push(
                        crate::conformance::session(&carry, Box::new(embedder.source.clone()))
                            .unwrap(),
                    );
                }
                break end;
            }
            if matches!(step, Step::Parked(_)) {
                saved.push(embedder.machine.semantic_checkpoint());
            }
            if cold {
                embedder.reopen(layout, true);
                embedder.machine.record_charges(tape.clone());
            }
            if matches!(step, Step::Parked(_)) {
                embedder.deliver_next();
            }
        };
        let charge = tape
            .steps()
            .into_iter()
            .flat_map(|weight| match weight {
                lash_kernel_exec::Weight::Units(n) => {
                    vec![lash_kernel_exec::Weight::Units(1); n as usize]
                }
                atomic => vec![atomic],
            })
            .collect::<Vec<_>>();
        (
            end,
            embedder.world.printed,
            embedder.requests,
            embedder.machine.meters().charged.unwrap(),
            saved,
            charge,
        )
    };
    let reference = observe(Layout(1), false, 64 << 10);
    let frozen = format!("{prefix}{}", "é🙂".repeat(80));
    let current = format!("{frozen}{}", part.repeat(80));
    assert_eq!(
        result(reference.0.clone()),
        Datum::Tuple(vec![text(&prefix), text(&frozen), text(&current)])
    );
    assert_eq!(reference.2.len(), 2);
    for layout in [Layout(0), Layout(1)] {
        assert_eq!(observe(layout, false, 64 << 10), reference);
        assert_eq!(observe(layout, true, 64 << 10), reference);
        // The capacity refusal is between the two effects, after the old
        // aliases were observed, before a later effect can escape.
        for cold in [false, true] {
            let refused = observe(layout, cold, 24 << 10);
            assert!(matches!(
                refused.0,
                End::Error(crate::RunError::Bound(crate::BoundExceeded {
                    bound: crate::Bound::Memory,
                    ..
                }))
            ));
            assert_eq!(refused.1, reference.1[..1]);
            assert_eq!(refused.2, reference.2[..1]);
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
                heap.session_binding(&lash_vocabulary::names::Name::new(name))
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

/// Certified scalar entries shared by the layout, verifier and limiter laws.
/// The general call uses the document's checked semantics as the oracle.
pub(super) fn scalar_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    _: &std::collections::BTreeMap<&'static str, lash_vocabulary::names::FunctionId>,
) -> Vec<(&'static str, lash_vocabulary::names::FunctionId)> {
    use lash_kernel_doc::scalar::{self, Pair, ScalarOp};
    use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};
    struct Scalar(ScalarOp);
    impl Scalar {
        fn answer(op: ScalarOp, args: &[Value]) -> Option<Value> {
            match args {
                [Value::Int(a), Value::Int(b)] => scalar::ints(op, *a, *b),
                [Value::Float(a), Value::Float(b)] => scalar::floats(op, *a, *b),
                _ => None,
            }
        }
    }
    impl NativeFunction for Scalar {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            Self::answer(self.0, call.args.values())
                .ok_or_else(|| NativeError::refused("scalar", "outside the scalar domain"))
        }
        fn prepare(&self) -> Option<PreparedNative> {
            let entry = match self.0 {
                ScalarOp::Add => |args: &[Value]| Self::answer(ScalarOp::Add, args),
                ScalarOp::Mul => |args: &[Value]| Self::answer(ScalarOp::Mul, args),
                ScalarOp::Div => |args: &[Value]| Self::answer(ScalarOp::Div, args),
                ScalarOp::RemTrunc => |args: &[Value]| Self::answer(ScalarOp::RemTrunc, args),
                ScalarOp::Lt => |args: &[Value]| Self::answer(ScalarOp::Lt, args),
                _ => unreachable!(),
            };
            Some(PreparedNative {
                pair: Some(Pair {
                    floats: Some(self.0),
                    ints: Some(self.0),
                }),
                entry,
                charged_entry: None,
                operation: "law.scalar",
            })
        }
    }
    let mut functions: Vec<_> = [("flat.add", ScalarOp::Add), ("flat.mul", ScalarOp::Mul), ("flat.div", ScalarOp::Div), ("flat.rem", ScalarOp::RemTrunc), ("flat.lt", ScalarOp::Lt)]
        .into_iter().map(|(name, op)| {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function {name}(a: Any, b: Any) -> {}\nkernel 1\ncharge sum(3, size(a), size(b), size(result))\nnative\n",
                if op.compares() { "Bool" } else { "Any" }
            )).unwrap();
            (name, registry.register(definition, Some(std::sync::Arc::new(Scalar(op)))).unwrap())
        }).collect();
    struct Identity;
    impl Identity {
        fn answer(args: &[Value]) -> Option<Value> {
            match args {
                [value @ (Value::Float(_) | Value::Int(_))] => Some(*value),
                _ => None,
            }
        }
    }
    impl NativeFunction for Identity {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            Self::answer(call.args.values())
                .ok_or_else(|| NativeError::refused("scalar", "outside the scalar domain"))
        }
        fn prepare(&self) -> Option<PreparedNative> {
            Some(PreparedNative {
                pair: None,
                entry: Self::answer,
                charged_entry: None,
                operation: "law.scalar.identity",
            })
        }
    }
    let definition = lash_kernel_doc::parse_definition(
        "function flat.identity(a: Any) -> Any\nkernel 1\ncharge sum(8, size(a))\nnative\n",
    )
    .unwrap();
    functions.push((
        "flat.identity",
        registry
            .register(definition, Some(std::sync::Arc::new(Identity)))
            .unwrap(),
    ));
    functions
}

pub(super) fn scalar_document(body: &str) -> String {
    format!(
        "definition \"function scalar() -> Any\\nkernel 1\\nmetering statements\\nbody {{ {} }}\"\nmain {{ let answer = call scalar() finish answer }}",
        super::embedder::fragment(body)
    )
}

type ScalarObserved = (
    String,
    Vec<Datum>,
    Vec<Request>,
    Vec<lash_kernel_exec::Weight>,
    u64,
    Vec<crate::conformance::Checkpoint>,
);

fn observe_scalar(
    source: &str,
    layout: Layout,
    record: bool,
    cold: bool,
    require_typed: bool,
) -> ScalarObserved {
    observe_scalar_bounded(
        source,
        layout,
        record,
        cold,
        require_typed,
        super::embedder::ROOMY,
    )
}

fn observe_scalar_bounded(
    source: &str,
    layout: Layout,
    record: bool,
    cold: bool,
    require_typed: bool,
    bounds: Bounds,
) -> ScalarObserved {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(if source == ALLOCATING {
                allocation_probes
            } else {
                scalar_probes
            }),
            bounds,
            layout,
            slice: if cold { 1 } else { u64::MAX },
            ..Setup::default()
        },
    );
    let tape = crate::ChargeTape::new();
    if record {
        embedder.machine.record_charges(tape.clone());
    }
    let mut checkpoints = Vec::new();
    let mut registered = 0;
    let end = loop {
        if !record {
            embedder.machine.count_execution();
        }
        let step = embedder.run();
        if !record {
            registered += embedder
                .machine
                .take_execution_census()
                .statements
                .get(&crate::perf_witness::execution::Owner::RegisterProgram)
                .copied()
                .unwrap_or(0);
        }
        match step {
            Step::Ended(end) => {
                break match end {
                    End::Finished(finished) => format!("finished {:?}", finished.result),
                    other => format!("{other:?}"),
                };
            }
            Step::Slice => {
                if cold {
                    embedder.reopen(layout, true);
                    if record {
                        embedder.machine.record_charges(tape.clone());
                    }
                }
            }
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                if cold {
                    embedder.reopen(layout, true);
                    if record {
                        embedder.machine.record_charges(tape.clone());
                    }
                }
                embedder.deliver_next();
            }
        }
    };
    if !record {
        assert!(
            registered > 0,
            "expression corpus must run register statements: {source}"
        );
    }
    if require_typed {
        assert!(
            crate::kernel_internals::register_work(&tape).blocks > 0,
            "the scalar layout must execute typed blocks"
        );
    }
    let events = tape
        .steps()
        .into_iter()
        .flat_map(|weight| match weight {
            lash_kernel_exec::Weight::Units(n) => {
                vec![lash_kernel_exec::Weight::Units(1); n as usize]
            }
            atomic => vec![atomic],
        })
        .collect();
    (
        end,
        embedder.world.printed,
        embedder.requests,
        events,
        embedder.machine.meters().charged.unwrap(),
        checkpoints,
    )
}

/// Typed scalar loops give the same values, events and charge as the
/// source layout, with and without a tape, warm or cold.
#[test]
fn scalar_layouts_agree() {
    for (case, body) in [
        "let i = 0 let s = 0 while flat.lt(i, 5) { if flat.lt(i, 2) { set s = flat.add(s, flat.mul(i, i)) } else { set s = flat.add(s, i) } set i = flat.add(i, 1) } print s do perform 0(s) as Any return s",
        "let i = 0.0 let s = -0.0 while flat.lt(i, 5.0) { set s = flat.add(flat.mul(s, -0.0), flat.div(0.0, 0.0)) set i = flat.add(i, 1.0) } print s do perform 0(s) as Any return s",
        "let i = 0.0 let s = -0.0 while flat.lt(i, 3.0) { set s = flat.rem(s, 2.0) set i = flat.add(i, 1.0) } print s return s",
        "let n = 0 let f = fn() { let old = flat.identity(n) set n = flat.add(old, 1) do perform 0(n) as Any return n } let a = apply f() let b = apply f() return (a, b, n)",
        "let n = 2 let f = fn() { set n = 30 return 4 } let answer = flat.mul(flat.identity(n), apply f()) do perform 0(answer) as Any return (answer, n)",
        "let n = 0 let f = fn() { set n = flat.add(n, 1) return false } try { let answer = flat.identity(apply f()) return answer } catch err { return n }",
        "let i = 0 let s = 0 while flat.lt(i, 3) { set s = flat.add(s, 1.0) set i = flat.add(i, 1) } return s",
        "let i = 0 let s = 9223372036854775806 try { while flat.lt(i, 3) { set s = flat.add(s, 1) set i = flat.add(i, 1) } } catch err { print (s, i) } return (s, i)",
        "let i = 0.0 let s = 1.5 try { while flat.lt(i, 3.0) { set i = flat.add(i, 1.0) set s = flat.rem(s, 2.0) } } catch err { print (s, i) } return (s, i)",
    ].into_iter().enumerate() {
        let source = scalar_document(body);
        for record in [false, true] {
            let reference = observe_scalar(&source, Layout(0), record, false, false);
            for layout in [Layout(0), Layout(1), Layout(2), Layout(0x5eed)] {
                assert_eq!(observe_scalar(&source, layout, record, false, record && case < 3 && layout.0 % 2 == 0), reference, "{body}, {layout:?}, tape={record}");
                assert_eq!(observe_scalar(&source, layout, record, true, false), reference, "cold {body}, {layout:?}, tape={record}");
            }
        }
    }
}

fn call_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_vocabulary::names::FunctionId>,
) -> Vec<(&'static str, lash_vocabulary::names::FunctionId)> {
    let mut probes = scalar_probes(registry, ids);
    probes.extend(
        lash_kernel_lib::numbers()
            .into_iter()
            .filter_map(|(definition, native)| {
                let name = match definition.name.as_str() {
                    "kind" => "kind",
                    "same" => "same",
                    _ => return None,
                };
                Some((name, registry.register(definition, Some(native)).unwrap()))
            }),
    );
    probes
}

mod calls;
mod guarded;
mod helpers;
mod windows;

pub(super) use helpers::{HELPERS, helper_probes};

/// K-MACH-008 / K-CHG-001: construction, native allocation and member
/// writes keep their source operands and first-refusal event prefix.
pub(super) const ALLOCATING: &str = r#"
definition "function allocate(x: Any) -> Any\nkernel 1\nmetering statements\nbody { let xs = [x, 2] let rec = {item: xs} let alias = rec do invoke law.put(alias, \"item\", xs) do invoke law.put(alias, \"extra\", x) remove rec.extra do invoke law.put(rec, \"extra\", xs) try { do invoke law.put(rec, 0, x) } catch err { print err.kind } let m = map{\"k\": rec} let seen = set{x, 2} do invoke law.put(xs, 0, 7) set rec.item = [xs[0], seen[x]] set m[\"other\"] = rec.item let text = text.concat(\"abcdefghij\", \"klmnopqrst\") let filled = work.fill(3) return (m[\"other\"], text, filled, seen[2], invoke law.box(xs[0])) }"
main {
  do perform 0("before") as Any
  let answer = call allocate(1)
  print answer
  do perform 0("after") as Any
  return answer
}
"#;

/// Allocating operations give the same values, effects, charges and
/// refusals under every layout, warm or cold.
#[test]
fn allocating_operations_agree() {
    let mut witness = Embedder::with(
        ALLOCATING,
        Setup {
            probes: Some(allocation_probes),
            ..Setup::default()
        },
    );
    super::observe::fully_observe(&mut witness.machine);
    use crate::perf_witness::execution::Owner;
    witness.machine.count_execution();
    let end = witness.run_to_end(&[]);
    let census = witness.machine.take_execution_census();
    assert!(matches!(end, End::Finished(_)), "{end:?}");
    assert!(
        census
            .statements
            .get(&Owner::RegisterProgram)
            .is_some_and(|statements| *statements > 0),
        "observed allocating bodies execute in the register program: {census:?}"
    );
    for record in [false, true] {
        let reference = observe_scalar(ALLOCATING, Layout(0), record, false, false);
        for layout in [Layout(0), Layout(1), Layout(2), Layout(0x5eed)] {
            assert_eq!(
                observe_scalar(ALLOCATING, layout, record, false, false),
                reference,
                "allocating {layout:?}, tape={record}"
            );
            assert_eq!(
                observe_scalar(ALLOCATING, layout, record, true, false),
                reference,
                "cold allocating {layout:?}, tape={record}"
            );
        }
    }
}

/// The same bodies and public signatures are authoritative for their
/// prepared heap paths; source layouts run the ordinary helper plans.
pub(super) fn allocation_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    _: &std::collections::BTreeMap<&'static str, lash_vocabulary::names::FunctionId>,
) -> Vec<(&'static str, lash_vocabulary::names::FunctionId)> {
    use lash_kernel_doc::plan::{HeapTemplate, HelperPlan, HelperTemplate};
    let mut functions = Vec::new();
    for (name, signature, body, registers, writes) in [
        ("law.box", "x: Int", "return (x,)", 1, false),
        (
            "law.put",
            "target: Any, index: Any, value: Any",
            "set target[index] = value return value",
            3,
            true,
        ),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}({signature}) -> Any\nkernel 1\ncharge 3\nbody {{ {body} }}"
        ))
        .unwrap();
        let function = registry.register(definition, None).unwrap();
        let plan = Box::leak(Box::new(HelperPlan {
            name,
            function,
            callees: &[],
            literals: &[],
            fields: &[],
            records: &[],
            registers,
            slots: &[],
            closures: &[],
            sites: &[],
            run: if writes {
                |ctx, _| {
                    ctx.begin()?;
                    ctx.param(0, 0)?;
                    ctx.param(1, 1)?;
                    ctx.param(2, 2)?;
                    ctx.set_index(0, 2)?;
                    Ok(ctx.take(2))
                }
            } else {
                |ctx, _| {
                    ctx.begin()?;
                    ctx.param(0, 0)?;
                    ctx.tuple(0, 1)?;
                    Ok(ctx.take(0))
                }
            },
            template: Some(HelperTemplate {
                depth: 1,
                callees: &[],
                entry: None,
                metered: None,
                forwards: Default::default(),
                heap: Some(HeapTemplate {
                    depth: 1,
                    writes,
                    element_write: false,
                    callees: &[],
                    entry: if writes {
                        |heap, args, _| {
                            let [target, index, value] = *args else {
                                return None;
                            };
                            heap.set_index(target, index, value)?;
                            Some(value)
                        }
                    } else {
                        |heap, args, _| heap.tuple(args)
                    },
                }),
            }),
        }));
        registry.install_plans(&[plan]).unwrap();
        functions.push((name, function));
    }
    functions
}

/// K-ITER / K-EVAL-008: loop headers and wide waiting calls retain their
/// ordered operands, iteration counts and source coordinates at every exit.
pub(super) fn control_document(body: &str) -> String {
    let definition = |signature: &str, body: &str| {
        let definition = format!(
            "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        );
        format!(
            "definition {}\n",
            serde_json::to_string(&definition).unwrap()
        )
    };
    let params = (0..17)
        .map(|i| format!("p{i}: Any"))
        .collect::<Vec<_>>()
        .join(", ");
    let sum = (1..17)
        .map(|i| format!("set p0 = flat.add(p0, p{i})\n"))
        .collect::<String>();
    let args = (0..17)
        .map(|i| {
            if i == 16 {
                "call relay(pause)".to_owned()
            } else {
                i.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}{}{}{}{}{}{}{}{}main {{ {body} }}",
        definition(&format!("wide({params})"), &format!("{sum} return p0")),
        definition(
            "relay(pause: Any)",
            "if pause { do perform 0(\"operand\") as Any } return 8"
        ),
        definition(
            "caller(pause: Any)",
            &format!("return flat.add(1000, call wide({args}))")
        ),
        definition(
            "through(f: Any, pause: Any)",
            &format!("return flat.add(1000, apply f({args}))")
        ),
        definition(
            "cleanup(mode: Any)",
            "try { if mode { return 1 } try { return 2 } finally { return 3 } } finally { do flat.add(1, 2) }"
        ),
        definition(
            "caught()",
            "try { return flat.add(false, 1) } catch err { return 7 } finally { do flat.add(1, 2) }"
        ),
        definition(
            "plain(xs: Any, n: Any)",
            "let i = 0 if true { while flat.lt(i, n) metered 2 { set i = flat.add(i, 1) for x in xs { if flat.lt(x, 2) { continue } if flat.lt(3, x) { break } } } } return i"
        ),
        definition(
            "make()",
            "let count = 0 return fn() { set count = flat.add(count, 1) return count }"
        ),
        definition(
            "loops(xs: Any, n: Any, pause: Any)",
            r#"
            let i = 0 let s = 0
            while flat.lt(i, n) metered 3 {
                try {
                set i = flat.add(i, 1)
                if flat.lt(i, 2) { continue }
                if flat.lt(4, i) { break }
                for x in xs metered 2 {
                    if flat.lt(x, 2) { continue }
                    if flat.lt(4, x) { break }
                    set s = flat.add(s, x)
                    if pause { do perform 0("body") as Any }
                } else { set s = flat.add(s, 10) }
                } finally { set s = flat.add(s, 1) }
            } else { set s = flat.add(s, 100) }
            try { return flat.add(s, call caller(pause)) }
            finally { set s = flat.add(s, 10000) }
        "#
        ),
    )
}

/// Loop exits, cleanups and wide calls continue the same under every
/// layout, warm or cold, and refuse at every loop header charge alike.
#[test]
fn control_exits_agree() {
    let mut comparisons = 0;
    for body in [
        "return call loops([1, 2, 3, 5], 7, false)",
        "return call loops([], 0, false)",
        "return call loops([1, 2], 3, false)",
        "return call loops([1, 2], 3, true)",
        "return flat.add(call cleanup(false), call cleanup(true))",
        "return call caught()",
        "let f = fn(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q) { return flat.add(a, q) } return call through(f, true)",
        "return call plain([], 0)",
        "return call plain([1, 2, 4], 3)",
        "let bump = call make() let one = apply bump() let value = call loops([1, 2], 3, false) return flat.add(one, apply bump())",
    ] {
        let source = control_document(body);
        let natural = observe_scalar(&source, Layout(0), true, false, false);
        for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
            for cold in [false, true] {
                assert_eq!(
                    observe_scalar(&source, layout, true, cold, false),
                    natural,
                    "{body}, {layout:?}, cold={cold}"
                );
                comparisons += 1;
            }
        }
    }
    println!("loop/wide-call continuations: {comparisons} paired runs");
}

/// A function's free cells keep their values and charge under every
/// layout, warm or cold.
#[test]
fn free_cells_agree() {
    use lash_kernel_doc::{
        Captions, CaptureValue, FunctionRepresentation, FunctionValue, StartConvention,
    };
    let source = r#"definition "function free() -> Any\nkernel 1\nmetering statements\nbody { set count = flat.add(count, 1) if flat.lt(count, 10) { do perform 0(count) as Any } try { return count } finally { set count = flat.add(count, 100) } }"
        main { print null return null }"#;
    let initial = Embedder::with(
        source,
        Setup {
            probes: Some(scalar_probes),
            ..Setup::default()
        },
    );
    let (program, _) = initial.program(true);
    assert!(
        crate::kernel_internals::call_islands(&program).unwrap() > 0,
        "the free-cell body must form a register program"
    );
    let export = &program.document.exports[&lash_vocabulary::Name::new("free")];
    let lash_kernel_doc::Export::Function(export) = export else {
        panic!("function export");
    };
    let callable = FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: Vec::new(),
        callable: lash_vocabulary::CallableId {
            link: program.link,
            function: export.function,
        },
        captures: std::collections::BTreeMap::from([(
            lash_vocabulary::Name::new("count"),
            CaptureValue::Data(int(4)),
        )]),
        signature: export.signature.clone(),
        convention: StartConvention::Direct,
        captions: Captions::empty(program.document.identity()),
        representation: FunctionRepresentation::Bare,
        creation: None,
        token: None,
    };
    let run = |layout, cold| {
        let mut setup = Setup {
            layout,
            probes: Some(scalar_probes),
            slice: if cold { 1 } else { u64::MAX },
            ..Setup::default()
        };
        setup.start.target = crate::Target::Callable(Box::new(callable.clone()));
        let mut embedder = Embedder::with(source, setup);
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        let mut saved = Vec::new();
        let end = loop {
            match embedder.run() {
                Step::Ended(end) => break result(end),
                Step::Slice => {
                    if cold {
                        embedder.reopen(layout, true);
                        embedder.machine.record_charges(tape.clone());
                    }
                }
                Step::Parked(_) => {
                    saved.push(embedder.machine.semantic_checkpoint());
                    embedder.reopen(layout, true);
                    embedder.machine.record_charges(tape.clone());
                    embedder.deliver_next();
                }
            }
        };
        (end, saved, lash_kernel_exec::Weight::expand(tape.steps()))
    };
    let reference = run(Layout(0), false);
    assert_eq!(reference.0, int(5));
    for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
        for cold in [false, true] {
            assert_eq!(
                run(layout, cold),
                reference,
                "free cells {layout:?}, cold={cold}"
            );
        }
    }
}

/// K-EVAL-008 / K-MACH-008: a resolution-scope exit retains the active
/// caller's cells while alternating callees fill the scope's cache.
#[test]
fn capture_scopes_agree() {
    let body = r#"
        let a = 0 let b = 0
        let f = fn() { set a = flat.add(a, 1) return a }
        let g = fn() { set b = flat.add(b, 1) return b }
        return fn(n) {
            let i = 0 let total = 0 let flag = false let picked = f
            while flat.lt(i, n) {
                if flag { set flag = false set picked = f }
                else { set flag = true set picked = g }
                set total = flat.add(total, apply picked())
                set i = flat.add(i, 1)
            }
            return total
        }
    "#;
    let definition =
        format!("function make_churn() -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}");
    let source = format!(
        "definition {}\nmain {{ let run = call make_churn() return apply run(2200) }}",
        serde_json::to_string(&definition).unwrap()
    );
    let reference = observe_scalar(&source, Layout(0), true, false, false);
    assert_eq!(reference.0, format!("finished {:?}", int(1_211_100)));
    for layout in [Layout(0), Layout(2)] {
        assert_eq!(
            observe_scalar(&source, layout, true, false, false),
            reference,
            "capture scope {layout:?}"
        );
    }
}

/// K-CHG-001/007: real typed iterations retain the header's unpaid tail.
/// Body-local values leave no roots in the parked continuation.
#[test]
fn typed_repeat_agrees() {
    for body in [
        "let i = 0.0 let s = 0.0 while flat.lt(i, 4.0) { let previous = flat.identity(i) set s = flat.add(s, previous) set i = flat.add(previous, 1.0) } if true { do perform 0(s) as Any } return s",
        "let i = 0.0 let s = 0.0 while flat.lt(i, 4.0) { set s = flat.add(s, i) set i = flat.add(i, 1.0) } if true { do perform 0(s) as Any } return s",
    ] {
        let source = scalar_document(body);
        crate::machine::islands::TYPED_REPEATED.with(|n| n.set(0));
        observe_scalar(&source, Layout(0), false, false, false);
        assert!(
            crate::machine::islands::TYPED_REPEATED.with(|n| n.get()) > 0,
            "real typed iterations execute"
        );
    }
}

mod accumulation;
