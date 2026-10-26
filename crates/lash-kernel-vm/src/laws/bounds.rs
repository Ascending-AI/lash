//! The bound laws (`K-BND-001`): each bound ends the run with its typed
//! error, and no `catch` or `finally` sees it (`K-ERR-004`).

use std::sync::Arc;

use lash_kernel_doc::parse_document;

use super::embedder::{Embedder, REPEAT_BUILDS, ROOMY, Setup, World, int, library, result};
use crate::{
    Bound, BoundExceeded, Bounds, CellStart, End, KernelMachine, Machine, RunError, Step, Target,
};

/// Wraps `body` so that a raise would be caught and a cleanup would
/// print.
fn guarded(body: &str) -> String {
    format!(
        r#"
definition "function waits(tag: Any) -> Any\nkernel 1\nmetering statements\nbody {{  do perform 0(tag) as Any  }}"

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
            function: Some(lash_vocabulary::names::FunctionName::new("work.fill").unwrap()),
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
    REPEAT_BUILDS.set(0);
    let bounds = Bounds {
        memory: 64 << 10,
        ..ROOMY
    };
    assert_eq!(
        passes("let big = work.repeat(100000)", bounds),
        BoundExceeded {
            function: Some(lash_vocabulary::names::FunctionName::new("work.repeat").unwrap()),
            ..exceeded(Bound::Memory, 64 << 10)
        }
    );
    assert_eq!(REPEAT_BUILDS.get(), 0);
    // Resident helper reads and writes warm the property proof and
    // summary before the native reservation. Cache/scratch residency
    // cannot let that reservation reach the following effect.
    let mut warm = Embedder::with(
        r#"definition "function touch(obj: Any, value: Any) -> Any\nkernel 1\nmetering statements\nbody { set obj.value = value\n return obj.value }"
main {
  let obj = {value: 1}
  let alias = obj
  let items = [1, 2]
  let i = 0
  while num.lt(i, 20) {
    set items[0] = call touch(alias, i)
    set i = num.add(items[0], 1)
  }
  let big = work.repeat(100000)
  do perform 0(obj.value) as Any
}"#,
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    assert_eq!(
        warm.run_to_end(&[]),
        End::Error(RunError::Bound(BoundExceeded {
            function: Some(lash_vocabulary::names::FunctionName::new("work.repeat").unwrap()),
            ..exceeded(Bound::Memory, 64 << 10)
        }))
    );
    assert_eq!(REPEAT_BUILDS.get(), 0);
    assert!(warm.requests.is_empty(), "the refusal precedes the effect");
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
    assert_eq!(REPEAT_BUILDS.get(), 200);
}

/// `K-BND-001`: the effects requested at one park are bounded.
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
            function: Some(lash_vocabulary::names::FunctionName::new("work.spin").unwrap()),
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
definition "function thrower(i: Any) -> Any\nkernel 1\nmetering statements\nbody {  let e = {kind: \"x\", n: [i, i]} throw e  }"

definition "function returner(i: Any) -> Any\nkernel 1\nmetering statements\nbody {  let r = {n: [i, i]} return r  }"

definition "function later(i: Any) -> Any\nkernel 1\nmetering statements\nbody {  do yield let r = [i, i] return r  }"

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
    let (make, locals) = lash_kernel_doc::parse_definition_with_locals(&format!(
        "function probe.counter(n: Any) -> Any\nkernel 1\ncharge 1\n{uses}native\n\
         body {{ return fn() {{ let i = 0 while num.lt(i, n) {{ set i = num.add(i, 1) }} return i }} }}\n"
    ))
    .unwrap();
    for local in locals {
        registry.register(local, None).unwrap();
    }
    registry.register(make, None).unwrap();
    let text = "numbers by_spelling\nkernel 1\n\
         main { let count = probe.counter(200000) let n = apply count() return n }\n";
    let program = crate::laws::embedder::program(
        Arc::new(super::embedder::document(text, &registry)),
        crate::PreparedLibrary::new(Arc::new(registry)),
    );
    let start = CellStart {
        target: Target::Main,
        args: Vec::new(),
        session: None,
        owner: lash_kernel_doc::heap::PageId([9; 32]),
        modes: crate::EffectModes::new(),
        host: Vec::new(),
    };
    let bounds = Bounds {
        charge_limit: 10_000,
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

/// A run exactly at each bound passes none of them, and
/// one more request than a park may hold passes that bound (`K-BND-001`,
/// `K-BND-002`, `K-TASK-010`).
#[test]
fn a_run_exactly_at_its_bounds_passes_none() {
    let text = r#"
definition "function c() -> Any\nkernel 1\nmetering statements\nbody {  return 1  }"

definition "function b() -> Any\nkernel 1\nmetering statements\nbody {  let r = call c() return r  }"

definition "function a() -> Any\nkernel 1\nmetering statements\nbody {  let r = call b() return r  }"

definition "function w(tag: Any) -> Any\nkernel 1\nmetering statements\nbody {  let r = perform 0(tag) as Any return r  }"

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
    let limits = lash_vocabulary::data_limits::KernelDataLimits::new(64, 352, 512, 512, 1).unwrap();
    embedder.machine = KernelMachine::start_with_data_limits(
        program,
        ROOMY,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: lash_kernel_doc::heap::PageId([9; 32]),
            modes: crate::EffectModes::new(),
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

/// FIG-6732: opting out removes charge work, including completion formulas,
/// rather than merely making the bound unbounded.
#[test]
fn charge_off_performs_no_charge_work() {
    crate::perf_witness::enable_work();
    let source = "main { let n = 0 for x in [1, 2, 3] metered 13 { set n = num.add(n, x) } while num.lt(n, 7) metered 17 { set n = num.add(n, 1) } print [n] return n }";
    let mut run = Embedder::with(
        source,
        Setup {
            bounds: Bounds {
                charge: crate::Charge::Off,
                charge_limit: 0,
                ..ROOMY
            },
            slice: 1,
            ..Setup::default()
        },
    );
    run.machine.count_work();
    assert_eq!(run.run(), Step::Slice, "Off must still yield slices");
    assert_eq!(result(run.run_to_end(&[])), int(7));
    assert_eq!(run.machine.meters().charged, None);
    let report = run.machine.work().unwrap();
    assert_eq!(report.charged, None);
    assert_eq!(report.residual(), None);
    assert!(report.codes.iter().all(|code| code.counts.self_charge.is_none() && code.counts.inclusive_charge.is_none()));
    assert!(
        report
            .natives
            .iter()
            .all(|native| native.counts.self_charge.is_none()
                && native.counts.zero_charge_calls.is_none())
    );
    let work = crate::perf_witness::take_work().unwrap();
    assert_eq!(work.charge_work, 0, "Off must remove charge work");
    assert!(
        work.vm_steps > 0,
        "the witness must still count Off execution"
    );
    let mut exact = Embedder::with(source, Setup::default());
    assert_eq!(result(exact.run_to_end(&[])), int(7));
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert!(
        work.charge_work > 0,
        "the witness must detect Exact charge work"
    );
}

/// FIG-6732: a fresh worker restores the run's choice, rather than its own.
#[test]
fn charge_off_survives_a_resume_on_a_fresh_worker() {
    let mut run = Embedder::with(
        "main { let first = num.add(1, 2) let answer = perform 0(first) as Any return num.add(answer, 4) }",
        Setup {
            bounds: Bounds {
                charge: crate::Charge::Off,
                charge_limit: 0,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    assert!(matches!(run.run(), Step::Parked(_)));
    let (root, _) = run.checkpoint();
    assert_eq!(root.meters.charge(), crate::Charge::Off);
    // New preparation, another layout and deliberately opposite worker defaults.
    let (program, _) = run.program(false);
    let checkpoint = lash_kernel_state::kernel_internals::checkpoint(root);
    run.machine = KernelMachine::open(
        program,
        Bounds {
            charge_limit: 0,
            ..ROOMY
        },
        &checkpoint,
        Box::new(run.source.clone()),
    )
    .unwrap();
    assert_eq!(run.machine.meters().charged, None);
    assert_eq!(result(run.run_to_end(&[])), int(7));
    assert_eq!(run.machine.meters().charged, None);
}

pub(super) const MEMORY_CHURN: &str = r#"main {
  let i = 0
  while num.lt(i, 200) {
    let held = work.fill(1000)
    set i = num.add(i, 1)
  }
  return i
}"#;

/// FIG-6731: disabling memory accounting removes census work and its bound,
/// while keeping the program's result and a durable cold continuation.
#[test]
fn memory_off_removes_accounting_and_survives_a_cold_resume() {
    let text = MEMORY_CHURN.replacen("main {", "main { do perform 0(\"park\") as Any", 1);
    let mut embedder = Embedder::with(
        &text,
        Setup {
            bounds: Bounds {
                memory: 0,
                memory_meter: crate::MemoryMeter::Off,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let (root, certificate) = embedder.checkpoint();
    assert_eq!(root.limits.memory, None);
    embedder.open_at(&root, certificate, crate::Layout::default(), true);
    embedder.deliver("park");
    crate::perf_witness::enable_work();
    let end = embedder.run_to_end(&[]);
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert_eq!(result(end), int(200));
    assert_eq!(
        work.heap.accounting_additions, 0,
        "Off added census entities"
    );
    assert_eq!(work.heap.refinement_attempts, 0, "Off recounted");
    assert_eq!(
        embedder.machine.meters().memory,
        0,
        "Off reports no memory meter"
    );
}

/// FIG-6731: Off's nursery does not even compute a reservation's size.
#[test]
fn an_unaccounted_nursery_does_no_reservation_bookkeeping() {
    let objects = lash_kernel_doc::arena::Objects::default();
    let mut nursery = lash_kernel_doc::Nursery::unaccounted(&objects);
    nursery.reserve(u64::MAX, u64::MAX).unwrap();
    nursery.hold(u64::MAX, u64::MAX).unwrap();
    assert_eq!(nursery.held().0, 0);
    assert!(nursery.reservations().is_empty());
}

/// FIG-6731: Off's durable session can feed another Off cell or a metered
/// cell, preserving edits through a second cold carry. A fresh meter prices
/// adopted roots once before the new cell runs.
#[test]
fn an_unmetered_session_can_feed_off_or_metered_cells() {
    let mut first = Embedder::with(
        "main { let keep = [7, 8] return 0 }",
        Setup {
            bounds: Bounds {
                memory_meter: crate::MemoryMeter::Off,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    let End::Finished(end) = first.run_to_end(&[]) else {
        panic!("the first cell must finish");
    };
    let carry = end.carry.unwrap();
    for policy in [crate::MemoryMeter::Off, crate::MemoryMeter::On] {
        let (mut program, _) = first.program(true);
        program.document = Arc::new(
            parse_document(&format!(
                "numbers by_spelling\n{}main {{ set keep[1] = 9 return keep[1] }}",
                first.library.header
            ))
            .unwrap(),
        );
        program = super::embedder::program(Arc::clone(&program.document), program.library.clone());
        let mut machine = KernelMachine::start(
            program.clone(),
            Bounds {
                memory_meter: policy,
                ..ROOMY
            },
            CellStart {
                session: Some(carry),
                ..Setup::default().start
            },
            Box::new(first.source.clone()),
        )
        .unwrap();
        let Step::Ended(End::Finished(end)) = machine.run(&mut World::default(), u64::MAX).unwrap()
        else {
            panic!("the editing cell must finish");
        };
        assert_eq!(end.result, int(9));
        program.document = Arc::new(
            parse_document(&format!(
                "numbers by_spelling\n{}main {{ return keep[1] }}",
                first.library.header
            ))
            .unwrap(),
        );
        program = super::embedder::program(Arc::clone(&program.document), program.library.clone());
        let mut reopened = KernelMachine::start(
            program,
            Bounds {
                memory_meter: policy,
                ..ROOMY
            },
            CellStart {
                session: end.carry,
                ..Setup::default().start
            },
            Box::new(first.source.clone()),
        )
        .unwrap();
        assert!(
            matches!(reopened.run(&mut World::default(), u64::MAX).unwrap(),
            Step::Ended(End::Finished(ref end)) if end.result == int(9))
        );
        assert_eq!(
            machine.meters().memory == 0,
            policy == crate::MemoryMeter::Off
        );
    }
}

/// K-CHG-004/005, SIZE-ONCE: the exact deep size survives edits; repeated
/// reads and deltas do work proportional to changed members, never the prefix.
#[test]
fn flat_charge_summaries_preserve_exact_graph_sizes() {
    use lash_kernel_doc::{NewObject, Value};
    use lash_kernel_state::arena::Arena;
    crate::perf_witness::enable_work();
    let mut arena = Arena::new();
    let map = arena.create(NewObject::Map(vec![])).unwrap();
    let Value::Map(handle) = map else {
        panic!("map")
    };
    let mut expected = 1;
    for index in 0..128 {
        arena
            .table_write(handle, Value::Int(index), Value::Int(7))
            .unwrap();
        expected += 2 + if index == 0 { 1 } else { 2 } + 2;
        assert_eq!(crate::data::deep_size(&arena, &map).unwrap(), expected);
    }
    arena
        .table_write(handle, Value::Int(9), Value::Null)
        .unwrap();
    expected -= 1;
    assert_eq!(crate::data::deep_size(&arena, &map).unwrap(), expected);
    arena.table_delete(handle, &Value::Int(9)).unwrap();
    expected -= 2 + 2 + 1;
    assert_eq!(crate::data::deep_size(&arena, &map).unwrap(), expected);
    let construction = crate::perf_witness::take_work().unwrap();
    println!(
        "SIZE-ONCE construction_members={} construction_graph_visits={}",
        construction.heap.flat_size_members, construction.flat_size_visits
    );
    assert!(
        construction.heap.flat_size_members + construction.flat_size_visits <= 2 * 128 + 2 + 2,
        "SIZE-ONCE: construction and deltas visit changed members once"
    );
    for _ in 0..64 {
        assert_eq!(crate::data::deep_size(&arena, &map).unwrap(), expected);
        arena
            .table_write(handle, Value::Int(10), Value::Int(7))
            .unwrap();
    }
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    println!(
        "SIZE-ONCE upkeep_members={} read_graph_visits={}",
        work.heap.flat_size_members, work.flat_size_visits
    );
    assert!(
        work.flat_size_visits + work.heap.flat_size_members <= 128,
        "SIZE-ONCE: unchanged prefix visited {} times",
        work.flat_size_visits + work.heap.flat_size_members
    );

    // The unsummarized graph walker is the semantic oracle, including
    // immutable diamonds, mutable aliases and cycles.
    fn exact(arena: &Arena, value: Value) -> u64 {
        let expected = crate::data::graph_deep_size(arena, &value).unwrap();
        assert_eq!(crate::data::deep_size(arena, &value).unwrap(), expected);
        expected
    }
    let list = arena.create(NewObject::List(vec![Value::Int(1)])).unwrap();
    let Value::List(list_handle) = list else {
        panic!("list")
    };
    let tuple = arena.tuple(vec![list, list], 64).unwrap();
    assert_eq!(exact(&arena, tuple), 7);
    arena.list_set(list_handle, 1, Value::Int(2)).unwrap();
    assert_eq!(exact(&arena, tuple), 10);
    arena.list_set(list_handle, 1, list).unwrap();
    assert_eq!(exact(&arena, tuple), 8);
    arena.table_write(handle, Value::Int(11), tuple).unwrap();
    exact(&arena, map);
    arena.table_write(handle, Value::Int(12), tuple).unwrap();
    exact(&arena, map);
    arena.table_delete(handle, &Value::Int(11)).unwrap();
    exact(&arena, map);
    let before = exact(&arena, map);
    assert!(arena.table_write(handle, list, Value::Null).is_err());
    assert_eq!(
        exact(&arena, map),
        before,
        "failed write leaves the value unchanged"
    );
    // Returning to flat eligibility rebuilds once at the mutation seam;
    // subsequent reads/deltas must never rescan its unchanged prefix.
    arena
        .table_write(handle, Value::Int(12), Value::Int(7))
        .unwrap();
    let flat_again = exact(&arena, map);
    crate::perf_witness::enable_work();
    for _ in 0..64 {
        assert_eq!(crate::data::deep_size(&arena, &map).unwrap(), flat_again);
        arena
            .table_write(handle, Value::Int(10), Value::Int(7))
            .unwrap();
    }
    let again = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert!(
        again.heap.flat_size_members + again.flat_size_visits <= 128,
        "SIZE-ONCE: returning to flat must restore bounded reads and deltas: {:?}",
        again
    );
    for object in [
        NewObject::Set(vec![Value::Int(1), Value::Int(2)]),
        NewObject::Record(vec![(arena.text("field"), Value::Int(2))]),
        NewObject::List(vec![Value::Null, Value::Int(2)]),
        NewObject::Tuple(vec![Value::Int(1), Value::Int(2)]),
    ] {
        let value = arena.create(object).unwrap();
        exact(&arena, value);
        let object_handle = value.handle().unwrap();
        let chilled = arena.chill(object_handle).unwrap().unwrap();
        assert!(
            crate::data::deep_size(&arena, &value).is_err(),
            "cold read retries"
        );
        arena.thaw(object_handle, chilled).unwrap();
        exact(&arena, value);
        match value {
            Value::List(h) => {
                arena.list_set(h, 1, Value::Int(0)).unwrap();
                exact(&arena, value);
                arena.list_remove(h, 0).unwrap();
                exact(&arena, value);
                arena.list_set(h, 1, Value::Null).unwrap();
                exact(&arena, value);
                assert!(arena.list_set(h, 9, Value::Null).is_err());
                exact(&arena, value);
            }
            Value::Set(h) | Value::Record(h) => {
                let key = if matches!(value, Value::Set(_)) {
                    Value::Int(1)
                } else {
                    arena.text("field")
                };
                arena.table_write(h, key, Value::Null).unwrap();
                exact(&arena, value);
                arena.table_delete(h, &key).unwrap();
                exact(&arena, value);
                arena.table_write(h, key, Value::Int(8)).unwrap();
                exact(&arena, value);
            }
            _ => {}
        }
    }
    // Synthetic leaf descriptors exercise arithmetic at the u64 boundary
    // without allocating an impossible payload. The exact walker uses these
    // same semantic lengths. Decrement the unsaturated sum after saturation.
    let huge = Value::Bytes(arena.immutable(
        lash_kernel_doc::arena::Body::Bytes(lash_kernel_doc::arena::Bytes {
            len: u64::MAX - 2,
            chunks: Box::new([]),
        }),
        Default::default(),
        None,
    ));
    let saturated = arena
        .create(NewObject::Map(vec![(Value::Int(1), huge)]))
        .unwrap();
    let Value::Map(sat) = saturated else {
        panic!("map")
    };
    assert_eq!(exact(&arena, saturated), u64::MAX);
    arena.table_write(sat, Value::Int(1), Value::Null).unwrap();
    assert_eq!(exact(&arena, saturated), 6);
    arena.table_write(sat, Value::Int(2), huge).unwrap();
    assert_eq!(exact(&arena, saturated), u64::MAX);
    arena.table_delete(sat, &Value::Int(2)).unwrap();
    assert_eq!(exact(&arena, saturated), 6);
    let old = saturated;
    arena.release(&mut |_| {});
    let reused = arena
        .create(NewObject::Map(vec![(Value::Int(1), Value::Null)]))
        .unwrap();
    assert!(
        crate::data::deep_size(&arena, &old).is_err(),
        "generation reuse rejects stale handles"
    );
    exact(&arena, reused);

    // Through the machine, formula completion, raises and cold reopen must
    // preserve the atomic charge sequence. The 10000 prefix distinguishes
    // these calls from the small fixture's construction/unit charges.
    struct Measure;
    impl lash_kernel_doc::NativeFunction for Measure {
        fn call(
            &self,
            call: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<Value, lash_kernel_doc::NativeError> {
            if matches!(call.args.values()[1], Value::Bool(true)) {
                return Err(lash_kernel_doc::NativeError::Raised(call.new.allocate(
                    NewObject::Error {
                        kind: Value::Text(lash_kernel_doc::arena::ShortText::new("law").unwrap()),
                        message: Value::Text(
                            lash_kernel_doc::arena::ShortText::new("raised").unwrap(),
                        ),
                        data: Value::Null,
                    },
                )));
            }
            Ok(Value::Null)
        }
    }
    fn probes(
        registry: &mut lash_kernel_doc::FunctionRegistry,
        _: &std::collections::BTreeMap<&'static str, lash_vocabulary::FunctionId>,
    ) -> Vec<(&'static str, lash_vocabulary::FunctionId)> {
        let definition = lash_kernel_doc::parse_definition("function law.measure(x: Any, raising: Bool) -> Any\nkernel 1\ncharge sum(10000, deep(x), size(result))\nnative\n").unwrap();
        vec![(
            "law.measure",
            registry
                .register(definition, Some(Arc::new(Measure)))
                .unwrap(),
        )]
    }
    let source = r#"main {
        let m = map{} let alias = m let i = 0
        while num.lt(i, 128) {
            set alias[i] = 7 do invoke law.measure(m, false) set i = num.add(i, 1)
        }
        set alias[9] = null do invoke law.measure(m, false)
        remove alias[9] do invoke law.measure(m, false)
        let child = [1] let diamond = map{"a": child, "b": child}
        do invoke law.measure(diamond, false)
        set child[1] = child do invoke law.measure(diamond, false)
        do perform 0("size") as Any
        do invoke law.measure(m, false)
        do invoke law.measure(diamond, false)
        try { do invoke law.measure(diamond, true) } catch e { }
        return null
    }"#;
    let mut expected = Vec::new();
    let mut total = 1;
    for i in 0..128 {
        total += 2 + if i == 0 { 1 } else { 2 } + 2;
        expected.push(10000 + total + 1);
    }
    expected.extend([
        10000 + total,
        10000 + total - 5,
        10014,
        10015,
        10000 + total - 5,
        10015,
        10014,
    ]);
    for cold in [false, true] {
        let mut embedder = Embedder::with(
            source,
            Setup {
                probes: Some(probes),
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        assert!(matches!(embedder.run(), Step::Parked(_)));
        let mut actual: Vec<_> = tape
            .steps()
            .iter()
            .filter_map(|weight| match weight {
                lash_kernel_exec::Weight::Atomic(n) if *n >= 10000 => Some(*n),
                _ => None,
            })
            .collect();
        if cold {
            embedder.reopen(crate::Layout::default(), true);
            let rest = crate::ChargeTape::new();
            embedder.machine.record_charges(rest.clone());
            embedder.deliver_next();
            assert_eq!(
                result(embedder.run_to_end(&[])),
                lash_kernel_doc::Datum::Null
            );
            actual.extend(rest.steps().iter().filter_map(|weight| match weight {
                lash_kernel_exec::Weight::Atomic(n) if *n >= 10000 => Some(*n),
                _ => None,
            }));
        } else {
            embedder.deliver_next();
            assert_eq!(
                result(embedder.run_to_end(&[])),
                lash_kernel_doc::Datum::Null
            );
            actual = tape
                .steps()
                .iter()
                .filter_map(|weight| match weight {
                    lash_kernel_exec::Weight::Atomic(n) if *n >= 10000 => Some(*n),
                    _ => None,
                })
                .collect();
        }
        assert_eq!(
            actual, expected,
            "formula completion and raise result-size zero, cold={cold}"
        );
    }
    let mut pressured = Embedder::with(
        source,
        Setup {
            probes: Some(probes),
            bounds: Bounds {
                memory: 4096,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    assert!(matches!(
        pressured.run_to_end(&[]),
        End::Error(RunError::Bound(BoundExceeded {
            bound: Bound::Memory,
            ..
        }))
    ));
    assert!(
        pressured.requests.is_empty(),
        "capacity refusal precedes the following effect"
    );
}
