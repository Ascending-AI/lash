//! Runs a lowered cell on the kernel machine, with scripted tools.
//!
//! The registry is the kernel library and the dialect's helpers, nothing
//! else. A run is recorded the way `witness/record.py` records CPython: one
//! epoch per stretch between deliveries, with the outcomes delivered into
//! it, the tool calls and sleeps it requested and the lines it printed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::{Environment, Lowered, NamedLibrary};
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Param, Signature,
    Timestamp, Type,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, Park, PreparedLibrary, Program,
    Request, RunError, Start, Step, Target, WaitId, register_machine_functions,
};
use serde::Deserialize;

use crate::define_helpers;

struct Installed {
    prepared: PreparedLibrary,
    library: NamedLibrary,
}

/// The kernel library with the dialect's helpers: the registry a machine
/// runs with and the library the front end lowers against.
fn installed() -> &'static Installed {
    static INSTALLED: OnceLock<Installed> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let mut registry = FunctionRegistry::new();
        register_machine_functions(&mut registry).unwrap_or_else(|error| panic!("{error}"));
        lash_kernel_lib::register_numbers(&mut registry).unwrap_or_else(|error| panic!("{error}"));
        lash_kernel_lib::register_collections(&mut registry)
            .unwrap_or_else(|error| panic!("{error}"));
        lash_kernel_lib::register_text_json(&mut registry)
            .unwrap_or_else(|error| panic!("{error}"));
        let mut library =
            NamedLibrary::from_registry(&registry).unwrap_or_else(|error| panic!("{error}"));
        for definition in define_helpers(&mut library).unwrap_or_else(|error| panic!("{error}")) {
            registry
                .register(definition, None)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        Installed {
            prepared: PreparedLibrary::new(Arc::new(registry)),
            library,
        }
    })
}

pub(crate) fn library() -> &'static NamedLibrary {
    &installed().library
}

/// The tools a cell may call: `echo(x)` answers `x` and `boom(x)` fails
/// with an error of kind `boom` whose message is `x`.
pub(crate) fn effects() -> BTreeMap<EffectName, Signature> {
    ["echo", "boom"]
        .into_iter()
        .map(|name| {
            let signature = Signature {
                params: vec![Param {
                    name: Name::new("x"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            };
            (EffectName::new(name).expect("a tool's name"), signature)
        })
        .collect()
}

/// Lowers `source` as a cell of a session that holds `bindings` and can
/// call the tools `effects`.
fn lower_with(
    source: &str,
    effects: &BTreeMap<EffectName, Signature>,
    bindings: &BTreeSet<Name>,
) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    crate::lower(
        source,
        &Environment {
            tool_roots: &std::collections::BTreeSet::new(),
            library: library(),
            effects,
            controls: controls(),
            bindings,
            functions: &std::collections::BTreeMap::new(),
        },
    )
}

/// The laws' turn-ending tool: `control_finish(x)` ends the turn with `x`,
/// where a law's host offers it.
pub(crate) fn controls() -> &'static std::collections::BTreeMap<
    lash_kernel_doc::EffectName,
    std::collections::BTreeSet<lash_kernel_dialect::EffectControl>,
> {
    static CONTROLS: std::sync::OnceLock<
        std::collections::BTreeMap<
            lash_kernel_doc::EffectName,
            std::collections::BTreeSet<lash_kernel_dialect::EffectControl>,
        >,
    > = std::sync::OnceLock::new();
    CONTROLS.get_or_init(|| {
        std::collections::BTreeMap::from([(
            lash_kernel_doc::EffectName::new("control_finish").expect("a tool's name"),
            std::collections::BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
        )])
    })
}

/// Lowers `source` as a first cell whose host offers `effects`.
pub(crate) fn lower_with_effects(
    source: &str,
    effects: &BTreeMap<EffectName, Signature>,
) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    lower_with(source, effects, &BTreeSet::new())
}

/// Lowers `source` as a first cell.
pub(crate) fn lower(source: &str) -> Result<Lowered, lash_kernel_dialect::Diagnostic> {
    lower_with(source, &effects(), &BTreeSet::new())
}

/// The kernel text of the document `source` lowers to.
pub(crate) fn kernel_text(source: &str) -> String {
    let lowered = lower(source).unwrap_or_else(|error| panic!("{error}"));
    lash_kernel_doc::print_document(&lowered.document)
}

const BOUNDS: Bounds = Bounds {
    charge: 50_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Default)]
