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

use lash_kernel_doc::{Datum, FunctionRegistry, parse_definition};

use lash_kernel_state::paged::run::continuation::{
    PointPhase, RunRecord, RunResume, RunTaskState, UnitKind,
};
use lash_kernel_value::names::FunctionId;

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

/// A checkpoint's continuation and object graph, without its ids.
type Logical = (Vec<Vec<u8>>, Vec<Vec<u8>>);

fn logical(checkpoint: &lash_kernel_state::semantic::Checkpoint) -> Logical {
    (
        checkpoint.continuation().to_vec(),
        checkpoint.graph().to_vec(),
    )
}

/// What a run leaves observable: its end, what it printed, every request
/// it handed out, every read-bound `perform` it made, each effect's
/// origin, and its charge.
#[derive(Debug, PartialEq)]
struct Trace {
    /// The run sealed at each park, and the carry it ended with, as their
    /// object graphs: ids depend on seal history and are not compared
    /// (contract §6, ObjectIds).
    checkpoints: Vec<Logical>,
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

/// Where a run's frames stood at its reopens, and how many parks and
/// reopens it made.
#[derive(Default)]
struct Seen {
    stood: Vec<Stood>,
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
            checkpoints.push(logical(&embedder.machine.semantic_checkpoint()));
            embedder.requests.extend(park.requests.iter().cloned());
            embedder.pending.extend(park.requests.iter().cloned());
        }
        if let Step::Ended(end) = &step {
            if let End::Finished(finished) = end
                && let Some(carry) = &finished.carry
            {
                checkpoints.push(logical(
                    &lash_kernel_state::semantic::session(carry, Box::new(embedder.source.clone()))
                        .expect("the carry reads back"),
                ));
            }
            break end_of(end);
        }
        if drive.reopens {
            hops += 1;
            seen.stood.extend(stood(&embedder.run_record()));
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
fn case(text: &str, order: &[&str], expected: &Datum, printed: &[Datum], want: &[Stand]) {
    let stood = exact(Drive::new(u64::MAX, false), text, order, expected, printed).stood;
    for stand in want {
        assert!(
            stood.iter().any(|at| stand.holds(at)),
            "the reopens saw no {stand:?} in {stood:?}: {text}"
        );
    }
}

/// One frame of a sealed run: its unit's kind, the site and phase of its
/// point, how many operands it holds there, and whether a frame stands
/// above it.
#[derive(Clone, Debug)]
struct Stood {
    kind: UnitKind,
    site: Site,
    phase: PointPhase,
    operands: usize,
    under: bool,
}

/// Every frame of every task in `record`.
fn stood(record: &RunRecord) -> Vec<Stood> {
    record
        .tasks
        .iter()
        .flat_map(|task| {
            let top = task.frames.len().saturating_sub(1);
            task.frames
                .iter()
                .enumerate()
                .map(move |(at, frame)| Stood {
                    kind: frame.unit.kind,
                    site: frame.site.clone(),
                    phase: frame.phase,
                    operands: frame.operands.len(),
                    under: at < top,
                })
        })
        .collect()
}

/// Where a reopen must have found a frame standing (`K-EVAL-008`).
#[derive(Clone, Copy, Debug)]
enum Stand {
    /// At a call nested in its statement's expression, its callee's frame
    /// above.
    Called,
    /// `main`'s frame at the call at this site of `main`, its callee above.
    CalledAt(&'static [u32]),
    /// Suspended inside its statement's expression, at an action of its
    /// own or under a callee, holding the operands the statement evaluated
    /// before it.
    Holding,
    /// A function body's frame at a call, its callee's frame above.
    BodyCalled,
    /// `main`'s frame at a call, its callee's frame above: a loop calling
    /// its protocol's hook.
    MainCalled,
    /// `main`'s top frame waiting at an action of its own.
    MainWaits,
}

impl Stand {
    fn holds(self, at: &Stood) -> bool {
        match self {
            Self::Called => at.phase == PointPhase::Call && at.under,
            Self::CalledAt(path) => {
                at.kind == UnitKind::Main
                    && at.site == Site::new(lash_kernel_value::sites::Unit::Main, path.to_vec())
                    && at.phase == PointPhase::Call
                    && at.under
            }
            Self::Holding => {
                at.operands > 0
                    && ((at.phase == PointPhase::Issued && !at.under)
                        || (at.phase == PointPhase::Call && at.under))
            }
            Self::MainCalled => {
                at.kind == UnitKind::Main && at.phase == PointPhase::Call && at.under
            }
            Self::MainWaits => {
                at.kind == UnitKind::Main && at.phase == PointPhase::Issued && !at.under
            }
            Self::BodyCalled => {
                at.kind != UnitKind::Main && at.phase == PointPhase::Call && at.under
            }
        }
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
        seen.stood.extend(at_reopens.stood);
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
        &[Stand::Called, Stand::Holding],
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
    case(
        MUTATED_RECEIVER,
        &["w", "m"],
        &expected,
        &[],
        &[Stand::Holding],
    );
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
        &[Stand::Called, Stand::Holding],
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
        &[Stand::Holding, Stand::CalledAt(&[1, 0, 0])],
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
    case(BODIES, &["w"], &expected, &[int(1)], &[Stand::Holding]);
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
        &[Stand::Called, Stand::Holding],
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
        &[Stand::Called, Stand::Holding],
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
        &[Stand::Called],
    );
}

/// CONT-GRAMMAR at the new sites: a frame parked in a `while` condition,
/// or in a place-first assignment's value with its target and index held,
/// refuses an open that drops an evaluated operand: a frame holds exactly
/// the stack its point's resume entry names.
#[test]
fn a_missing_role_at_a_condition_or_a_place_is_refused() {
    for source in [
        "main { let i = 0\nwhile [i, perform 0(false) as Any][1] { set i = 1 }\nreturn i }\n",
        "main { let t = [0]\nset t[0] = [1, perform 0(\"w\") as Any][1] place first\nreturn t }\n",
    ] {
        let mut embedder = park_where(source, waits_in_main);
        refused(
            &mut embedder,
            |record| {
                let top = main_top(record);
                assert!(!top.operands.is_empty(), "{source}");
                top.operands.pop();
            },
            "another stack",
        );
    }
}

/// `K-EVAL-004`: a nested call reads a callee held in a variable before its
/// arguments and keeps it as a role while they run, so an argument that
/// assigns the variable changes the next call's callee, not this one's.
#[test]
fn a_nested_call_reads_its_callee_before_its_arguments() {
    const SOURCE: &str = "definition \"function g(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return [\\\"g\\\", x]  }\"\n\ndefinition \"function h(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  return [\\\"h\\\", x]  }\"\n\nmain { let f = &g\nlet swap = fn() { set f = &h let w = perform 0(\"w\") as Any return w }\nlet r = [apply f(apply swap()), apply f(2)]\nreturn r }\n";
    let expected = list(&[list(&[text("g"), text("w")]), list(&[text("h"), int(2)])]);
    case(SOURCE, &["w"], &expected, &[], &[Stand::Called]);
}

/// A task cancelled while it waits at a nested `perform` resumes at its
/// request with the raise, at the action (`K-TASK-017`).
#[test]
fn cancel_at_a_nested_wait_resumes_exactly() {
    const SOURCE: &str = "definition \"function work(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = [perform 0(x) as Any] return v[0]  }\"\n\nmain { let h = spawn call work(0)\ndo cancel h\nlet r = \"none\"\ntry { let v = join h\nset r = v } catch e { set r = e.kind }\nreturn r }\n";
    case(SOURCE, &[], &text("cancelled"), &[], &[Stand::MainWaits]);
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

/// Runs `source` at slice 0 until `want` accepts its sealed record, and
/// leaves the run there, reopened cold.
fn park_where(source: &str, want: impl Fn(&RunRecord) -> bool) -> Embedder {
    let mut embedder = Embedder::with(source, setup(0));
    for _ in 0..200 {
        let step = embedder.run();
        if !matches!(step, Step::Ended(_)) && want(&embedder.run_record()) {
            embedder.reopen(true);
            return embedder;
        }
        match step {
            Step::Ended(end) => panic!("the run ended at {end:?} before it stood as wanted"),
            Step::Slice => {}
            Step::Parked(_) => embedder.deliver_next(),
        }
    }
    panic!("the run never stood where it was wanted")
}

/// `main`'s top frame.
fn main_top(record: &mut RunRecord) -> &mut lash_kernel_state::paged::run::continuation::RunFrame {
    record.tasks[0].frames.last_mut().expect("main has a frame")
}

/// `main`'s top frame waits at an action of `main`.
fn waits_in_main(record: &RunRecord) -> bool {
    record.tasks[0]
        .frames
        .last()
        .is_some_and(|top| top.unit.kind == UnitKind::Main && top.phase == PointPhase::Issued)
}

/// `main` parked at the nested `perform` of [`NESTED_AWAIT`].
fn waiting() -> Embedder {
    park_where(NESTED_AWAIT, |record| {
        waits_in_main(record) && record.tasks[0].frames[0].site == embedder::main_site([1, 0, 1])
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
    park_where(NESTED_AWAIT, |record| {
        stood(record)
            .iter()
            .any(|at| Stand::CalledAt(&[1, 0, 0]).holds(at))
    })
}

/// CONT-GRAMMAR: `embedder`'s run, sealed with `edit` made to its saved
/// record, refuses to open, naming `named`.
fn refused(embedder: &mut Embedder, edit: impl FnOnce(&mut RunRecord), named: &str) {
    let refusal = embedder
        .reopen_edited(edit)
        .expect_err("the edited record opened");
    assert!(refusal.contains(named), "{refusal} names no {named}");
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
    // The frame under the callee: a site at a wait is refused earlier, by
    // admission, as no wait point.
    let mut embedder = called();
    refused(
        &mut embedder,
        |record| record.tasks[0].frames[0].site = embedder::main_site([9, 9]),
        "saves no point",
    );
}

/// CONT-GRAMMAR: a call's phase saved at a wait: a wait's task stands
/// issued at its perform.
#[test]
fn a_called_phase_at_a_wait_is_refused() {
    let mut embedder = waiting();
    refused(
        &mut embedder,
        |record| main_top(record).phase = PointPhase::Call,
        "WaitPoint",
    );
}

/// CONT-GRAMMAR: a wait's phase saved at a call: a frame under another
/// stands at its call.
#[test]
fn a_waiting_phase_at_a_call_is_refused() {
    let mut embedder = called();
    refused(
        &mut embedder,
        |record| record.tasks[0].frames[0].phase = PointPhase::Issued,
        "SuspendedCall",
    );
}

/// CONT-GRAMMAR: an operand the point does not hold.
#[test]
fn an_extra_role_is_refused() {
    let mut embedder = waiting();
    refused(
        &mut embedder,
        |record| {
            let top = main_top(record);
            let first = top.operands[0];
            top.operands.push(first);
        },
        "another stack",
    );
}

/// CONT-GRAMMAR: a slot the frame holds, saved twice.
#[test]
fn a_role_saved_twice_is_refused() {
    let mut embedder = waiting();
    refused(
        &mut embedder,
        |record| {
            let top = main_top(record);
            assert!(top.slots.len() > 1, "main holds two slots");
            top.slots[1] = top.slots[0].clone();
        },
        "twice",
    );
}

/// Q4: a saved slot names one slot of its unit by its declaring site and
/// role; a slot its unit does not declare refuses the open (two saved
/// under one: [`a_role_saved_twice_is_refused`]).
#[test]
fn a_slot_two_saved_roles_share_or_its_body_does_not_key_is_refused() {
    let mut embedder = waiting();
    refused(
        &mut embedder,
        |record| main_top(record).slots[0].site = vec![9, 9],
        "declares no slot",
    );
}

/// CONT-GRAMMAR: a saved wait on a task that a value, not its request,
/// resumes.
#[test]
fn a_task_state_that_disagrees_is_refused() {
    let mut embedder = waiting();
    refused(
        &mut embedder,
        |record| {
            record.tasks[0].state = RunTaskState::Ready(RunResume::Value(0));
            record.ready.push(0);
        },
        "OrphanWait",
    );
}

/// CONT-GRAMMAR: a frame waiting in the body of a `for` over a list,
/// opened cold, refuses its loop's walk and cursor moved to a site no
/// loop of the frame's statement stands at.
#[test]
fn a_loop_moved_off_its_site_is_refused() {
    const IN_LOOP: &str = "main { let out = []\nfor x in [1, 2, 3] { let v = perform 0(x) as Any set out[list.len(out)] = v }\nreturn out }";
    let mut embedder = Embedder::with(IN_LOOP, setup(u64::MAX));
    assert!(matches!(embedder.run(), Step::Parked(_)));
    embedder.reopen(true);
    refused(
        &mut embedder,
        |record| {
            for slot in &mut main_top(record).slots {
                if matches!(
                    slot.role,
                    lash_kernel_state::paged::run::continuation::SlotRole::Loop(_)
                ) {
                    slot.site = vec![9, 9];
                }
            }
        },
        "declares no slot",
    );
}

/// A statement that evaluated 15, 16, 17 or 40 operands before its wait
/// keeps every one, and resumes exactly from a cold open at every slice.
#[test]
fn a_continuation_keeps_every_operand_a_long_statement_evaluated() {
    for count in [15usize, 16, 17, 40] {
        let operands: String = (0..count).map(|i| format!("{i}, ")).collect();
        let source =
            format!("main {{ let r = [{operands}perform 0(\"w\") as Any]\nreturn list.len(r) }}\n");
        let mut embedder = park_where(&source, waits_in_main);
        assert_eq!(
            main_top(&mut embedder.run_record()).operands.len(),
            count,
            "{count} operands"
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
    case(NESTED_JOIN, &["b", "a"], &expected, &[], &[Stand::Holding]);
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
        &[Stand::Called, Stand::Holding],
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
        seen.stood.iter().any(|at| Stand::BodyCalled.holds(at)),
        "the reopens saw {:?}",
        seen.stood
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
