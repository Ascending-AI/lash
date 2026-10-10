//! Parked runs (`K-MACH-007`, `K-MACH-008`, `docs/kernel/parked-state.md`):
//! the resume law over the schema's rows, the rebuild law over a set of
//! programs, and the fragment law over what a save rewrites.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use lash_kernel_doc::{
    Datum, EffectName, Integer, Name, Object, ObjectId, TaskIdentity, Unit, Value, parse_definition,
};
use lash_kernel_state::{
    Baseline, EffectOutcome as SavedOutcome, Ended, Entered, Fragment, Incoming, ParkedRun,
    PerformState, Request as SavedRequest, Root, RootState, SavedFragment, TaskState, WaitId,
};

use super::embedder::{Embedder, ROOMY, Setup, label};
use crate::{
    Bindings, Bounds, End, ImportError, KernelMachine, Layout, Machine, Outcome, PreparedLibrary,
    Program, Request, Start, Step, Target,
};

type Observed = (End, Vec<Datum>, Vec<Request>, u64);

fn int(value: i64) -> Value {
    Value::Int(Integer::from(value))
}

/// Answers the oldest pending request.
fn deliver_next(embedder: &mut Embedder) {
    let next = label(
        embedder
            .pending
            .first()
            .expect("a park with nothing pending"),
    );
    embedder.deliver(&next);
}

/// Runs a document to its end, answering the oldest request at each park.
/// With `relay`, the run moves to a machine built afresh at every safe
/// point: at each park before and after its outcome is delivered, and at
/// each slice. Every hop takes another layout, and every other hop swaps
/// which implementation of `pair.twice` runs.
fn observe(text: &str, slice: u64, relay: bool) -> Observed {
    let mut embedder = Embedder::with(
        text,
        Setup {
            slice,
            ..Setup::default()
        },
    );
    let mut hops = 0u64;
    let mut hop = |embedder: &mut Embedder| {
        if relay {
            hops += 1;
            embedder.relay(
                Layout(hops.wrapping_mul(0x9e37_79b9) | 1),
                hops.is_multiple_of(2),
            );
        }
    };
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => hop(&mut embedder),
            Step::Parked(_) => {
                hop(&mut embedder);
                deliver_next(&mut embedder);
                hop(&mut embedder);
            }
        }
    };
    (
        end,
        embedder.world.printed,
        embedder.requests,
        embedder.machine.meters().charged,
    )
}

const EXPRESSIONS: &str = r#"
main {
  let m = map{"a": [1, (2, 3)], "b": {k: text.concat("x", "y")}}
  let n = num.add(list.len(m["a"]), pair.twice(4))
  let got = perform echo(n) as Any
  let t = (got, m["b"].k, [got, n])
  let doubled = invoke pair.twice(got)
  let again = perform echo(t) as Any
  return (again, m, num.add(n, doubled))
}
"#;

const HELPER_FROM_TWO_PLACES: &str = r#"
fn fetch(tag) {
  let r = perform echo(tag) as Any
  return (tag, r)
}
main {
  let a = call fetch("a")
  let out = []
  for t in ["b", "c"] {
    let v = call fetch(t)
    set out[list.len(out)] = v
  }
  let d = call fetch("d")
  return (a, out, d)
}
"#;

const CALLBACK_IN_MAP: &str = r#"
main {
  let seen = 0
  let f = fn(x) {
    let r = perform echo(x) as Any
    set seen = num.add(seen, 1)
    return (r, seen)
  }
  let xs = ["p", "q", "r"]
  let ys = invoke each.map(f, xs)
  return (ys, seen)
}
"#;

/// A cleanup block that waits, entered normally, by a throw of a list no
/// variable names, and by `continue`.
const CLEANUP_WITH_A_WAIT: &str = r#"
main {
  let log = []
  for mode in [0, 1, 2] {
    try {
      try {
        if num.lt(mode, 1) {
          set log[list.len(log)] = "body"
        } else {
          if num.lt(mode, 2) { throw [mode, "thrown"] } else { continue }
        }
      } finally {
        let got = perform echo(mode) as Any
        set log[list.len(log)] = got
      }
    } catch e {
      set log[list.len(log)] = e
    }
  }
  return log
}
"#;