struct Console {
    lines: Vec<String>,
}

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_string(),
            message: "the laws' host has no projection".to_string(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, value: &Datum) {
        self.lines.push(match value {
            Datum::Text(line) => line.clone(),
            other => format!("{other:?}"),
        });
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// One stretch of a run between deliveries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct Epoch {
    /// The outcomes delivered before the stretch ran, in order.
    pub(crate) delivered: Vec<String>,
    /// The tool calls and sleeps the stretch requested, in order.
    pub(crate) asked: Vec<String>,
    /// The lines the stretch printed.
    pub(crate) logged: Vec<String>,
}

/// A whole run: its epochs and how it ended, `ok` or the error.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Recorded {
    pub(crate) epochs: Vec<Epoch>,
    pub(crate) end: String,
}

impl Recorded {
    /// Every line the run printed, in order.
    pub(crate) fn lines(&self) -> Vec<&str> {
        self.epochs
            .iter()
            .flat_map(|epoch| epoch.logged.iter().map(String::as_str))
            .collect()
    }
}

fn shown(datum: Option<&Datum>) -> String {
    match datum {
        Some(Datum::Text(text)) => text.clone(),
        Some(Datum::Int(value)) => value.to_string(),
        other => format!("{other:?}"),
    }
}

/// What a request is called in a delivery script: `echo:a`, `boom:x`,
/// `sleep:50`.
fn label(request: &Request) -> String {
    match request {
        Request::Effect(effect) => format!("{}:{}", effect.effect, shown(effect.args.first())),
        Request::Sleep(sleep) => format!("sleep:{}", sleep.duration.as_millis()),
    }
}

fn answer(request: &Request) -> (WaitId, Outcome) {
    match request {
        Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
        Request::Effect(effect) => {
            let argument = effect.args.first().cloned().unwrap_or(Datum::Null);
            let outcome = if effect.effect.as_str() == "boom" {
                Outcome::Failed(ErrorDatum {
                    kind: "boom".to_string(),
                    message: shown(Some(&argument)),
                    data: Datum::Null,
                })
            } else {
                Outcome::Completed(argument)
            };
            (effect.wait, outcome)
        }
    }
}

fn ending(end: End) -> String {
    match end {
        End::Finished(_) => "ok".to_string(),
        End::Error(RunError::Uncaught(Datum::Error(error))) => {
            format!("error {}: {}", error.kind, error.message)
        }
        End::Error(RunError::Uncaught(other)) => format!("error {other:?}"),
        End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
        }) => format!(
            "tasks outstanding: {} unfinished, {} unobserved",
            unfinished.len(),
            unobserved.len()
        ),
        End::Error(error) => format!("run error: {error}"),
        End::Failed(reason) => format!("failed {reason:?}"),
        End::Cancelled => "cancelled".to_string(),
    }
}

/// Lowers `source` as a cell of a session that holds `bindings`, admits
/// the document and starts a machine on it. Gives the machine and the
/// document's kernel text.
fn start(
    source: &str,
    effects: BTreeMap<EffectName, Signature>,
    bindings: Bindings,
) -> (KernelMachine, String) {
    let names: BTreeSet<Name> = bindings.variables.keys().cloned().collect();
    let lowered = lower_with(source, &effects, &names).unwrap_or_else(|error| panic!("{error}"));
    let text = lash_kernel_doc::print_document(&lowered.document);
    if let Err(invalid) =
        lash_kernel_doc::validate_annotations(&lowered.annotations, &lowered.document)
    {
        panic!("{invalid}\n{text}");
    }
    let mut environment = lash_kernel_check::Environment::new(library());
    environment.effects = effects;
    environment.bindings = names;
    if let Err(refusal) = lash_kernel_check::admit(&lowered.document, &environment) {
        panic!("{refusal:?}\n{text}");
    }
    let program = Program {
        document: Arc::new(lowered.document),
        library: installed().prepared.clone(),
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings,
    };
    let machine =
        KernelMachine::start(program, BOUNDS, start).unwrap_or_else(|error| panic!("{error}"));
    (machine, text)
}

