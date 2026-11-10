//! Continuation laws (`K-EVAL-008`): a frame that stands at an action
//! nested in its statement's expression keeps the operands its statement
//! evaluated as roles, named by their sites; a resume entry loads them
//! in whatever machine the run is opened in; and the run leaves what a
//! run that never parked leaves, at every slice and every reopen.
//!
//! - CONT-EXACT: each case runs uninterrupted, then at slices MAX, 1 and 0,
//!   sealed and opened cold at every slice and park under alternating
//!   registries, and leaves the same end, prints, requests,
//!   charge.
//! - CONT-ONCE: what comes before the action is neither run nor charged
//!   again: the charge and effects are the uninterrupted run's.
//! - CONT-GRAMMAR: a saved continuation the grammar does not admit is
//!   refused, naming its site.
//! - ROOTS-CANON: the memory verdict is the uninterrupted run's at every
//!   slice and reopen. Plans may observe the bound one declared slack apart
//!   (`K-BND-003`); each refusing write here holds 1 MiB, far past it, so
//!   every plan refuses at the same statement.
//! - SITE-NESTED: a nested action's wait is named by its own site.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, FunctionRegistry, TaskId, parse_definition};

use lash_kernel_state::paged::{
    ContinuationPhase, ContinuationRecord, HeaderRecord, IncomingRecord, RoleRecord, RolesRecord,
    TaskStateRecord,
};
use lash_kernel_value::names::FunctionId;

use crate::machine::kernel::{RestoreRefusal, SitePath};
use lash_kernel_value::sites::Site;

use super::embedder::{self, Embedder, Setup, label};
use crate::{Bounds, End, Machine, Request, Step};

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
/// it handed out, every read-bound `perform` it made, each effect's
/// origin, and its charge.
#[derive(Debug, PartialEq)]
struct Trace {
    /// The run sealed at each park, and the carry it ended with, encoded.
    checkpoints: Vec<Vec<u8>>,
    end: String,
    printed: Vec<String>,
    requests: Vec<String>,
    performed: Vec<String>,
    /// The origin of each effect request, then of each read, in order.
    origins: Vec<Site>,
    charged: u64,
}

fn end_of(end: &End) -> String {
    match end {
        // A finished run compares by its result: its carried root names
        // pages a reopened run sealed on the way.
        End::Finished(finished) => format!("Finished({:?})", finished.result),
        other => format!("{other:?}"),
    }
}

