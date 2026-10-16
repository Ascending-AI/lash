//! Continuation laws (`K-EVAL-008`): a frame that stands at an action
//! nested in its statement's expression keeps the operands its statement
//! evaluated as roles, named by their sites; a resume entry loads them
//! under whatever layout the run is opened in; and the run leaves what a
//! run that never parked leaves, at every slice and every reopen.
//!
//! - CONT-EXACT: each case runs uninterrupted, then at slices MAX, 1 and 0,
//!   sealed and opened cold at every slice and park under alternating
//!   layouts and registries, and leaves the same end, prints, requests,
//!   charge and statement starts.
//! - CONT-ONCE: what comes before the action is neither run nor charged
//!   again: the charge and the statement starts are the uninterrupted
//!   run's.
//! - CONT-GRAMMAR: a saved continuation the grammar does not admit is
//!   refused, naming its site.
//! - ROOTS-CANON: the memory verdict is the uninterrupted run's at every
//!   slice and reopen.
//! - SITE-NESTED: a nested action's wait is named by its own site.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, FunctionId, FunctionRegistry, TaskId, Value, parse_definition};
use lash_kernel_state::paged::{
    HeaderRecord, IncomingRecord, PendingRecord, PhaseRecord, RoleRecord, RolesRecord,
    TaskStateRecord,
};

use super::embedder::{self, Embedder, Setup, label};
use crate::{Bounds, End, Layout, Machine, Observe, Request, Step};

/// Library functions whose bodies run where they are called: `body.say(t)`
/// prints `t` and gives it, and `body.inc(n)` gives `n + 1`. Each states a
/// native implementation that is not registered.
fn bodies(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut register = |name: &'static str, body: &str| {
        let definition = parse_definition(&format!(
            "function {name}(x: Any) -> Any\nkernel 1\ncharge 1\nuse num.add = @{}\nnative\n\
             body {{ {body} }}\n",
            ids["num.add"]
        ))
        .unwrap();
        (name, registry.register(definition, None).unwrap())
    };
    vec![
        register("body.say", "print x return x"),
        register("body.inc", "return num.add(x, 1)"),
    ]
}

fn setup(slice: u64) -> Setup {
    Setup {
        slice,
        probes: Some(bodies),
        ..Setup::default()
    }
}

/// What a run leaves observable: its end, what it printed, every request
/// it handed out, its charge, and how often each statement started.
#[derive(Debug, PartialEq)]
struct Trace {
    end: String,
    printed: Vec<String>,
    requests: Vec<String>,
    charged: u64,
    starts: BTreeMap<u32, u64>,
}

fn end_of(end: &End) -> String {
    match end {
        // A finished run compares by its result: its carried root names
        // pages a reopened run sealed on the way.
        End::Finished(finished) => format!("Finished({:?})", finished.result),
        other => format!("{other:?}"),
    }
}

/// The layout and registry a run is opened in at hop `hop`: odd layouts
/// most hops, an even one every other, and `pair.twice` alternating
/// between its native and its body.
fn hop(hop: u64) -> (Layout, bool) {
    let mixed = hop.wrapping_mul(0x9e37_79b9);
    let layout = if hop.is_multiple_of(2) {
        Layout(mixed & !1)
    } else {
        Layout(mixed | 1)
    };
    (layout, hop.is_multiple_of(2))
}

/// How a run is driven.
#[derive(Clone, Copy)]
struct Drive {
    slice: u64,
    /// Sealed and opened cold at every slice and park.
    reopens: bool,
    bounds: Bounds,
    /// The library functions the run's text may call.
    probes: embedder::Probes,
}

impl Drive {
    fn new(slice: u64, reopens: bool) -> Self {
        Self {
            slice,
            reopens,
            bounds: embedder::ROOMY,
            probes: bodies,
        }
    }
}

/// Where a run's frames stood at its reopens: the phases of nested
/// actions and the headers of protocol loops.
#[derive(Default)]
struct Seen {
    phases: Vec<PhaseRecord>,
    headers: Vec<HeaderRecord>,
}

