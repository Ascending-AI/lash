//! The task rules (`K-TASK`): one case per rule, in kernel text, with a
//! scripted delivery order. Each asserts values, errors, effect identities
//! and the order of side effects.

use std::sync::Arc;

use lash_kernel_doc::{Datum, EffectIdentity, LoopIteration, SpawnIdentity, TaskIdentity};

use super::embedder::{Embedder, Setup, function_site, int, main_site, record, result, run, text};
use crate::{
    DeliverError, Delivered, End, Machine, MachineError, Outcome, Request, RunError, Step, WaitId,
};

const WORKER: &str = r#"
fn worker(tag) {
  print ("start", tag)
  let r = perform echo(tag) as Any
  print ("end", tag)
  return r
}
"#;

fn pair(a: &str, b: &str) -> Datum {
    Datum::Tuple(vec![text(a), text(b)])
}

fn spawned(parent: TaskIdentity, site: lash_kernel_doc::Site, occurrence: u64) -> TaskIdentity {
    TaskIdentity::Spawned(SpawnIdentity {
        parent: Arc::new(parent),
        site,
        occurrence,
    })
}

fn identity(request: &Request) -> &EffectIdentity {
    match request {
        Request::Effect(effect) => &effect.identity,
        Request::Sleep(sleep) => &sleep.identity,
    }
}

fn outstanding(end: End) -> (Vec<TaskIdentity>, Vec<TaskIdentity>) {
    match end {
        End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
        }) => (unfinished, unobserved),
        other => panic!("the run did not end with tasks outstanding: {other:?}"),
    }
}

