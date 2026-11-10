//! Calls inside expressions run as machine frames (FIG-6436): every frame
//! counts toward the call-depth bound (`K-BND-002`), a slice and a cancel
//! land between any two statements, a callee's included (`K-MACH-002`,
//! `K-MACH-006`), and entering a callee changes no ready order
//! (`K-TASK-002`). A run is sealed only at a safe point (`K-MACH-007`).

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, FunctionRegistry, parse_definition};

use lash_kernel_value::names::FunctionId;

use super::embedder::{Embedder, ROOMY, Setup, int, result, text};
use crate::machine::Boundary;
use crate::{Bound, BoundExceeded, Bounds, End, Machine, RunError, Step};

/// The probes: `probe.long(tag)` prints `[tag, i]` for `i` from 0 to 3 and
/// gives `tag`; `probe.hold(n)` keeps `n` texts of 1 MiB, printing the
/// count before each; `deep.d0(n)` gives `n` and each `deep.dK(n)` gives
/// `deep.dJ(n) + 1` for `J = K - 1`, from inside an expression. Each
/// states a native implementation that is not registered, so its body
/// runs, inside the expression that calls it.
fn probes(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut register = |name: &str, params: &str, uses: String, body: &str| {
        let definition = parse_definition(&format!(
            "function {name}({params}) -> Any\nkernel 1\ncharge 1\n{uses}native\nbody {{ {body} }}\n"
        ))
        .unwrap();
        registry.register(definition, None).unwrap()
    };

    let uses = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("use {name} = @{}\n", ids[name]))
            .collect::<String>()
    };

    let mut probes = vec![
        (
            "probe.long",
            register(
                "probe.long",
                "tag: Any",
                uses(&["num.lt", "num.add"]),
                "let i = 0 while num.lt(i, 4) { print [tag, i] set i = num.add(i, 1) } return tag",
            ),
        ),
        (
            "probe.hold",
            register(
                "probe.hold",
                "n: Any",
                uses(&["num.lt", "num.add", "work.repeat", "list.len"]),
                "let xs = [] let i = 0 while num.lt(i, n) { print i set xs[i] = work.repeat(1048576) \
                 set i = num.add(i, 1) } return list.len(xs)",
            ),
        ),
    ];
    let mut below = register("deep.d0", "n: Any", String::new(), "return n");
    for level in 1..=100 {
        let name: &'static str = Box::leak(format!("deep.d{level}").into_boxed_str());
        let uses = format!(
            "use num.add = @{}\nuse deep.d{} = @{below}\n",
            ids["num.add"],
            level - 1
        );
        let body = format!("return num.add(deep.d{}(n), 1)", level - 1);
        below = register(name, "n: Any", uses, &body);
        if level == 100 {
            probes.push((name, below));
        }
    }
    probes.push((
        "deep.walk",
        register("deep.walk", "n: Any", String::new(),
            "let first = true for item in n { if first { set first = false } else { let saved = n return saved } } return 0"),
    ));
    probes.push((
        "deep.loop",
        register(
            "deep.loop",
            "n: Any",
            String::new(),
            "let first = true while true { if first { set first = false } else { let saved = n return saved } } return 0",
        ),
    ));
    probes.push((
        "deep.guard",
        register(
            "deep.guard",
            "n: Any",
            String::new(),
            "try { let saved = n return saved } catch e { return 0 } return 0",
        ),
    ));
    probes.push((
        "deep.rescue",
        register(
            "deep.rescue",
            "n: Any",
            String::new(),
            "try { throw 1 } catch e { let saved = n return saved } return 0",
        ),
    ));
    probes.push((
        "deep.cleanup",
        register(
            "deep.cleanup",
            "n: Any",
            String::new(),
            "try { let saved = n return saved } finally { let kept = n } return 0",
        ),
    ));
    probes.push((
        "deep.recover",
        register(
            "deep.recover",
            "n: Any",
            String::new(),
            "try { throw 1 } catch e { let saved = n return saved } finally { let kept = n } return 0",
        ),
    ));
    probes.push((
        "deep.supersede",
        register(
            "deep.supersede",
            "n: Any",
            String::new(),
            "try { return [9] } finally { let saved = n return saved } return 0",
        ),
    ));
    probes.push((
        "deep.brake",
        register(
            "deep.brake",
            "n: Any",
            String::new(),
            "while true { try { let saved = n break } finally { let kept = n } } return n",
        ),
    ));
    probes.push((
        "deep.skip",
        register(
            "deep.skip",
            "n: Any",
            String::new(),
            "let first = true while first { try { set first = false let saved = n continue } finally { let kept = n } } return n",
        ),
    ));
    probes.push((
        "deep.halt",
        register(
            "deep.halt",
            "n: Any",
            String::new(),
            "while true { let saved = n break } return n",
        ),
    ));
    probes.push((
        "deep.pass",
        register(
            "deep.pass",
            "n: Any",
            String::new(),
            "let first = true while first { set first = false let saved = n continue } return n",
        ),
    ));
    probes.push((
        "deep.stop",
        register(
            "deep.stop",
            "n: Any",
            String::new(),
            "for item in n { let saved = n break } return n",
        ),
    ));
    probes.push((
        "deep.next",
        register(
            "deep.next",
            "n: Any",
            String::new(),
            "for item in n { let saved = n continue } return n",
        ),
    ));
    probes.push((
        "deep.catch",
        register(
            "deep.catch",
            "n: Any",
            String::new(),
            "try { let saved = n throw saved } catch e { return e } return 0",
        ),
    ));
    probes.push((
        "deep.fault",
        register(
            "deep.fault",
            "n: Any",
            String::new(),
            "try { try { let saved = n throw saved } finally { let kept = n } } catch e { return e } return 0",
        ),
    ));
    probes.push((
        "deep.rethrow",
        register(
            "deep.rethrow",
            "n: Any",
            String::new(),
            "try { try { throw 1 } catch e { let saved = n throw saved } finally { let kept = n } } catch f { return f } return 0",
        ),
    ));
    probes.push((
        "deep.branch",
        register(
            "deep.branch",
            "n: Any",
            String::new(),
            "if true { let saved = n return saved } return 0",
        ),
    ));
    probes.push((
        "deep.forward",
        register(
            "deep.forward",
            "n: Any",
            format!("use deep.d100 = @{below}\n"),
            "return deep.d100(n)",
        ),
    ));
    probes.push((
        "deep.hold",
        register(
            "deep.hold",
            "n: Any",
            format!("{}use deep.d100 = @{below}\n", uses(&["num.add"])),
            "return num.add(n[0], deep.d100(0))",
        ),
    ));
    probes
}