/// Runs `text`, answering parks with the requests `order` names and then
/// oldest first. Gives the trace and where its frames stood at each
/// reopen.
fn drive(text: &str, order: &[&str], drive: Drive) -> (Trace, Seen) {
    let setup = Setup {
        bounds: drive.bounds,
        probes: Some(drive.probes),
        ..setup(drive.slice)
    };
    let mut embedder = match Embedder::try_with(text, setup) {
        Ok(embedder) => embedder,
        Err(error) => {
            let trace = Trace {
                end: format!("start refused: {error:?}"),
                printed: Vec::new(),
                requests: Vec::new(),
                charged: 0,
                starts: BTreeMap::new(),
            };
            return (trace, Seen::default());
        }
    };
    embedder.machine.set_observe(Observe::Counts);
    let mut starts: BTreeMap<u32, u64> = BTreeMap::new();
    let mut seen = Seen::default();
    let mut order = order.iter();
    let mut hops = 0;
    let end = loop {
        let step = match embedder.machine.run(&mut embedder.world, drive.slice) {
            Ok(step) => step,
            Err(error) => break format!("machine refused: {error:?}"),
        };
        for (ordinal, count) in embedder.machine.take_ran() {
            *starts.entry(ordinal).or_default() += count;
        }
        if let Step::Parked(park) = &step {
            embedder.requests.extend(park.requests.iter().cloned());
            embedder.pending.extend(park.requests.iter().cloned());
        }
        if let Step::Ended(end) = &step {
            break end_of(end);
        }
        if drive.reopens {
            hops += 1;
            seen.phases.extend(
                embedder
                    .machine
                    .pending_phases()
                    .into_iter()
                    .map(|(_, _, phase)| phase),
            );
            seen.headers.extend(embedder.machine.loop_headers());
            let (layout, native_twice) = hop(hops);
            if let Err(error) = embedder.try_reopen(layout, native_twice) {
                break format!("reopen refused: {error}");
            }
            embedder.machine.set_observe(Observe::Counts);
        }
        if matches!(step, Step::Parked(_)) {
            let next = match order.next() {
                Some(name) => (*name).to_string(),
                None => label(
                    embedder
                        .pending
                        .first()
                        .expect("a park with nothing pending"),
                ),
            };
            embedder.deliver(&next);
        }
    };
    let trace = Trace {
        end,
        printed: embedder
            .world
            .printed
            .iter()
            .map(|datum| format!("{datum:?}"))
            .collect(),
        requests: embedder.requests.iter().map(|r| format!("{r:?}")).collect(),
        charged: embedder.machine.meters().charged,
        starts,
    };
    (trace, seen)
}

/// CONT-EXACT and CONT-ONCE of one case: the uninterrupted run gives
/// `expected` and prints `printed`; every sliced and reopened run leaves
/// its trace; the reopened runs see each phase of `want`.
fn case(text: &str, order: &[&str], expected: &Datum, printed: &[Datum], want: &[PhaseRecord]) {
    let phases = exact(Drive::new(u64::MAX, false), text, order, expected, printed).phases;
    for phase in want {
        assert!(phases.contains(phase), "the reopens saw {phases:?}: {text}");
    }
}

/// The run of `text` under `base`, uninterrupted, gives `expected` and
/// prints `printed`, and every sliced and reopened run leaves its trace.
/// Gives where the reopened runs' frames stood.
fn exact(base: Drive, text: &str, order: &[&str], expected: &Datum, printed: &[Datum]) -> Seen {
    let at = |slice, reopens| Drive {
        slice,
        reopens,
        ..base
    };
    let (reference, _) = drive(text, order, at(u64::MAX, false));
    assert_eq!(reference.end, format!("Finished({expected:?})"), "{text}");
    let printed: Vec<String> = printed.iter().map(|datum| format!("{datum:?}")).collect();
    assert_eq!(reference.printed, printed, "{text}");
    let mut seen = Seen::default();
    for slice in [u64::MAX, 1, 0] {
        let (sliced, _) = drive(text, order, at(slice, false));
        assert_eq!(sliced, reference, "slice {slice}: {text}");
        let (reopened, at_reopens) = drive(text, order, at(slice, true));
        assert_eq!(reopened, reference, "reopened at slice {slice}: {text}");
        seen.phases.extend(at_reopens.phases);
        seen.headers.extend(at_reopens.headers);
    }
    seen
}

fn text(datum: &str) -> Datum {
    Datum::Text(datum.to_string())
}

fn int(datum: i64) -> Datum {
    embedder::int(datum)
}

fn list(datum: &[Datum]) -> Datum {
    Datum::List(datum.to_vec())
}

fn tuple(datum: &[Datum]) -> Datum {
    Datum::Tuple(datum.to_vec())
}

/// Calls and a wait nested in one call's arguments.
const NESTED_AWAIT: &str = "fn a() { print \"a\" return 1 }\n\
    fn c() { let v = perform echo(\"c\") as Any print \"c\" return v }\n\
    fn observe(p, q, r) { print (p, q, r) return [p, q, r] }\n\
    main { let x = \"tool\"\n\
    let r = call observe(call a(), perform echo(x) as Any, call c())\n\
    return r }\n";