const FAN_OUT_WITH_ONE_ENDED: &str = r#"
fn worker(tag) {
  let r = perform echo(tag) as Any
  let s = perform echo(r) as Any
  print ("end", tag)
  return (tag, s)
}
fn quick(tag) { return tag }
main {
  let hs = []
  for tag in ["a", "b"] {
    let h = spawn call worker(tag)
    set hs[list.len(hs)] = h
  }
  let q = spawn call quick("q")
  set hs[list.len(hs)] = q
  let rs = join all hs
  return rs
}
"#;

const CANCELLED_MID_CLEANUP: &str = r#"
fn guarded(tag) {
  try {
    let r = perform echo(tag) as Any
    return r
  } finally {
    let c = perform echo("cleanup") as Any
    print ("cleaned", c)
  }
}
main {
  let h = spawn call guarded("g")
  do cancel h
  let out = "unset"
  try { set out = join h } catch e { set out = e.kind }
  return out
}
"#;

/// `K-ITER-003`, `K-KEY-003`, `K-KEY-005`: removals on both sides of a
/// live cursor, including removing every entry and then reinserting.
const TABLE_CHURN: &str = r#"
main {
  let m = map{1.0: "first", 2: "two", 3: "three", 4: "four"}
  let s = set{1.0, 2, 3, 4}
  let keys = []
  let first = true
  for key in m {
    set keys[list.len(keys)] = key
    if first {
      set first = false
      remove m[1] remove m[2] remove m[3]
      set m[1] = "again" set m[4.0] = "updated"
      remove s[1] remove s[2] remove s[3]
      set s[1] = true set s[4.0] = true
    }
    let row = (key, m, s)
    do perform echo(row) as Any
  }
  set first = true
  for key in s {
    set keys[list.len(keys)] = key
    if first {
      set first = false
      remove s[4] remove s[1]
      set s[2.0] = true set s[2] = true
    }
    let row = (key, s)
    do perform echo(row) as Any
  }
  return (keys, m, s)
}
"#;

/// The rebuild law (`K-MACH-008`, gate 4 of `docs/kernel/design.md` §11):
/// park at every safe point, discard the executable, rebuild it under
/// another layout with a native function swapped for its kernel body, and
/// resume. Values, errors, effect identities, interleaving and charges
/// are those of the run that never parked.
#[test]
fn a_run_rebuilt_at_every_safe_point_runs_the_same() {
    let programs = [
        ("expression-heavy code", EXPRESSIONS),
        (
            "an effectful helper called from two places",
            HELPER_FROM_TWO_PLACES,
        ),
        ("a callback inside map", CALLBACK_IN_MAP),
        ("a cleanup block with a wait", CLEANUP_WITH_A_WAIT),
        ("a fan-out with one member ended", FAN_OUT_WITH_ONE_ENDED),
        ("a cancelled task mid-cleanup", CANCELLED_MID_CLEANUP),
        ("live map and set iteration through removals", TABLE_CHURN),
    ];
    for (name, text) in programs {
        let straight = observe(text, u64::MAX, false);
        assert!(
            matches!(straight.0, End::Finished(_)),
            "{name}: {:?}",
            straight.0
        );
        assert!(!straight.2.is_empty(), "{name} parks");
        assert_eq!(observe(text, u64::MAX, true), straight, "{name}, at parks");
        // A slice of 0 returns after every statement.
        assert_eq!(observe(text, 0, true), straight, "{name}, at slices");
    }
}

/// Every park of a run, as the state exported there.
fn parks(text: &str, setup: Setup) -> Vec<ParkedRun> {
    let mut embedder = Embedder::with(text, setup);
    let mut parks = Vec::new();
    loop {
        match embedder.run() {
            Step::Ended(_) => return parks,
            Step::Slice => {}
            Step::Parked(_) => {
                parks.push(embedder.machine.export().unwrap());
                deliver_next(&mut embedder);
            }
        }
    }
}