fn setup(bounds: Bounds, slice: u64) -> Setup {
    Setup {
        bounds,
        slice,
        probes: Some(probes),
        ..Setup::default()
    }
}

/// What a law compares of a run: its end, what it printed, the labels of
/// what it requested, and its charge.
type Observed = (End, Vec<Datum>, Vec<String>, u64);

/// Runs `program` in slices of `slice`, answering each request oldest
/// first, with `at_slice` called at each slice return. Gives what the run
/// did, and how many prints there were at each slice return.
fn observe(
    program: &str,
    slice: u64,
    mut at_slice: impl FnMut(&mut Embedder),
) -> (Observed, Vec<usize>) {
    let mut embedder = Embedder::with(program, setup(ROOMY, slice));
    let mut slices = Vec::new();
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => {
                slices.push(embedder.world.printed.len());
                at_slice(&mut embedder);
            }
            Step::Parked(_) => embedder.deliver_next(),
        }
    };
    let charged = crate::Machine::meters(&embedder.machine)
        .charged
        .expect("exact charge");
    let asked = embedder.asked();
    ((end, embedder.world.printed, asked, charged), slices)
}

fn pair(tag: &str, i: i64) -> Datum {
    Datum::List(vec![text(tag), int(i)])
}

/// FRAME-DEEP (`K-BND-002`): a call inside an expression is a frame like
/// any other. A chain of 101 library bodies, each calling the next inside
/// an expression, runs to its end under a call-depth bound of 101 (main
/// stopped every such chain at 64), and passes a bound of 100 with the
/// typed bound error. Frames called from statements and from expressions
/// count together.
#[test]
fn expression_calls_recurse_to_the_call_depth_bound() {
    let run = |program: &str, call_depth: u32| {
        let bounds = Bounds {
            call_depth,
            ..ROOMY
        };

        Embedder::with(program, setup(bounds, u64::MAX)).run_to_end(&[])
    };
    let chain = "main { return deep.d100(0) }";
    assert_eq!(result(run(chain, 101)), int(100));
    let refused = |end: End| match end {
        End::Error(RunError::Bound(BoundExceeded { bound, limit, .. })) => (bound, limit),
        other => panic!("the chain passed no bound: {other:?}"),
    };
    assert_eq!(refused(run(chain, 100)), (Bound::CallDepth, 100));
    let under_a_call = "definition \"function g() -> Any\\nkernel 1\\nmetering statements\\nbody {  let r = deep.d100(0) return r  }\"\n main { let r = call g() return r }";
    assert_eq!(result(run(under_a_call, 102)), int(100));
    assert_eq!(refused(run(under_a_call, 101)), (Bound::CallDepth, 101));
    assert_eq!(result(run("main { return deep.hold([7]) }", 102)), int(107));

    let scoped = |name: &str| {
        let bounds = Bounds {
            call_depth: 2,
            ..ROOMY
        };
        let args = if matches!(name, "deep.walk" | "deep.next") {
            "[7, 8]"
        } else {
            "[7]"
        };
        let mut embedder = Embedder::with(
            &format!("main {{ return {name}({args}) }}"),
            setup(bounds, u64::MAX),
        );
        assert_eq!(
            result(embedder.run_to_end(&[])),
            Datum::List(if matches!(name, "deep.walk" | "deep.next") {
                vec![int(7), int(8)]
            } else {
                vec![int(7)]
            }),
            "{name} under a call depth of 2"
        );
    };
    for name in [
        "deep.branch",
        "deep.guard",
        "deep.rescue",
        "deep.cleanup",
        "deep.recover",
        "deep.supersede",
        "deep.brake",
        "deep.skip",
        "deep.halt",
        "deep.pass",
        "deep.stop",
        "deep.next",
        "deep.catch",
        "deep.fault",
        "deep.rethrow",
        "deep.loop",
        "deep.walk",
    ] {
        scoped(name);
    }
    // A call inside a cleanup that holds a pending return publishes its
    // Enter at its source occurrence, like the call that reached the cleanup.
    let unwind = "definition \"function f(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  try { let saved = n return saved } finally { let kept = [call g(n)] } return 0  }\"\ndefinition \"function g(n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return n  }\"\n main { let r = call f([7]) return r }";

    let mut embedder = Embedder::with(unwind, setup(ROOMY, u64::MAX));
    assert_eq!(result(embedder.run_to_end(&[])), Datum::List(vec![int(7)]));
}