/// [`NESTED_AWAIT`] with each action a statement of its own.
const NESTED_AWAIT_HOISTED: &str = "fn a() { print \"a\" return 1 }\n\
    fn c() { let v = perform echo(\"c\") as Any print \"c\" return v }\n\
    fn observe(p, q, r) { print (p, q, r) return [p, q, r] }\n\
    main { let x = \"tool\"\n\
    let t0 = call a()\n\
    let t1 = perform echo(x) as Any\n\
    let t2 = call c()\n\
    let r = call observe(t0, t1, t2)\n\
    return r }\n";

#[test]
fn nested_await_resumes_exactly() {
    let expected = list(&[int(1), text("tool"), text("c")]);
    let printed = [
        text("a"),
        text("c"),
        tuple(&[int(1), text("tool"), text("c")]),
    ];
    case(
        NESTED_AWAIT,
        &["tool", "c"],
        &expected,
        &printed,
        &[PhaseRecord::Called, PhaseRecord::Waiting],
    );
    case(
        NESTED_AWAIT_HOISTED,
        &["tool", "c"],
        &expected,
        &printed,
        &[],
    );
}

/// A role holds the value its operand had when it was evaluated, while
/// another task changes the record it was read from during the park.
const MUTATED_RECEIVER: &str = "fn writer(o) { do perform echo(\"w\") as Any set o.x = 2 return null }\n\
    main { let o = {x: 1}\n\
    let h = spawn call writer(o)\n\
    let r = [o.x, perform echo(\"m\") as Any]\n\
    let done = join h\n\
    return (r, o.x) }\n";

const MUTATED_RECEIVER_HOISTED: &str = "fn writer(o) { do perform echo(\"w\") as Any set o.x = 2 return null }\n\
    main { let o = {x: 1}\n\
    let h = spawn call writer(o)\n\
    let t0 = o.x\n\
    let t1 = perform echo(\"m\") as Any\n\
    let r = [t0, t1]\n\
    let done = join h\n\
    return (r, o.x) }\n";

#[test]
fn parked_role_keeps_the_evaluated_value() {
    let expected = tuple(&[list(&[int(1), text("m")]), int(2)]);
    case(
        MUTATED_RECEIVER,
        &["w", "m"],
        &expected,
        &[],
        &[PhaseRecord::Waiting],
    );
    case(MUTATED_RECEIVER_HOISTED, &["w", "m"], &expected, &[], &[]);
}

/// A callback that spawns, inside a call inside a call inside an
/// expression.
const CALLBACK: &str = "fn work(x) { let v = [perform echo(x) as Any] return v[0] }\n\
    fn collect(f, xs) { let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out }\n\
    fn gather(hs) { let rs = join all hs return rs }\n\
    fn tag(t) { print t return t }\n\
    main { let r = (call tag(\"a\"),\n\
      call gather(call collect(fn(x) { let h = spawn call work(x) return h }, [1, 2])),\n\
      call tag(\"b\"))\n\
    return r }\n";

const CALLBACK_HOISTED: &str = "fn work(x) { let v = perform echo(x) as Any return v }\n\
    fn collect(f, xs) { let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out }\n\
    fn gather(hs) { let rs = join all hs return rs }\n\
    fn tag(t) { print t return t }\n\
    main { let t0 = call tag(\"a\")\n\
    let f = fn(x) { let h = spawn call work(x) return h }\n\
    let xs = [1, 2]\n\
    let t1 = call collect(f, xs)\n\
    let t2 = call gather(t1)\n\
    let t3 = call tag(\"b\")\n\
    let r = (t0, t2, t3)\n\
    return r }\n";

#[test]
fn callback_inside_expression_resumes_exactly() {
    let expected = tuple(&[text("a"), list(&[int(1), int(2)]), text("b")]);
    case(
        CALLBACK,
        &["echo", "echo"],
        &expected,
        &[text("a"), text("b")],
        &[PhaseRecord::Called, PhaseRecord::Waiting],
    );
    case(
        CALLBACK_HOISTED,
        &["echo", "echo"],
        &expected,
        &[text("a"), text("b")],
        &[],
    );
}

/// A `spawn` inside an expression: the child runs to its first wait, and
/// the spawner goes on with the handle before any other ready task
/// (`K-TASK-002`), keeping what its statement evaluated.
const SPAWNED: &str = "fn work(x) { print x let v = perform echo(x) as Any return v }\n\
    fn other() { print \"other\" return 0 }\n\
    main { let o = spawn call other()\n\
    let hs = [body.say(\"before\"), spawn call work(\"child\"), body.say(\"after\")]\n\
    let h = hs[1]\n\
    let r = join h\n\
    let d = join o\n\
    return (hs[0], r, hs[2]) }\n";

