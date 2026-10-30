//! Runs a lowered cell on the kernel machine, with scripted tools.
//!
//! A run is recorded the way `witness/async/record.mjs` records Node: one
//! epoch per stretch between deliveries, with the outcomes delivered into
//! it, the tool calls and sleeps it requested and the lines it printed.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::Environment;
use lash_kernel_doc::{Datum, ErrorDatum, FunctionRegistry};

use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Request,
    RunError, Step, Target, WaitId,
};
use serde::Deserialize;
use {
    lash_kernel_value::EffectName, lash_kernel_value::Name, lash_kernel_value::Param,
    lash_kernel_value::Signature, lash_kernel_value::Type,
};

/// The real kernel registry, with every dialect helper defined against it.
pub(super) fn registry() -> &'static Arc<FunctionRegistry> {
    static REGISTRY: OnceLock<Arc<FunctionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Arc::new(lash_kernel_conformance::installed_functions().expect("installed package natives"))
    })
}

/// [`registry`] with its library bodies compiled, once for every run.
pub(super) fn prepared() -> &'static PreparedLibrary {
    static PREPARED: OnceLock<PreparedLibrary> = OnceLock::new();
    PREPARED.get_or_init(|| {
        PreparedLibrary::with_helpers(
            Arc::clone(registry()),
            Arc::clone(
                &lash_kernel_releases::helper_release()
                    .expect("build helpers")
                    .helpers,
            ),
        )
    })
}

/// The tools a cell may call: `host.echo(x)` answers `x` and `host.boom(x)` fails
/// with an error of kind `boom` whose message is `x`.
pub(crate) fn effects() -> BTreeMap<EffectName, Signature> {
    ["host.echo", "host.boom", "control.finish"]
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

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: lash_kernel_vm::Bounds::DEFAULT_CALL_DEPTH,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Default)]
struct Console {
    lines: Vec<String>,
}