/// Whether `pair.twice` runs natively in the registry a run is opened in
/// at hop `hop`: it alternates between its native and its body.
fn native_twice(hop: u64) -> bool {
    hop.is_multiple_of(2)
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
    /// The entry the run starts on, with its one argument, or `main`.
    entry: Option<(&'static str, i64)>,
    /// `echo` bound as a read (`K-EFF-012`).
    reads: bool,
    cleanup: bool,
    /// A reopening run is also sealed and opened cold after each delivery,
    /// before the delivered task consumes its outcome.
    reopen_delivered: bool,
    /// At its `n`th park the run is asked to cancel, and nothing more is
    /// delivered.
    cancel_at: Option<usize>,
}

impl Drive {
    fn new(slice: u64, reopens: bool) -> Self {
        Self {
            slice,
            reopens,
            bounds: embedder::ROOMY,
            probes: bodies,
            entry: None,
            reads: false,
            cleanup: false,
            reopen_delivered: false,
            cancel_at: None,
        }
    }
}

/// Where a run's frames stood at its reopens: the phases of nested
/// actions and the headers of protocol loops; and how many `perform`s its
/// generated plans made.
#[derive(Default)]
struct Seen {
    phases: Vec<ContinuationPhase>,
    headers: Vec<HeaderRecord>,
    parks: usize,
    reopens: u64,
}

/// Runs `text`, answering parks with the requests `order` names and then
/// oldest first. Gives the trace and where its frames stood at each
/// reopen.
fn drive(text: &str, order: &[&str], drive: Drive) -> (Trace, Seen) {
    let mut setup = Setup {
        bounds: drive.bounds,
        reads: drive.reads,
        cleanup: drive.cleanup,
        probes: Some(drive.probes),
        ..setup(drive.slice)
    };
    if let Some((entry, arg)) = drive.entry {
        setup.start.target = crate::Target::Entry(lash_kernel_value::names::Name::new(entry));
        setup.start.args = vec![int(arg)];
    }
    if drive.reads {
        setup.start.modes = crate::EffectModes::new();
    }
    let mut embedder = match Embedder::try_with(text, setup) {
        Ok(embedder) => embedder,
        Err(error) => {
            let trace = Trace {
                checkpoints: Vec::new(),
                end: format!("start refused: {error:?}"),
                printed: Vec::new(),
                requests: Vec::new(),
                performed: Vec::new(),
                origins: Vec::new(),
                charged: 0,
            };
            return (trace, Seen::default());
        }
    };
    let mut seen = Seen::default();
    let mut order = order.iter();
    let mut hops = 0;
    let mut parks = 0;
    let mut checkpoints = Vec::new();
    let end = loop {
        let step = match embedder.machine.run(&mut embedder.world, drive.slice) {
            Ok(step) => step,
            Err(error) => break format!("machine refused: {error:?}"),
        };
        if let Step::Parked(park) = &step {
            checkpoints.push(lash_kernel_value::tokens::encode_body(
                &embedder.checkpoint(),
            ));
            embedder.requests.extend(park.requests.iter().cloned());
            embedder.pending.extend(park.requests.iter().cloned());
        }
        if let Step::Ended(end) = &step {
            if let End::Finished(finished) = end
                && let Some(carry) = &finished.carry
            {
                checkpoints.push(lash_kernel_value::tokens::encode_body(carry));
            }
            break end_of(end);
        }
        if drive.reopens {
            hops += 1;
            if let Err(error) = embedder.try_reopen(native_twice(hops)) {
                break format!("reopen refused: {error}");
            }
        }
        if matches!(step, Step::Parked(_)) {
            parks += 1;
            if drive.cancel_at == Some(parks) {
                embedder.world.cancel = true;
                continue;
            }
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
            if drive.reopens && drive.reopen_delivered {
                hops += 1;
                if let Err(error) = embedder.try_reopen(native_twice(hops)) {
                    break format!("reopen refused after delivery: {error}");
                }
            }
        }
    };
    seen.parks = parks;
    seen.reopens = hops;
    let trace = Trace {
        checkpoints,
        end,
        printed: embedder
            .world
            .printed
            .iter()
            .map(|datum| format!("{datum:?}"))
            .collect(),
        requests: embedder.requests.iter().map(|r| format!("{r:?}")).collect(),
        performed: embedder
            .world
            .performed
            .iter()
            .map(|read| format!("{read:?}"))
            .collect(),
        origins: embedder
            .requests
            .iter()
            .map(|request| match request {
                Request::Effect(effect) => effect.origin.clone(),
            })
            .chain(embedder.world.performed.iter().map(|(_, _, at)| at.clone()))
            .collect(),
        charged: embedder.machine.meters().charged.expect("exact charge"),
    };
    (trace, seen)
}

/// CONT-EXACT and CONT-ONCE of one case: the uninterrupted run gives
/// `expected` and prints `printed`; every sliced and reopened run leaves
/// its trace; the reopened runs see each phase of `want`.
fn case(
    text: &str,
    order: &[&str],
    expected: &Datum,
    printed: &[Datum],
    want: &[ContinuationPhase],
) {
    let phases = exact(Drive::new(u64::MAX, false), text, order, expected, printed).phases;
    for phase in want {
        assert!(phases.contains(phase), "the reopens saw {phases:?}: {text}");
    }
}

/// The run of `text` under `base`, uninterrupted, gives `expected` and
/// prints `printed`, and every sliced and reopened run leaves its trace.
/// Gives where the reopened runs' frames stood.
fn exact(base: Drive, text: &str, order: &[&str], expected: &Datum, printed: &[Datum]) -> Seen {
    let (reference, seen) = everywhere(base, text, order);
    assert_eq!(reference.end, format!("Finished({expected:?})"), "{text}");
    let printed: Vec<String> = printed.iter().map(|datum| format!("{datum:?}")).collect();
    assert_eq!(reference.printed, printed, "{text}");
    seen
}

/// The uninterrupted run of `text` under `base`, after checking that every
/// sliced and reopened run leaves its trace; and where the reopened runs'
/// frames stood.
fn everywhere(base: Drive, text: &str, order: &[&str]) -> (Trace, Seen) {
    let at = |slice, reopens| Drive {
        slice,
        reopens,
        ..base
    };
    let (reference, _) = drive(text, order, at(u64::MAX, false));
    let mut seen = Seen::default();
    for slice in [u64::MAX, 1, 0] {
        let (sliced, _) = drive(text, order, at(slice, false));
        assert_eq!(sliced, reference, "slice {slice}: {text}");
        let (reopened, at_reopens) = drive(text, order, at(slice, true));
        assert_eq!(reopened, reference, "reopened at slice {slice}: {text}");
        seen.phases.extend(at_reopens.phases);
        seen.headers.extend(at_reopens.headers);
    }
    (reference, seen)
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
const NESTED_AWAIT: &str = "definition \"function a() -> Any\\nkernel 1\\nmetering statements\\nbody {  print \\\"a\\\" return 1  }\"\n\ndefinition \"function c() -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(\\\"c\\\") as Any print \\\"c\\\" return v  }\"\n\ndefinition \"function observe(p: Any, q: Any, r: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  print (p, q, r) return [p, q, r]  }\"\n\nmain { let x = \"tool\"\nlet r = call observe(call a(), perform 0(x) as Any, call c())\nreturn r }\n";

/// [`NESTED_AWAIT`] with each action a statement of its own.
const NESTED_AWAIT_HOISTED: &str = "definition \"function a() -> Any\\nkernel 1\\nmetering statements\\nbody {  print \\\"a\\\" return 1  }\"\n\ndefinition \"function c() -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(\\\"c\\\") as Any print \\\"c\\\" return v  }\"\n\ndefinition \"function observe(p: Any, q: Any, r: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  print (p, q, r) return [p, q, r]  }\"\n\nmain { let x = \"tool\"\nlet t0 = call a()\nlet t1 = perform 0(x) as Any\nlet t2 = call c()\nlet r = call observe(t0, t1, t2)\nreturn r }\n";

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
        &[ContinuationPhase::Call { phase: 0 }, STANDING],
    );
    let (_, crossed) = drive(NESTED_AWAIT, &["tool", "c"], Drive::new(u64::MAX, true));
    assert!(
        crossed.parks >= 2,
        "CONT-CROSS: both nested waits must park"
    );
    assert!(crossed.reopens >= 2, "CONT-CROSS: both parks must reopen");
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
const MUTATED_RECEIVER: &str = "definition \"function writer(o: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  do perform 0(\\\"w\\\") as Any set o.x = 2 return null  }\"\n\nmain { let o = {x: 1}\nlet h = spawn call writer(o)\nlet r = [o.x, perform 0(\"m\") as Any]\nlet done = join h\nreturn (r, o.x) }\n";

const MUTATED_RECEIVER_HOISTED: &str = "definition \"function writer(o: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  do perform 0(\\\"w\\\") as Any set o.x = 2 return null  }\"\n\nmain { let o = {x: 1}\nlet h = spawn call writer(o)\nlet t0 = o.x\nlet t1 = perform 0(\"m\") as Any\nlet r = [t0, t1]\nlet done = join h\nreturn (r, o.x) }\n";

#[test]
fn parked_role_keeps_the_evaluated_value() {
    let expected = tuple(&[list(&[int(1), text("m")]), int(2)]);
    case(MUTATED_RECEIVER, &["w", "m"], &expected, &[], &[STANDING]);
    case(MUTATED_RECEIVER_HOISTED, &["w", "m"], &expected, &[], &[]);
}

/// A callback that spawns, inside a call inside a call inside an
/// expression.
const CALLBACK: &str = "definition \"function work(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = [perform 0(x) as Any] return v[0]  }\"\n\ndefinition \"function collect(f: Any, xs: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out  }\"\n\ndefinition \"function gather(hs: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let rs = join all hs return rs  }\"\n\ndefinition \"function tag(t: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  print t return t  }\"\n\nmain { let r = (call tag(\"a\"),\ncall gather(call collect(fn(x) { let h = spawn call work(x) return h }, [1, 2])),\ncall tag(\"b\"))\nreturn r }\n";

const CALLBACK_HOISTED: &str = "definition \"function work(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(x) as Any return v  }\"\n\ndefinition \"function collect(f: Any, xs: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out  }\"\n\ndefinition \"function gather(hs: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let rs = join all hs return rs  }\"\n\ndefinition \"function tag(t: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  print t return t  }\"\n\nmain { let t0 = call tag(\"a\")\nlet f = fn(x) { let h = spawn call work(x) return h }\nlet xs = [1, 2]\nlet t1 = call collect(f, xs)\nlet t2 = call gather(t1)\nlet t3 = call tag(\"b\")\nlet r = (t0, t2, t3)\nreturn r }\n";

#[test]
fn callback_inside_expression_resumes_exactly() {
    let expected = tuple(&[text("a"), list(&[int(1), int(2)]), text("b")]);
    case(
        CALLBACK,
        &["echo", "echo"],
        &expected,
        &[text("a"), text("b")],
        &[ContinuationPhase::Call { phase: 0 }, STANDING],
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
const SPAWNED: &str = "definition \"function work(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  print x let v = perform 0(x) as Any return v  }\"\n\ndefinition \"function other() -> Any\\nkernel 1\\nmetering statements\\nbody {  print \\\"other\\\" return 0  }\"\n\nmain { let o = spawn call other()\nlet hs = [body.say(\"before\"), spawn call work(\"child\"), body.say(\"after\")]\nlet h = hs[1]\nlet r = join h\nlet d = join o\nreturn (hs[0], r, hs[2]) }\n";

const SPAWNED_HOISTED: &str = "definition \"function work(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  print x let v = perform 0(x) as Any return v  }\"\n\ndefinition \"function other() -> Any\\nkernel 1\\nmetering statements\\nbody {  print \\\"other\\\" return 0  }\"\n\nmain { let o = spawn call other()\nlet t0 = body.say(\"before\")\nlet h = spawn call work(\"child\")\nlet t2 = body.say(\"after\")\nlet r = join h\nlet d = join o\nreturn (t0, r, t2) }\n";

#[test]
fn a_spawn_inside_an_expression_resumes_exactly() {
    let expected = tuple(&[text("before"), text("child"), text("after")]);
    let printed = [text("other"), text("before"), text("child"), text("after")];
    case(
        SPAWNED,
        &["child"],
        &expected,
        &printed,
        &[
            STANDING,
            Embedder::with(SPAWNED, setup(0))
                .machine
                .source_call_phase(&embedder::main_site([1, 0, 0])),
        ],
    );
    case(SPAWNED_HOISTED, &["child"], &expected, &printed, &[]);
}

/// Library bodies called inside expressions: in a `while` condition, in
/// an assignment's index while the right-hand side waits in register 0,
/// beside `if` and `let` expressions, and `pair.twice`, whose native
/// answers on alternate reopens.
const BODIES: &str = "main { let xs = [9, 0, 0, 0]\nlet i = 0\nwhile num.lt(body.inc(i), 4) {\nset xs[body.inc(i)] = [i, let(j = body.inc(i), if(num.lt(j, 2), body.say(j), j))]\nset i = body.inc(i)\n}\nreturn (xs, pair.twice(body.inc(1)), [i, perform 0(\"w\") as Any]) }\n";

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
        &[
            Embedder::with(BODIES, setup(0))
                .machine
                .source_call_phase(&embedder::main_site([2, 0, 0])),
            STANDING,
        ],
    );
}

/// A `while` whose condition holds a library body, a wait and a call that
/// waits, each nested (`K-EVAL-008`): the condition is evaluated afresh at
/// every header, a park inside it resumes in that evaluation, and the loop
/// runs its `else` on exhaustion and skips it on `break` (`K-FORM-013`).
const WHILE_CONDITION: &str = "definition \"function more(i: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(i) as Any return num.lt(v, 2)  }\"\n\nmain { let i = 0\nlet seen = []\nwhile [body.say(i), perform 0(\"w\") as Any, call more(i)][2] {\nset seen[list.len(seen)] = i\nset i = body.inc(i)\n} else { print \"exhausted\" }\nlet j = 0\nwhile [perform 0(\"b\") as Any, call more(j)][1] {\nset j = body.inc(j)\nif num.lt(0, j) { break }\n} else { print \"never\" }\nreturn (i, seen, j) }\n";

const WHILE_CONDITION_HOISTED: &str = "definition \"function more(i: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(i) as Any return num.lt(v, 2)  }\"\n\nmain { let i = 0\nlet seen = []\nlet t0 = body.say(i)\nlet t1 = perform 0(\"w\") as Any\nlet go = call more(i)\nwhile go {\nset seen[list.len(seen)] = i\nset i = body.inc(i)\nset t0 = body.say(i)\nset t1 = perform 0(\"w\") as Any\nset go = call more(i)\n} else { print \"exhausted\" }\nlet j = 0\nlet u0 = perform 0(\"b\") as Any\nlet more_j = call more(j)\nwhile more_j {\nset j = body.inc(j)\nif num.lt(0, j) { break }\nset u0 = perform 0(\"b\") as Any\nset more_j = call more(j)\n} else { print \"never\" }\nreturn (i, seen, j) }\n";

#[test]
fn while_condition_actions_resume_exactly() {
    let expected = tuple(&[int(2), list(&[int(0), int(1)]), int(1)]);
    let printed = [int(0), int(1), int(2), text("exhausted")];
    case(
        WHILE_CONDITION,
        &[],
        &expected,
        &printed,
        &[ContinuationPhase::Call { phase: 0 }, STANDING],
    );
    case(WHILE_CONDITION_HOISTED, &[], &expected, &printed, &[]);
}

/// An assignment's place holds actions in its target and its index
/// (`K-EVAL-003`): value first, the right-hand side waits and then the
/// place is evaluated, the value held as a role; place first, the target
/// and the index are evaluated, then the right-hand side waits holding
/// them as roles. A park between the place and the write evaluates none
/// of it again.
const PLACES: &str = "definition \"function key(k: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(k) as Any return v  }\"\n\ndefinition \"function same(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return x  }\"\n\nmain { let t = [0, 0, 0]\nlet r = {f: 0}\nset [body.say(\"target\"), t][1][[perform 0(2) as Any, call key(1)][1]] = [body.say(\"value\"), perform 0(\"v\") as Any][1]\nset [body.say(\"first target\"), t][1][[perform 0(0) as Any, call key(2)][1]] = [body.say(\"first value\"), perform 0(\"w\") as Any][1] place first\nset t[0] = perform 0(\"x\") as Any place first\nset [call same(r)][0].f = call key(\"f\") place first\nreturn (t, r) }\n";

const PLACES_HOISTED: &str = "definition \"function key(k: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(k) as Any return v  }\"\n\ndefinition \"function same(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return x  }\"\n\nmain { let t = [0, 0, 0]\nlet r = {f: 0}\nlet a0 = body.say(\"value\")\nlet a1 = perform 0(\"v\") as Any\nlet a2 = body.say(\"target\")\nlet a3 = perform 0(2) as Any\nlet a4 = call key(1)\nset [a2, t][1][[a3, a4][1]] = [a0, a1][1]\nlet b0 = body.say(\"first target\")\nlet b1 = perform 0(0) as Any\nlet b2 = call key(2)\nlet b3 = body.say(\"first value\")\nlet b4 = perform 0(\"w\") as Any\nset [b0, t][1][[b1, b2][1]] = [b3, b4][1]\nlet c0 = perform 0(\"x\") as Any\nset t[0] = c0\nlet d0 = call same(r)\nlet d1 = call key(\"f\")\nset [d0][0].f = d1\nreturn (t, r) }\n";

#[test]
fn assignment_place_actions_resume_exactly() {
    let expected = tuple(&[
        list(&[text("x"), text("v"), text("w")]),
        Datum::Record(vec![("f".to_string(), text("f"))]),
    ]);
    let printed = [
        text("value"),
        text("target"),
        text("first target"),
        text("first value"),
    ];
    case(
        PLACES,
        &[],
        &expected,
        &printed,
        &[ContinuationPhase::Call { phase: 0 }, STANDING],
    );
    case(PLACES_HOISTED, &[], &expected, &printed, &[]);
}

/// `K-EVAL-003`: which of a place's target and an action in the value is
/// evaluated first is the assignment's order. The value's callee
/// rebinds the target's variable and waits; value first, the write goes
/// to the rebound list, place first, to the one the place read before.
#[test]
fn an_assignments_order_decides_what_its_place_reads() {
    let program = |order: &str| {
        format!(
            "main {{ let a = [0]\nlet b = [0]\nlet u = a\nlet swap = fn() {{ set u = b let x = perform 0(\"s\") as Any return x }}\nset u[0] = apply swap(){order}\nreturn (a, b) }}\n"
        )
    };
    let written = list(&[text("s")]);
    let untouched = list(&[int(0)]);
    case(
        &program(""),
        &[],
        &tuple(&[untouched.clone(), written.clone()]),
        &[],
        &[],
    );
    case(
        &program(" place first"),
        &[],
        &tuple(&[written, untouched]),
        &[],
        &[ContinuationPhase::Call { phase: 0 }],
    );
}

/// CONT-GRAMMAR at the new sites: a frame parked in a `while` condition,
/// or in a place-first assignment's value with its target and index as
/// roles, refuses a restore that drops a role.
#[test]
fn a_missing_role_at_a_condition_or_a_place_is_refused() {
    for source in [
        "main { let i = 0\nwhile [i, perform 0(false) as Any][1] { set i = 1 }\nreturn i }\n",
        "main { let t = [0]\nset t[0] = [1, perform 0(\"w\") as Any][1] place first\nreturn t }\n",
    ] {
        let mut embedder = park_where(source, |embedder, _| {
            embedder.machine.source_action_site().is_some()
        });
        edit_roles(&mut embedder, |roles| {
            assert!(!roles.is_empty(), "{source}");
            let index = roles
                .iter()
                .position(|role| {
                    matches!(
                        role.coordinate,
                        lash_kernel_state::paged::RoleCoordinate::Source {
                            kind: lash_kernel_state::kernel_internals::logical::SlotKind::Operand,
                            ..
                        }
                    )
                })
                .expect("an evaluated operand role");
            roles.remove(index);
        });
        refused(&mut embedder, "canonical source roles differ");
    }
}

/// `K-EVAL-004`: a nested call reads a callee held in a variable before its
/// arguments and keeps it as a role while they run, so an argument that
/// assigns the variable changes the next call's callee, not this one's.
#[test]
fn a_nested_call_reads_its_callee_before_its_arguments() {
    const SOURCE: &str = "definition \"function g(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return [\\\"g\\\", x]  }\"\n\ndefinition \"function h(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return [\\\"h\\\", x]  }\"\n\nmain { let f = &g\nlet swap = fn() { set f = &h let w = perform 0(\"w\") as Any return w }\nlet r = [apply f(apply swap()), apply f(2)]\nreturn r }\n";
    let expected = list(&[list(&[text("g"), text("w")]), list(&[text("h"), int(2)])]);
    case(
        SOURCE,
        &["w"],
        &expected,
        &[],
        &[ContinuationPhase::Call { phase: 0 }],
    );
}

/// A task cancelled while it waits at a nested `perform` resumes at its
/// request with the raise, at the action (`K-TASK-017`).
#[test]
fn cancel_at_a_nested_wait_resumes_exactly() {
    const SOURCE: &str = "definition \"function work(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = [perform 0(x) as Any] return v[0]  }\"\n\nmain { let h = spawn call work(0)\ndo cancel h\nlet r = \"none\"\ntry { let v = join h\nset r = v } catch e { set r = e.kind }\nreturn r }\n";
    case(SOURCE, &[], &text("cancelled"), &[], &[STANDING]);
}

/// SITE-NESTED (`K-EFF-008`, `K-SITE-002`): each wait of a statement that
/// holds several is named by its own action's site, counted per site and
/// loop iteration.
#[test]
fn each_nested_wait_is_named_by_its_own_site() {
    const SOURCE: &str = "main { let out = []\nfor i in [0, 1] { set out[list.len(out)] = (perform 0(\"a\") as Any, perform 0(1) as Any) }\nreturn out }\n";
    let mut embedder = Embedder::with(SOURCE, setup(u64::MAX));
    embedder.run_to_end(&[]);
    let named: Vec<(String, u64, u64)> = embedder
        .requests
        .iter()
        .map(|request| {
            let identity = match request {
                Request::Effect(effect) => &effect.identity,
            };
            let identity = lash_kernel_value::kernel_internals::effect_parts(identity);
            let iteration = identity.loops.first().map_or(99, |at| at.iteration);
            (identity.site.to_string(), identity.occurrence, iteration)
        })
        .collect();
    let perform = embedder::main_site([1, 1, 0, 2, 0]).to_string();
    let second = embedder::main_site([1, 1, 0, 2, 1]).to_string();
    assert_eq!(
        named,
        [
            (perform.clone(), 0, 0),
            (second.clone(), 0, 0),
            (perform, 1, 1),
            (second, 1, 1),
        ]
    );
}

/// K-EFF-008: one task counts a function's effect across sequential and
/// recursive calls, independent of frames and cold reopens.
#[test]
fn sequential_and_recursive_calls_keep_task_wide_effect_identities() {
    const SOURCE: &str = "definition \"function work(self: Any, n: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(n) as Any\\nif num.lt(0, n) { let r = apply self(self, num.add(n, -1)) return r }\\nreturn v  }\"\n\nmain { let a = call work(&work, 2) let b = call work(&work, 1) return b }\n";
    let mut warm = Embedder::with(SOURCE, setup(u64::MAX));
    warm.run_to_end(&[]);
    let expected: Vec<_> = (0..5)
        .map(|occurrence| lash_kernel_value::sites::EffectIdentity {
            task: lash_kernel_value::sites::TaskIdentity::Main,
            site: embedder::function_site("work", [0, 0]),
            occurrence,
            loops: Vec::new(),
        })
        .collect();
    let identities = |requests: &[Request]| {
        requests
            .iter()
            .map(|request| {
                let Request::Effect(effect) = request;
                lash_kernel_value::kernel_internals::effect_parts(&effect.identity).clone()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(identities(&warm.requests), expected);
    let mut cold = Embedder::with(SOURCE, setup(u64::MAX));
    let mut checkpoints = Vec::new();
    loop {
        match cold.run() {
            Step::Parked(_) => {
                checkpoints.push(cold.machine.semantic_checkpoint());
                cold.reopen(true);
                assert_eq!(
                    cold.machine.semantic_checkpoint(),
                    *checkpoints.last().unwrap()
                );
                cold.deliver_next();
            }
            Step::Ended(end) => {
                assert_eq!(embedder::result(end), int(0));
                break;
            }
            Step::Slice => panic!("unbounded slice"),
        }
    }
    assert_eq!(checkpoints.len(), 5, "CONT-CROSS: five calls reopened");
    assert_eq!(identities(&cold.requests), expected);
    assert_eq!(
        cold.machine.meters().charged.expect("exact charge"),
        warm.machine.meters().charged.expect("exact charge")
    );
    let uninterrupted = drive(SOURCE, &[], Drive::new(u64::MAX, false)).0;
    let reopened = drive(SOURCE, &[], Drive::new(u64::MAX, true)).0;
    assert_eq!(uninterrupted, reopened);

    // More suspended tasks than the coordinate cache can retain: their
    // unprojected counts still belong to each task across eviction and reload.
    let numbers = (0..70).map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let source = format!(
        "{} main {{ let hs = [] for i in [{numbers}] {{
        let h = spawn call work(&work, 1) set hs[list.len(hs)] = h
        }} do join all hs return 0 }}",
        SOURCE.split("main {").next().unwrap()
    );
    let mut warm = Embedder::with(&source, setup(u64::MAX));
    assert_eq!(embedder::result(warm.run_to_end(&[])), int(0));
    let mut by_task = BTreeMap::<_, Vec<_>>::new();
    for identity in identities(&warm.requests) {
        by_task
            .entry(identity.task)
            .or_default()
            .push(identity.occurrence);
    }
    assert_eq!(by_task.len(), 70);
    assert!(by_task.values().all(|counts| counts == &[0, 1]));
    let mut cold = Embedder::with(&source, setup(u64::MAX));
    let mut hops = 0;
    loop {
        match cold.run() {
            Step::Parked(_) => {
                hops += 1;
                cold.reopen(true);
                cold.deliver_next();
            }
            Step::Ended(end) => {
                assert_eq!(embedder::result(end), int(0));
                break;
            }
            Step::Slice => panic!("unbounded slice"),
        }
    }
    assert!(
        hops >= 2,
        "CONT-CROSS: both waits across all 70 tasks reopened"
    );
    assert_eq!(identities(&cold.requests), identities(&warm.requests));
    assert_eq!(
        cold.machine.meters().charged.expect("exact charge"),
        warm.machine.meters().charged.expect("exact charge")
    );
}