/// `K-TASK-001`: every task has its own calls and all share the heap; one
/// runs at a time.
#[test]
fn tasks_share_the_heap_and_run_one_at_a_time() {
    let (end, _) = run(r#"
fn adds(xs, tag) {
  set xs[list.len(xs)] = tag
  do yield
  set xs[list.len(xs)] = tag
}
main {
  let xs = []
  let a = spawn call adds(xs, "a")
  let b = spawn call adds(xs, "b")
  let hs = [a, b]
  do join all hs
  return xs
}"#);
    assert_eq!(
        result(end),
        Datum::List(["a", "b", "a", "b"].map(text).to_vec())
    );
}

/// `K-TASK-002`, `K-TASK-024`: a spawned task runs at once to its first
/// wait, then the spawner goes on; the park hands out what both asked for,
/// in request order, each under its task's identity.
#[test]
fn spawn_runs_the_new_task_to_its_first_wait_and_then_the_spawner() {
    let mut embedder = Embedder::new(&format!(
        r#"{WORKER}
main {{
  print "a"
  let h1 = spawn call worker("w1")
  print "b"
  let h2 = spawn call worker("w2")
  print "c"
  let r1 = join h1
  let r2 = join h2
  return (r1, r2)
}}"#
    ));
    assert!(matches!(embedder.run(), Step::Parked(_)));
    assert_eq!(embedder.asked(), ["w1", "w2"]);
    assert_eq!(
        embedder.world.printed,
        [
            text("a"),
            pair("start", "w1"),
            text("b"),
            pair("start", "w2"),
            text("c")
        ]
    );
    for (request, (wait, spawn)) in embedder.requests.iter().zip([(0, 1), (1, 3)]) {
        let Request::Effect(effect) = request else {
            panic!("a perform asks for an effect");
        };
        assert_eq!(effect.wait, WaitId(wait));
        assert_eq!(
            effect.identity,
            EffectIdentity {
                task: spawned(TaskIdentity::Main, main_site([spawn, 0]), 0),
                site: function_site("worker", [1, 0]),
                occurrence: 0,
                loops: Vec::new(),
            }
        );
    }
    embedder.deliver("w2");
    embedder.deliver("w1");
    assert_eq!(result(embedder.run_to_end(&[])), pair("w1", "w2"));
    assert_eq!(
        embedder.world.printed[5..],
        [pair("end", "w2"), pair("end", "w1")]
    );
}

/// `K-TASK-003`, `K-TASK-005`, `K-TASK-025`: outcomes delivered between two
/// runs make their tasks ready in the order they were delivered, and the
/// queue is first in, first out.
#[test]
fn delivered_outcomes_wake_their_tasks_in_delivery_order() {
    let mut embedder = Embedder::new(&format!(
        r#"{WORKER}
main {{
  let a = spawn call worker("a")
  let b = spawn call worker("b")
  let c = spawn call worker("c")
  let hs = [a, b, c]
  let rs = join all hs
  return rs
}}"#
    ));
    embedder.run();
    embedder.world.printed.clear();
    for name in ["c", "a", "b"] {
        assert_eq!(embedder.deliver(name), Delivered::Accepted);
    }
    let end = embedder.run_to_end(&[]);
    assert_eq!(
        result(end),
        Datum::List(vec![text("a"), text("b"), text("c")])
    );
    assert_eq!(
        embedder.world.printed,
        [pair("end", "c"), pair("end", "a"), pair("end", "b")]
    );
}

/// `K-TASK-004`: only a wait ends a task's turn. A call, a host read, a
/// `print`, a `spawn` and a `cancel` do not let a ready task in.
#[test]
fn nothing_but_a_wait_pauses_a_task() {
    let (end, embedder) = run(r#"
fn background() { do yield print "background" }
fn one() { return 1 }
main {
  let b = spawn call background()
  let x = call one()
  let t = clock
  let r = random
  print "printed"
  let q = spawn call one()
  do cancel q
  print "still main"
  do join b
  return x
}"#);
    assert_eq!(result(end), int(1));
    assert_eq!(
        embedder.world.printed,
        [text("printed"), text("still main"), text("background")]
    );
}

/// `K-TASK-006`: `yield` puts the running task at the back of the queue,
/// and continues at once when the queue is otherwise empty.
#[test]
fn yield_goes_to_the_back_of_the_ready_queue() {
    let (end, embedder) = run(r#"
fn ping(tag) {
  print (tag, 1)
  do yield
  print (tag, 2)
  do yield
  print (tag, 3)
  return tag
}
main {
  let a = spawn call ping("a")
  print "main 1"
  let b = spawn call ping("b")
  print "main 2"
  do yield
  print "main 3"
  let hs = [a, b]
  let rs = join all hs
  return rs
}"#);
    let at = |tag: &str, step: i64| Datum::Tuple(vec![text(tag), int(step)]);
    assert_eq!(result(end), Datum::List(vec![text("a"), text("b")]));
    assert_eq!(
        embedder.world.printed,
        [
            at("a", 1),
            text("main 1"),
            at("b", 1),
            text("main 2"),
            at("a", 2),
            at("b", 2),
            text("main 3"),
            at("a", 3),
            at("b", 3),
        ]
    );
    // Alone in the queue, a yielding task never parks the run.
    let mut alone = Embedder::new("main { do yield do yield return 1 }");
    assert!(matches!(alone.run(), Step::Ended(End::Finished(_))));
}

/// `K-TASK-007`: a `join` on a handle whose task has ended continues at
/// once; the task does not leave the front.
#[test]
fn join_on_an_ended_handle_continues_at_once() {
    let (end, embedder) = run(r#"
fn quick() { return 1 }
fn slow() { do yield print "slow" return 2 }
main {
  let s = spawn call slow()
  let q = spawn call quick()
  let v = join q
  print "joined"
  let qs = [q]
  let vs = join all qs
  print "joined the list"
  let w = join s
  return (v, vs, w)
}"#);
    assert_eq!(
        result(end),
        Datum::Tuple(vec![int(1), Datum::List(vec![int(1)]), int(2)])
    );
    assert_eq!(
        embedder.world.printed,
        [text("joined"), text("joined the list"), text("slow")]
    );
}

/// `K-TASK-008`: when a task ends, the tasks joined on its handle alone
/// become ready in the order they joined, and then the list joins it
/// decides.
#[test]
fn joiners_wake_in_join_order_and_list_joins_after_them() {
    let (end, embedder) = run(r#"
fn target() { let r = perform echo("t") as Any return r }
fn single(h, tag) { let v = join h print tag return v }
fn many(h, tag) { let hs = [h] let v = join all hs print tag return v }
main {
  let t = spawn call target()
  let m = spawn call many(t, "list")
  let a = spawn call single(t, "first")
  let b = spawn call single(t, "second")
  let rest = [m, a, b]
  do join all rest
  return "done"
}"#);
    assert_eq!(result(end), text("done"));
    assert_eq!(
        embedder.world.printed,
        [text("first"), text("second"), text("list")]
    );
}

/// `K-TASK-009`: joining a handle again gives the same result value, or a
/// raise of the same error value.
#[test]
fn joining_a_handle_twice_gives_the_same_answer() {
    let (end, _) = run(r#"
fn fine() { return [1] }
fn broken() { throw {kind: "x"} }
main {
  let h = spawn call fine()
  let a = join h
  let b = join h
  let g = spawn call broken()
  let e1 = null
  let e2 = null
  try { do join g } catch e { set e1 = e }
  try { do join g } catch e { set e2 = e }
  set a[1] = 2
  set e1.seen = true
  return (b, e2)
}"#);
    assert_eq!(
        result(end),
        Datum::Tuple(vec![
            Datum::List(vec![int(1), int(2)]),
            record([("kind", text("x")), ("seen", Datum::Bool(true))]),
        ])
    );
}

/// `K-TASK-010`: a `join` takes task handles; a task that joins its own
/// handle raises `join_self`; a handle may appear in a list twice.
#[test]
fn join_takes_task_handles_and_never_the_joiner_itself() {
    let (end, _) = run(r#"
fn me(box) { do yield let h = box.h do join h }
fn us(box) { do yield let hs = [box.h] do join all hs }
fn one() { return 1 }
main {
  let kinds = []
  try { do join 1 } catch e { set kinds[0] = e.kind }
  let not_handles = [1]
  try { do join all not_handles } catch e { set kinds[1] = e.kind }
  try { do join race "x" } catch e { set kinds[2] = e.kind }
  let box = {h: null}
  let h = spawn call me(box)
  set box.h = h
  try { do join h } catch e { set kinds[3] = e.kind }
  let h = spawn call us(box)
  set box.h = h
  try { do join h } catch e { set kinds[4] = e.kind }
  let q = spawn call one()
  let twice = (q, q)
  let both = join all twice
  return (kinds, both)
}"#);
    assert_eq!(
        result(end),
        Datum::Tuple(vec![
            Datum::List(
                [
                    "type_error",
                    "type_error",
                    "type_error",
                    "join_self",
                    "join_self"
                ]
                .map(text)
                .to_vec()
            ),
            Datum::List(vec![int(1), int(1)]),
        ])
    );
}

/// Runs three tasks `a`, `b` and `c` under one list `join`, delivering in
/// `order`, and gives what the join yielded or raised. The tasks named in
/// `failing` raise their tag once their effect is answered.
fn joined(mode: &str, order: &[&str], failing: &[&str]) -> Datum {
    let callee = |tag: &str| {
        if failing.contains(&tag) {
            "fails"
        } else {
            "works"
        }
    };
    let (a, b, c) = (callee("a"), callee("b"), callee("c"));
    let mut embedder = Embedder::new(&format!(
        r#"
fn works(tag) {{ let r = perform echo(tag) as Any return r }}
fn fails(tag) {{ do perform echo(tag) as Any throw tag }}
main {{
  let a = spawn call {a}("a")
  let b = spawn call {b}("b")
  let c = spawn call {c}("c")
  let hs = [a, b, c]
  let out = null
  try {{ set out = join {mode} hs }} catch e {{ set out = ("raised", e) }}
  print out
  do join settled hs
  return out
}}"#
    ));
    result(embedder.run_to_end(order))
}

fn raised(error: Datum) -> Datum {
    Datum::Tuple(vec![text("raised"), error])
}

/// `K-TASK-011`: `join all` gives the results in member order, or raises
/// at the first member to fail.
#[test]
fn join_all_returns_every_result_or_raises_at_the_first_failure() {
    assert_eq!(
        joined("all", &["c", "a", "b"], &[]),
        Datum::List(vec![text("a"), text("b"), text("c")])
    );
    assert_eq!(
        joined("all", &["c", "b", "a"], &["a", "b"]),
        raised(text("b"))
    );
    // Decided when it starts: the first failed member in list order.
    let (end, _) = run(r#"
fn now(v) { return v }
fn bad(v) { throw v }
main {
  let x = spawn call bad("x")
  let n = spawn call now(1)
  let y = spawn call bad("y")
  let hs = [n, y, x]
  let out = null
  try { do join all hs } catch e { set out = e }
  let none = []
  let empty = join all none
  return (out, empty)
}"#);
    assert_eq!(
        result(end),
        Datum::Tuple(vec![text("y"), Datum::List(Vec::new())])
    );
}

/// `K-TASK-012`: `join settled` waits for every member and never raises
/// for one.
#[test]
fn join_settled_reports_every_member() {
    assert_eq!(
        joined("settled", &["b", "c", "a"], &["b"]),
        Datum::List(vec![
            record([("status", text("ok")), ("value", text("a"))]),
            record([("status", text("error")), ("error", text("b"))]),
            record([("status", text("ok")), ("value", text("c"))]),
        ])
    );
    let (end, _) = run("main { let none = [] let rs = join settled none return rs }");
    assert_eq!(result(end), Datum::List(Vec::new()));
}

/// `K-TASK-013`: `join race` returns or raises as the first member to end
/// did.
#[test]
fn join_race_follows_the_first_member_run_to_end() {
    assert_eq!(joined("race", &["c", "b", "a"], &["b"]), text("c"));
    assert_eq!(joined("race", &["b", "c", "a"], &["b"]), raised(text("b")));
    let (end, _) = run(r#"
fn now(v) { return v }
fn bad(v) { throw v }
main {
  let x = spawn call bad("x")
  let n = spawn call now(1)
  let hs = [n, x]
  let first = join race hs
  let none = ()
  let kind = null
  try { do join race none } catch e { set kind = e.kind }
  return (first, kind)
}"#);
    assert_eq!(result(end), Datum::Tuple(vec![int(1), text("empty_join")]));
}

/// `K-TASK-014`: `join any` returns the first success, and raises
/// `all_failed` with every error, in member order, when there is none.
#[test]
fn join_any_returns_the_first_success_or_every_error() {
    assert_eq!(joined("any", &["b", "c", "a"], &["b"]), text("c"));
    let mut embedder = Embedder::new(
        r#"
fn fails(tag) { do perform echo(tag) as Any throw tag }
fn now(v) { return v }
fn bad(v) { throw v }
main {
  let a = spawn call fails("a")
  let b = spawn call fails("b")
  let hs = [a, b]
  let failure = null
  try { do join any hs } catch e { set failure = (e.kind, e.data) }
  let x = spawn call bad("x")
  let n = spawn call now(1)
  let started = [x, n]
  let first = join any started
  let none = []
  let kind = null
  try { do join any none } catch e { set kind = e.kind }
  return (failure, first, kind)
}"#,
    );
    assert_eq!(
        result(embedder.run_to_end(&["b", "a"])),
        Datum::Tuple(vec![
            Datum::Tuple(vec![
                text("all_failed"),
                Datum::List(vec![text("a"), text("b")])
            ]),
            int(1),
            text("empty_join"),
        ])
    );
}

/// `K-TASK-015`: a list `join` cancels nothing. A member that has not
/// ended when it returns keeps running.
#[test]
fn a_list_join_cancels_no_member() {
    let mut embedder = Embedder::new(&format!(
        r#"{WORKER}
main {{
  let a = spawn call worker("a")
  let b = spawn call worker("b")
  let hs = [a, b]
  let first = join race hs
  print ("race", first)
  let other = join a
  return other
}}"#
    ));
    let end = embedder.run_to_end(&["b", "a"]);
    assert_eq!(result(end), text("a"));
    assert!(embedder.withdrawn.is_empty());
    assert_eq!(
        embedder.world.printed[2..],
        [
            pair("end", "b"),
            Datum::Tuple(vec![text("race"), text("b")]),
            pair("end", "a")
        ]
    );
}

/// `K-TASK-016`: a task's error is data on its handle. It stops no other
/// task and does not end the run; it is observed by a `join` that raises
/// it, or by membership of a list `join` that has returned.
#[test]
fn a_task_error_is_data_on_its_handle() {
    const FAILS: &str = "fn fails(tag) { do perform echo(tag) as Any throw tag }\n";
    // Never joined: the run's end names the task.
    let mut unobserved = Embedder::new(&format!(
        "{FAILS}{WORKER} main {{ do spawn call fails(\"f\") let w = spawn call worker(\"w\") let r = join w return r }}"
    ));
    let (unfinished, errors) = outstanding(unobserved.run_to_end(&["f", "w"]));
    assert_eq!(unfinished, []);
    assert_eq!(errors, [spawned(TaskIdentity::Main, main_site([0, 0]), 0)]);
    // The other task ran on after the failure.
    assert_eq!(unobserved.world.printed.last(), Some(&pair("end", "w")));

    // Observed by a join that raises it.
    let mut observed = Embedder::new(&format!(
        "{FAILS} main {{ let f = spawn call fails(\"f\") try {{ do join f }} catch e {{ return e }} }}"
    ));
    assert_eq!(result(observed.run_to_end(&[])), text("f"));

    // Observed as a member of a list join that returned before it failed.
    let mut member = Embedder::new(&format!(
        r#"{FAILS}{WORKER}
main {{
  let f = spawn call fails("f")
  let w = spawn call worker("w")
  let hs = [f, w]
  let first = join race hs
  do perform echo("gate") as Any
  return first
}}"#
    ));
    assert_eq!(result(member.run_to_end(&["w", "f", "gate"])), text("w"));
}

const CLEANS_UP: &str = r#"
fn cleans_up() {
  try {
    do perform echo("slow") as Any
    print "not reached"
  } catch e {
    print ("caught", e.kind)
    do perform echo("cleanup") as Any
    print "cleaned"
    throw e
  } finally {
    print "finally"
  }
}
"#;

/// `K-TASK-017`: `cancel` raises `cancelled` at the target's wait; its
/// `catch` and `finally` blocks run and may wait; the withdrawn wait is
/// reported at the next park and its outcome is dropped.
#[test]
fn cancel_raises_at_the_wait_and_cleanup_runs_and_may_wait() {
    let mut embedder = Embedder::new(&format!(
        r#"{CLEANS_UP}
main {{
  let h = spawn call cleans_up()
  do perform echo("gate") as Any
  do cancel h
  print "cancelled"
  let kind = null
  try {{ do join h }} catch e {{ set kind = e.kind }}
  do cancel h
  return kind
}}"#
    ));
    assert!(matches!(embedder.run(), Step::Parked(_)));
    assert_eq!(embedder.asked(), ["slow", "gate"]);
    embedder.deliver("gate");
    let Step::Parked(park) = embedder.run() else {
        panic!("the cleanup waits");
    };
    assert_eq!(park.withdrawn, [WaitId(0)]);
    assert_eq!(park.requests.len(), 1);
    assert_eq!(embedder.deliver("slow"), Delivered::Dropped);
    assert_eq!(result(embedder.run_to_end(&["cleanup"])), text("cancelled"));
    assert_eq!(
        embedder.world.printed,
        [
            text("cancelled"),
            pair("caught", "cancelled"),
            text("cleaned"),
            text("finally"),
        ]
    );
}

/// `K-TASK-017`: a ready task is cancelled in place of the result it was
/// resuming with; the running task's own `cancel` raises; a task
/// cancelled again while it cleans up is raised in again.
#[test]
fn cancel_reaches_a_ready_task_the_running_task_and_a_task_cleaning_up() {
    // Ready: `w`'s outcome is delivered, and `main` cancels it first.
    let mut ready = Embedder::new(
        r#"
fn waits() {
  try { do perform echo("w") as Any return "delivered" } catch e { return e.kind }
}
fn cancels_itself(box) {
  do yield
  let h = box.h
  try { do cancel h } catch e { return e.kind }
  return "not raised"
}
main {
  let w = spawn call waits()
  do perform echo("gate") as Any
  do cancel w
  let from_ready = join w
  let box = {h: null}
  let s = spawn call cancels_itself(box)
  set box.h = s
  let from_self = join s
  return (from_ready, from_self)
}"#,
    );
    ready.run();
    ready.deliver("gate");
    ready.deliver("w");
    assert_eq!(
        result(ready.run_to_end(&[])),
        pair("cancelled", "cancelled")
    );

    // Cleaning up: the second cancel raises at the cleanup's own wait.
    let mut again = Embedder::new(&format!(
        r#"{CLEANS_UP}
main {{
  let h = spawn call cleans_up()
  do perform echo("gate 1") as Any
  do cancel h
  do perform echo("gate 2") as Any
  do cancel h
  let kind = null
  try {{ do join h }} catch e {{ set kind = e.kind }}
  return kind
}}"#
    ));
    assert_eq!(
        result(again.run_to_end(&["gate 1", "gate 2"])),
        text("cancelled")
    );
    assert_eq!(
        again.world.printed,
        [pair("caught", "cancelled"), text("finally")]
    );
}

/// `K-TASK-018`, `K-TASK-019`: at the run's end a task that was never a
/// member of a passed list `join` and has not ended is a typed error; a
/// member of one is cancelled and runs no cleanup; a raise in `main` is
/// the run's error whatever the other tasks did.
#[test]
fn the_run_ends_with_its_tasks_accounted_for() {
    let mut unfinished = Embedder::new(&format!(
        "{WORKER} main {{ do spawn call worker(\"w\") return 1 }}"
    ));
    let (tasks, errors) = outstanding(unfinished.run_to_end(&[]));
    assert_eq!(tasks, [spawned(TaskIdentity::Main, main_site([0, 0]), 0)]);
    assert_eq!(errors, []);
    assert_eq!(
        unfinished.machine.run(&mut unfinished.world, u64::MAX),
        Err(MachineError::Ended)
    );
    assert_eq!(
        unfinished
            .machine
            .deliver(WaitId(0), Outcome::Completed(Datum::Null)),
        Err(DeliverError::Ended)
    );

    // A member of a join that returned is cancelled, with no cleanup.
    let (end, embedder) = run(&format!(
        r#"{CLEANS_UP}
fn quick() {{ return 1 }}
main {{
  let slow = spawn call cleans_up()
  let q = spawn call quick()
  let hs = [slow, q]
  let first = join race hs
  return first
}}"#
    ));
    assert_eq!(result(end), int(1));
    assert_eq!(embedder.world.printed, []);

    // `finish` and `fail` end the run from any task; no `finally` runs.
    let (end, embedder) = run(r#"
fn ends() { try { finish "from a task" } finally { print "finally" } }
main { let h = spawn call ends() do join h }"#);
    assert_eq!(result(end), text("from a task"));
    assert_eq!(embedder.world.printed, []);
    let (end, _) = run("main { fail {reason: \"no\"} }");
    assert_eq!(end, End::Failed(record([("reason", text("no"))])));

    // `main` ending in a raise is the run's error.
    let mut raised = Embedder::new(&format!(
        "{WORKER} main {{ do spawn call worker(\"w\") throw \"plain\" }}"
    ));
    let End::Error(RunError::Uncaught(error)) = raised.run_to_end(&[]) else {
        panic!("main raised");
    };
    assert_eq!(error, text("plain"));
}

/// `K-TASK-020`, `K-EFF-008`: a task is identified by its spawner, the
/// spawn's site and that site's occurrence; an effect by its task, its
/// site, the site's occurrence in that task and the loops around it,
/// outermost first across the task's active calls.
#[test]
fn tasks_and_effects_carry_their_identities() {
    let mut embedder = Embedder::new(
        r#"
fn fetch(i) {
  let out = []
  for j in ["x", "y"] {
    let key = (i, j)
    let r = perform echo(key) as Any
    set out[list.len(out)] = r
  }
  return out
}
fn worker(i) {
  while true {
    let rs = call fetch(i)
    return rs
  }
}
main {
  let hs = []
  for i in [10, 20] {
    let h = spawn call worker(i)
    set hs[list.len(hs)] = h
  }
  let rs = join all hs
  return rs
}"#,
    );
    let end = embedder.run_to_end(&[]);
    let row = |i: i64| {
        Datum::List(vec![
            Datum::Tuple(vec![int(i), text("x")]),
            Datum::Tuple(vec![int(i), text("y")]),
        ])
    };
    assert_eq!(result(end), Datum::List(vec![row(10), row(20)]));
    let expected: Vec<EffectIdentity> = [(0, 0), (1, 0), (0, 1), (1, 1)]
        .into_iter()
        .map(|(task, iteration)| EffectIdentity {
            task: spawned(TaskIdentity::Main, main_site([1, 1, 0, 0]), task),
            site: function_site("fetch", [1, 1, 1, 0]),
            occurrence: iteration,
            loops: vec![
                LoopIteration {
                    site: function_site("worker", [0]),
                    iteration: 0,
                },
                LoopIteration {
                    site: function_site("fetch", [1]),
                    iteration,
                },
            ],
        })
        .collect();
    let identities: Vec<EffectIdentity> = embedder.requests.iter().map(identity).cloned().collect();
    assert_eq!(identities, expected);

    // A task spawned by a spawned task names its whole line of spawners.
    let mut nested = Embedder::new(
        r#"
fn leaf() { do sleep 5 }
fn branch() { let l = spawn call leaf() do join l }
main { let b = spawn call branch() do join b }"#,
    );
    nested.run_to_end(&[]);
    let branch = spawned(TaskIdentity::Main, main_site([0, 0]), 0);
    let Request::Sleep(sleep) = &nested.requests[0] else {
        panic!("a sleep asks for a sleep");
    };
    assert_eq!(sleep.duration, std::time::Duration::from_millis(5));
    assert_eq!(
        sleep.identity.task,
        spawned(branch, function_site("branch", [0, 0]), 0)
    );
}

/// `K-TASK-021`: `tasks.unfinished()` gives the handles of every other
/// task that has not ended, in spawn order.
#[test]
fn tasks_unfinished_lists_the_other_live_tasks() {
    let mut embedder = Embedder::new(&format!(
        r#"{WORKER}
fn quick() {{ return 1 }}
fn counts() {{ do yield return list.len(tasks.unfinished()) }}
main {{
  let a = spawn call worker("a")
  let q = spawn call quick()
  let b = spawn call worker("b")
  let live = tasks.unfinished()
  for h in live {{ do cancel h }}
  let hs = [a, q, b]
  let outcomes = join settled hs
  let statuses = []
  for s in outcomes {{ set statuses[list.len(statuses)] = s.status }}
  let c = spawn call counts()
  let seen = join c
  return (list.len(live), statuses, seen)
}}"#
    ));
    assert_eq!(
        result(embedder.run_to_end(&[])),
        Datum::Tuple(vec![
            int(2),
            Datum::List(vec![text("error"), text("ok"), text("error")]),
            // From inside a task: `main`, and not the caller.
            int(1),
        ])
    );
}

/// `K-TASK-023`: when no task is ready and none waits on an effect or a
/// sleep, the run ends in a deadlock that names the waiting tasks.
#[test]
fn tasks_that_only_wait_on_each_other_deadlock() {
    let (end, _) = run(r#"
fn waits(box) { do yield let h = box.other do join h }
main {
  let box = {other: null}
  let a = spawn call waits(box)
  set box.other = a
  let box2 = {other: a}
  let b = spawn call waits(box2)
  set box.other = b
  do join a
}"#);
    let a = spawned(TaskIdentity::Main, main_site([1, 0]), 0);
    let b = spawned(TaskIdentity::Main, main_site([4, 0]), 0);
    assert_eq!(
        end,
        End::Error(RunError::Deadlock {
            waiting: vec![TaskIdentity::Main, a, b]
        })
    );
}

/// `K-MACH-004`: an outcome for a wait never handed out, delivered twice
/// or of the wrong kind is an error to the embedder and changes nothing.
#[test]
fn deliver_refuses_what_no_wait_can_take() {
    let mut embedder =
        Embedder::new("main { let a = perform echo(\"a\") as Any do sleep 0 return a }");
    let done = || Outcome::Completed(text("other"));
    assert_eq!(
        embedder.machine.deliver(WaitId(0), done()),
        Err(DeliverError::UnknownWait { wait: WaitId(0) })
    );
    embedder.run();
    assert_eq!(
        embedder.machine.deliver(WaitId(0), Outcome::Elapsed),
        Err(DeliverError::WrongOutcome { wait: WaitId(0) })
    );
    assert_eq!(
        embedder.machine.deliver(WaitId(7), done()),
        Err(DeliverError::UnknownWait { wait: WaitId(7) })
    );
    embedder.deliver("a");
    assert_eq!(
        embedder.machine.deliver(WaitId(0), done()),
        Err(DeliverError::AlreadyDelivered { wait: WaitId(0) })
    );
    // A sleep of zero is still a wait (`K-EFF-010`).
    assert_eq!(result(embedder.run_to_end(&[])), text("a"));
    assert_eq!(embedder.asked(), ["a", "sleep"]);
}

/// `K-MACH-002`, `K-MACH-005`, `K-MACH-006`: a slice return changes no
/// value, no order and no charge; a cancel the host reports at a slice
/// boundary or at a park ends the run there.
#[test]
fn slices_change_nothing_and_a_host_cancel_ends_the_run() {
    let program = format!(
        r#"{WORKER}
main {{
  let hs = []
  for tag in ["a", "b", "c"] {{
    let h = spawn call worker(tag)
    set hs[list.len(hs)] = h
  }}
  let rs = join all hs
  return rs
}}"#
    );
    let observe = |slice: u64| {
        let mut embedder = Embedder::with(
            &program,
            Setup {
                slice,
                ..Setup::default()
            },
        );
        let mut slices = 0;
        let end = loop {
            match embedder.run() {
                Step::Ended(end) => break end,
                Step::Slice => slices += 1,
                Step::Parked(_) => {
                    for name in ["c", "b", "a"] {
                        embedder.deliver(name);
                    }
                }
            }
        };
        (
            (
                end,
                embedder.world.printed,
                embedder.requests,
                embedder.machine.meters().charged,
            ),
            slices,
        )
    };
    let (whole, no_slices) = observe(u64::MAX);
    let (sliced, slices) = observe(3);
    assert_eq!(no_slices, 0);
    assert!(slices > 5, "{slices}");
    assert_eq!(sliced, whole);

    let mut at_slice = Embedder::with(
        "main { while true { } }",
        Setup {
            slice: 10,
            ..Setup::default()
        },
    );
    assert_eq!(at_slice.run(), Step::Slice);
    at_slice.world.cancel = true;
    assert_eq!(at_slice.run(), Step::Ended(End::Cancelled));

    let mut at_park = Embedder::new(
        "main { try { do perform echo(\"a\") as Any } finally { print \"cleanup\" } }",
    );
    at_park.world.cancel = true;
    assert_eq!(at_park.run(), Step::Ended(End::Cancelled));
    assert_eq!(at_park.world.printed, []);
}