/// The resume law for the rows that are derived or discarded: runs that
/// differ only in what is not saved park as the same state, so they
/// resume the same. The executable's layout, which implementation of a
/// function runs, where the fuel slices fell and when the heap was
/// collected leave no trace in a parked run.
#[test]
fn what_is_not_saved_leaves_no_trace_in_a_parked_run() {
    for text in [EXPRESSIONS, CALLBACK_IN_MAP, FAN_OUT_WITH_ONE_ENDED] {
        let plain = parks(text, Setup::default());
        assert!(!plain.is_empty());
        let layout = Setup {
            layout: Layout(0x5eed),
            ..Setup::default()
        };
        assert_eq!(parks(text, layout), plain, "the layout");
        let body = Setup {
            native_twice: false,
            ..Setup::default()
        };
        assert_eq!(parks(text, body), plain, "the implementation");
        let sliced = Setup {
            slice: 1,
            ..Setup::default()
        };
        assert_eq!(parks(text, sliced), plain, "the slices");
        // A memory bound a few objects wide collects at almost every
        // allocation.
        let tight = Setup {
            bounds: Bounds {
                memory: 4096,
                ..ROOMY
            },
            ..Setup::default()
        };
        assert_eq!(parks(text, tight), plain, "the collections");
    }
}

/// One row of the schema: a program, how far it runs before it is parked,
/// a change to that one row, and what is watched after the resume.
struct Row {
    name: &'static str,
    text: &'static str,
    slice: u64,
    park: fn(&mut Embedder),
    /// Whether `pair.twice` runs natively or its kernel body runs.
    native_twice: bool,
    /// Applied to both states of the pair.
    prepare: fn(&mut ParkedRun),
    change: fn(&mut ParkedRun),
    watch: fn(&mut Embedder) -> String,
}

fn first_park(embedder: &mut Embedder) {
    assert!(matches!(embedder.run(), Step::Parked(_)));
}

fn unprepared(_: &mut ParkedRun) {}

fn to_the_end(embedder: &mut Embedder) -> String {
    let end = embedder.run_to_end(&[]);
    format!(
        "{end:?} {:?} {:?} {}",
        embedder.world.printed,
        embedder.requests,
        embedder.machine.meters().charged
    )
}

fn next_step(embedder: &mut Embedder) -> String {
    format!("{:?}", embedder.run())
}

impl Row {
    fn new(name: &'static str, text: &'static str, change: fn(&mut ParkedRun)) -> Self {
        Self {
            name,
            text,
            slice: u64::MAX,
            park: first_park,
            native_twice: true,
            prepare: unprepared,
            change,
            watch: to_the_end,
        }
    }
}

const LOOP: &str = r#"
main {
  let xs = [1, 2]
  let total = 0
  for x in (10, 20, 30) {
    let got = perform echo(x) as Any
    set total = num.add(total, num.add(got, x))
  }
  let more = [total]
  return (total, xs, more)
}
"#;

const WORKERS: &str = r#"
fn worker(tag) {
  let r = perform echo(tag) as Any
  print ("first", tag)
  let s = perform echo(r) as Any
  print ("second", tag)
  return s
}
main {
  let a = spawn call worker("a")
  let b = spawn call worker("b")
  let ra = join a
  let rb = join b
  return (ra, rb)
}
"#;

const JOINERS: &str = r#"
fn slow() { let r = perform echo("s") as Any return r }
fn single(h, tag) { let v = join h print tag return v }
fn many(h, tag) { let hs = [h] let v = join all hs print tag return v }
main {
  let s = spawn call slow()
  let w1 = spawn call single(s, "w1")
  let w2 = spawn call single(s, "w2")
  let m1 = spawn call many(s, "m1")
  let m2 = spawn call many(s, "m2")
  let hs = [w1, w2, m1, m2]
  let rs = join all hs
  return rs
}
"#;

const FAILED_TASK: &str = r#"
fn bad() { throw "bad" }
main {
  let h = spawn call bad()
  do perform echo("x") as Any
  return 1
}
"#;

const ENDED_TASK: &str = r#"
fn quick() { return 1 }
main {
  let q = spawn call quick()
  do perform echo("x") as Any
  let v = join q
  return v
}
"#;

const CLEANUP_BY_CONTINUE: &str = r#"
main {
  let log = []
  for x in [1, 2] {
    try { continue } finally {
      let got = perform echo(x) as Any
      set log[list.len(log)] = got
    }
  }
  return log
}
"#;