/// SITE-NESTED, `K-SITE-004`: a wait in a `while` condition is named by
/// its own site, counted at each evaluation of the condition, and inside
/// its loop, as the derived execution site names it.
#[test]
fn a_wait_in_a_while_condition_is_named_inside_its_loop() {
    const SOURCE: &str = "main { let i = 0\nwhile [perform 0(\"c\") as Any, num.lt(i, 2)][1] { set i = num.add(i, 1) }\nreturn i }\n";
    let mut embedder = Embedder::with(SOURCE, setup(u64::MAX));
    embedder.run_to_end(&[]);
    let named: Vec<(String, u64, Vec<String>)> = embedder
        .requests
        .iter()
        .map(|request| {
            let Request::Effect(effect) = request;
            let identity = lash_kernel_value::kernel_internals::effect_parts(&effect.identity);
            let loops = identity
                .loops
                .iter()
                .map(|at| at.site.to_string())
                .collect();
            (identity.site.to_string(), identity.occurrence, loops)
        })
        .collect();
    let perform = embedder::main_site([1, 0, 0, 0]).to_string();
    let inside = vec![embedder::main_site([1]).to_string()];
    assert_eq!(
        named,
        [
            (perform.clone(), 0, inside.clone()),
            (perform.clone(), 1, inside.clone()),
            (perform, 2, inside),
        ]
    );
}