const SPAWNED_HOISTED: &str = "fn work(x) { print x let v = perform echo(x) as Any return v }\n\
    fn other() { print \"other\" return 0 }\n\
    main { let o = spawn call other()\n\
    let t0 = body.say(\"before\")\n\
    let h = spawn call work(\"child\")\n\
    let t2 = body.say(\"after\")\n\
    let r = join h\n\
    let d = join o\n\
    return (t0, r, t2) }\n";

#[test]
fn a_spawn_inside_an_expression_resumes_exactly() {
    let expected = tuple(&[text("before"), text("child"), text("after")]);
    let printed = [text("other"), text("before"), text("child"), text("after")];
    case(
        SPAWNED,
        &["child"],
        &expected,
        &printed,
        &[PhaseRecord::Spawned, PhaseRecord::Called],
    );
    case(SPAWNED_HOISTED, &["child"], &expected, &printed, &[]);
}

/// Library bodies called inside expressions: in a `while` condition, in
/// an assignment's index while the right-hand side waits in register 0,
/// beside `if` and `let` expressions, and `pair.twice`, whose native
/// answers on alternate reopens.
const BODIES: &str = "main { let xs = [9, 0, 0, 0]\n\
    let i = 0\n\
    while num.lt(body.inc(i), 4) {\n\
      set xs[body.inc(i)] = [i, let(j = body.inc(i), if(num.lt(j, 2), body.say(j), j))]\n\
      set i = body.inc(i)\n\
    }\n\
    return (xs, pair.twice(body.inc(1)), [i, perform echo(\"w\") as Any]) }\n";

#[test]
fn library_bodies_inside_expressions_resume_exactly() {
    let expected = tuple(&[
        list(&[
            int(9),
            list(&[int(0), int(1)]),
            list(&[int(1), int(2)]),
            list(&[int(2), int(3)]),
        ]),
        int(4),
        list(&[int(3), text("w")]),
    ]);
    case(
        BODIES,
        &["w"],
        &expected,
        &[int(1)],
        &[PhaseRecord::Called, PhaseRecord::Waiting],
    );
}

/// `K-EVAL-004`: a nested call reads a callee held in a variable before its
/// arguments and keeps it as a role while they run, so an argument that
/// assigns the variable changes the next call's callee, not this one's.
#[test]
fn a_nested_call_reads_its_callee_before_its_arguments() {
    const SOURCE: &str = "fn g(x) { return [\"g\", x] }\n\
        fn h(x) { return [\"h\", x] }\n\
        main { let f = &g\n\
        let swap = fn() { set f = &h let w = perform echo(\"w\") as Any return w }\n\
        let r = [apply f(apply swap()), apply f(2)]\n\
        return r }\n";
    let expected = list(&[list(&[text("g"), text("w")]), list(&[text("h"), int(2)])]);
    case(SOURCE, &["w"], &expected, &[], &[PhaseRecord::Called]);
}

/// A task cancelled while it waits at a nested `perform` resumes at its
/// request with the raise, at the action (`K-TASK-017`).
#[test]
fn cancel_at_a_nested_wait_resumes_exactly() {
    const SOURCE: &str = "fn work(x) { let v = [perform echo(x) as Any] return v[0] }\n\
        main { let h = spawn call work(0)\n\
        do cancel h\n\
        let r = \"none\"\n\
        try { let v = join h\n\
        set r = v } catch e { set r = e.kind }\n\
        return r }\n";
    case(
        SOURCE,
        &[],
        &text("cancelled"),
        &[],
        &[PhaseRecord::Waiting],
    );
}

/// SITE-NESTED (`K-EFF-008`, `K-SITE-002`): each wait of a statement that
/// holds several is named by its own action's site, counted per site and
/// loop iteration.
#[test]
fn each_nested_wait_is_named_by_its_own_site() {
    const SOURCE: &str = "main { let out = []\n\
        for i in [0, 1] { set out[list.len(out)] = (perform echo(\"a\") as Any, sleep(1)) }\n\
        return out }\n";
    let mut embedder = Embedder::with(SOURCE, setup(u64::MAX));
    embedder.run_to_end(&[]);
    let named: Vec<(String, u64, u64)> = embedder
        .requests
        .iter()
        .map(|request| {
            let identity = match request {
                Request::Effect(effect) => &effect.identity,
                Request::Sleep(sleep) => &sleep.identity,
            };
            let iteration = identity.loops.first().map_or(99, |at| at.iteration);
            (identity.site.to_string(), identity.occurrence, iteration)
        })
        .collect();
    let perform = embedder::main_site([1, 1, 0, 2, 0]).to_string();
    let sleep = embedder::main_site([1, 1, 0, 2, 1]).to_string();
    assert_eq!(
        named,
        [
            (perform.clone(), 0, 0),
            (sleep.clone(), 0, 0),
            (perform, 1, 1),
            (sleep, 1, 1),
        ]
    );
}