const THROWN_LIST: &str = r#"
main {
  try {
    try { throw [1, 2] } finally { do perform echo("c") as Any }
  } catch e {
    return e
  }
}
"#;

const TWICE: &str = r#"
main {
  let y = invoke pair.twice(3)
  return y
}
"#;

const SPAWNING: &str = r#"
fn quick() { return 1 }
main {
  let h = spawn call quick()
  let v = join h
  return v
}
"#;

fn perform_of(parked: &mut ParkedRun, task: usize) -> &mut lash_kernel_state::Perform {
    match &mut parked.tasks[task].handle.state {
        TaskState::Performing(perform) => perform,
        other => panic!("task {task} is not in a wait: {other:?}"),
    }
}

fn requested(parked: &mut ParkedRun) {
    let SavedRequest::Effect { state, .. } = &mut perform_of(parked, 1).request else {
        panic!("effect")
    };
    *state = PerformState::Requested;
}

fn list_join_of(parked: &mut ParkedRun, task: usize) -> &mut lash_kernel_state::ListJoin {
    match &mut parked.tasks[task].handle.state {
        TaskState::JoiningMany(join) => join,
        other => panic!("task {task} is not in a list join: {other:?}"),
    }
}

/// The saved rows of the schema, one change to each.
fn rows() -> Vec<Row> {
    vec![
        Row::new("the charge", LOOP, |parked| parked.run.charged += 5),
        Row::new("the objects allocated", LOOP, |parked| {
            parked.run.objects_allocated += 7;
        }),
        Row::new("the waits issued", LOOP, |parked| {
            parked.run.waits_issued += 3
        }),
        Row {
            park: |embedder| {
                first_park(embedder);
                embedder.deliver("a");
                embedder.deliver("b");
            },
            ..Row::new("the ready queue", WORKERS, |parked| {
                parked.run.ready.reverse()
            })
        },
        Row {
            prepare: |parked| parked.run.waits_issued = 9,
            watch: next_step,
            ..Row::new("the unreported withdrawn waits", LOOP, |parked| {
                parked.run.withdrawn.insert(WaitId(4));
                parked.run.unreported.push(WaitId(4));
            })
        },
        Row {
            prepare: |parked| parked.run.waits_issued = 9,
            watch: |embedder| {
                let outcome = Outcome::Completed(Datum::Null);
                format!("{:?}", embedder.machine.deliver(WaitId(4), outcome))
            },
            ..Row::new("the withdrawn waits", LOOP, |parked| {
                parked.run.withdrawn.insert(WaitId(4));
            })
        },
        Row::new("a session binding", LOOP, |parked| {
            parked.session.insert(Name::new("total"), int(100));
        }),
        Row::new("a heap object", LOOP, |parked| {
            let Some(Value::List(xs)) = parked.session.get(&Name::new("xs")) else {
                panic!("`xs` is a list");
            };
            parked.objects.insert(*xs, Object::List(vec![int(7)]));
        }),
        Row::new("a task's identity", WORKERS, |parked| {
            let TaskIdentity::Spawned(spawn) = &mut parked.tasks[1].handle.identity else {
                panic!("task 1 was spawned");
            };
            spawn.occurrence = 7;
        }),
        Row {
            watch: next_step,
            ..Row::new("a wait's state: requested or admitted", WORKERS, requested)
        },
        Row {
            park: |embedder| {
                first_park(embedder);
                embedder.deliver("a");
            },
            ..Row::new("a wait's committed outcome", WORKERS, |parked| {
                let outcome = SavedOutcome::Completed(Datum::Text("other".to_string()));
                let SavedRequest::Effect { state, .. } = &mut perform_of(parked, 1).request else {
                    panic!("effect")
                };
                *state = PerformState::Committed(outcome);
            })
        },
        Row {
            prepare: |parked| parked.run.waits_issued = 9,
            watch: |embedder| {
                let wait = match embedder.pending.first() {
                    Some(Request::Effect(effect)) => effect.wait,
                    other => panic!("an effect is pending: {other:?}"),
                };
                let outcome = Outcome::Completed(Datum::Null);
                format!("{:?}", embedder.machine.deliver(wait, outcome))
            },
            ..Row::new("a wait's number", WORKERS, |parked| {
                perform_of(parked, 1).wait = WaitId(5);
            })
        },
        Row {
            prepare: requested,
            watch: next_step,
            ..Row::new("a wait's effect name", WORKERS, |parked| {
                let SavedRequest::Effect { effect, .. } = &mut perform_of(parked, 1).request else {
                    panic!("task 1 performs an effect");
                };
                *effect = EffectName::new("boom").unwrap();
            })
        },
        Row {
            prepare: requested,
            watch: next_step,
            ..Row::new("a wait's arguments", WORKERS, |parked| {
                let SavedRequest::Effect { args, .. } = &mut perform_of(parked, 1).request else {
                    panic!("task 1 performs an effect");
                };
                args.push(Datum::Null);
            })
        },
        Row {
            prepare: requested,
            watch: next_step,
            ..Row::new("a wait's identity", WORKERS, |parked| {
                let SavedRequest::Effect { identity, .. } = &mut perform_of(parked, 1).request
                else {
                    panic!("task 1 performs an effect");
                };
                identity.occurrence = 3;
            })
        },
        Row::new("a handle's joiners", JOINERS, |parked| {
            parked.tasks[1].handle.joiners.reverse();
        }),
        Row::new("a list join's mode", JOINERS, |parked| {
            list_join_of(parked, 0).mode = lash_kernel_doc::JoinMode::Race;
        }),
        Row::new("a list join's members", JOINERS, |parked| {
            // The member dropped no longer wakes the join.
            let dropped = list_join_of(parked, 0).members.pop().unwrap();
            parked.tasks[dropped.0 as usize]
                .handle
                .joiners
                .retain(|joiner| joiner.0 != 0);
        }),
        Row::new(
            "whether a task's error was observed",
            FAILED_TASK,
            |parked| {
                parked.tasks[1].handle.observed = true;
            },
        ),
        Row::new(
            "whether a task was a member of a passed join",
            FAILED_TASK,
            |parked| {
                parked.tasks[1].handle.passed = true;
            },
        ),
        Row {
            // The error itself is gone once no handle reaches the task.
            prepare: |parked| {
                parked.tasks[1].handle.state = TaskState::Ended(Ended::Returned(Value::Null));
            },
            ..Row::new("whether a task failed", FAILED_TASK, |parked| {
                parked.tasks[1].handle.failed = false;
            })
        },
        Row::new("how a task ended", ENDED_TASK, |parked| {
            parked.tasks[1].handle.state = TaskState::Ended(Ended::Returned(int(2)));
        }),
        Row::new("a task's occurrence counts", LOOP, |parked| {
            parked.tasks[0].handle.occurrences[0].count += 1;
        }),
        Row {
            slice: 1,
            park: |embedder| {
                // A slice ends between the `spawn` and the spawner's
                // going on with the handle.
                loop {
                    assert!(matches!(embedder.run(), Step::Slice));
                    let parked = embedder.machine.export().unwrap();
                    if matches!(parked.tasks[0].handle.state, TaskState::Resuming(_)) {
                        return;
                    }
                }
            },
            ..Row::new("what a ready task resumes with", SPAWNING, |parked| {
                parked.tasks[0].handle.state = TaskState::Resuming(Incoming::Value(int(5)));
            })
        },
        Row {
            slice: 1,
            park: |embedder| assert!(matches!(embedder.run(), Step::Slice)),
            ..Row::new("a call's pending statement", LOOP, |parked| {
                let statement = &mut parked.tasks[0].calls[0].call.statement;
                assert_eq!(statement.path, [1]);
                statement.path = vec![2];
            })
        },
        Row::new("a call's bindings", LOOP, |parked| {
            let binding = &mut parked.tasks[0].calls[0].call.bindings[0];
            assert_eq!(binding.name, Name::new("x"));
            binding.value = lash_kernel_state::Bound::Value(int(11));
        }),
        Row::new("a loop's iterations started", LOOP, |parked| {
            parked.tasks[0].calls[0].call.loops[0].started += 1;
        }),
        Row::new("a loop's position", LOOP, |parked| {
            parked.tasks[0].calls[0].call.loops[0].position = Some(2);
        }),
        Row::new("what a loop iterates", LOOP, |parked| {
            let taken = Value::Tuple(vec![int(10), int(21), int(30)].into());
            parked.tasks[0].calls[0].held.iterated[0] = taken;
        }),
        Row::new("how a finally was entered", CLEANUP_BY_CONTINUE, |parked| {
            let entered = &mut parked.tasks[0].calls[0].call.finally[0].entered;
            assert_eq!(*entered, Entered::Continue);
            *entered = Entered::Break;
        }),
        Row::new("the value a finally leaves with", THROWN_LIST, |parked| {
            parked.tasks[0].calls[0].held.departing[0] = Value::Null;
        }),
        // A slice ends inside the kernel body of a function with a native
        // implementation, whose formula reads the arguments when it ends.
        Row {
            slice: 1,
            native_twice: false,
            park: |embedder| loop {
                assert!(matches!(embedder.run(), Step::Slice));
                if embedder.machine.export().unwrap().tasks[0].calls.len() == 2 {
                    return;
                }
            },
            ..Row::new("a library call's arguments", TWICE, |parked| {
                let held = &mut parked.tasks[0].calls[1].held;
                assert!(held.arguments.is_some());
                held.arguments = Some(vec![int(0)]);
            })
        },
    ]
}