/// The callee's prints `[tag, 0]` to `[tag, 3]`.
fn long(tag: &str) -> Vec<Datum> {
    (0..4).map(|i| pair(tag, i)).collect()
}

/// Long straight-line scalar callees, with a captured cell shared across
/// calls, and outer evaluated operands retained while another callee parks.
fn straight_line_calls() -> String {
    let work = "set n = num.add(n, 1) ".repeat(96);
    format!(
        r#"main {{
  let shared = 5
  let leaf = fn(n) {{ {work} return num.add(n, shared) }}
  let relay = fn(f, n) {{
    let a = apply f(n)
    let b = apply f(num.add(n, 1))
    return num.add(a, b)
  }}
  let first = (apply relay(leaf, 1), perform 0("between") as Any, apply relay(leaf, 2))
  set shared = 7
  let later = apply relay(leaf, 3)
  return (first, later)
}}"#
    )
}

/// SLICE-IN-CALLEE (`K-MACH-002`, `K-MACH-005`, `K-BND-003`): a slice lands
/// between two statements of a body called inside an expression, and at
/// slices MAX, 1 and 0 the run ends the same, prints and requests the
/// same and is charged the same. The memory bound gives one verdict at
/// one statement of such a body, whatever the slicing and the collection.
#[test]
fn a_slice_lands_inside_an_expression_callee_and_changes_nothing() {
    let program = r#"main {
  let w = perform 0("before") as Any
  let v = [probe.long("a"), probe.long("b")]
  let z = perform 0("after") as Any
  print v
  return (w, v, z)
}"#;
    let (whole, none) = observe(program, u64::MAX, |_| {});
    assert!(none.is_empty());
    let mut printed = long("a");
    printed.extend(long("b"));
    printed.push(Datum::List(vec![text("a"), text("b")]));
    assert_eq!(whole.1, printed);
    assert_eq!(whole.2, ["before", "after"]);
    for slice in [1, 0] {
        let (sliced, slices) = observe(program, slice, |_| {});
        assert_eq!(sliced, whole, "slice {slice}");
        if slice == 0 {
            // Main ran each expression's callee to its end in one step.
            assert!(
                slices.iter().any(|&prints| (1..4).contains(&prints)),
                "no slice returned inside the callee: {slices:?}"
            );
        }
    }

    // K-MACH-002/007: polling must also reach the interior of a long
    // straight-line body, with no loop backedge on which to rely.
    let straight = straight_line_calls();
    let (whole, _) = observe(&straight, u64::MAX, |_| {});
    for slice in [0, 1, 17, 129] {
        let mut inside = 0;
        let (sliced, _) = observe(&straight, slice, |embedder| {
            inside += usize::from(embedder.machine.pending_phases().len() >= 2);
        });
        assert_eq!(sliced, whole, "straight-line slice {slice}");
        assert!(inside > 0, "a finite slice must reach the nested callee");
    }

    // K-BND-002: explicit call frames count together with the real caller.
    let recursive = "main { let down = fn(self, n) { if num.lt(n, 4) { return num.add(apply self(self, num.add(n, 1)), 1) } return 0 } return apply down(down, 0) }";
    for depth in [4, 8] {
        let bounds = Bounds {
            call_depth: depth,
            ..ROOMY
        };
        let end = Embedder::with(recursive, setup(bounds, 7)).run_to_end(&[]);
        if depth == 4 {
            assert!(matches!(
                end,
                End::Error(RunError::Bound(BoundExceeded {
                    bound: Bound::CallDepth,
                    ..
                }))
            ));
        } else {
            assert_eq!(result(end), int(4));
        }
    }

    let memory = End::Error(RunError::Bound(BoundExceeded {
        bound: Bound::Memory,
        limit: 64 << 20,
        function: None,
        site: Some(Box::new(lash_kernel_value::sites::Site::new(
            lash_kernel_value::sites::Unit::Main,
            vec![0],
        ))),
    }));
    let held = "main { let n = list.len([probe.hold(100), 1]) return n }";
    let verdict = |slice: u64, collect: bool| {
        let bounds = Bounds {
            memory: 64 << 20,
            ..ROOMY
        };
        let mut embedder = Embedder::with(held, setup(bounds, slice));
        if collect {
            embedder.machine.boundary = Some(Box::new(|_| Boundary::Collect));
        }
        let end = match embedder.run_to_end(&[]) {
            End::Error(RunError::Bound(exceeded)) => End::Error(RunError::Bound(BoundExceeded {
                function: None,
                ..exceeded
            })),
            other => other,
        };
        (end, embedder.world.printed)
    };
    let (end, first) = verdict(u64::MAX, false);
    assert_eq!(end, memory);
    assert!(first.len() > 1 && first.len() < 100, "{first:?}");
    for slice in [u64::MAX, 7, 1, 0] {
        for collect in [false, true] {
            assert_eq!(
                verdict(slice, collect),
                (memory.clone(), first.clone()),
                "slice {slice}, collected {collect}"
            );
        }
    }
}