/// What a run of `text` ends with and prints under a 64 MiB bound, in
/// slices of `slice`, answering each park oldest first; collected at every
/// statement boundary when `collect`, and opened cold at every slice and
/// park when `reopens`.
fn verdict(text: &str, slice: u64, collect: bool, reopens: bool) -> (String, Vec<String>) {
    let bounds = Bounds {
        memory: 64 << 20,
        ..embedder::ROOMY
    };
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds,
            ..setup(slice)
        },
    );
    let mut hops = 0;
    let end = loop {
        if collect {
            embedder.machine.boundary = Some(Box::new(|_| crate::machine::Boundary::Collect));
        }
        let step = embedder.run();
        if let Step::Ended(end) = &step {
            break end_of(end);
        }
        if reopens {
            hops += 1;
            let (layout, native_twice) = hop(hops);
            if let Err(error) = embedder.try_reopen(layout, native_twice) {
                break format!("reopen refused: {error}");
            }
        }
        if matches!(step, Step::Parked(_)) {
            embedder.deliver_next();
        }
    };
    let printed = embedder
        .world
        .printed
        .iter()
        .map(|datum| format!("{datum:?}"))
        .collect();
    (end, printed)
}

/// ROOTS-CANON (`K-BND-003`, `K-EVAL-008`): a memory decision depends on
/// what the run can still reach, and a role keeps what it holds alive as a
/// variable would. A 1 MiB text is held as a role across a wait, a call, a
/// call that waits and a library body, each in a loop that writes it until
/// the bound refuses, and as a role of every frame of a recursion called
/// inside an expression. Each ends with the uninterrupted run's verdict at
/// the same statement, whatever the slicing, whether garbage is collected
/// at every boundary and whether the run is opened cold at every slice;
/// and the wait's verdict is that of the program with the wait hoisted
/// into a variable.
#[test]
fn the_memory_verdict_is_the_uninterrupted_runs_at_every_slice() {
    let held = super::verdict::MIB_TEXT;
    let writes = |operand: &str| {
        format!(
            "fn waits() {{ let w = perform echo(\"w\") as Any return w }}\n\
             fn one() {{ return 1 }}\n\
             main {{\n  {held}\n  let xs = []\n  let i = 0\n  while num.lt(i, 100) {{\n    \
             print i\n    {operand}\n    set i = num.add(i, 1)\n  }}\n  return list.len(xs)\n}}"
        )
    };
    let nested = writes("set xs[i] = [s, perform echo(\"w\") as Any][0]");
    let hoisted = writes("let w = perform echo(\"w\") as Any\n    set xs[i] = [s, w][0]");
    let programs = [
        nested.clone(),
        writes("set xs[i] = [s, call one()][0]"),
        writes("set xs[i] = [s, call waits()][0]"),
        writes("set xs[i] = [s, body.inc(1)][0]"),
        format!(
            "fn f(n, t) {{\n  print n\n  if num.lt(n, 100) {{ let m = num.add(n, 1) \
             let r = [t, call f(m, t)] return r[1] }}\n  return n\n}}\n\
             main {{\n  {held}\n  let r = [s, call f(0, s)]\n  return r[1]\n}}"
        ),
    ];
    for program in &programs {
        let (end, printed) = verdict(program, u64::MAX, false, false);
        assert!(end.contains("Memory"), "{end}: {program}");
        assert!(printed.len() > 10, "{printed:?}");
        for (slice, collect, reopens) in [
            (u64::MAX, true, false),
            (7, false, false),
            (1, false, false),
            (0, false, false),
            (7, true, false),
            (0, false, true),
            (7, true, true),
        ] {
            assert_eq!(
                verdict(program, slice, collect, reopens),
                (end.clone(), printed.clone()),
                "slice {slice}, collected {collect}, reopened {reopens}: {program}"
            );
        }
    }
    assert_eq!(
        verdict(&nested, u64::MAX, false, false),
        verdict(&hoisted, u64::MAX, false, false)
    );
}