/// The resume law for the saved rows: for each, two parked states that
/// differ only there resume differently.
#[test]
fn every_saved_row_changes_how_a_run_resumes() {
    for row in rows() {
        let mut embedder = Embedder::with(
            row.text,
            Setup {
                slice: row.slice,
                native_twice: row.native_twice,
                ..Setup::default()
            },
        );
        (row.park)(&mut embedder);
        let mut kept = embedder.machine.export().unwrap();
        (row.prepare)(&mut kept);
        let mut changed = kept.clone();
        (row.change)(&mut changed);
        assert_ne!(changed, kept, "{} is a row", row.name);
        let resume = |parked: ParkedRun| {
            let mut resumed = embedder
                .resumed(parked)
                .unwrap_or_else(|error| panic!("{}: {error}", row.name));
            (row.watch)(&mut resumed)
        };
        assert_ne!(resume(changed), resume(kept), "{}", row.name);
    }
}

/// What a parked run pins is checked before anything resumes: a state of
/// another kernel version, another document or another set of function
/// identities is refused.
#[test]
fn a_parked_run_resumes_only_under_what_it_pins() {
    let mut embedder = Embedder::new(LOOP);
    first_park(&mut embedder);
    let parked = embedder.machine.export().unwrap();
    let refused = |change: fn(&mut ParkedRun)| {
        let mut changed = parked.clone();
        change(&mut changed);
        embedder.resumed(changed).map(|_| ()).unwrap_err()
    };
    assert!(matches!(
        refused(|parked| parked.run.kernel += 1),
        ImportError::KernelVersion { .. }
    ));
    assert!(matches!(
        refused(|parked| parked.run.functions.clear()),
        ImportError::Malformed { .. }
    ));
    let mut other = Embedder::new("main { do perform echo(1) as Any }");
    first_park(&mut other);
    let foreign = other.machine.export().unwrap();
    assert!(matches!(
        embedder.resumed(foreign).map(|_| ()).unwrap_err(),
        ImportError::DocumentMismatch { .. }
    ));
}