/// The program SCHED-TURN runs: two workers that print, yield and wait,
/// spawned before `main` calls `probe.long` twice, nested in one
/// expression or hoisted into statements of their own.
fn scheduled(callee: &str) -> String {
    format!(
        r#"definition "function scalar(n: Any) -> Any\nkernel 1\nmetering statements\nbody {{ let a = num.add(n, 1) set a = num.add(a, 1) return a }}"

definition "function worker(tag: Any) -> Any\nkernel 1\nmetering statements\nbody {{ \n  print tag\n  do yield\n  let r = perform 0(tag) as Any\n  print r\n  return r\n }}"

main {{
  let x = spawn call worker("x")
  let y = spawn call worker("y")
  let before = num.add(call scalar(0), call scalar(1))
  {callee}
  let hs = [x, y]
  let rs = join all hs
  return (v, rs)
}}"#
    )
}

/// SCHED-TURN (`K-TASK-002`, `K-TASK-003`, `K-MACH-005`, `K-MACH-006`):
/// entering a callee inside an expression and a slice inside it change no
/// ready order. With two other tasks ready, the prints and requests at
/// slices MAX, 1 and 0 are those of the program that calls the body from
/// statements of its own, and a cancel the host reports while the callee
/// runs ends both runs at the same print.
#[test]
fn entering_and_slicing_an_expression_callee_changes_no_ready_order() {
    let nested = scheduled("let v = [probe.long(\"a\"), probe.long(\"b\")]");
    let hoisted = scheduled(
        "let p = invoke probe.long(\"a\") let q = invoke probe.long(\"b\") let v = [p, q]",
    );
    let mut printed = vec![text("x"), text("y")];
    printed.extend(long("a"));
    printed.extend(long("b"));
    printed.extend([text("x"), text("y")]);
    let value = Datum::Tuple(vec![
        Datum::List(vec![text("a"), text("b")]),
        Datum::List(vec![text("x"), text("y")]),
    ]);
    for program in [&nested, &hoisted] {
        let (whole, _) = observe(program, u64::MAX, |_| {});
        assert_eq!(
            (result(whole.0.clone()), &whole.1, &whole.2),
            (
                value.clone(),
                &printed,
                &vec!["x".to_string(), "y".to_string()]
            )
        );
        for slice in [1, 0] {
            assert_eq!(observe(program, slice, |_| {}).0, whole, "slice {slice}");
        }
    }
    // The host cancels once `[a, 1]` is printed: at the next slice
    // boundary, inside the callee in both programs.
    let cancelled = |program: &str| {
        observe(program, 0, |embedder| {
            if embedder.world.printed.contains(&pair("a", 1)) {
                embedder.world.cancel = true;
            }
        })
        .0
    };
    let hoisted = cancelled(&hoisted);
    assert_eq!(hoisted.0, End::Cancelled);
    assert_eq!(
        hoisted.1,
        [text("x"), text("y"), pair("a", 0), pair("a", 1)]
    );
    let nested = cancelled(&nested);
    assert_eq!((&nested.0, &nested.1), (&hoisted.0, &hoisted.1));
}

