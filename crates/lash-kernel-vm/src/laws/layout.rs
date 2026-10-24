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
    wide_windows_agree();
    scalar_layouts_agree();
    call_prefixes_agree();
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
    let reference = observe(Layout(1).without_regions(), false, 64 << 10);
    let frozen = format!("{prefix}{}", "é🙂".repeat(80));
    let current = format!("{frozen}{}", part.repeat(80));
    assert_eq!(
        result(reference.0.clone()),
        Datum::Tuple(vec![text(&prefix), text(&frozen), text(&current)])
    );
    assert_eq!(reference.2.len(), 2);
    for layout in [Layout(0), Layout(1).without_regions()] {
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
    [("flat.add", ScalarOp::Add), ("flat.mul", ScalarOp::Mul), ("flat.div", ScalarOp::Div), ("flat.rem", ScalarOp::RemTrunc), ("flat.lt", ScalarOp::Lt)]
        .into_iter().map(|(name, op)| {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function {name}(a: Any, b: Any) -> {}\nkernel 1\ncharge sum(3, size(a), size(b), size(result))\nnative\n",
                if op.compares() { "Bool" } else { "Any" }
            )).unwrap();
            (name, registry.register(definition, Some(std::sync::Arc::new(Scalar(op)))).unwrap())
        }).collect()
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
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(scalar_probes),
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
    let end = loop {
        match embedder.run() {
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
    if require_typed {
        assert!(
            crate::kernel_internals::region_work(&tape).blocks > 0,
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

fn scalar_layouts_agree() {
    for (case, body) in [
        "let i = 0 let s = 0 while flat.lt(i, 5) { if flat.lt(i, 2) { set s = flat.add(s, flat.mul(i, i)) } else { set s = flat.add(s, i) } set i = flat.add(i, 1) } print s do perform 0(s) as Any return s",
        "let i = 0.0 let s = -0.0 while flat.lt(i, 5.0) { set s = flat.add(flat.mul(s, -0.0), flat.div(0.0, 0.0)) set i = flat.add(i, 1.0) } print s do perform 0(s) as Any return s",
        "let i = 0.0 let s = -0.0 while flat.lt(i, 3.0) { set s = flat.rem(s, 2.0) set i = flat.add(i, 1.0) } print s return s",
        "let i = 0 let s = 0 while flat.lt(i, 3) { set s = flat.add(s, 1.0) set i = flat.add(i, 1) } return s",
        "let i = 0 let s = 9223372036854775806 try { while flat.lt(i, 3) { set s = flat.add(s, 1) set i = flat.add(i, 1) } } catch err { print (s, i) } return (s, i)",
        "let i = 0.0 let s = 1.5 try { while flat.lt(i, 3.0) { set i = flat.add(i, 1.0) set s = flat.rem(s, 2.0) } } catch err { print (s, i) } return (s, i)",
    ].into_iter().enumerate() {
        let source = scalar_document(body);
        for record in [false, true] {
            let reference = observe_scalar(&source, Layout(0).without_regions(), record, false, false);
            for layout in [Layout(0), Layout(1), Layout(2), Layout(0x5eed)] {
                assert_eq!(observe_scalar(&source, layout, record, false, record && case < 3 && layout.0 % 2 == 0), reference, "{body}, {layout:?}, tape={record}");
                assert_eq!(observe_scalar(&source, layout, record, true, false), reference, "cold {body}, {layout:?}, tape={record}");
            }
        }
    }
}

/// K-CHG-001/002 and K-BND-002: a fused call still evaluates every
/// argument, admits depth at each frame, and charges the same ordered
/// events. Sweep every refusal point, including between the two calls of
/// a kind test, with and without a tape. The unfused source layout is the
/// semantic oracle; neither a physical frame nor a memo entry is an oracle.
fn call_prefixes_agree() {
    let mut comparisons = 0;
    let mut entries = 0;
    let definitions = [
        ("leaf(x: Any)", "return 1"),
        ("method(receiver: Any, x: Any)", "return 1"),
        (
            "ordinary(x: Any)",
            "return flat.add(call leaf(flat.add(x, 1)), call leaf(flat.mul(x, 2)))",
        ),
        (
            "receiver(x: Any)",
            "return call method(null, flat.add(x, 2))",
        ),
        ("tag(x: Any)", "return same(kind(x), \"record\")"),
    ]
    .into_iter()
    .map(|(signature, body)| {
        let definition = format!(
            "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        );
        format!(
            "definition {}\n",
            serde_json::to_string(&definition).unwrap()
        )
    })
    .collect::<String>();
    for body in [
        "let a = call ordinary(3) print a let b = call receiver(4) print b do perform 0(\"after\") as Any return b",
        "let f = fn(x) { return 1 } let a = flat.add(apply f(flat.add(1, 2)), apply f(4)) print a do perform 0(\"after\") as Any return a",
        "return call method(nope, flat.add(1, 2))",
        "return call method(null, flat.add(nope, 2))",
        "let xs = [null, false, 1, 1.0, \"x\", (), [], map{}, set{}, {value: 1}] for x in xs { let a = call tag(x) let memo = same(kind(x), \"record\") let named = kind(x) let separate = same(named, \"record\") print (a, memo, separate) } return null",
    ] {
        let source = format!("{definitions}main {{ {body} }}");
        let observe = |layout, bounds, record| {
            let mut embedder = Embedder::with(
                &source,
                Setup {
                    probes: Some(call_probes),
                    layout,
                    bounds,
                    ..Setup::default()
                },
            );
            let tape = crate::ChargeTape::new();
            if record {
                embedder.machine.record_charges(tape.clone());
            }
            let mut end = embedder.run_to_end(&[]);
            // The final carry has its own layout-independent laws. Here the
            // oracle is the result/error, event prefix and effects at refusal.
            if let End::Finished(finished) = &mut end {
                finished.carry.take();
            }
            let entered = crate::kernel_internals::region_work(&tape).calls;
            (
                (
                    end,
                    embedder.world.printed,
                    embedder.requests,
                    embedder.machine.meters().charged.unwrap(),
                    lash_kernel_exec::Weight::expand(tape.steps()),
                ),
                entered,
            )
        };
        let (natural, entered) = observe(Layout(0), super::embedder::ROOMY, true);
        entries += entered;
        if body.starts_with("let a") || body.starts_with("let f") {
            assert!(entered > 0, "the law must execute register calls: {body}");
        }
        for depth in [2, super::embedder::ROOMY.call_depth] {
            for limit in 0..=natural.3 + 1 {
                for record in [false, true] {
                    let bounds = Bounds {
                        call_depth: depth,
                        charge_limit: limit,
                        ..super::embedder::ROOMY
                    };
                    let reference = observe(Layout(0).without_regions(), bounds, record).0;
                    let actual = observe(Layout(0), bounds, record).0;
                    assert_eq!(
                        actual, reference,
                        "{body}, depth={depth}, bound={limit}, tape={record}"
                    );
                    comparisons += 1;
                }
            }
        }
    }
    println!("call event prefixes: {comparisons} paired runs, {entries} natural register entries");
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

/// K-EVAL-008 / K-MACH-008 / K-CHG-001: wide windows keep source
/// slots and captured cells at their semantic coordinates, including the
/// operands of a waiting caller, every finite-slice exit and cold reopen.
fn wide_windows_agree() {
    let definition = |name: &str, body: &str| {
        let function =
            format!("function {name} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}");
        format!("definition {}\n", serde_json::to_string(&function).unwrap())
    };
    let run = |source: &str, layout, cold, bounds| {
        let mut embedder = Embedder::with(
            source,
            Setup {
                layout,
                bounds,
                probes: Some(scalar_probes),
                slice: if cold { 31 } else { u64::MAX },
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        let mut checkpoints = Vec::new();
        let end = loop {
            let step = embedder.run();
            if let Step::Ended(mut end) = step {
                if let End::Finished(finished) = &mut end {
                    finished.carry.take();
                }
                break end;
            }
            if matches!(step, Step::Parked(_)) {
                checkpoints.push(embedder.machine.semantic_checkpoint());
            }
            if cold {
                embedder.reopen(layout, true);
                embedder.machine.record_charges(tape.clone());
            }
            if matches!(step, Step::Parked(_)) {
                embedder.deliver_next();
            }
        };
        (
            (
                end,
                embedder.world.printed,
                embedder.requests,
                embedder.machine.meters().charged.unwrap(),
                lash_kernel_exec::Weight::expand(tape.steps()),
                checkpoints,
            ),
            crate::kernel_internals::region_work(&tape).calls,
        )
    };
    let mut comparisons = 0;
    for width in [17, 260] {
        let locals = (0..width)
            .map(|i| format!("let v{i} = {i}\n"))
            .collect::<String>();
        // No other document callee can satisfy the entry witness: this
        // function itself must run through its wide register window.
        let simple = format!(
            "{}main {{ let answer = call wide(1) print answer return answer }}",
            definition(
                "wide(x: Any)",
                &format!("{locals} return flat.add(x, v{})", width - 1),
            )
        );
        let (natural, entered) = run(&simple, Layout(0), false, super::embedder::ROOMY);
        assert_eq!(result(natural.0.clone()), int(width));
        assert!(entered > 0, "a {width}-slot function must enter registers");
        let parked = format!(
            "{}{}main {{ return call wide(true) }}",
            definition(
                "relay(x: Any, pause: Any)",
                "if pause { do perform 0(\"wide\") as Any } return flat.add(x, 1)",
            ),
            definition(
                "wide(pause: Any)",
                &format!(
                    "{locals} return flat.add(v1, call relay(v{}, pause))",
                    width - 1
                ),
            ),
        );
        let recursive = format!(
            "main {{ let wide = fn(self, n) {{ {locals} if flat.lt(n, 1) {{ return v1 }} return flat.add(v1, apply self(self, flat.add(n, -1))) }} let answer = apply wide(wide, 12) print answer return answer }}"
        );
        let closure = format!(
            "{}main {{ let f = call make() return apply f() }}",
            definition(
                "make()",
                &format!(
                    "{locals} let f = fn() {{ {} return flat.add(a0, a{}) }} return f",
                    (0..width)
                        .map(|i| format!("let a{i} = v{i}\n"))
                        .collect::<String>(),
                    width - 1,
                ),
            ),
        );
        for source in [&simple, &parked, &recursive, &closure] {
            let reference = run(
                source,
                Layout(0).without_regions(),
                false,
                super::embedder::ROOMY,
            )
            .0;
            for layout in [Layout(0), Layout(2), Layout(0x5eed)] {
                assert_eq!(
                    run(source, layout, false, super::embedder::ROOMY).0,
                    reference
                );
                comparisons += 1;
                // Cold wide callers retain their live operands at a park.
                // Recursive budget exits are exercised whole above.
                if source != &recursive {
                    assert_eq!(
                        run(source, layout, true, super::embedder::ROOMY).0,
                        reference
                    );
                    comparisons += 1;
                }
            }
        }
        // Refusals just before/after frame entry and at a later operation
        // must preserve the actual ordered event prefix and no later effect.
        for charge_limit in [0, 1, 2, natural.3 - 1, natural.3] {
            let bounds = Bounds {
                charge_limit,
                ..super::embedder::ROOMY
            };
            assert_eq!(
                run(&simple, Layout(0), false, bounds).0,
                run(&simple, Layout(0).without_regions(), false, bounds).0,
            );
            comparisons += 1;
        }
    }
    // K-BND-003: the same entry reservation must bound all live wide
    // frames. Each caller needs every local after the recursive call, so
    // their holdings cannot disappear through a statement kill set.
    let locals = (0..260)
        .map(|i| format!("let v{i} = {i}\n"))
        .collect::<String>();
    let sums = (0..260)
        .map(|i| format!("set total = flat.add(total, v{i})\n"))
        .collect::<String>();
    let source = format!(
        "main {{ let wide = fn(self, n) {{ {locals} if flat.lt(n, 1) {{ return 0 }} let total = apply self(self, flat.add(n, -1)) {sums} return total }} let answer = apply wide(wide, 100) print answer return answer }}"
    );
    let bounds = Bounds {
        memory: 256 << 10,
        ..super::embedder::ROOMY
    };
    for layout in [Layout(0).without_regions(), Layout(0)] {
        let (seen, _) = run(&source, layout, false, bounds);
        assert!(matches!(
            seen.0,
            End::Error(crate::RunError::Bound(crate::BoundExceeded {
                bound: crate::Bound::Memory,
                ..
            }))
        ));
        assert!(
            seen.1.is_empty(),
            "a memory refusal precedes the later print"
        );
    }
    println!("wide windows: {comparisons} paired runs, two memory refusals");
}