/// Runs `source` at slice 0 until `want` accepts the phases its frames
/// stand at, and leaves the run there.
fn park_where(
    source: &str,
    want: impl Fn(&mut Embedder, &[(TaskId, usize, ContinuationPhase)]) -> bool,
) -> Embedder {
    let mut embedder = Embedder::with(source, setup(0));
    for _ in 0..200 {
        let step = embedder.run();
        let phases = embedder.machine.pending_phases();
        if want(&mut embedder, &phases) {
            embedder.reopen(true);
            assert_eq!(
                embedder.machine.restore_refusal(TaskId::MAIN, 0).unwrap(),
                None
            );
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

/// A frame standing at an action whose Enter is accepted and whose result
/// is not yet delivered: a spawn, a wait, a join or a yield.
const STANDING: ContinuationPhase = ContinuationPhase::Source {
    phase: 0,
    progress: 2,
};

fn only(phase: ContinuationPhase) -> Vec<(TaskId, usize, ContinuationPhase)> {
    vec![(TaskId::MAIN, 0, phase)]
}

/// `main` parked at the nested `perform` of [`NESTED_AWAIT`].
fn waiting() -> Embedder {
    park_where(NESTED_AWAIT, |embedder, _| {
        embedder.machine.source_action_site() == Some(vec![1, 0, 1])
    })
}

/// A run of [`NESTED_AWAIT`] at its first park.
fn parked() -> Embedder {
    let mut embedder = Embedder::with(NESTED_AWAIT, setup(0));
    for _ in 0..200 {
        match embedder.run() {
            Step::Parked(_) => return embedder,
            Step::Slice => {}
            Step::Ended(end) => panic!("the run ended unparked: {end:?}"),
        }
    }
    panic!("the run never parked")
}

/// `main` standing at the nested `call a()`, its callee's frame above.
fn called() -> Embedder {
    let phase = Embedder::with(NESTED_AWAIT, setup(0))
        .machine
        .source_call_phase(&embedder::main_site([1, 0, 0]));
    park_where(NESTED_AWAIT, |embedder, _| {
        embedder
            .machine
            .source_pending(TaskId::MAIN, 0)
            .is_some_and(|pending| pending.site == [1, 0, 0] && pending.phase == phase)
    })
}

/// The cold row is edited after sealing, so the independent frame decoder
/// reads the forged continuation before any publication can reject its ledger.
fn refused(embedder: &mut Embedder, named: &str) {
    let refusal = match embedder.machine.restore_refusal(TaskId::MAIN, 0) {
        Ok(Some(refusal)) => format!("continuation refused: {refusal}"),
        Err(fault) => fault,
        Ok(None) => panic!("the grammar accepted the corrupted continuation"),
    };
    assert!(refusal.contains(named), "{refusal} names no {named}");
    let problem = run_out(embedder);
    assert!(problem.contains(named), "{problem} names no {named}");
}

/// The refusal the grammar gives `main`'s bottom frame as `embedder`
/// stands, cold: the run, run on and answered oldest first, ends with that
/// refusal once it reaches the frame.
fn refused_here(embedder: &mut Embedder) -> RestoreRefusal {
    let refusal = embedder
        .machine
        .restore_refusal(TaskId::MAIN, 0)
        .expect("the frame row reads")
        .expect("the grammar refuses the frame");
    let problem = run_out(embedder);
    let named = format!("continuation refused: {refusal}");
    assert!(problem.contains(&named), "{problem} names no {named}");
    refusal
}

/// How the run ends as `embedder` stands, run on and answered oldest first.
fn run_out(embedder: &mut Embedder) -> String {
    loop {
        let step = match embedder.machine.run(&mut embedder.world, 0) {
            Ok(step) => step,
            Err(error) => break format!("{error:?}"),
        };
        if let Step::Parked(park) = &step {
            embedder.pending.extend(park.requests.iter().cloned());
        }
        match step {
            Step::Ended(end) => break format!("{end:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                let request = embedder.pending.remove(0);
                if let Err(error) = embedder
                    .machine
                    .deliver(embedder::wait_of(&request), embedder::answer(&request))
                {
                    break format!("{error:?}");
                }
            }
        }
    }
}

fn path<const N: usize>(path: [u32; N]) -> SitePath {
    SitePath(path.into())
}

/// Edits the roles of `main`'s saved continuation.
fn edit_roles(embedder: &mut Embedder, edit: impl FnOnce(&mut Vec<RoleRecord>)) {
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, access| {
            let pending = pending.as_mut().expect("a saved continuation");
            let mut roles = pending.roles(access).unwrap();
            edit(&mut roles);
            // Only the roles change; the rest of the continuation stands.
            pending.roles =
                ContinuationRecord::new(pending.site.clone(), pending.phase, roles, access)
                    .unwrap()
                    .roles;
        })
        .unwrap();
}

/// A page source the open must not touch.
struct Untouched;

impl lash_kernel_state::paged::PageSource for Untouched {
    fn read(
        &mut self,
        _: &[lash_kernel_doc::heap::PageRef],
    ) -> Result<
        Vec<Result<lash_kernel_state::paged::PageAnswer, lash_kernel_doc::heap::HeapFault>>,
        lash_kernel_doc::heap::HeapFault,
    > {
        panic!("the open read a page of a root it refuses")
    }
    fn seal(
        &mut self,
        _: lash_kernel_state::paged::PageBody,
    ) -> Result<lash_kernel_doc::heap::PageRef, lash_kernel_doc::heap::HeapFault> {
        panic!("the open wrote a page for a root it refuses")
    }
}

/// CONT-FORMAT (contract §9): a root its envelope states under another
/// parked-state format is refused with that format, before the open reads
/// or writes a page; the same root stated under the format the build reads
/// opens. A witness is a version number or a domain string (the page digest
/// domain changes every page's identity), and either refuses alike.
#[test]
fn a_root_stated_under_another_format_is_refused() {
    use lash_kernel_state::paged::{FormatValue, StatedFormat};
    let mut embedder = parked();
    let root = embedder.checkpoint();
    let root = lash_kernel_state::kernel_internals::checkpoint(root);
    let cases = [
        (
            "KERNEL_PARKED_STATE_VERSION",
            FormatValue::Number(16),
            FormatValue::Number(15),
        ),
        (
            "PAGE_DIGEST_DOMAIN",
            FormatValue::Text("successor-domain".into()),
            FormatValue::Text("predecessor-domain".into()),
        ),
    ];
    for (surface, expected, observed) in cases {
        let foreign = StatedFormat {
            surface,
            expected: expected.clone(),
            observed: observed.clone(),
        };
        let opened = crate::KernelMachine::open(
            embedder.program(true).0,
            embedder.bounds,
            &root.clone().stated(foreign),
            Box::new(Untouched),
        );
        assert_eq!(
            opened.err(),
            Some(crate::OpenError::Format {
                surface,
                expected: expected.clone(),
                observed,
            }),
            "{surface}"
        );
        let current = StatedFormat {
            surface,
            expected: expected.clone(),
            observed: expected,
        };
        assert!(
            crate::KernelMachine::open(
                embedder.program(true).0,
                embedder.bounds,
                &root.clone().stated(current),
                Box::new(embedder.source.clone()),
            )
            .is_ok(),
            "{surface}"
        );
    }
}

/// CONT-RETIRED (contract §9): state the paged engine wrote is refused
/// typed, as retired, before any of it is interpreted: its parked run,
/// whose root names the paged heap's header, and its carried
/// session, whose root is the paged engine's record.
#[test]
fn the_paged_engines_state_is_refused_as_retired() {
    use lash_kernel_doc::heap::HeapFault;
    use lash_kernel_state::kernel_internals::PagedHeap;
    use lash_kernel_state::paged::{
        CarryRoots, EditEpoch, HeapLifecycle as _, KeyedRoot, SemanticHeap as _, SessionRoot,
    };
    use lash_kernel_value::tokens::{RetiredState, TokenError, decode_body, encode_body};
    let mut embedder = parked();
    let mut heap = PagedHeap::fresh(Box::new(embedder.source.clone()), EditEpoch(1));
    let session = heap
        .carry(CarryRoots {
            bindings: KeyedRoot::Empty,
            functions: KeyedRoot::Empty,
        })
        .unwrap();
    assert_eq!(
        decode_body::<SessionRoot>(&encode_body(&session)).err(),
        Some(TokenError::Retired(RetiredState::Session))
    );
    let header = session.pages()[0];
    let mut root = embedder.checkpoint();
    root.heap = header;
    let opened = crate::KernelMachine::open(
        embedder.program(true).0,
        embedder.bounds,
        &lash_kernel_state::kernel_internals::checkpoint(root),
        Box::new(embedder.source.clone()),
    );
    assert_eq!(
        opened.err(),
        Some(crate::OpenError::Heap(HeapFault::Retired {
            page: header.id,
            state: RetiredState::ParkedRun,
        }))
    );
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
    refused(
        &mut embedder,
        "canonical source continuation is not admitted",
    );
}

/// CONT-GRAMMAR: a call's phase saved at a wait.
#[test]
fn a_called_phase_at_a_wait_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            let pending = pending.as_mut().unwrap();
            let ContinuationPhase::Source { phase, .. } = pending.phase else {
                panic!("a standing region wait")
            };
            pending.phase = ContinuationPhase::Call { phase };
        })
        .unwrap();
    refused(
        &mut embedder,
        "canonical source continuation is not admitted",
    );
}