/// Runs `source` at slice 0 until `want` accepts the phases its frames
/// stand at, and leaves the run there.
fn park_where(source: &str, want: impl Fn(&[(TaskId, usize, PhaseRecord)]) -> bool) -> Embedder {
    let mut embedder = Embedder::with(source, setup(0));
    for _ in 0..200 {
        let step = embedder.run();
        let phases = embedder.machine.pending_phases();
        if want(&phases) {
            return embedder;
        }
        match step {
            Step::Ended(_) => panic!("the run ended before {phases:?} matched"),
            Step::Slice => {}
            Step::Parked(_) => embedder.deliver_next(),
        }
    }
    panic!("the run never stood where it was wanted")
}

fn only(phase: PhaseRecord) -> Vec<(TaskId, usize, PhaseRecord)> {
    vec![(TaskId::MAIN, 0, phase)]
}

/// `main` parked at the nested `perform` of [`NESTED_AWAIT`].
fn waiting() -> Embedder {
    park_where(NESTED_AWAIT, |phases| phases == only(PhaseRecord::Waiting))
}

/// `main` standing at the nested `call a()`, its callee's frame above.
fn called() -> Embedder {
    park_where(NESTED_AWAIT, |phases| phases == only(PhaseRecord::Called))
}

/// Opens `embedder` cold under an odd layout and runs it on, answering
/// requests oldest first: the refusal it ends with.
fn refused(embedder: &mut Embedder) -> String {
    if let Err(error) = embedder.try_reopen(Layout(0x9e37_79b9 | 1), true) {
        return error;
    }
    loop {
        let step = match embedder.machine.run(&mut embedder.world, 0) {
            Ok(step) => step,
            Err(error) => return format!("{error:?}"),
        };
        if let Step::Parked(park) = &step {
            embedder.pending.extend(park.requests.iter().cloned());
        }
        match step {
            Step::Ended(end) => return format!("{end:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                let request = embedder.pending.remove(0);
                if let Err(error) = embedder
                    .machine
                    .deliver(embedder::wait_of(&request), embedder::answer(&request))
                {
                    return format!("{error:?}");
                }
            }
        }
    }
}

/// Edits the roles of `main`'s saved continuation.
fn edit_roles(embedder: &mut Embedder, edit: impl FnOnce(&mut Vec<RoleRecord>)) {
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, access| {
            let pending = pending.as_mut().expect("a saved continuation");
            let mut roles = pending.roles(access).unwrap();
            edit(&mut roles);
            *pending =
                PendingRecord::new(pending.site.clone(), pending.phase, roles, access).unwrap();
        })
        .unwrap();
}

/// CONT-GRAMMAR: an action site the statement does not hold.
#[test]
fn an_unknown_site_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            pending.as_mut().unwrap().site = vec![9, 9];
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("no nested action at"), "{problem}");
}

/// CONT-GRAMMAR: a call's phase saved at a wait.
#[test]
fn a_called_phase_at_a_wait_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            pending.as_mut().unwrap().phase = PhaseRecord::Called;
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(
        problem.contains("cannot stand at phase called"),
        "{problem}"
    );
}

/// CONT-GRAMMAR: a wait's phase saved at a call.
#[test]
fn a_waiting_phase_at_a_call_is_refused() {
    let mut embedder = called();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            pending.as_mut().unwrap().phase = PhaseRecord::Waiting;
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(
        problem.contains("cannot stand at phase waiting"),
        "{problem}"
    );
}

/// CONT-GRAMMAR: a continuation saved on a frame nothing resumes.
#[test]
fn a_continuation_on_a_fresh_frame_is_refused() {
    // After `let x`, the frame stands at `let r`, whose first nested
    // action is `call a()`, at suffix [0, 0].
    let mut embedder = park_where(NESTED_AWAIT, <[_]>::is_empty);
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, access| {
            *pending = Some(
                PendingRecord::new(vec![0, 0], PhaseRecord::Called, Vec::new(), access).unwrap(),
            );
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(
        problem.contains("cannot stand at phase called"),
        "{problem}"
    );
}

/// CONT-GRAMMAR: a role the action keeps is not saved.
#[test]
fn a_missing_role_is_refused() {
    let mut embedder = waiting();
    edit_roles(&mut embedder, |roles| {
        roles.remove(0);
    });
    let problem = refused(&mut embedder);
    assert!(problem.contains("is not saved"), "{problem}");
}

/// CONT-GRAMMAR: a saved role the action does not keep.
#[test]
fn an_extra_role_is_refused() {
    let mut embedder = waiting();
    edit_roles(&mut embedder, |roles| {
        roles.push(RoleRecord {
            site: vec![9],
            value: Value::Null,
        });
    });
    let problem = refused(&mut embedder);
    assert!(problem.contains("is no role"), "{problem}");
}

/// CONT-GRAMMAR: a saved wait on a task that a value, not its request,
/// resumes.
#[test]
fn a_task_state_that_disagrees_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_task_state(
            TaskId::MAIN,
            TaskStateRecord::Resuming {
                incoming: IncomingRecord::Value(Value::Null),
                requeue: false,
            },
        )
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("task's state disagrees"), "{problem}");
}

