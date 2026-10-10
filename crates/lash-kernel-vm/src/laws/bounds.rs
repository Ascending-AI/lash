//! The bound laws (`K-BND-001`): each bound ends the run with its typed
//! error, and no `catch` or `finally` sees it (`K-ERR-004`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use lash_kernel_doc::{
    NativeCall, NativeError, NativeFunction, Value, parse_definition, parse_document,
};

use super::embedder::{Embedder, REPEAT_BUILDS, ROOMY, Setup, World, int, library, result};
use crate::{
    Bound, BoundExceeded, Bounds, End, KernelMachine, Machine, Program, RunError, Start, Step,
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
fn the_memory_bound_counts_what_is_live() {
    let bounds = Bounds {
        memory: 64 << 10,
        ..ROOMY
    };
    assert_eq!(
        passes(
            "let xs = [] while true { set xs[list.len(xs)] = \"0123456789\" }",
            bounds
        ),
        BoundExceeded {
            function: Some(lash_kernel_doc::FunctionName::new("list.len").unwrap()),
            ..exceeded(Bound::Memory, 64 << 10)
        }
    );
    // The same allocations, dropped as they are made, fit.
    let mut embedder = Embedder::with(
        r#"main {
  let i = 0
  while num.lt(i, 5000) {
    let garbage = [i, "0123456789", [i]]
    set i = num.add(i, 1)
  }
  return i
}"#,
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    assert_eq!(result(embedder.run_to_end(&[])), int(5000));
    assert!(embedder.machine.meters().memory <= 64 << 10);
}

/// `K-BND-001`: an allocation a native function makes counts too.
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
/// moves the collections across every such point.
#[test]
fn a_collection_frees_nothing_the_run_can_reach() {
    let mut embedder = Embedder::with(
        r#"
fn thrower(i) { let e = {kind: "x", n: [i, i]} throw e }
fn returner(i) { let r = {n: [i, i]} return r }
fn later(i) { do yield let r = [i, i] return r }
main {
  let i = 0
  let total = 0
  while num.lt(i, 300) {
    let pad = work.fill(i)
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
    assert_eq!(result(embedder.run_to_end(&[])), int(3 * (299 * 300 / 2)));
}

/// Returns null; what a law reads is its definition's charge formula.
struct Measured;
impl NativeFunction for Measured {
    fn call(&self, _call: NativeCall<'_>) -> Result<Value, NativeError> {
        Ok(Value::Null)
    }
}

const GROWN: i64 = 10_000;

/// How long a library body takes to grow a list of `GROWN` one-member
/// lists, calling after each step a function charged `charge` on the list.
fn grow_in_a_body(charge: &str) -> Duration {
    let library = library(true);
    let mut registry = (*library.registry).clone();
    let id = |name: &str| library.ids[name];
    let measure = parse_definition(&format!(
        "function probe.measure(xs: Any) -> Any\nkernel 1\ncharge {charge}\nnative\n"
    ))
    .unwrap();
    let measure = registry
        .register(measure, Some(Arc::new(Measured)))
        .unwrap();
    let uses = format!(
        "use list.len = @{}\nuse num.lt = @{}\nuse num.add = @{}\nuse probe.measure = @{measure}\n",
        id("list.len"),
        id("num.lt"),
        id("num.add")
    );
    let grow = parse_definition(&format!(
        "function probe.grow(n: Any) -> Any\nkernel 1\ncharge 1\n{uses}\
         body {{ let out = [] let i = 0 while num.lt(i, n) {{ set out[list.len(out)] = [i] \
         let m = probe.measure(out) set i = num.add(i, 1) }} return list.len(out) }}\n"
    ))
    .unwrap();
    let grow = registry.register(grow, None).unwrap();
    let text = format!(
        "numbers by_spelling\nkernel 1\n{uses}use probe.grow = @{grow}\n\
         main {{ let n = invoke probe.grow({GROWN}) return n }}\n"
    );
    let program = Program {
        document: Arc::new(parse_document(&text).unwrap()),
        library: crate::PreparedLibrary::new(Arc::new(registry)),
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Default::default(),
    };
    let mut machine = KernelMachine::start(program, ROOMY, start).unwrap();
    let started = Instant::now();
    let step = machine.run(&mut World::default(), u64::MAX).unwrap();
    let elapsed = started.elapsed();
    match step {
        Step::Ended(end) => assert_eq!(result(end), int(GROWN)),
        other => panic!("the run did not end: {other:?}"),
    }
    elapsed
}

/// `K-CHG-007`: inside a library body nothing is charged, so a call there
/// costs no accounting. A body that calls a function charged by the deep
/// size of the list it grows runs as fast as one that calls a function
/// charged a constant; evaluating each discarded formula made it quadratic,
/// and Test262 harness loops ran for minutes before their charge bound
/// fired (FIG-5788).
#[test]
fn a_call_inside_a_library_body_does_no_accounting() {
    let constant = grow_in_a_body("1");
    let deep = grow_in_a_body("deep(xs)");
    assert!(
        deep < constant * 4 + Duration::from_secs(1),
        "deep-size formula {deep:?}, constant formula {constant:?}"
    );
}