impl Host for Console {
    fn perform(
        &mut self,
        use_: &lash_kernel_value::OperationUse,
        _: &[Datum],
        _: &lash_kernel_value::sites::PositionLabel,
        _: &lash_kernel_value::sites::Site,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_kernel_vm::HostFault> {
        crate::tests::linked_reads::read(use_, 0)
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

/// What a request is called in a delivery script: `host.echo:a`, `host.boom:x`,
/// `sleep:50`.
fn label(request: &Request) -> String {
    match request {
        Request::Effect(effect) => {
            let argument = match effect.args.first() {
                Some(Datum::Text(text)) => text.clone(),
                Some(Datum::Float(value)) => value.get().to_string(),
                other => format!("{other:?}"),
            };
            let name = crate::tests::linked::operation(&effect.use_);
            if name.as_str() == "typescript.sleep" {
                let Some(Datum::Int(milliseconds)) = effect.args.first() else {
                    panic!("sleep milliseconds")
                };
                format!("sleep:{milliseconds}")
            } else {
                format!("{name}:{argument}")
            }
        }
    }
}

fn answer(request: &Request) -> (WaitId, Outcome) {
    match request {
        Request::Effect(effect) => {
            let argument =
                if crate::tests::linked::operation(&effect.use_).as_str() == "typescript.sleep" {
                    Datum::Null
                } else {
                    effect.args.first().cloned().unwrap_or(Datum::Null)
                };
            let outcome = if crate::tests::linked::operation(&effect.use_).as_str() == "host.boom" {
                Outcome::Failed(ErrorDatum {
                    kind: "boom".to_string(),
                    message: match &argument {
                        Datum::Text(text) => text.clone(),
                        other => format!("{other:?}"),
                    },
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
        End::Error(RunError::Uncaught {
            error: Datum::Text(text),
            ..
        }) => format!("error {text}"),
        End::Error(RunError::Uncaught {
            error: Datum::Error(error),
            ..
        }) => {
            format!("error {}: {}", error.kind, error.message)
        }
        End::Error(RunError::Uncaught { error: other, .. }) => format!("error {other:?}"),
        End::Error(RunError::TasksOutstanding {
            unfinished,
            unobserved,
            ..
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

/// How a cell that calls no tool ended: the value it gave `finish`, or
/// the kind of the error nothing caught.
#[derive(Debug, PartialEq)]
pub(crate) enum Ended {
    Finished(Datum),
    Raised(String),
}

/// Lowers `source` as a first cell and runs it to its end, answering only
/// `finish`: the value it ended with.
pub(crate) fn end(source: &str) -> Ended {
    end_with_bindings(source, super::pages::Session::default())
}

/// Runs a cell with kernel session data supplied by the law.
pub(crate) fn end_with_bindings(source: &str, bindings: super::pages::Session) -> Ended {
    let (mut machine, text) = start_with_bindings(source, bindings);
    let mut finished_with = None;
    loop {
        match machine.run(&mut Console::default(), u64::MAX) {
            Ok(Step::Parked(park)) if finished_with.is_none() => {
                let [Request::Effect(effect)] = park.requests.as_slice() else {
                    panic!("only `finish` is answered: {park:?}\n{text}");
                };
                assert_eq!(
                    crate::tests::linked::operation(&effect.use_).as_str(),
                    "control.finish",
                    "{text}"
                );
                finished_with = Some(effect.args.first().cloned().unwrap_or(Datum::Null));
                machine
                    .deliver(effect.wait, Outcome::Completed(Datum::Null))
                    .unwrap_or_else(|error| panic!("{error}"));
            }
            Ok(Step::Ended(End::Finished(_))) => {
                return Ended::Finished(
                    finished_with
                        .unwrap_or_else(|| panic!("the cell ended without `finish`\n{text}")),
                );
            }
            Ok(Step::Ended(End::Error(RunError::Uncaught {
                error: Datum::Error(error),
                ..
            }))) => {
                return Ended::Raised(error.kind);
            }
            other => panic!("{other:?}\n{text}"),
        }
    }
}

/// Lowers `source` as a first cell and starts a machine on it. Gives the
/// machine and the document's kernel text.
fn start(source: &str) -> (KernelMachine, String) {
    start_with_bindings(source, super::pages::Session::default())
}

/// FIG-7315: Number recurrences execute typed scalar packets, including
/// hoisted counters; BigInt recurrences retain their exact integer domain.
#[test]
fn number_recurrences_execute_typed_scalar_regions_and_keep_bigint_exact() {
    for source in [
        "function empty_loop(n) { var j; for (j = 0; j < n; j++) {} return j; } console.log(empty_loop(10));",
        "function sum() { let s = 0; for (let i = 0; i < 10; i++) s = s + i * 2; return s; } console.log(sum());",
        "function sum() { var s, i; s = 0; for (i = 0; i < 10; i++) s += i * 2; return s; } console.log(sum());",
        "function sum() { var s, i, a; s = 0; a = 0.5; for (i = 0; i < 10; i++) { s += a * a; a += 1; } return s; } console.log(sum());",
    ] {
        let (mut machine, text) = start(source);
        let tape = lash_kernel_vm::ChargeTape::new();
        machine.record_charges(tape.clone());
        let mut console = Console::default();
        assert!(
            matches!(
                machine.run(&mut console, u64::MAX),
                Ok(Step::Ended(End::Finished(_)))
            ),
            "{text}"
        );
        let work = lash_kernel_vm::perf_witness::register_work(&tape);
        if source.contains("empty_loop") {
            assert!(
                work.blocks > 0,
                "Number loop executed no whole iteration: {work:?}\n{text}"
            );
        }
        assert!(
            work.typed_packets > 0,
            "Number loop executed no typed scalar packet: {work:?}\n{text}"
        );
        assert_eq!(
            console.lines,
            if source.contains("empty_loop") {
                vec!["10"]
            } else if source.contains("0.5") {
                vec!["332.5"]
            } else {
                vec!["90"]
            }
        );
        println!("Number recurrence: {work:?}");
    }
    let number_recurrence = super::main_text(
        "function double() { let n = 1; for (let i = 0; i < 10; i++) n = n * 2; return n; }",
    );
    assert!(
        !number_recurrence.contains("ts.mul("),
        "{number_recurrence}"
    );
    assert_eq!(
        end(
            "let s = 900719925474099300000000000000000n; for (let i = 0n; i < 10n; i++) s = s + i * 2n; await control.finish(String(s));"
        ),
        Ended::Finished(Datum::Text("900719925474099300000000000000090".into()))
    );
    assert_eq!(
        end(
            "function mul(a, b) { return a * b; } function step(a) { return a++; } let mixed = ''; try { mul(2n, 3); } catch (e) { mixed = e.name; } await control.finish(JSON.stringify([mul(2, 3), String(mul(2n, 3n)), String(step(9007199254740993n)), mixed]));"
        ),
        Ended::Finished(Datum::Text(
            "[6,\"6\",\"9007199254740993\",\"TypeError\"]".into()
        ))
    );
}

fn start_with_bindings(source: &str, values: super::pages::Session) -> (KernelMachine, String) {
    let library = super::library();
    let effects = effects();
    let bindings = values.names();
    let environment_namespaces = crate::tests::linked::namespaces(&effects, super::controls())
        .expect("the test's effects are operations");
    let environment = Environment::new(library, &environment_namespaces);
    let lowered = crate::lower(
        source,
        &environment,
        lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment {
            bindings: &bindings,
            functions: &std::collections::BTreeMap::new(),
        }),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let text = lash_kernel_doc::print_document(&lowered.document);
    if let Err(invalid) = lash_kernel_doc::validate_document(&lowered.document, library) {
        panic!("{invalid}\n{text}");
    }
    let program =
        crate::tests::linked::program(Arc::clone(lowered.document.shared()), prepared().clone());
    let function_origins = lash_kernel_vm::interface::FunctionOrigins {
        sites: lowered
            .written
            .iter()
            .map(|(site, written)| ((program.link, site.clone()), written.clone()))
            .collect(),
        ..Default::default()
    };
    let start = CellStart {
        target: Target::Main,
        args: Vec::new(),
        session: values.root,
        owner: super::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine = super::pages::with_source(program, BOUNDS, start, &values.source)
        .unwrap_or_else(|error| panic!("{error}"));
    machine.set_options(lash_kernel_vm::MachineOptions {
        function_origins,
        ..Default::default()
    });
    (machine, text)
}

/// Lowers `source` as a first cell and runs it to its end. At each park the
/// next batch of `deliveries` is delivered, in the order it lists; once the
/// script is used up, the oldest pending request.
pub(crate) fn run(source: &str, deliveries: &[Vec<String>]) -> Recorded {
    let (mut machine, text) = start(source);
    let mut console = Console::default();
    let mut pending: Vec<Request> = Vec::new();
    let mut script = deliveries.iter();
    let mut epochs = Vec::new();
    let mut epoch = Epoch::default();
    loop {
        let step = machine
            .run(&mut console, u64::MAX)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        epoch.logged = std::mem::take(&mut console.lines);
        match step {
            Step::Slice => unreachable!("the slice is unbounded"),
            Step::Ended(end) => {
                epochs.push(epoch);
                return Recorded {
                    epochs,
                    end: ending(end),
                };
            }
            Step::Parked(park) => {
                epoch.asked = park.requests.iter().map(label).collect();
                pending.extend(park.requests);
                epochs.push(std::mem::take(&mut epoch));
                let batch = match script.next() {
                    Some(batch) => batch.clone(),
                    None => vec![label(pending.first().expect("a park with nothing pending"))],
                };
                for name in &batch {
                    let index = pending
                        .iter()
                        .position(|request| label(request) == *name)
                        .unwrap_or_else(|| panic!("no pending request `{name}`\n{text}"));
                    let (wait, outcome) = answer(&pending.remove(index));
                    machine
                        .deliver(wait, outcome)
                        .unwrap_or_else(|error| panic!("{error}"));
                }
                epoch.delivered = batch;
            }
        }
    }
}

/// Runs an explicit workflow or frozen-function entry through the public machine.
pub(crate) fn end_of_entry(
    document: lash_kernel_doc::Document,
    entry: &str,
    args: Vec<Datum>,
) -> (Vec<String>, Datum) {
    let text = lash_kernel_doc::print_document(&document);
    let program = crate::tests::linked::program(Arc::new(document), prepared().clone());
    let start = CellStart {
        target: Target::Entry(Name::new(entry)),
        args,
        session: None,
        owner: super::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine = super::pages::start(program, BOUNDS, start)
        .unwrap_or_else(|error| panic!("{error}\n{text}"));
    let mut console = Console::default();
    let mut asked = Vec::new();
    loop {
        match machine
            .run(&mut console, u64::MAX)
            .unwrap_or_else(|error| panic!("{error}\n{text}"))
        {
            Step::Slice => unreachable!("the slice is unbounded"),
            Step::Ended(End::Finished(finished)) => return (asked, finished.result),
            Step::Ended(other) => panic!("{other:?}\n{text}"),
            Step::Parked(park) => {
                for request in &park.requests {
                    asked.push(label(request));
                    let (wait, outcome) = answer(request);
                    machine
                        .deliver(wait, outcome)
                        .unwrap_or_else(|error| panic!("{error}"));
                }
            }
        }
    }
}

/// Executes with a fresh serialized import at slices and parks. Neither
/// source facts nor operand metadata may be required by resumed execution.
pub(super) fn plain_record_resume(source: &str, slice: u64, restore: bool) -> (Datum, u64, usize) {
    let program = crate::tests::linked::program(
        Arc::clone(
            super::lower_with_effects(source)
                .expect("law source")
                .document
                .shared(),
        ),
        prepared().clone(),
    );
    let source_pages = super::pages::Source::default();
    let mut machine = super::pages::with_source(
        program.clone(),
        BOUNDS,
        CellStart {
            owner: super::pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        &source_pages,
    )
    .unwrap();
    let mut result = None;
    let mut imports = 0;
    loop {
        let step = machine
            .run(&mut Console::default(), slice)
            .expect("law run");
        if restore && matches!(step, Step::Slice | Step::Parked(_)) {
            super::pages::reopen(&mut machine, &program, BOUNDS, &source_pages);
            imports += 1;
        }
        match step {
            Step::Slice => {}
            Step::Parked(park) => {
                for request in park.requests {
                    let Request::Effect(effect) = request;
                    let value = effect.args.first().cloned().unwrap_or(Datum::Null);
                    if crate::tests::linked::operation(&effect.use_).as_str() == "control.finish" {
                        result = Some(value.clone());
                    }
                    machine
                        .deliver(effect.wait, Outcome::Completed(value))
                        .expect("deliver");
                }
            }
            Step::Ended(End::Finished(_)) => {
                let charged = machine.meters().charged.expect("exact charge");
                return (result.expect("finish result"), charged, imports);
            }
            other => panic!("{other:?}"),
        }
    }
}

/// TS-PLAIN-PARK: a nonterminal await invalidates source facts for the
/// cell, including an await between a member reference and its write.
#[test]
fn plain_record_mid_path_park_resumes_the_guarded_heap() {
    let source = "const r = { next: { leaf: 0 } }; r.next.leaf = await host.echo(7); await control.finish(r);";
    let text = super::main_text_with_effects(source);
    assert!(text.contains("__lash_brand"), "{text}");
    let plain = plain_record_resume(source, u64::MAX, false);
    let restored = plain_record_resume(source, u64::MAX, true);
    assert_eq!((&plain.0, plain.1), (&restored.0, restored.1));
    assert_eq!(restored.2, 2, "echo and finish parks were imported");
    assert_eq!(
        end("const r = { next: { leaf: 7 } }; await control.finish(r);"),
        Ended::Finished(plain.0)
    );
}

/// K-CHG-007, K-MACH-007, TS-PLAIN-PATH: direct member access preserves
/// its result and exact charge over serialized slices.
#[test]
fn plain_record_paths_account_charge_across_slice_resume() {
    let source = "const r = { next: { leaf: 0 } }; for (let n = 0; n < 3; n++) { r.next.leaf = n; } await control.finish(r);";
    let text = super::main_text_with_effects(source);
    assert!(!text.contains("__lash_brand"), "{text}");
    let plain = plain_record_resume(source, u64::MAX, false);
    let sliced = plain_record_resume(source, 1, true);
    assert_eq!((&plain.0, plain.1), (&sliced.0, sliced.1));
    assert!(sliced.2 > 10, "multiple member steps were imported");
}

/// TS-STABLE-PATH: a closed ancestor traversal is paid once per loop,
/// independently of the number of leaf writes; sliced imports pay exactly
/// the same charge and return the same graph as uninterrupted execution.
#[test]
fn stable_ancestor_paths_traverse_once_and_resume_exactly() {
    fn source(depth: usize, iterations: usize) -> String {
        let mut source = "const tip = { leaf: 0 };\n".to_string();
        let mut parent = "tip".to_string();
        for level in 0..depth {
            let name = format!("r{level}");
            source.push_str(&format!("const {name} = {{ next: {parent} }};\n"));
            parent = name;
        }
        let half = ".next".repeat(depth / 2);
        let rest = ".next".repeat(depth - depth / 2);
        source.push_str(&format!(
            "for (let i = 0; i < {iterations}; i++) {{ const middle = {parent}{half}; middle{rest}.leaf = i; }} await control.finish({parent});"
        ));
        source
    }
    let shallow_once = plain_record_resume(&source(4, 1), u64::MAX, false);
    let shallow_many = plain_record_resume(&source(4, 4), u64::MAX, false);
    let deep_once = plain_record_resume(&source(24, 1), u64::MAX, false);
    let deep_source = source(24, 4);
    let deep_many = plain_record_resume(&deep_source, u64::MAX, false);
    assert_eq!(
        deep_many.1 - deep_once.1,
        shallow_many.1 - shallow_once.1,
        "additional iterations must not repay the ancestors"
    );
    let sliced = plain_record_resume(&deep_source, 1, true);
    assert_eq!((&deep_many.0, deep_many.1), (&sliced.0, sliced.1));
    assert!(sliced.2 > 24);
}

/// K-MACH-007/008, K-CHG-007 and ECMA SameValueZero: atomic admission
/// survives argument parks and serialized charge cuts with exactly the same
/// total charge, key identities and results as uninterrupted execution.
#[test]
fn map_admission_preserves_exact_charge_across_slices_and_argument_parks() {
    let source = "const key = {}; const f = () => 1; const m = new Map(); m.set(-0, 1); m.set(NaN, 2); m.set(key, 3); m.set(f, await host.echo(7)); m.set(); const textMap = new Map(); const s = new Set(); for (const text of ['x', 'a long text key past the inline limit']) { textMap.set(text, await host.echo(text)); s.add(text); } textMap.set('x', 'replacement'); for (let i = 0; i < 3; i++) { textMap.set(String(i), i); } key.x = 9; const checks = [m.size === 5, textMap.size === 5, textMap.has(String(1)), textMap.get(String(1)) === 1, textMap.get('x') === 'replacement', textMap.get('a long text key past the inline limit') === 'a long text key past the inline limit', s.has('x'), s.has('a long text key past the inline limit'), s.size === 2, m.get(0) === 1, m.get(NaN) === 2, m.get(key) === 3, m.get(f) === 7, m.has(), m.get() === undefined, m.delete(f), !m.has(f)]; let valid = true; for (const check of checks) { valid = valid && check; } await control.finish(valid);";
    let plain = plain_record_resume(source, u64::MAX, false);
    assert_eq!(plain.0, Datum::Bool(true));
    for slice in [1, 7, u64::MAX] {
        let restored = plain_record_resume(source, slice, true);
        assert_eq!((&plain.0, plain.1), (&restored.0, restored.1));
        assert!(restored.2 >= 2, "both effect parks were imported");
    }
}

/// ENTRY-CONV and FN-CROSS: a workflow Export and a packed frozen callable
/// preserve source results under the declared public argument contract.
#[test]
fn a_function_crossing_to_a_tool_uses_its_source_entry_convention() {
    for (function, packed, args, expected) in [
        (
            "async function f(n: number): Promise<number> { const shared = [1]; const fact = async (x: number): Promise<number> => x <= 1 ? shared[0] : x * (await fact(x - 1)); return await fact(n); }",
            false,
            vec![Datum::Float(lash_kernel_doc::Float::new(2.0))],
            Datum::Float(lash_kernel_doc::Float::new(2.0)),
        ),
        (
            "async function f(x: any, extra: any): any { return x; }",
            false,
            vec![Datum::Text("first".into()), Datum::Null],
            Datum::Text("first".into()),
        ),
        (
            "async function f(x?: any): any { return x; }",
            false,
            vec![],
            Datum::Null,
        ),
        (
            "async function f(): any { async function child() { return 7; } return child(); }",
            false,
            vec![],
            Datum::Float(lash_kernel_doc::Float::new(7.0)),
        ),
        (
            "function f(unused: any): any { return this === undefined; }",
            true,
            vec![Datum::Null],
            Datum::Bool(true),
        ),
        (
            "async function f(x: any, extra: any): any { function inner() { return arguments.length; } return inner(); }",
            false,
            vec![Datum::Null, Datum::Null],
            Datum::Float(lash_kernel_doc::Float::new(0.0)),
        ),
        (
            "function f(): any { return undefined; }",
            false,
            vec![],
            Datum::Null,
        ),
        // FIG-6822: JSON.stringify leaves out a member that is undefined
        // and spells an undefined element null.
        (
            "async function f(x?: any): any { return [x, { a: x, b: 1, c: [x] }]; }",
            false,
            vec![],
            Datum::List(vec![
                Datum::Null,
                Datum::Record(vec![
                    ("b".into(), Datum::Float(lash_kernel_doc::Float::new(1.0))),
                    ("c".into(), Datum::List(vec![Datum::Null])),
                ]),
            ]),
        ),
        (
            "async function f(x: any, extra: any): any { return [this === undefined, arguments.length, arguments[1], x]; }",
            true,
            vec![Datum::Null, Datum::Text("extra".into())],
            Datum::List(vec![
                Datum::Bool(true),
                Datum::Float(lash_kernel_doc::Float::new(2.0)),
                Datum::Text("extra".into()),
                Datum::Null,
            ]),
        ),
        (
            "async function f(x: any, extra: any): any { const read = () => arguments.length; return read(); }",
            true,
            vec![Datum::Null, Datum::Null],
            Datum::Float(lash_kernel_doc::Float::new(2.0)),
        ),
    ] {
        let (_, tail) = function
            .split_once("function f")
            .expect("a source function");
        let effects = effects();
        let environment_namespaces = crate::tests::linked::namespaces(&effects, super::controls())
            .expect("the test's effects are operations");
        let environment = Environment::new(super::library(), &environment_namespaces);
        let entry = crate::lower(
            &format!("export async function run{tail}"),
            &environment,
            lash_kernel_dialect::Mode::Module,
        )
        .expect("the same body imports as an entry")
        .document
        .into_document();
        let convention = super::linked_inspection::exported(&entry, &Name::new("run"))
            .convention
            .clone();
        let (_, result) = end_of_entry(entry, "run", args.clone());
        assert_eq!(result, expected, "workflow: {function}");
        let source = format!("{function} await host.echo(f);");
        let (mut machine, text) = start(&source);
        let mut host = Console::default();
        let function = loop {
            match machine
                .run(&mut host, u64::MAX)
                .unwrap_or_else(|e| panic!("{e}\n{text}"))
            {
                Step::Slice => {}
                Step::Parked(park) => {
                    let Request::Effect(effect) = &park.requests[0];
                    let Datum::FunctionValue(function) = &effect.args[0] else {
                        panic!("a frozen function: {:?}", effect.args);
                    };
                    break function.clone();
                }
                other => panic!("{other:?}\n{text}"),
            }
        };
        assert_eq!(
            matches!(
                convention,
                lash_kernel_doc::StartConvention::TsArgsList { .. }
            ),
            packed,
            "a workflow Export retains its selected ABI"
        );
        assert!(
            matches!(
                function.convention,
                lash_kernel_doc::StartConvention::TsArgsList { .. }
            ),
            "a frozen token retains its packed closure ABI"
        );
        let document = super::lower_with_effects(&source)
            .unwrap()
            .document
            .into_document();
        let program = super::linked::program(Arc::new(document), prepared().clone());
        let mut restarted = super::pages::start(
            program,
            BOUNDS,
            CellStart {
                target: Target::Callable(function),
                args,
                session: None,
                owner: super::pages::OWNER,
                modes: Default::default(),
                host: vec![],
            },
        )
        .unwrap();
        let Step::Ended(End::Finished(result)) =
            restarted.run(&mut Console::default(), u64::MAX).unwrap()
        else {
            panic!("callable result")
        };
        assert_eq!(result.result, expected, "{source}");
    }
}

/// FIG-7173: cell execution keeps JS semantics while crossing preserves admission evidence.
#[test]
fn cells_keep_javascript_text_semantics_and_ship_conversion_facts() {
    use lash_kernel_dialect::workflow::ImplicitTextConversionPolicy as Policy;
    use lash_kernel_vm::{FunctionOrigins, MachineOptions};
    let body = "const answer = async (event: { identity_id: string }): Promise<string> => `answered by ${event}`;";
    let source = format!("{body} console.log(await answer({{ identity_id: 'sam' }}));");
    let recorded = run(&source, &[]);
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["answered by [object Object]"]);

    let source = format!("{body} await control.finish(answer);");
    let lowered =
        super::lower_with_effects(&source).expect("a cell is never refused by source policy");
    let program =
        crate::tests::linked::program(Arc::clone(lowered.document.shared()), prepared().clone());
    let mut machine = super::pages::start(
        program.clone(),
        BOUNDS,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: super::pages::OWNER,
            modes: Default::default(),
            host: Vec::new(),
        },
    )
    .expect("start the cell");
    machine.set_options(MachineOptions {
        function_origins: FunctionOrigins {
            sites: lowered
                .written
                .iter()
                .map(|(site, written)| ((program.link, site.clone()), written.clone()))
                .collect(),
            environments: Default::default(),
        },
        ..Default::default()
    });
    let Step::Parked(park) = machine
        .run(&mut Console::default(), u64::MAX)
        .expect("cell executes")
    else {
        panic!("finish carries the callable");
    };
    let [Request::Effect(effect)] = park.requests.as_slice() else {
        panic!("only finish");
    };
    let [Datum::FunctionValue(function)] = effect.args.as_slice() else {
        panic!("the callable crosses as typed data");
    };
    assert_eq!(function.implicit_text_conversions.len(), 1);
    let diagnostic = Policy::Refuse
        .check(&function.implicit_text_conversions)
        .expect_err("promotion refuses the retained fact");
    assert_eq!(diagnostic.code(), "TS_IMPLICIT_TEXT_CONVERSION");
    let start = source.find("${event}").unwrap() + 2;
    assert_eq!(
        diagnostic.span(),
        Some(lash_kernel_doc::Span {
            start,
            end: start + 5
        })
    );
    let lash_kernel_dialect::DiagnosticReason::ImplicitTextConversion(conversion) =
        &diagnostic.reason
    else {
        panic!("implicit conversion reason");
    };
    assert_eq!(
        conversion.operand_type,
        lash_kernel_dialect::SourceOperandType::Record(
            [(
                "identity_id".into(),
                lash_kernel_dialect::SourceOperandType::Text
            )]
            .into()
        )
    );
    Policy::Allow
        .check(&function.implicit_text_conversions)
        .expect("the host can allow it");
}

/// TS16 / K-CHG / K-BND: amplifying integer operations bound their work
/// before allocating the enormous result, and a guest catch cannot hide it.
#[test]
fn bigint_work_is_metered_before_amplifying_allocations() {
    for expression in [
        "2n ** 100000000000000000000n",
        "1n << 100000000000000000000n",
        "BigInt.asUintN(1099511627776, -1n)",
    ] {
        let source = format!("try {{ {expression}; }} catch (e) {{ console.log('caught'); }}");
        let (mut machine, _) = start(&source);
        let mut console = Console::default();
        let step = machine.run(&mut console, u64::MAX).unwrap();
        let Step::Ended(End::Error(RunError::Bound(exceeded))) = step else {
            panic!("amplification must end at a bound: {expression}: {step:?}");
        };
        let power = lash_kernel_dialect::Library::resolve(super::library(), "int.pow").unwrap();
        assert_eq!(
            exceeded.bound,
            lash_kernel_vm::Bound::Guard { function: power },
            "{expression}"
        );
        assert!(console.lines.is_empty(), "a bound cannot be caught");
        assert!(
            machine.meters().memory < BOUNDS.memory,
            "reservations precede allocation"
        );
    }
}

/// FIG-7430 / K-BND-002: ordinary recursive source must use the standard depth.
#[test]
fn standard_depth_admits_five_thousand_guest_calls() {
    assert_eq!(
        end(
            "function descend(n) { if (n === 0) return 0; return 1 + descend(n - 1); } await control.finish(descend(5000));"
        ),
        Ended::Finished(Datum::Float(lash_kernel_doc::Float::new(5000.0)))
    );
}