/// CONT-GRAMMAR: a wait's phase saved at a call.
#[test]
fn a_waiting_phase_at_a_call_is_refused() {
    let mut embedder = called();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            let pending = pending.as_mut().unwrap();
            let ContinuationPhase::Call { phase } = pending.phase else {
                panic!("a suspended region call")
            };
            pending.phase = ContinuationPhase::Source { phase, progress: 2 };
        })
        .unwrap();
    refused(
        &mut embedder,
        "canonical source continuation is not admitted",
    );
}

/// CONT-GRAMMAR: a continuation saved on a frame nothing resumes.
#[test]
fn a_continuation_on_a_fresh_frame_is_refused() {
    let mut saved = None;
    called()
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            saved = pending.clone();
        })
        .unwrap();
    // The same root invocation, before it has entered that call.
    let mut embedder = park_where(NESTED_AWAIT, |embedder, _| {
        embedder
            .machine
            .source_pending(TaskId::MAIN, 0)
            .is_some_and(|pending| pending.site == [0] && pending.unpaid.is_some())
    });
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| *pending = saved)
        .unwrap();
    refused(&mut embedder, "cannot stand at Called");
}

/// CONT-GRAMMAR: a saved role the action does not keep.
#[test]
fn an_extra_role_is_refused() {
    let mut embedder = waiting();
    edit_roles(&mut embedder, |roles| {
        roles.push(RoleRecord {
            coordinate: lash_kernel_state::paged::RoleCoordinate::Source {
                kind: lash_kernel_state::kernel_internals::logical::SlotKind::Operand,
                role: lash_kernel_state::kernel_internals::logical::SourceRole::Operand(
                    embedder::main_site([0, 0]),
                ),
            },
            value: lash_kernel_doc::heap::PageValue::Null,
        });
    });
    refused(&mut embedder, "canonical source roles differ");
}