/// Runs `source` to its end as a cell of a session that holds `bindings`
/// and can call the tools `effects`. At each park, `at_park` is given the
/// lines printed since the park before, the park and the document's
/// kernel text, and answers the waits it chooses to. Gives the lines
/// printed after the last park and the run's end.
pub(crate) fn drive(
    source: &str,
    effects: BTreeMap<EffectName, Signature>,
    bindings: Bindings,
    mut at_park: impl FnMut(Vec<String>, Park, &str) -> Vec<(WaitId, Outcome)>,
) -> (Vec<String>, End) {
    let (mut machine, text) = start(source, effects, bindings);
    let mut console = Console::default();
    loop {
        let step = machine
            .run(&mut console, u64::MAX)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        let lines = std::mem::take(&mut console.lines);
        match step {
            Step::Slice => unreachable!("the slice is unbounded"),
            Step::Ended(end) => return (lines, end),
            Step::Parked(park) => {
                for (wait, outcome) in at_park(lines, park, &text) {
                    machine
                        .deliver(wait, outcome)
                        .unwrap_or_else(|error| panic!("{error}"));
                }
            }
        }
    }
}

/// Lowers `source` as a first cell and runs it to its end. At each park the
/// next batch of `deliveries` is delivered, in the order it lists; once the
/// script is used up, the oldest pending request.
pub(crate) fn run(source: &str, deliveries: &[Vec<String>]) -> Recorded {
    run_cell(source, deliveries, Bindings::default()).0
}

/// The lines a cell that calls no tool prints, and how it ends.
pub(crate) fn output(source: &str) -> (Vec<String>, String) {
    let recorded = run(source, &[]);
    let lines = recorded.lines().into_iter().map(str::to_string).collect();
    (lines, recorded.end)
}

/// Runs the cells of one session in order, each starting with the
/// bindings the one before it left, and gives every line they printed.
pub(crate) fn session(cells: &[&str]) -> Vec<String> {
    let mut bindings = Bindings::default();
    let mut lines = Vec::new();
    for cell in cells {
        let (recorded, carried) = run_cell(cell, &[], bindings);
        assert_eq!(recorded.end, "ok", "{cell}");
        lines.extend(recorded.lines().into_iter().map(str::to_string));
        bindings = carried.unwrap_or_default();
    }
    lines
}

/// [`run`] in a session that holds `bindings`. Also gives the bindings a
/// cell that finished leaves to the next.
fn run_cell(
    source: &str,
    deliveries: &[Vec<String>],
    bindings: Bindings,
) -> (Recorded, Option<Bindings>) {
    let mut pending: Vec<Request> = Vec::new();
    let mut script = deliveries.iter();
    let mut epochs = Vec::new();
    let mut delivered = Vec::new();
    let (logged, end) = drive(source, effects(), bindings, |logged, park, text| {
        pending.retain(|request| {
            let wait = match request {
                Request::Effect(effect) => effect.wait,
                Request::Sleep(sleep) => sleep.wait,
            };
            !park.withdrawn.contains(&wait)
        });
        epochs.push(Epoch {
            delivered: std::mem::take(&mut delivered),
            asked: park.requests.iter().map(label).collect(),
            logged,
        });
        pending.extend(park.requests);
        let batch = match script.next() {
            Some(batch) => batch.clone(),
            None => {
                vec![label(pending.first().unwrap_or_else(|| {
                    panic!("a park with nothing pending\n{text}")
                }))]
            }
        };
        let outcomes = batch
            .iter()
            .map(|name| {
                let index = pending
                    .iter()
                    .position(|request| label(request) == *name)
                    .unwrap_or_else(|| panic!("no pending request `{name}`\n{text}"));
                answer(&pending.remove(index))
            })
            .collect();
        delivered = batch;
        outcomes
    });
    assert!(script.next().is_none(), "unused delivery batches");
    epochs.push(Epoch {
        delivered,
        asked: Vec::new(),
        logged,
    });
    let carried = match &end {
        End::Finished(finished) => Some(finished.bindings.clone()),
        _ => None,
    };
    (
        Recorded {
            epochs,
            end: ending(end),
        },
        carried,
    )
}

