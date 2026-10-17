//! The bound laws (`K-BND-001`): each bound ends the run with its typed
//! error, and no `catch` or `finally` sees it (`K-ERR-004`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lash_kernel_doc::{parse_definition, parse_document};

use super::embedder::{Embedder, REPEAT_BUILDS, ROOMY, Setup, World, int, library, result};
use crate::{
    Bound, BoundExceeded, Bounds, CellStart, End, KernelMachine, Machine, Program, RunError, Step,
    Target,
};

/// Wraps `body` so that a raise would be caught and a cleanup would
/// print.
fn guarded(body: &str) -> String {
    format!(
        r#"
fn down(n) {{ let r = call down(n) return r }}
fn waits(tag) {{ do perform echo(tag) as Any }}
main {{
  try {{ {body} }} catch e {{ print "caught" }} finally {{ print "cleanup" }}
  return "survived"
}}"#
    )
}

/// The bound a run of `body` passes under `bounds`.
fn passes(body: &str, bounds: Bounds) -> BoundExceeded {
    let mut embedder = Embedder::with(
        &guarded(body),
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    let end = embedder.run_to_end(&[]);
    assert_eq!(embedder.world.printed, [], "a bound is not a raise");
    match end {
        End::Error(RunError::Bound(exceeded)) => exceeded,
        other => panic!("the run passed no bound: {other:?}"),
    }
}

fn exceeded(bound: Bound, limit: u64) -> BoundExceeded {
    BoundExceeded {
        bound,
        limit,
        function: None,
    }
}

/// `K-CHG-008`: the charge that takes the run past its bound ends it.
#[test]
fn the_charge_bound_ends_the_run() {
    let bounds = Bounds {
        charge: 1_000,
        ..ROOMY
    };
    assert_eq!(
        passes("while true { }", bounds),
        exceeded(Bound::Charge, 1_000)
    );
}

/// `K-BND-001`, `K-BND-003`: memory that is live counts against the
/// bound, and memory the run no longer reaches does not.
#[test]
fn the_memory_bound_holds_inside_a_native_call() {
    let bounds = Bounds {
        memory: 64 << 10,
        ..ROOMY
    };
    assert_eq!(
        passes("let big = work.fill(100000)", bounds),
        BoundExceeded {
            function: Some(lash_kernel_doc::FunctionName::new("work.fill").unwrap()),
            ..exceeded(Bound::Memory, 64 << 10)
        }
    );
    // What fits once fits every time: the garbage of earlier calls is
    // collected before a native call is refused.
    let mut embedder = Embedder::with(
        r#"main {
  let i = 0
  while num.lt(i, 200) {
    let filled = work.fill(1000)
    set i = num.add(i, 1)
  }
  return i
}"#,
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    assert_eq!(result(embedder.run_to_end(&[])), int(200));
}

/// `K-BND-001`, `K-LIB-007`: a native function reserves what it is about
/// to build, and a reservation the bound has no room for ends the run
/// before the function builds anything.
#[test]
fn the_memory_bound_refuses_a_native_reservation_before_the_allocation() {
    let bounds = Bounds {
        memory: 64 << 10,
        ..ROOMY
    };
    assert_eq!(
        passes("let big = work.repeat(100000)", bounds),
        BoundExceeded {
            function: Some(lash_kernel_doc::FunctionName::new("work.repeat").unwrap()),
            ..exceeded(Bound::Memory, 64 << 10)
        }
    );
    assert_eq!(REPEAT_BUILDS.load(Ordering::Relaxed), 0);
    // A reservation is refused only by what is live: the garbage of
    // earlier calls is collected and the call made again.
    let mut embedder = Embedder::with(
        r#"main {
  let i = 0
  while num.lt(i, 200) {
    let held = work.repeat(1000)
    set i = num.add(i, 1)
  }
  return i
}"#,
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    assert_eq!(result(embedder.run_to_end(&[])), int(200));
    assert_eq!(REPEAT_BUILDS.load(Ordering::Relaxed), 200);
}

/// `K-BND-002`: call depth is counted per task.
#[test]
fn the_call_depth_bound_ends_the_run() {
    let bounds = Bounds {
        call_depth: 50,
        ..ROOMY
    };
    assert_eq!(
        passes("do call down(1)", bounds),
        exceeded(Bound::CallDepth, 50)
    );
}

/// `K-TASK-022`: a `spawn` that would pass the task bound ends the run.
#[test]
fn the_task_bound_ends_the_run() {
    let bounds = Bounds {
        live_tasks: 3,
        ..ROOMY
    };
    assert_eq!(
        passes("while true { do spawn call waits(\"w\") }", bounds),
        exceeded(Bound::LiveTasks, 3)
    );
}

/// `K-BND-001`: the effects and sleeps requested at one park are bounded.
#[test]
fn the_requests_per_park_bound_ends_the_run() {
    let bounds = Bounds {
        requests_per_park: 2,
        ..ROOMY
    };
    assert_eq!(
        passes("while true { do spawn call waits(\"w\") }", bounds),
        exceeded(Bound::RequestsPerPark, 2)
    );
}

/// `K-TASK-010`: a list `join` longer than the member bound ends the run.
#[test]
fn the_join_member_bound_ends_the_run() {
    let bounds = Bounds {
        join_members: 2,
        ..ROOMY
    };
    assert_eq!(
        passes(
            "let h = spawn call waits(\"w\") let hs = [h, h, h] do join all hs",
            bounds
        ),
        exceeded(Bound::JoinMembers, 2)
    );
}

/// `K-LIB-008`: a native call that would pass its guard's limit ends the
/// run with a bound error naming the function; one that stays within it
/// returns.
#[test]
fn a_native_guard_ends_the_run() {
    let mut embedder = Embedder::new("main { return work.spin(100) }");
    let function = embedder.library.ids["work.spin"];
    assert_eq!(result(embedder.run_to_end(&[])), int(100));
    assert_eq!(
        passes("let spun = work.spin(101)", ROOMY),
        BoundExceeded {
            function: Some(lash_kernel_doc::FunctionName::new("work.spin").unwrap()),
            ..exceeded(Bound::Guard { function }, 100)
        }
    );
}

/// `K-BND-003`, `K-VAL-011`: collecting the heap frees nothing the run
/// can still reach. A value in flight between two frames, or between a
/// wait and the statement it resumes, is held by no variable; the padding
/// moves the collections across every such point. `keep` outlives every
/// collection while the objects allocated around it are freed, and is read
/// and written after each.
#[test]
fn a_collection_frees_nothing_the_run_can_reach() {
    let mut embedder = Embedder::with(
        r#"
fn thrower(i) { let e = {kind: "x", n: [i, i]} throw e }
fn returner(i) { let r = {n: [i, i]} return r }
fn later(i) { do yield let r = [i, i] return r }
main {
  let keep = [7, 8]
  let i = 0
  let total = 0
  while num.lt(i, 300) {
    let pad = work.fill(i)
    set total = num.add(total, keep[1])
    set keep[1] = num.add(keep[1], 1)
    try { do call thrower(i) } catch e { let n = e.n set total = num.add(total, n[0]) }
    let r = call returner(i)
    let n = r.n
    set total = num.add(total, n[1])
    let a = spawn call later(i)
    let b = spawn call later(i)
    let hs = [a, b]
    let rs = join all hs
    let first = rs[1]
    set total = num.add(total, first[0])
    set i = num.add(i, 1)
  }
  return total
}"#,
        Setup {
            bounds: Bounds {
                memory: 16 << 10,
                live_tasks: 1_000,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    assert_eq!(
        result(embedder.run_to_end(&[])),
        int(3 * (299 * 300 / 2) + (8..308).sum::<i64>())
    );
}

/// `K-CHG-005`: a value copied out is charged its deep size, a heap object
/// reached more than once counted the first time only, however many
/// objects the value holds.
#[test]
fn a_shared_object_is_charged_once_in_a_wide_graph() {
    let lists: Vec<String> = (1..=10).map(|n| format!("a{n}")).collect();
    let made: String = (1..=10).map(|n| format!("let a{n} = [{n}] ")).collect();
    let body = format!("{made}let x = [{0}, {0}]", lists.join(", "));
    let charged = |body: &str| {
        let mut embedder = Embedder::new(&format!("main {{ {body} }}"));
        embedder.run_to_end(&[]);
        embedder.machine.meters().charged
    };
    // `return x`: 1 statement, 1 variable node, and `x` copied out: the
    // list of 20 members (21) and each of its ten lists once, `[n]` (2)
    // with its one-word integer (2).
    assert_eq!(
        charged(&format!("{body} return x")) - charged(&body),
        2 + 21 + 10 * 4
    );
}

/// `K-CHG-007`: a helper, a function with only a kernel-code body, is
/// ordinary code: its forms are charged as they run, so a helper that runs
/// away ends at the charge bound like any other code. (A function with a native
/// implementation is still charged its formula alone, whichever of its
/// implementations runs: `laws::layout::a_native_implementation_and_a_kernel_body_run_the_same`.)
#[test]
fn a_helper_that_runs_away_ends_at_the_charge_bound() {
    let library = library(true);
    let mut registry = (*library.registry).clone();
    let id = |name: &str| library.ids[name];
    let uses = format!(
        "use num.lt = @{}\nuse num.add = @{}\n",
        id("num.lt"),
        id("num.add")
    );
    let spin = parse_definition(&format!(
        "function probe.spin(n: Any) -> Any\nkernel 1\ncharge 1\n{uses}\
         body {{ let i = 0 while num.lt(i, n) {{ set i = num.add(i, 1) }} return i }}\n"
    ))
    .unwrap();
    let spin = registry.register(spin, None).unwrap();
    let text = format!(
        "numbers by_spelling\nkernel 1\nuse probe.spin = @{spin}\n\
         main {{ let n = invoke probe.spin(200000) return n }}\n"
    );
    let program = Program {
        release: lash_kernel_doc::heap::PageId([8; 32]),
        document: Arc::new(parse_document(&text).unwrap()),
        library: crate::PreparedLibrary::new(Arc::new(registry)),
    };
    let start = CellStart {
        target: Target::Main,
        args: Vec::new(),
        session: None,
        owner: lash_kernel_doc::heap::PageId([9; 32]),
        modes: crate::EffectModes::recorded(),
        host: Vec::new(),
    };
    let bounds = Bounds {
        charge: 10_000,
        ..ROOMY
    };
    let mut machine = super::embedder::start_machine(program, bounds, start).unwrap();
    match machine.run(&mut World::default(), u64::MAX).unwrap() {
        Step::Ended(End::Error(RunError::Bound(exceeded))) => {
            assert_eq!((exceeded.bound, exceeded.limit), (Bound::Charge, 10_000));
        }
        other => panic!("the helper's loop was not charged: {other:?}"),
    }
}

/// `K-CHG-007`, `K-LIB-004`: a closure made in the body of a function with a
/// native implementation runs only where it is applied, which that body
/// cannot do, so it is ordinary code: one that escapes the body and runs
/// away ends at the charge bound (FIG-5825).
#[test]
fn a_closure_a_native_backed_body_makes_is_charged_where_it_runs() {
    let library = library(true);
    let mut registry = (*library.registry).clone();
    let id = |name: &str| library.ids[name];
    let uses = format!(
        "use num.lt = @{}\nuse num.add = @{}\n",
        id("num.lt"),
        id("num.add")
    );
    // Its native implementation is not registered, so its body runs.
    let make = parse_definition(&format!(
        "function probe.counter(n: Any) -> Any\nkernel 1\ncharge 1\n{uses}native\n\
         body {{ return fn() {{ let i = 0 while num.lt(i, n) {{ set i = num.add(i, 1) }} return i }} }}\n"
    ))
    .unwrap();
    let make = registry.register(make, None).unwrap();
    let text = format!(
        "numbers by_spelling\nkernel 1\nuse probe.counter = @{make}\n\
         main {{ let count = probe.counter(200000) let n = apply count() return n }}\n"
    );
    let program = Program {
        release: lash_kernel_doc::heap::PageId([8; 32]),
        document: Arc::new(parse_document(&text).unwrap()),
        library: crate::PreparedLibrary::new(Arc::new(registry)),
    };
    let start = CellStart {
        target: Target::Main,
        args: Vec::new(),
        session: None,
        owner: lash_kernel_doc::heap::PageId([9; 32]),
        modes: crate::EffectModes::recorded(),
        host: Vec::new(),
    };
    let bounds = Bounds {
        charge: 10_000,
        ..ROOMY
    };
    let mut machine = super::embedder::start_machine(program, bounds, start).unwrap();
    match machine.run(&mut World::default(), u64::MAX).unwrap() {
        Step::Ended(End::Error(RunError::Bound(exceeded))) => {
            assert_eq!((exceeded.bound, exceeded.limit), (Bound::Charge, 10_000));
        }
        other => panic!("the escaped closure's loop was not charged: {other:?}"),
    }
}

/// `K-CHG-007`: a helper's charge is its body's work; its formula is not
/// charged on top. Two helpers with one body and different formulas cost
/// the same.
#[test]
fn a_helper_is_charged_its_body_and_not_its_formula() {
    let charged = |formula: &str| {
        let library = library(true);
        let mut registry = (*library.registry).clone();
        let uses = format!("use num.add = @{}\n", library.ids["num.add"]);
        let step = parse_definition(&format!(
            "function probe.step(n: Any) -> Any\nkernel 1\ncharge {formula}\n{uses}\
             body {{ return num.add(n, 1) }}\n"
        ))
        .unwrap();
        let step = registry.register(step, None).unwrap();
        let text = format!(
            "numbers by_spelling\nkernel 1\nuse probe.step = @{step}\n\
             main {{ let n = invoke probe.step(1) return n }}\n"
        );
        let program = Program {
            release: lash_kernel_doc::heap::PageId([8; 32]),
            document: Arc::new(parse_document(&text).unwrap()),
            library: crate::PreparedLibrary::new(Arc::new(registry)),
        };
        let start = CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: lash_kernel_doc::heap::PageId([9; 32]),
            modes: crate::EffectModes::recorded(),
            host: Vec::new(),
        };
        let mut machine = super::embedder::start_machine(program, ROOMY, start).unwrap();
        match machine.run(&mut World::default(), u64::MAX).unwrap() {
            Step::Ended(End::Finished(_)) => machine.meters().charged,
            other => panic!("the helper did not finish: {other:?}"),
        }
    };
    assert_eq!(charged("1"), charged("100000"));
}

/// A run exactly at each bound passes none of them, and
/// one more request than a park may hold passes that bound (`K-BND-001`,
/// `K-BND-002`, `K-TASK-010`).
#[test]
fn a_run_exactly_at_its_bounds_passes_none() {
    let text = r#"
fn c() { return 1 }
fn b() { let r = call c() return r }
fn a() { let r = call b() return r }
fn w(tag) { let r = perform echo(tag) as Any return r }
main {
  let d = call a()
  let x = spawn call w("x")
  let y = spawn call w("y")
  let hs = [x, y]
  let rs = join all hs
  return (d, rs)
}"#;
    let bounds = Bounds {
        call_depth: 3,
        requests_per_park: 2,
        join_members: 2,
        ..ROOMY
    };
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    let end = embedder.run_to_end(&["x", "y"]);
    assert_eq!(
        result(end),
        lash_kernel_doc::Datum::Tuple(vec![
            int(1),
            lash_kernel_doc::Datum::List(vec![
                super::embedder::text("x"),
                super::embedder::text("y"),
            ]),
        ])
    );
    assert_eq!(
        passes(
            "do spawn call waits(\"a\") do spawn call waits(\"b\") do spawn call waits(\"c\")",
            bounds
        ),
        exceeded(Bound::RequestsPerPark, 2)
    );
}

/// KERNEL-DATA-POLICY: deployment value-depth guards reach tuple construction.
#[test]
fn a_host_value_depth_policy_is_honored_by_a_kernel_run() {
    let mut embedder = Embedder::new("main { return (1, (2, 3)) }");
    let (program, _) = embedder.program(false);
    let limits = lash_kernel_doc::KernelDataLimits::new(64, 352, 512, 512, 1).unwrap();
    embedder.machine = KernelMachine::start_with_data_limits(
        program,
        ROOMY,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: lash_kernel_doc::heap::PageId([9; 32]),
            modes: crate::EffectModes::recorded(),
            host: Vec::new(),
        },
        limits,
        Box::new(super::embedder::Source::default()),
    )
    .unwrap();
    let mut world = World::default();
    let step = embedder.machine.run(&mut world, 100_000).unwrap();
    assert!(
        format!("{step:?}").contains("configured 1-level"),
        "{step:?}"
    );
}