/// CONT-GRAMMAR: a role the action keeps, saved twice.
#[test]
fn a_role_saved_twice_is_refused() {
    let mut embedder = waiting();
    edit_roles(&mut embedder, |roles| roles.push(roles[0].clone()));
    refused(&mut embedder, "source continuation duplicates a frame role");
}

/// Q4: a saved region role names one slot of its body by kind and full
/// source discriminator. Two saved roles under one slot, which is what a
/// non-injective RoleKey table would write, and a role its body does not
/// key under that kind, refuse the reopen as malformed.
#[test]
fn a_slot_two_saved_roles_share_or_its_body_does_not_key_is_refused() {
    use lash_kernel_state::kernel_internals::logical::SlotKind;
    use lash_kernel_state::paged::RoleCoordinate;
    type Edit = fn(&mut Vec<RoleRecord>);
    let cases: [(&str, Edit); 2] = [
        ("source continuation duplicates a frame role", |roles| {
            let twice = roles
                .iter()
                .find(|role| matches!(role.coordinate, RoleCoordinate::Source { .. }))
                .expect("a saved source role")
                .clone();
            roles.push(twice);
        }),
        (
            "source continuation role is not a role of its body",
            |roles| {
                let RoleCoordinate::Source { kind, .. } = &mut roles
                    .iter_mut()
                    .find(|role| matches!(role.coordinate, RoleCoordinate::Source { .. }))
                    .expect("a saved source role")
                    .coordinate
                else {
                    unreachable!()
                };
                *kind = match kind {
                    SlotKind::Control => SlotKind::Departure,
                    _ => SlotKind::Control,
                };
            },
        ),
    ];
    for (named, edit) in cases {
        // `main` parked at the nested `perform`, keeping the operands it
        // evaluated; cold, so the edit reaches its saved row.
        let mut embedder = Embedder::with(NESTED_AWAIT, setup(0));
        loop {
            let step = embedder.run();
            let saved = embedder.machine.pending_roles();
            if let [(TaskId::MAIN, 0, roles)] = &saved[..]
                && roles.len() > 1
                && embedder.machine.pending_phases() == only(STANDING)
            {
                break;
            }
            match step {
                Step::Ended(end) => panic!("the run ended at {end:?} before it parked"),
                Step::Slice => {}
                Step::Parked(_) => embedder.deliver_next(),
            }
        }
        embedder.try_reopen(true).expect("the run reopens");
        edit_roles(&mut embedder, edit);
        let problem = run_out(&mut embedder);
        assert!(problem.contains(named), "{problem} names no {named}");
    }
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
                incoming: IncomingRecord::Value(lash_kernel_doc::heap::PageValue::Null),
                requeue: false,
            },
        )
        .unwrap();
    embedder.reopen(true);
    assert_eq!(
        refused_here(&mut embedder),
        RestoreRefusal::TaskState {
            action: path([1, 0, 1])
        }
    );
}