/// A parked run resumes only in the bodies of the functions its document
/// lists, which are the functions it pins (`docs/kernel/parked-state.md`,
/// `executable`). The prepared library holds every body the registry has,
/// so a state that stands in a body of another function is refused, even
/// one shaped exactly like the body the run parked in.
#[test]
fn a_parked_call_in_a_body_the_document_does_not_list_is_refused() {
    let mut embedder = Embedder::new(
        "main { let f = fn(x) { let r = perform echo(x) as Any return r } \
         let y = invoke each.twice(f, 1) return y }",
    );
    first_park(&mut embedder);
    let mut parked = embedder.machine.export().unwrap();
    let mut registry = (*super::embedder::library(true).registry).clone();
    let again = parse_definition(
        "function each.again(f: Fn(x: Any) -> Any, x: Any) -> Any\nkernel 1\ncharge 5\n\
         body { let a = apply f(x) let b = apply f(a) return b }\n",
    )
    .unwrap();
    let again = Unit::Library(registry.register(again, None).unwrap());
    let twice = Unit::Library(embedder.library.ids["each.twice"]);
    let mut moved = 0;
    for call in parked.tasks.iter_mut().flat_map(|task| &mut task.calls) {
        if call.call.statement.unit == twice {
            call.call.statement.unit = again.clone();
            for binding in &mut call.call.bindings {
                binding.declared.unit = again.clone();
            }
            moved += 1;
        }
    }
    assert_eq!(moved, 1, "the run parks inside `each.twice`");
    let (program, _) = embedder.program(true);
    let program = Program {
        document: program.document,
        library: PreparedLibrary::new(Arc::new(registry)),
    };
    assert!(matches!(
        KernelMachine::import(program, ROOMY, parked).map(|_| ()),
        Err(ImportError::Malformed { .. })
    ));
}