/// A continuation keeps up to `ROLE_FLAT` roles flat in its frame's row
/// and spills a longer list to a sequence of `ROLE_CHUNK`-role cells; a
/// statement that evaluated 16, 17 or 40 operands before its wait keeps
/// every one, and resumes exactly from a cold open at every slice.
#[test]
fn a_continuation_keeps_its_roles_flat_and_spills_a_long_list() {
    for (count, cells) in [(16, None), (17, Some(2)), (40, Some(3))] {
        let operands: String = (0..count).map(|i| format!("{i}, ")).collect();
        let source = format!(
            "main {{ let r = [{operands}perform echo(\"w\") as Any]\n\
             return list.len(r) }}\n"
        );
        let mut embedder = park_where(&source, |phases| phases == only(PhaseRecord::Waiting));
        let mut saved = None;
        embedder
            .machine
            .tamper_pending(TaskId::MAIN, 0, |pending, access| {
                let pending = pending.as_ref().unwrap();
                let shape = match &pending.roles {
                    RolesRecord::Flat(_) => None,
                    RolesRecord::Spilled(tree) => Some(tree.len()),
                };
                saved = Some((shape, pending.roles(access).unwrap().len()));
            })
            .unwrap();
        assert_eq!(saved, Some((cells, count)), "{count} roles");
        let (reference, _) = drive(&source, &["w"], Drive::new(u64::MAX, false));
        assert_eq!(
            reference.end,
            format!("Finished({:?})", int(count as i64 + 1))
        );
        for slice in [1, 0] {
            assert_eq!(drive(&source, &["w"], Drive::new(slice, true)).0, reference);
        }
    }
}

/// `live_roles` names exactly the roles each resume entry loads, for a
/// nested action under every expression form and statement position,
/// under layout 0 and an odd layout; and its refusals are typed.
#[test]
fn live_roles_match_every_resume_entry() {
    const POSITIONS: &str = "fn inner(p) { return [p] }\n\
        main {\n\
        let a = (perform echo(\"a\") as Any, 0)\n\
        let b = [perform echo(\"b\") as Any, 1]\n\
        let m = map{perform echo(\"mk\") as Any: perform echo(\"mv\") as Any}\n\
        let s = set{perform echo(\"s\") as Any}\n\
        let r = {f: perform echo(\"r\") as Any}\n\
        let i = b[perform echo(\"i\") as Any]\n\
        let n = call inner(perform echo(\"n\") as Any)\n\
        let p = perform echo([perform echo(\"p\") as Any]) as Any\n\
        let lib = list.len([perform echo(\"lib\") as Any])\n\
        let inv = invoke list.len([perform echo(\"inv\") as Any])\n\
        let cl = fn(x) { return [perform echo(\"cl\") as Any, x] }\n\
        let ap = [1, apply cl([perform echo(\"ap\") as Any][0])]\n\
        let t = [0]\n\
        set t = [perform echo(\"set\") as Any]\n\
        set t[0] = [1, perform echo(\"rhs\") as Any]\n\
        set t[body.inc(0)] = 2\n\
        remove t[body.inc(0)]\n\
        if [perform echo(\"if\") as Any][0] { let c = 0 }\n\
        for q in [perform echo(\"for\") as Any] { let d = q }\n\
        try { let u = [perform echo(\"try\") as Any] } catch e { let c = 0 }\n\
        let e = [if(true, perform echo(\"then\") as Any, 0), let(v = 1, [v, perform echo(\"in\") as Any])]\n\
        let sp = [1, spawn call inner(2)]\n\
        let sl = [2, sleep(1)]\n\
        return (a, perform echo(\"ret\") as Any)\n\
        }\n";
    for layout in [Layout::default(), Layout(0x9e37_79b9 | 1)] {
        let embedder = Embedder::with(
            POSITIONS,
            Setup {
                layout,
                ..setup(u64::MAX)
            },
        );
        let exe = embedder.machine.executable();
        let document = &embedder.program(true).0.document;
        let mut found = 0;
        for block in exe.blocks() {
            for (index, &stmt) in block.stmts.iter().enumerate() {
                let at = block.site.child(index as u32);
                let (stream, row, _) = exe.statement(stmt);
                for (site, act) in &stream.nested_at {
                    let entry = &stream.nested[act.ix()];
                    // The statement's own entries: a closure's statements
                    // have their own.
                    if site.unit != at.unit
                        || matches!(site.unit, lash_kernel_doc::Unit::Library(_))
                        || entry.depth as usize != at.path.len()
                        || !site.path.starts_with(&at.path)
                    {
                        continue;
                    }
                    // The entry resumes inside its statement's ops, right
                    // after the op that issues it.
                    let ops = if let crate::compile::Form::While { test, .. } = &row.form {
                        row.ops.len + test.len
                    } else {
                        row.ops.len
                    };
                    assert!(entry.resume <= ops, "{site}");
                    let issued = stream.ops[(row.ops.start + entry.resume - 1) as usize];
                    assert!(
                        matches!(
                            issued,
                            lash_kernel_exec::Op::Nested { act: named, .. } if named == *act
                        ) || matches!(issued, lash_kernel_exec::Op::Call { args, .. }
                                if args.start == entry.dst),
                        "{site}: {issued:?}"
                    );
                    let roles = lash_kernel_doc::live_roles(document, site)
                        .unwrap_or_else(|error| panic!("{site}: {error}"));
                    let loaded: Vec<lash_kernel_doc::Site> =
                        entry.roles.iter().map(|(role, _)| role.clone()).collect();
                    assert_eq!(roles, loaded, "at {site}");
                    found += 1;
                }
            }
        }
        assert!(found >= 28, "only {found} nested actions compiled");
        let site = |path: Vec<u32>| lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, path);
        assert_eq!(
            lash_kernel_doc::live_roles(document, &site(vec![0])),
            Err(lash_kernel_doc::LiveRolesError::NotAction)
        );
        assert_eq!(
            lash_kernel_doc::live_roles(document, &site(vec![999])),
            Err(lash_kernel_doc::LiveRolesError::NoSuchNode)
        );
    }
}