/// CONT-GRAMMAR: a frame waiting in the body of a `for` over a list,
/// opened cold, refuses a saved loop header moved past its cursor (to a
/// check, which only a guarded table walk holds) and a saved loop moved
/// to a site no loop of the frame's statement stands at.
#[test]
fn a_loop_moved_past_its_cursor_or_off_its_site_is_refused() {
    const IN_LOOP: &str = "main { let out = []\nfor x in [1, 2, 3] { let v = perform 0(x) as Any set out[list.len(out)] = v }\nreturn out }";
    let parked = || {
        let mut embedder = Embedder::with(IN_LOOP, setup(u64::MAX));
        assert!(matches!(embedder.run(), Step::Parked(_)));
        embedder.reopen(true);
        embedder
    };
    let mut embedder = parked();
    embedder
        .machine
        .tamper_loop(TaskId::MAIN, 0, |record, _| {
            record.header = HeaderRecord::Check;
        })
        .unwrap();
    refused(
        &mut embedder,
        "canonical source continuation is not admitted",
    );
    let mut embedder = parked();
    embedder
        .machine
        .tamper_loop(TaskId::MAIN, 0, |record, access| {
            let moved = Site::new(lash_kernel_value::sites::Unit::Main, [9, 9]);
            record.site = lash_kernel_state::paged::SiteRef::new(&moved, access).unwrap();
        })
        .unwrap();
    refused(
        &mut embedder,
        "canonical source continuation is not admitted",
    );
}

/// A continuation keeps up to `ROLE_FLAT` roles flat in its frame's row
/// and spills a longer list to a sequence of `ROLE_CHUNK`-role cells; a
/// statement that evaluated 16, 17 or 40 operands before its wait keeps
/// every one, and resumes exactly from a cold open at every slice.
#[test]
fn a_continuation_keeps_its_roles_flat_and_spills_a_long_list() {
    for count in [15usize, 16, 17, 40] {
        // The wait's argument is live beside the list's evaluated operands.
        let roles = count + 1;
        let cells = (roles > lash_kernel_state::paged::ROLE_FLAT)
            .then_some(roles.div_ceil(lash_kernel_state::paged::ROLE_CHUNK) as u64);
        let operands: String = (0..count).map(|i| format!("{i}, ")).collect();
        let source =
            format!("main {{ let r = [{operands}perform 0(\"w\") as Any]\nreturn list.len(r) }}\n");
        let mut embedder = park_where(&source, |embedder, _| {
            embedder.machine.source_action_site().is_some()
        });
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
        assert_eq!(
            saved,
            Some((cells, roles)),
            "{count} operands and the wait argument"
        );
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

/// A `join` on one handle and on a list, nested in an expression: the
/// frame stands joining, its operands kept as roles (`K-EVAL-008`).
const NESTED_JOIN: &str = "definition \"function w(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(x) as Any return v  }\"\n\nmain { let h = spawn call w(\"a\") let g = spawn call w(\"b\") let hs = [h, g]\nlet r = [1, join(h), join all(hs)]\nreturn r }\n";

const NESTED_JOIN_HOISTED: &str = "definition \"function w(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(x) as Any return v  }\"\n\nmain { let h = spawn call w(\"a\") let g = spawn call w(\"b\") let hs = [h, g]\nlet t0 = join h\nlet t1 = join all hs\nlet r = [1, t0, t1]\nreturn r }\n";

#[test]
fn nested_join_resumes_exactly() {
    let expected = list(&[int(1), text("a"), list(&[text("a"), text("b")])]);
    case(NESTED_JOIN, &["b", "a"], &expected, &[], &[STANDING]);
    case(NESTED_JOIN_HOISTED, &["b", "a"], &expected, &[], &[]);
}

mod body_performs;
mod protocol;

/// FIG-6471 / CONT-EXACT: do discards only the final value; nested calls,
/// waits, yields and computed cancellation keep their evaluation and resume order.
#[test]
fn do_expressions_resume_after_each_action_without_repeating_operands() {
    case(
        r#"definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  do perform 0(\"child\") as Any return null  }"

definition "function idle() -> Any\nkernel 1\nmetering statements\nbody {  do yield return null  }"

main {
  do call child()
  let yielded = [yield]
  print yielded
  do [1, yield, perform 0("x") as Any]
  try { do let(h = spawn call idle(), (cancel(h), join(h))) } catch e {}
  return "done"
}"#,
        &["child", "x"],
        &text("done"),
        &[list(&[Datum::Null])],
        &[ContinuationPhase::Call { phase: 0 }, STANDING],
    );
}

/// A return policy is an ordinary body that may wait. Its print makes
/// the instant adoption starts observable through the public machine.
fn return_policy(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut probes = bodies(registry, ids);
    let definition = parse_definition("function body.adopt(x: Any) -> Any\nkernel 1\ncharge 0\nbody { print \"adopt\" let value = join x return value }").unwrap();
    probes.push(("body.adopt", registry.register(definition, None).unwrap()));
    probes
}

/// K-FORM-015, K-FORM-017: evaluate now, then unwind, then adopt.
#[test]
fn a_return_policy_runs_after_source_finalizers() {
    let source = r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  return 7  }"

definition "function value() -> Any\nkernel 1\nmetering statements\nbody {  print \"evaluated\" return spawn call child()  }"

definition "function work() -> Any\nkernel 1\nmetering statements\nbody { \n  try { return call value() via body.adopt }\n  finally { print \"finally\" }\n }"

main { let value = call work() return value }
"#;
    let seen = exact(
        Drive {
            probes: return_policy,
            ..Drive::new(u64::MAX, false)
        },
        source,
        &[],
        &int(7),
        &[text("evaluated"), text("finally"), text("adopt")],
    );
    // Some reopen stood inside the waiting policy, its frame at the policy
    // call.
    assert!(
        seen.phases.contains(&ContinuationPhase::Call { phase: 13 }),
        "the reopens saw {:?}",
        seen.phases
    );
}

/// The policies below: one raises, one calls the closure it is given.
fn policies(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut probes = bodies(registry, ids);
    for (name, text) in [
        (
            "body.reject",
            "function body.reject(x: Any) -> Any\nkernel 1\ncharge 0\nbody { print \"reject\" throw \"rejected\" }",
        ),
        (
            "body.call",
            "function body.call(x: Any) -> Any\nkernel 1\ncharge 0\nbody { print \"call\" let value = apply x() return value }",
        ),
    ] {
        let definition = parse_definition(text).unwrap();
        probes.push((name, registry.register(definition, None).unwrap()));
    }
    probes
}

/// K-FORM-017: the policy runs once the frame's handlers are gone, so what
/// it raises leaves the frame past its own catch, after its finally.
#[test]
fn a_return_policy_raise_leaves_its_frame() {
    let source = r#"
definition "function work() -> Any\nkernel 1\nmetering statements\nbody { \n  try { return 5 via body.reject }\n  catch e { return \"inside\" }\n  finally { print \"finally\" }\n }"

main { let r = "none" try { let v = call work() set r = v } catch e { set r = e } return r }
"#;
    exact(
        Drive {
            probes: policies,
            ..Drive::new(u64::MAX, false)
        },
        source,
        &[],
        &text("rejected"),
        &[text("finally"), text("reject")],
    );
}

/// K-FORM-017: the frame's bindings are gone when its policy runs; a
/// closure over one keeps the binding's cell.
#[test]
fn a_return_policy_calls_a_closure_over_a_released_binding() {
    let source = r#"
definition "function work() -> Any\nkernel 1\nmetering statements\nbody { \n  let n = 41\n  let f = fn() { return num.add(n, 1) }\n  try { return f via body.call }\n  finally { print \"finally\" }\n }"

main { let value = call work() return value }
"#;
    exact(
        Drive {
            probes: policies,
            ..Drive::new(u64::MAX, false)
        },
        source,
        &[],
        &int(42),
        &[text("finally"), text("call")],
    );
}

/// K-FORM-017: a finalizer's own return drops the held value and policy.
#[test]
fn a_finalizer_return_discards_the_interrupted_return_policy() {
    let source = r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  return 7  }"

definition "function work(held: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  try { return held via body.adopt }\n  finally { print \"override\" return 9 }\n }"

main { let held = spawn call child() let value = call work(held) do join held return value }
"#;
    exact(
        Drive {
            probes: return_policy,
            ..Drive::new(u64::MAX, false)
        },
        source,
        &[],
        &int(9),
        &[text("override")],
    );
}