fn changed(saved: &lash_kernel_state::Saved) -> Vec<Root> {
    saved.changed().map(|(root, _)| root.clone()).collect()
}

fn session(name: &str) -> Root {
    Root::Session(Name::new(name))
}

const MAIN_TASK: Root = Root::Task(lash_kernel_doc::TaskId::MAIN);

fn call(depth: u32) -> Root {
    Root::Call {
        task: lash_kernel_doc::TaskId::MAIN,
        depth,
    }
}

/// The fragment law. Writing one object through an alias rewrites the one
/// fragment that owns it; a call that returns takes its fragment with it;
/// an object only a pending throw holds is owned by the call's held
/// values.
#[test]
fn a_save_rewrites_the_fragments_that_changed_and_no_others() {
    // `ys` is an alias of `xs`, which owns the list as the first root to
    // reach it. Between the parks the task and its call moved on too.
    let mut aliased = Embedder::new(
        r#"main {
  let xs = [1]
  let ys = xs
  let zs = [9]
  do perform echo("one") as Any
  set ys[0] = 2
  do perform echo("two") as Any
  return (xs, zs)
}"#,
    );
    first_park(&mut aliased);
    let first = aliased.machine.save(&Baseline::default()).unwrap();
    assert_eq!(first.fragments.len(), 5);
    assert_eq!(changed(&first).len(), 5);
    let idle = aliased.machine.save(&first.baseline).unwrap();
    assert_eq!(changed(&idle), []);
    aliased.deliver("one");
    first_park(&mut aliased);
    let second = aliased.machine.save(&first.baseline).unwrap();
    assert_eq!(changed(&second), [session("xs"), MAIN_TASK, call(0)]);
    assert_eq!(second.fragments[&session("ys")], SavedFragment::Unchanged);
    assert_eq!(second.fragments[&session("zs")], SavedFragment::Unchanged);
    assert!(second.removed.is_empty());

    let mut returning = Embedder::new(
        r#"fn inner() {
  let big = [1, 2, 3]
  do perform echo("in") as Any
  return big
}
main {
  let r = call inner()
  do perform echo("out") as Any
  return r
}"#,
    );
    first_park(&mut returning);
    let inside = returning.machine.save(&Baseline::default()).unwrap();
    assert_eq!(changed(&inside), [MAIN_TASK, call(0), call(1)]);
    returning.deliver("in");
    first_park(&mut returning);
    let outside = returning.machine.save(&inside.baseline).unwrap();
    assert_eq!(outside.removed, [call(1)]);
    // The list the call owned is the session binding's now.
    assert_eq!(changed(&outside), [session("r"), MAIN_TASK, call(0)]);

    let mut throwing = Embedder::new(THROWN_LIST);
    first_park(&mut throwing);
    let saved = throwing.machine.save(&Baseline::default()).unwrap();
    let held = Root::Held {
        task: lash_kernel_doc::TaskId::MAIN,
        depth: 0,
    };
    let SavedFragment::Changed(bytes) = &saved.fragments[&held] else {
        panic!("the held values are written");
    };
    let fragment: Fragment = serde_json::from_slice(bytes).unwrap();
    let RootState::Held(values) = &fragment.root else {
        panic!("the fragment is the held values'");
    };
    let [Value::List(thrown)] = values.departing.as_slice() else {
        panic!("a list is on its way out: {values:?}");
    };
    let owned: Vec<ObjectId> = fragment.objects.iter().map(|owned| owned.id).collect();
    assert_eq!(owned, [*thrown]);
}