/// A witness must consume every declared arrival; otherwise it tested a
/// different schedule from the one its reference recorder was given.
#[test]
#[should_panic(expected = "unused delivery batches")]
fn witness_rejects_unused_delivery_batches() {
    run("print('done')", &[vec!["echo:unused".into()]]);
}

/// FIG-6102, K-CHG-007: integer add, sub and iadd do numeric work in
/// their own bodies, without activating another Python helper. Their
/// interpreted operations and numeric native charges account for the run.
#[test]
fn integer_arithmetic_runs_directly_and_accounts_for_its_body() {
    // Parameters have no local integer proof; this law exercises the
    // guarded helpers, independently of constant/local lowering.
    let source =
        "def arithmetic(a, b):\n    c = a + b\n    d = a - b\n    a += b\narithmetic(7, 3)\n";
    let (mut machine, _) = start(source, effects(), Bindings::default());
    machine.count_work();
    assert!(matches!(
        machine.run(&mut Console::default(), u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    let work = machine.work().expect("work counting is enabled");
    assert_eq!(work.residual(), 0, "every unit belongs to executed work");
    let mut helpers = BTreeMap::new();
    for code in &work.codes {
        if code.symbol.name.starts_with("py.") {
            helpers.insert(code.symbol.name.as_str(), code.counts.calls);
            assert!(code.counts.self_charge > 0, "a helper charges its body");
        }
    }
    assert_eq!(
        helpers,
        BTreeMap::from([("py.add", 1), ("py.iadd", 1), ("py.sub", 1)]),
        "integer arithmetic must not activate nested Python numeric helpers"
    );
}

// These previous definitions are held only by this law, as sealed releases
// hold old bodies. The unsealed production baseline keeps current bodies.
const PREVIOUS_ARITHMETIC: &str = r#"
use kind
use same
use add
use sub
use text.concat
use list.concat
use tuple.concat
use list.len
use set.contains
use bool.not
use py.is_number
use py.operand_error
use py.to_list
use py.add

function py.add(left: Any, right: Any) -> Any
kernel 1
charge sum(4, size(left), size(right))
body {
  let k = kind(left)
  if same(k, kind(right)) {
    if same(k, "text") {
      return text.concat(left, right)
    }
    if same(k, "list") {
      return list.concat(left, right)
    }
    if same(k, "tuple") {
      return tuple.concat(left, right)
    }
  }
  let ok = invoke py.is_number(left)
  if ok {
    set ok = invoke py.is_number(right)
  }
  if ok {
    return add(left, right)
  }
  do invoke py.operand_error("+", left, right)
  return null
}

function py.iadd(left: Any, right: Any) -> Any
kernel 1
charge sum(4, size(left), size(right))
body {
  if same(kind(left), "list") {
    let items = invoke py.to_list(right)
    for item in items {
      set left[list.len(left)] = item
    }
    return left
  }
  let out = invoke py.add(left, right)
  return out
}

function py.sub(left: Any, right: Any) -> Any
kernel 1
charge sum(4, size(left), size(right))
body {
  let ok = invoke py.is_number(left)
  if ok {
    set ok = invoke py.is_number(right)
  }
  if ok {
    return sub(left, right)
  }
  if same(kind(left), "set") {
    if same(kind(right), "set") {
      let out = set{}
      for member in left {
        if bool.not(set.contains(right, member)) {
          set out[member] = true
        }
      }
      return out
    }
  }
  do invoke py.operand_error("-", left, right)
  return null
}
"#;

const ARITHMETIC_RESUME: &str = "\
n = await echo(7)
x = n + 3
y = n - 3
n += 3
print(n, x, y)
big = 9223372036854775807
big += 1
print(big, big + 1, big - 1)
x = 1.5
x += 2
print(x, 2 + 1.5, 1.5 + 2, 2 - 1.5, 1.5 - 2)
a = [1]
alias = a
a += a
a += (3,)
print(alias, alias is a)
print('a' + 'b', (1,) + (2,), [1] + [2], {1, 2} - {2})
t = 'a'
t += 'b'
u = (1,)
u += (2,)
print(t, u)
try:
    True + 1
except TypeError as e:
    print(e)
try:
    1 - 'x'
except TypeError as e:
    print(e)
try:
    a += 1
except TypeError as e:
    print(e)
";

fn arithmetic_observation(
    program: Program,
    bounds: Bounds,
    rebuild: bool,
) -> (End, Vec<String>, Vec<String>, u64) {
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds,
        Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: Bindings::default(),
        },
    )
    .expect("the arithmetic document starts");
    let mut host = Console::default();
    let mut requests = Vec::new();
    loop {
        let step = machine
            .run(&mut host, if rebuild { 1 } else { u64::MAX })
            .expect("the arithmetic document runs");
        if let Step::Ended(end) = step {
            return (end, host.lines, requests, machine.meters().charged);
        }
        if rebuild {
            let state = machine
                .export()
                .expect("every arithmetic safe point exports");
            // Use the serialized state, rather than keeping any prepared
            // execution coordinates from the previous machine.
            let bytes = serde_json::to_vec(&state).expect("state serializes");
            let state = serde_json::from_slice(&bytes).expect("state deserializes");
            machine = KernelMachine::import(program.clone(), bounds, state)
                .expect("every arithmetic safe point imports");
        }
        if let Step::Parked(park) = step {
            for request in park.requests {
                requests.push(label(&request));
                let (wait, outcome) = answer(&request);
                machine
                    .deliver(wait, outcome)
                    .expect("the echo outcome is delivered");
            }
        }
    }
}

/// K-MACH-007/008, K-CHG-007: new arithmetic identities preserve each
/// document's result, effects, aliases and exact body charge across slices.
/// Previous definitions held beside them still resolve their original pins.
#[test]
fn arithmetic_successors_and_held_previous_definitions_resume_exactly() {
    use lash_kernel_dialect::Library;
    let current = installed().prepared.registry();
    let names = ["py.add", "py.iadd", "py.sub"];
    let ids: BTreeSet<_> = names
        .iter()
        .map(|name| library().resolve(name).unwrap())
        .collect();
    let mut previous = NamedLibrary::resolving(current, |id| !ids.contains(id)).unwrap();
    let definitions = lash_kernel_dialect::define_functions(PREVIOUS_ARITHMETIC, &mut previous)
        .expect("the previous arithmetic definitions resolve");
    let mut registry = current.as_ref().clone();
    for definition in definitions {
        assert!(
            !ids.contains(&definition.identity().unwrap()),
            "a successor has a new identity"
        );
        registry
            .register(definition, None)
            .expect("previous and current bodies coexist");
    }
    let prepared = PreparedLibrary::new(Arc::new(registry));
    let effects = effects();
    let mut observations = Vec::new();
    for named in [library(), &previous] {
        let lowered = crate::lower(
            ARITHMETIC_RESUME,
            &Environment {
                tool_roots: &BTreeSet::new(),
                library: named,
                effects: &effects,
                controls: &BTreeMap::new(),
                bindings: &BTreeSet::new(),
                functions: &BTreeMap::new(),
            },
        )
        .expect("the arithmetic witness lowers");
        let program = Program {
            document: Arc::new(lowered.document),
            library: prepared.clone(),
        };
        let straight = arithmetic_observation(program.clone(), BOUNDS, false);
        assert!(matches!(straight.0, End::Finished(_)), "{:?}", straight.0);
        assert_eq!(straight.2, ["echo:7"]);
        assert_eq!(
            arithmetic_observation(
                program,
                Bounds {
                    charge: straight.3,
                    ..BOUNDS
                },
                true
            ),
            straight
        );
        observations.push(straight);
    }
    assert_eq!(
        observations[0].0, observations[1].0,
        "old and new documents finish identically"
    );
    assert_eq!(
        observations[0].1, observations[1].1,
        "numeric and fallback observations agree"
    );
    assert!(
        observations[0].3 < observations[1].3,
        "the new bodies do less work"
    );
}

/// FIG-6115: integer-closed loop definitions need no dynamic arithmetic
/// guards, including when the accumulator promotes beyond signed 64 bits.
#[test]
fn integer_facts_remove_loop_guards_and_preserve_bigints() {
    let source = "s = 9223372036854775807\nfor i in range(3):\n    s += i\nprint(s)\n";
    assert_eq!(
        output(source),
        (vec!["9223372036854775810".into()], "ok".into())
    );
    assert!(
        !kernel_text(source).contains("py.iadd"),
        "proven integer addition is direct"
    );
}

/// FIG-6115: a nested function may change a captured binding after its
/// integer initialization; the enclosing loop must retain dynamic dispatch.
#[test]
fn integer_facts_exclude_captured_and_nonlocal_bindings() {
    let source = r#"
def outer():
    x = 1
    def change():
        nonlocal x
        x = 'a'
    for i in range(2):
        if i == 0:
            change()
        x += 'b'
    print(x)
outer()
"#;
    assert_eq!(output(source), (vec!["abb".into()], "ok".into()));
    assert!(kernel_text(source).contains("py.iadd"));
    for read_only in [
        "x = 1\ndef read():\n    return x\nfor i in range(2):\n    x += i\nprint(read())",
        "x = 1\nread = lambda: x\nfor i in range(2):\n    x += i\nprint(read())",
    ] {
        assert_eq!(output(read_only), (vec!["2".into()], "ok".into()));
        assert!(kernel_text(read_only).contains("py.iadd"));
    }
}

/// FIG-6115: any non-integer definition reaching a loop use prevents an
/// integer fact, including a string/float at a later iteration's backedge.
#[test]
fn integer_facts_exclude_string_and_float_reassignments() {
    for (source, expected) in [
        (
            "x = 1\nfor i in range(2):\n    x = 'a'\n    x += 'b'\nprint(x)",
            "ab",
        ),
        (
            "x = 1\nfor i in range(2):\n    x += 1\n    x = 1.5\nprint(x)",
            "1.5",
        ),
        ("x = 1\nif True:\n    x = 1.5\nx += 1\nprint(x)", "2.5"),
    ] {
        assert_eq!(output(source), (vec![expected.into()], "ok".into()));
        assert!(kernel_text(source).contains("py.iadd"));
    }
}

/// FIG-6115: a definition on only one branch cannot bypass the Python
/// unbound-local check at a merged use, even if every definition is int.
#[test]
fn integer_facts_check_unbound_paths() {
    let source = r#"
def f(flag):
    if flag:
        x = 1
    try:
        x += 1
        print(x)
    except UnboundLocalError:
        print('unbound')
f(False)
f(True)
"#;
    assert_eq!(
        output(source),
        (vec!["unbound".into(), "2".into()], "ok".into())
    );
    assert!(kernel_text(source).contains("py.iadd"));
}

/// FIG-6115: Python implicitly deletes an except target; the next read
/// raises its unbound-local error. Explicit variable del remains refused.
#[test]
fn integer_facts_exclude_deleted_bindings() {
    let source = r#"
def f():
    x = 1
    try:
        raise ValueError()
    except ValueError as x:
        pass
    try:
        x += 1
    except UnboundLocalError:
        print('deleted')
    x = 1
    x += 1
    print(x)
f()
"#;
    assert_eq!(
        output(source),
        (vec!["deleted".into(), "2".into()], "ok".into())
    );
    assert!(kernel_text(source).contains("py.iadd"));
    assert_eq!(
        lower("x = 1\ndel x")
            .expect_err("variable del is outside the dialect")
            .code,
        crate::diagnostics::Code::DeleteUnsupported.as_str()
    );
}

/// FIG-6115: global declarations allow a call to replace an integer with
/// another type; facts cannot cross the scope/call boundary.
#[test]
fn integer_facts_exclude_global_bindings() {
    let source = r#"
x = 1
def change():
    global x
    x = 1.5
change()
x += 1
print(x)
"#;
    assert_eq!(output(source), (vec!["2.5".into()], "ok".into()));
    assert!(kernel_text(source).contains("py.iadd"));
}

/// FIG-6115: supported bool-to-int conversion remains guarded; integer
/// facts preserve signed floor division/modulo and negative-power results.
#[test]
fn integer_facts_preserve_boolean_and_signed_arithmetic() {
    let source = r#"
x = -7
for i in range(2):
    x += i
print(x // 4, x % 4)
x = int(True)
for i in range(2):
    x += i
print(x)
x = int(False)
for i in range(2):
    x += i
print(x)
x = 2 ** -1
x += 1
print(x)
x = True
try:
    x += 1
except TypeError:
    print('bool remains guarded')
"#;
    assert_eq!(
        output(source),
        (
            vec![
                "-2 2".into(),
                "2".into(),
                "1".into(),
                "1.5".into(),
                "bool remains guarded".into()
            ],
            "ok".into()
        )
    );
}

/// FIG-6115, K-MACH-007/008, K-CHG-007: serialization at every safe point
/// (including an await in the loop) restores the exact facts' values,
/// requests, result and charge; no compiler fact becomes runtime state.
#[test]
fn integer_facts_resume_a_parked_loop_exactly() {
    let source = r#"
s = 9223372036854775807
for i in range(3):
    s += i
    if i == 1:
        await echo(s)
print(s)
"#;
    assert!(!kernel_text(source).contains("py.iadd"));
    let lowered = lower(source).expect("the integer loop lowers");
    let program = Program {
        document: Arc::new(lowered.document),
        library: installed().prepared.clone(),
    };
    let straight = arithmetic_observation(program.clone(), BOUNDS, false);
    assert!(matches!(straight.0, End::Finished(_)));
    assert_eq!(straight.1, ["9223372036854775810"]);
    assert_eq!(straight.2, ["echo:9223372036854775808"]);
    assert_eq!(
        arithmetic_observation(
            program,
            Bounds {
                charge: straight.3,
                ..BOUNDS
            },
            true
        ),
        straight
    );
}

/// FIG-6115: a shadowed range is an arbitrary callable, and a restored
/// binding is an unknown reaching definition, even in an integer recurrence.
#[test]
fn integer_facts_exclude_shadowed_ranges_and_restored_values() {
    let source = r#"
def f(range):
    for x in range(2):
        x += 1
        print(x)
f(lambda n: [1.5])
"#;
    assert_eq!(output(source), (vec!["2.5".into()], "ok".into()));
    assert!(kernel_text(source).contains("py.iadd"));
    assert_eq!(
        session(&["x = 1.5", "for i in range(2):\n    x += 1\nprint(x)"]),
        ["3.5"]
    );
}

/// K-LTXT-012: Python materializes the iterable before admission, names the
/// first mismatch, and never coerces a member to str.
#[test]
fn join_admission_keeps_materialization_and_indexed_type_errors() {
    let (lines, end) = output(
        r#"
print('|'.join(['', '😀', 'é', '']))
print('|'.join(('a', 'b')), '-'.join('ab'), ','.join([]))
for values in [[None, 7], ['a', 7, None], ['a', 'b', None], ['a', []]]:
    try:
        print('|'.join(values))
    except TypeError as error:
        print(error)
try:
    print('|'.join(7))
except TypeError as error:
    print(error)
"#,
    );
    assert_eq!(end, "ok");
    assert_eq!(
        lines,
        [
            "|😀|é|",
            "a|b a-b ",
            "sequence item 0: expected str instance, NoneType found",
            "sequence item 1: expected str instance, int found",
            "sequence item 2: expected str instance, NoneType found",
            "sequence item 1: expected str instance, list found",
            "'int' object is not iterable"
        ]
    );
}