/// `K-MACH-007`, `K-EVAL-008`: a run is at a safe point at every slice,
/// one inside a callee called inside an expression included: sealed there
/// and opened cold at every slice, it ends as
/// the run never sealed.
#[test]
fn a_seal_inside_an_expression_callee_is_exact() {
    let straight = straight_line_calls();
    let mut active = Embedder::with(&straight, setup(ROOMY, 17));
    let mut reference = Embedder::with(&straight, setup(ROOMY, 17));
    let tape = crate::ChargeTape::new();
    let reference_tape = crate::ChargeTape::new();
    active.machine.record_charges(tape.clone());
    reference.machine.record_charges(reference_tape.clone());

    let mut checkpoints = 0;
    loop {
        let step = active.run();
        assert_eq!(format!("{step:?}"), format!("{:?}", reference.run()));
        assert_eq!(
            crate::Weight::expand(tape.steps()),
            crate::Weight::expand(reference_tape.steps()),
            "ordered committed charge events under different region splits"
        );
        if matches!(step, Step::Ended(_)) {
            break;
        }
        assert_eq!(
            active.machine.semantic_checkpoint(),
            reference.machine.semantic_checkpoint()
        );
        let root = active.checkpoint();
        let expected = reference.checkpoint();
        // Compare the canonical frame/task/ready roots themselves, not
        // merely the values read back from them.
        assert_eq!(root.tasks, expected.tasks);
        assert_eq!(root.ready, expected.ready);
        assert_eq!(root.waits, expected.waits);

        checkpoints += 1;
        active.open_at(&root, true);
        active.machine.record_charges(tape.clone());
        assert_eq!(
            active.machine.semantic_checkpoint(),
            reference.machine.semantic_checkpoint()
        );
        if matches!(step, Step::Parked(_)) {
            active.deliver_next();
            reference.deliver_next();
        }
    }
    assert!(checkpoints > 20);

    assert_eq!(active.world.printed, reference.world.printed);
    assert_eq!(active.requests, reference.requests);

    let program = scheduled("let v = [probe.long(\"a\"), probe.long(\"b\")]");
    let mut uninterrupted = Embedder::with(&program, setup(ROOMY, u64::MAX));
    let whole_end = uninterrupted.run_to_end(&[]);
    let mut whole = (
        whole_end,
        uninterrupted.world.printed.clone(),
        uninterrupted.asked(),
        uninterrupted.machine.meters().charged.unwrap(),
    );
    let mut embedder = Embedder::with(&program, setup(ROOMY, 0));
    let mut sealed = 0;
    let mut end = loop {
        let step = embedder.run();
        if !matches!(step, Step::Ended(_)) {
            let root = embedder.checkpoint();
            sealed += 1;
            embedder.open_at(&root, true);
        }
        match step {
            Step::Ended(end) => break end,
            Step::Slice => {}
            Step::Parked(_) => embedder.deliver_next(),
        }
    };
    assert!(sealed >= 10, "{sealed} sealed");
    let (charged, asked) = (
        crate::Machine::meters(&embedder.machine)
            .charged
            .expect("exact charge"),
        embedder.asked(),
    );
    let carried = |end: &mut End, source: &super::embedder::Source| {
        let End::Finished(finished) = end else {
            panic!("the expression callee finishes")
        };
        finished.carry.take().map(|root| {
            lash_kernel_state::semantic::session(&root, Box::new(source.clone())).unwrap()
        })
    };
    assert_eq!(
        carried(&mut end, &embedder.source),
        carried(&mut whole.0, &uninterrupted.source)
    );
    assert_eq!((end, embedder.world.printed, asked, charged), whole);
}