/// Positions an action cannot be nested in are refused with a typed
/// reason: a `while` condition and an assignment's place; and `yield`
/// and `cancel` stay statements.
#[test]
fn refused_positions_are_typed() {
    let library = embedder::library(true);
    for (text, reason) in [
        (
            "main { let x = 1\n  while [perform echo(\"w\") as Any][0] { let c = x } }",
            "a `while` condition",
        ),
        (
            "main { let t = [0]\n  set t[perform echo(\"i\") as Any] = 9 }",
            "an assignment's place",
        ),
        (
            "main { let t = [0]\n  set [perform echo(\"t\") as Any][0] = 9 }",
            "an assignment's place",
        ),
    ] {
        let text = format!("numbers by_spelling\n{}{text}", library.header);
        let document = lash_kernel_doc::parse_document(&text)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        let problem =
            lash_kernel_doc::validate_document(&document, library.registry.as_ref()).unwrap_err();
        let problem = format!("{problem}");
        assert!(
            problem.contains(&format!("an action cannot be nested in {reason}")),
            "`{text}` refused with {problem}"
        );
    }
    // A nested `join` is admitted (`nested_join_resumes_exactly`).
    for text in [
        "main { let r = [yield] }",
        "main { let h = spawn call f() let r = [cancel h] }",
    ] {
        let text = format!(
            "numbers by_spelling\n{}fn f() {{ return 1 }}\n{text}",
            library.header
        );
        let admitted = lash_kernel_doc::parse_document(&text)
            .map_err(|error| error.to_string())
            .and_then(|document| {
                lash_kernel_doc::validate_document(&document, library.registry.as_ref())
                    .map_err(|error| error.to_string())
            });
        assert!(admitted.is_err(), "`{text}` was admitted");
    }
}

/// A `join` on one handle and on a list, nested in an expression: the
/// frame stands joining, its operands kept as roles (`K-EVAL-008`).
const NESTED_JOIN: &str = "fn w(x) { let v = perform echo(x) as Any return v }\n\
    main { let h = spawn call w(\"a\") let g = spawn call w(\"b\") let hs = [h, g]\n\
    let r = [1, join(h), join all(hs)]\n\
    return r }\n";

const NESTED_JOIN_HOISTED: &str = "fn w(x) { let v = perform echo(x) as Any return v }\n\
    main { let h = spawn call w(\"a\") let g = spawn call w(\"b\") let hs = [h, g]\n\
    let t0 = join h\n\
    let t1 = join all hs\n\
    let r = [1, t0, t1]\n\
    return r }\n";

#[test]
fn nested_join_resumes_exactly() {
    let expected = list(&[int(1), text("a"), list(&[text("a"), text("b")])]);
    case(
        NESTED_JOIN,
        &["b", "a"],
        &expected,
        &[],
        &[PhaseRecord::Joining],
    );
    case(NESTED_JOIN_HOISTED, &["b", "a"], &expected, &[], &[]);
}

mod protocol;