/// K-TASK-017, K-FORM-017: cancellation at a cleanup wait replaces
/// the held return; no adoption call starts on the cancelled path.
#[test]
fn cancellation_during_unwind_discards_the_return_policy() {
    let source = r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  return 7  }"

definition "function work(held: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  try { return held via body.adopt }\n  finally { print \"cleanup\" do perform 0(\"cleanup\") as Any }\n }"

main {
  let held = spawn call child()
  let worker = spawn call work(held)
  do cancel worker
  let outcome = null
  try { do join worker } catch e { set outcome = e.kind }
  do join held
  return outcome
}
"#;
    exact(
        Drive {
            probes: return_policy,
            ..Drive::new(u64::MAX, false)
        },
        source,
        &[],
        &text("cancelled"),
        &[text("cleanup")],
    );
}

/// K-FORM-015, CONT-EXACT, CONT-ONCE: a held return survives a
/// cleanup park, then its policy can park too, without repeating either.
#[test]
fn a_return_policy_adopts_once_after_a_cold_cleanup_park() {
    let source = r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  do perform 0(\"value\") as Any return 7  }"

definition "function value() -> Any\nkernel 1\nmetering statements\nbody {  print \"evaluated\" return spawn call child()  }"

definition "function work() -> Any\nkernel 1\nmetering statements\nbody { \n  try { return call value() via body.adopt }\n  finally { do perform 0(\"cleanup\") as Any print \"cleaned\" }\n }"

main { let value = call work() return value }
"#;
    // CONT-EXACT: the return held across cleanup came through an active
    // scalar frame with a nested call; neither adoption nor cleanup repeats.
    let source = source.replace(
        "return 7",
        "let leaf = fn(n) { let a = num.add(n, 1) set a = num.add(a, 1) return a } return apply leaf(5)",
    );
    exact(
        Drive {
            probes: return_policy,
            ..Drive::new(u64::MAX, false)
        },
        &source,
        &["cleanup", "value"],
        &int(7),
        &[text("evaluated"), text("cleaned"), text("adopt")],
    );
}

/// A run-root cleanup probe that waits for the tasks still running, as
/// `tasks.cancel_all` does, then `ends`.
fn cleanup_probe(
    name: &'static str,
    ends: &str,
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut probes = bodies(registry, ids);
    let definition = parse_definition(&format!(
        "function {name}() -> Any\nkernel 1\ncharge 0\nuse tasks.unfinished = @{}\n\
         body {{ print \"clean\" let hs = invoke tasks.unfinished() let rs = join all hs {ends} }}",
        ids["tasks.unfinished"]
    ))
    .unwrap();
    probes.push((name, registry.register(definition, None).unwrap()));
    probes
}

/// The entry `run`, started with `n`, under the run-root cleanup
/// `body.clean`, which returns a value the run discards.
fn cleaned(n: i64) -> Drive {
    Drive {
        cleanup: true,
        probes: |registry, ids| cleanup_probe("body.clean", "return \"discarded\"", registry, ids),
        entry: Some(("run", n)),
        ..Drive::new(u64::MAX, false)
    }
}

/// K-TASK-019, CONT-EXACT, CONT-ONCE: an entry's root cleanup runs once,
/// as the run's root frame leaves and after its `finally` blocks, never
/// for a nested call of the entry's function; its result is discarded and
/// the held return ends the run, across a park in the cleanup reopened
/// cold.
#[test]
fn a_root_cleanup_runs_once_after_source_finalizers_and_resumes_exactly() {
    let source = r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  do perform 0(\"child\") as Any print \"child\"  }"

definition "function run(self: Any, n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  if num.lt(0, n) { let inner = apply self(self, 0) print \"inner\" return inner }\n  try { let h = spawn call child() return 7 }\n  finally { print \"finally\" }\n }"

main { }
"#;
    exact(
        cleaned(1),
        source,
        &[],
        &int(7),
        &[text("finally"), text("inner"), text("clean"), text("child")],
    );
}

/// K-TASK-019: a cleanup holds the body's raise and re-raises it, and its
/// own raise replaces what it holds, at every slice and cold reopen.
#[test]
fn a_root_cleanup_holds_the_raise_and_its_own_raise_replaces_it() {
    let raises = r#"
definition "function child() -> Any\nkernel 1\nmetering statements\nbody {  do perform 0(\"child\") as Any print \"child\"  }"

definition "function run(n: Any) -> Any\nkernel 1\nmetering statements\nbody {  let h = spawn call child() throw \"held\"  }"

main { }
"#;
    let printed = [
        format!("{:?}", text("clean")),
        format!("{:?}", text("child")),
    ];
    let (trace, _) = everywhere(cleaned(0), raises, &[]);
    assert!(trace.end.contains("held"), "{}", trace.end);
    assert_eq!(trace.printed, printed);
    let (trace, _) = everywhere(
        Drive {
            probes: |registry, ids| cleanup_probe("body.clean", "throw \"cleanup\"", registry, ids),
            ..cleaned(0)
        },
        raises,
        &[],
    );
    assert!(
        trace.end.contains("cleanup") && !trace.end.contains("held"),
        "{}",
        trace.end
    );
    assert_eq!(trace.printed, printed);
}