/// Seam E (`docs/design/kernel-end-state.md`, §D invariant 2): a frame is
/// projected only when it changed since the last projection or is its
/// task's top frame. A run 40 calls deep, every frame holding bindings,
/// projects at every slice return and every resume into a nested action,
/// and no projection visits more than the top frame and the caller it
/// pushed or returned into, however deep the stack grows. The frozen
/// engine's frames-visited counter went with it (FIG-7688); the run's
/// answer and slicing stand.
#[test]
fn a_projection_visits_the_changed_frames_and_the_top_however_deep_the_stack() {
    let program = "definition \"function down(self: Any, i: Any, n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { \\n  if num.lt(i, n) { let a = i let b = [i, n] let r = num.add(apply self(self, num.add(i, 1), n), 0) return r }\\n  let j = 0 while num.lt(j, 6) { set j = num.add(j, 1) } return j\\n }\"\n\nmain { let r = call down(&down, 0, 40) return r }";
    let mut embedder = Embedder::with(program, setup(ROOMY, 1));
    let mut slices = 0;
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => slices += 1,
            Step::Parked(_) => embedder.deliver_next(),
        }
    };
    assert_eq!(result(end), int(6));
    assert!(slices > 80, "{slices} slices");
}

/// K-EVAL-008 / K-FORM-004: a cold branch may create a locally shared
/// variable before a later statement can re-enter the register loop.
/// Its cell identity, including closures subsequently made from it,
/// survives that boundary and a cold reopen.
#[test]
fn a_cold_branch_keeps_its_locally_shared_cell_when_the_loop_reenters() {
    let program = r#"main {
  let choose = fn(flag) {
    if flag {
      let n = 0
      let bump = fn() { set n = num.add(n, 1) return n }
      let a = apply bump()
      let pure = num.add(1, 2)
      let b = apply bump()
      return (a, b, n)
    }
    return null
  }
  return apply choose(true)
}"#;
    let expected = Datum::Tuple(vec![int(1), int(2), int(2)]);
    for slice in [u64::MAX, 17] {
        let mut active = Embedder::with(program, setup(ROOMY, slice));
        let mut reference = Embedder::with(program, setup(ROOMY, slice));
        assert_eq!(result(reference.run_to_end(&[])), expected);
        let end = loop {
            match active.run() {
                Step::Ended(end) => break end,
                Step::Slice => {
                    let root = active.checkpoint();
                    active.open_at(&root, true);
                }
                Step::Parked(_) => panic!("this cell-sharing fixture has no effects"),
            }
        };
        assert_eq!(result(end), expected);
        assert_eq!(
            active.machine.meters().charged,
            reference.machine.meters().charged
        );
    }
}