/// The fragment law at size: on a heap of 10^5 objects under a thousand
/// roots, a write to one object rewrites that root's fragment and no
/// other. Prints what the saves cost.
#[test]
fn one_write_on_a_large_heap_rewrites_one_fragment() {
    const ROOTS: u64 = 1000;
    const EACH: u64 = 100;
    let mut bindings = Bindings::default();
    for root in 0..ROOTS {
        let outer = ObjectId(root * EACH);
        let inner = (1..EACH).map(|offset| ObjectId(root * EACH + offset));
        bindings.objects.insert(
            outer,
            Object::List(inner.clone().map(Value::List).collect()),
        );
        for id in inner {
            bindings
                .objects
                .insert(id, Object::List(vec![int(id.0 as i64)]));
        }
        bindings
            .variables
            .insert(Name::new(format!("v{root}")), Value::List(outer));
    }
    let mut embedder = Embedder::with(
        r#"main {
  do perform echo("one") as Any
  set v500[0] = 5
  do perform echo("two") as Any
  return 1
}"#,
        Setup {
            start: Start {
                target: Target::Main,
                args: Vec::new(),
                bindings,
            },
            ..Setup::default()
        },
    );
    first_park(&mut embedder);
    let started = Instant::now();
    let first = embedder.machine.save(&Baseline::default()).unwrap();
    let whole = started.elapsed();
    assert_eq!(changed(&first).len() as u64, ROOTS + 2);
    let bytes = |saved: &lash_kernel_state::Saved| -> usize {
        saved.changed().map(|(_, bytes)| bytes.len()).sum::<usize>() + saved.header.len()
    };
    let whole_bytes = bytes(&first);

    let started = Instant::now();
    let idle = embedder.machine.save(&first.baseline).unwrap();
    let unchanged = started.elapsed();
    assert_eq!(changed(&idle), []);

    embedder.deliver("one");
    first_park(&mut embedder);
    let started = Instant::now();
    let second = embedder.machine.save(&first.baseline).unwrap();
    let one = started.elapsed();
    assert_eq!(changed(&second), [session("v500"), MAIN_TASK, call(0)]);
    eprintln!(
        "parked-state save, {} objects under {ROOTS} roots: every fragment {whole:?} \
         ({whole_bytes} bytes); nothing changed {unchanged:?}; one root changed {one:?} \
         ({} bytes)",
        ROOTS * EACH,
        bytes(&second),
    );

    // What was written reads back as the run.
    let mut stored: BTreeMap<Root, Vec<u8>> = BTreeMap::new();
    for saved in [&first, &second] {
        for (root, bytes) in saved.changed() {
            stored.insert(root.clone(), bytes.to_vec());
        }
    }
    let parts = stored.iter().map(|(root, bytes)| (root, bytes.as_slice()));
    let (loaded, _) = ParkedRun::load(&second.header, parts).unwrap();
    assert_eq!(loaded, embedder.machine.export().unwrap());
}

/// V06 / K-TASK-003: the restored FIFO contains every runnable task once.
#[test]
fn a_ready_queue_is_an_exact_permutation_of_runnable_tasks() {
    let mut embedder = Embedder::new(WORKERS);
    first_park(&mut embedder);
    embedder.deliver("a");
    embedder.deliver("b");
    let parked = embedder.machine.export().unwrap();
    assert_eq!(parked.run.ready.len(), 2);
    let mut duplicate = serde_json::to_value(&parked).unwrap();
    duplicate["run"]["ready"][1] = duplicate["run"]["ready"][0].clone();
    assert!(serde_json::from_value::<ParkedRun>(duplicate).is_err());
    assert!(lash_kernel_state::ReadyQueue::try_from(vec![parked.run.ready[0]; 2]).is_err());
    for ready in [vec![parked.run.ready[0]], vec![]] {
        let mut changed = parked.clone();
        changed.run.ready = ready.try_into().unwrap();
        assert!(matches!(
            embedder.resumed(changed).map(|_| ()),
            Err(ImportError::Malformed { .. })
        ));
    }
    let mut reversed = parked;
    reversed.run.ready.reverse();
    assert!(embedder.resumed(reversed).is_ok());
}
