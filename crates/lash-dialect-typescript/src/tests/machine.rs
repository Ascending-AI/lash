//! Runs a lowered cell on the kernel machine, with scripted tools.
//!
//! A run is recorded the way `witness/async/record.mjs` records Node: one
//! epoch per stretch between deliveries, with the outcomes delivered into
//! it, the tool calls and sleeps it requested and the lines it printed.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::Environment;
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Param, Signature,
    Timestamp, Type,
};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, RunError, Step, Target, WaitId,
};
use serde::Deserialize;

use crate::define_helpers;

/// The real kernel registry, with every dialect helper defined against it.
pub(super) fn registry() -> &'static Arc<FunctionRegistry> {
    static REGISTRY: OnceLock<Arc<FunctionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = super::kernel_registry();
        let mut library = lash_kernel_dialect::NamedLibrary::from_registry(&registry)
            .expect("unique kernel names");
        let mut helpers = Vec::new();
        for definition in define_helpers(&mut library).unwrap_or_else(|error| panic!("{error}")) {
            helpers.push(
                registry
                    .register(definition, None)
                    .unwrap_or_else(|error| panic!("{error}")),
            );
        }
        // The operations' natives, as a worker installs them.
        lash_ext_ts_core::implement(&mut registry, helpers).expect("operation natives");
        Arc::new(registry)
    })
}

/// [`registry`] with its library bodies compiled, once for every run.
pub(super) fn prepared() -> &'static PreparedLibrary {
    static PREPARED: OnceLock<PreparedLibrary> = OnceLock::new();
    PREPARED.get_or_init(|| PreparedLibrary::new(Arc::clone(registry())))
}

/// The tools a cell may call: `echo(x)` answers `x` and `boom(x)` fails
/// with an error of kind `boom` whose message is `x`.
pub(crate) fn effects() -> BTreeMap<EffectName, Signature> {
    ["echo", "boom", "finish"]
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
    charge: 10_000_000,
    memory: 64 << 20,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Default)]
struct Console {
    lines: Vec<String>,
    random_bits: std::collections::VecDeque<u64>,
}

impl Host for Console {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        self.random_bits.pop_front().unwrap_or(0)
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

/// What a request is called in a delivery script: `echo:a`, `boom:x`,
/// `sleep:50`.
fn label(request: &Request) -> String {
    match request {
        Request::Effect(effect) => {
            let argument = match effect.args.first() {
                Some(Datum::Text(text)) => text.clone(),
                Some(Datum::Float(value)) => value.get().to_string(),
                other => format!("{other:?}"),
            };
            format!("{}:{argument}", effect.effect)
        }
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
        End::Error(RunError::Uncaught(Datum::Text(text))) => format!("error {text}"),
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
                assert_eq!(effect.effect.as_str(), "finish", "{text}");
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
            Ok(Step::Ended(End::Error(RunError::Uncaught(Datum::Error(error))))) => {
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

fn start_with_bindings(source: &str, values: super::pages::Session) -> (KernelMachine, String) {
    let library = super::library();
    let effects = effects();
    let bindings = values.names();
    let environment = Environment {
        tool_roots: &std::collections::BTreeSet::new(),
        library,
        effects: &effects,
        controls: super::controls(),
    };
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
    let program = Program {
        release: super::pages::RELEASE,
        document: Arc::clone(lowered.document.shared()),
        library: prepared().clone(),
    };
    let start = CellStart {
        target: Target::Main,
        args: Vec::new(),
        session: values.root,
        owner: super::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    };
    let machine = super::pages::with_source(
        program,
        BOUNDS,
        start,
        lash_kernel_vm::Layout::default(),
        &values.source,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    (machine, text)
}

/// FIG-6368, K-HOST-003: direct and borrowed Math.random calls preserve
/// the kernel's [0, 1) draws, including values in the upper half.
#[test]
fn math_random_preserves_the_kernel_unit_interval_for_direct_and_borrowed_calls() {
    let (mut machine, text) = start(
        "const draw = Math.random; const values = []; for (let i = 0; i < 4; i++) { values.push(Math.random()); values.push(draw()); } await finish(values);",
    );
    let mut console = Console {
        random_bits: [0, 1 << 62, 1 << 63, u64::MAX]
            .into_iter()
            .flat_map(|bits| [bits, bits])
            .collect(),
        ..Console::default()
    };
    let Step::Parked(park) = machine.run(&mut console, u64::MAX).expect("random draws") else {
        panic!("the cell must finish with its draws\n{text}");
    };
    let [Request::Effect(effect)] = park.requests.as_slice() else {
        panic!("only finish is requested: {park:?}");
    };
    assert_eq!(effect.effect.as_str(), "finish");
    let [Datum::List(values)] = effect.args.as_slice() else {
        panic!("finish carries the draws: {:?}", effect.args);
    };
    assert_eq!(values.len(), 8);
    assert!(console.random_bits.is_empty(), "all fixed draws were used");
    let draws: Vec<f64> = values
        .iter()
        .map(|value| {
            let Datum::Float(value) = value else {
                panic!("Math.random must return a float: {value:?}");
            };
            let value = value.get();
            assert!((0.0..1.0).contains(&value), "draw outside [0, 1): {value}");
            value
        })
        .collect();
    for (offset, form) in [(0, "direct"), (1, "borrowed")] {
        assert!(
            draws
                .iter()
                .skip(offset)
                .step_by(2)
                .any(|value| *value >= 0.5),
            "{form} Math.random must reach the upper half: {draws:?}"
        );
    }
    for pair in draws.as_chunks::<2>().0 {
        assert_eq!(pair[0], pair[1], "the same host bits give the same draw");
    }
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

/// Executes with a fresh serialized import at slices and parks. Neither
/// source facts nor operand metadata may be required by resumed execution.
pub(super) fn plain_record_resume(source: &str, slice: u64, restore: bool) -> (Datum, u64, usize) {
    let program = Program {
        release: super::pages::RELEASE,
        document: Arc::clone(
            super::lower_with_effects(source)
                .expect("law source")
                .document
                .shared(),
        ),
        library: prepared().clone(),
    };
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
        lash_kernel_vm::Layout::default(),
        &source_pages,
    )
    .unwrap();
    if !restore {
        machine.count_work();
    }
    let mut result = None;
    let mut imports = 0;
    loop {
        let step = machine
            .run(&mut Console::default(), slice)
            .expect("law run");
        if restore && matches!(step, Step::Slice | Step::Parked(_)) {
            super::pages::reopen(
                &mut machine,
                &program,
                BOUNDS,
                &source_pages,
                lash_kernel_vm::Layout::default(),
            );
            imports += 1;
        }
        match step {
            Step::Slice => {}
            Step::Parked(park) => {
                for request in park.requests {
                    let Request::Effect(effect) = request else {
                        panic!("effect only")
                    };
                    let value = effect.args.first().cloned().unwrap_or(Datum::Null);
                    if effect.effect.as_str() == "finish" {
                        result = Some(value.clone());
                    }
                    machine
                        .deliver(effect.wait, Outcome::Completed(value))
                        .expect("deliver");
                }
            }
            Step::Ended(End::Finished(_)) => {
                let charged = machine.meters().charged;
                if !restore {
                    let work = machine.work().expect("work counted");
                    assert_eq!(work.charged, charged);
                    assert_eq!(work.residual(), 0);
                    assert_eq!(work.insertion_residual(), 0);
                }
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
    let source = "const r = { next: { leaf: 0 } }; r.next.leaf = await echo(7); await finish(r);";
    let text = super::main_text_with_effects(source);
    assert!(text.contains("__ts_brand"), "{text}");
    let plain = plain_record_resume(source, u64::MAX, false);
    let restored = plain_record_resume(source, u64::MAX, true);
    assert_eq!((&plain.0, plain.1), (&restored.0, restored.1));
    assert_eq!(restored.2, 2, "echo and finish parks were imported");
    assert_eq!(
        end("const r = { next: { leaf: 7 } }; await finish(r);"),
        Ended::Finished(plain.0)
    );
}

/// K-CHG-007, K-MACH-007, TS-PLAIN-PATH: direct member work has zero
/// accounting residual and the same exact charge over serialized slices.
#[test]
fn plain_record_paths_account_charge_across_slice_resume() {
    let source = "const r = { next: { leaf: 0 } }; for (let n = 0; n < 3; n++) { r.next.leaf = n; } await finish(r);";
    let text = super::main_text_with_effects(source);
    assert!(!text.contains("__ts_brand"), "{text}");
    let plain = plain_record_resume(source, u64::MAX, false);
    let sliced = plain_record_resume(source, 1, true);
    assert_eq!((&plain.0, plain.1), (&sliced.0, sliced.1));
    assert!(sliced.2 > 10, "multiple member steps were imported");
}

/// TS-NUMERIC-ENTRY: one float admission per recursive call replaces
/// per-operation coercion, without changing the result or exact accounting.
#[test]
fn numeric_recursive_calls_admit_once_and_account_real_work() {
    let source = "function fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); } await finish(fib(10));";
    let (mut machine, _) = start(source);
    machine.count_work();
    let mut result = None;
    loop {
        match machine.run(&mut Console::default(), u64::MAX).expect("run") {
            Step::Parked(park) => {
                for request in park.requests {
                    let Request::Effect(effect) = request else {
                        panic!("effect")
                    };
                    let value = effect.args.first().cloned().unwrap_or(Datum::Null);
                    result = Some(value.clone());
                    machine
                        .deliver(effect.wait, Outcome::Completed(value))
                        .expect("deliver");
                }
            }
            Step::Ended(End::Finished(_)) => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        result,
        Some(Datum::Float(lash_kernel_doc::Float::new(55.0)))
    );
    let work = machine.work().expect("counted");
    assert_eq!(work.residual(), 0);
    assert_eq!(work.insertion_residual(), 0);
    assert_eq!(
        work.natives
            .iter()
            .find(|native| native.symbol.name == "kind")
            .expect("kind admission")
            .counts
            .calls,
        178,
        "177 recursive invocations plus the finish boundary"
    );
    for code in &work.codes {
        assert!(
            !matches!(code.symbol.name.as_str(), "ts.lt" | "ts.sub" | "ts.add"),
            "numeric recursion executed {}",
            code.symbol.name
        );
    }
}

/// TS-NUMERIC-POSITIONAL: direct recursive entries admit their normalized
/// parameter, rather than bypassing the numeric body through the packed entry.
#[test]
fn numeric_entry_positional_recursion_executes_no_coercion_helpers() {
    let (mut machine, _) = start(
        "function fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); } console.log(fib(10));",
    );
    machine.count_work();
    let mut console = Console::default();
    assert!(matches!(
        machine.run(&mut console, u64::MAX).expect("run"),
        Step::Ended(End::Finished(_))
    ));
    assert_eq!(console.lines, ["55"]);
    let work = machine.work().expect("counted");
    assert_eq!(work.residual(), 0);
    assert_eq!(work.insertion_residual(), 0);
    for code in &work.codes {
        assert!(
            !matches!(code.symbol.name.as_str(), "ts.lt" | "ts.sub" | "ts.add"),
            "positional recursion executed {}",
            code.symbol.name
        );
    }
}

/// TS-NUMERIC-PARK: the entry proof is code, with no transient facts needed
/// by a serialized continuation stopped inside the loop.
#[test]
fn numeric_entry_loop_resumes_with_identical_result_and_charge() {
    let source = "function step(n) { return n + 1; } let s = 0; for (let i = 0; i < 3; i++) { await echo(i); s = step(s); } await finish(s);";
    let plain = plain_record_resume(source, u64::MAX, false);
    let restored = plain_record_resume(source, u64::MAX, true);
    assert_eq!((&plain.0, plain.1), (&restored.0, restored.1));
    assert_eq!(restored.2, 4);
    assert_eq!(plain.0, Datum::Float(lash_kernel_doc::Float::new(3.0)));
    let guarded = source.replace("let s = 0", "const escaped = step; let s = 0");
    assert_eq!(plain_record_resume(&guarded, u64::MAX, true).0, plain.0);
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
            "for (let i = 0; i < {iterations}; i++) {{ const middle = {parent}{half}; middle{rest}.leaf = i; }} await finish({parent});"
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
    let source = "const key = {}; const f = () => 1; const m = new Map(); m.set(-0, 1); m.set(NaN, 2); m.set(key, 3); m.set(f, await echo(7)); m.set(); key.x = 9; const checks = [m.size === 5, m.get(0) === 1, m.get(NaN) === 2, m.get(key) === 3, m.get(f) === 7, m.has(), m.get() === undefined, m.delete(f), !m.has(f)]; let valid = true; for (const check of checks) { valid = valid && check; } await finish(valid);";
    let plain = plain_record_resume(source, u64::MAX, false);
    assert_eq!(plain.0, Datum::Bool(true));
    for slice in [1, 7, u64::MAX] {
        let restored = plain_record_resume(source, slice, true);
        assert_eq!((&plain.0, plain.1), (&restored.0, restored.1));
        assert!(restored.2 >= 2, "both effect parks were imported");
    }
}

/// FIG-6528: Array.sort reaches the stable logarithmic kernel sort on a
/// 1,000-record page, including ties and a seeded adversarial ordering.
#[test]
fn sort_comparisons_are_bounded_by_n_log_n() {
    for rank in [
        "1000 - i",
        "(seed = (seed * 1664525 + 1013904223) % 4294967296) % 31",
        "i",
        "0",
    ] {
        let source = format!(
            r#"
            const xs = []; let seed = 6528;
            for (let i = 0; i < 1000; i++) {{ xs.push({{key: {rank}, pos: i}}); }}
            let calls = 0;
            xs.sort((a, b) => {{ calls++; return a.key - b.key; }});
            let valid = true;
            for (let i = 1; i < 1000; i++) {{
                if (xs[i-1].key > xs[i].key ||
                    (xs[i-1].key === xs[i].key && xs[i-1].pos > xs[i].pos)) {{ valid = false; }}
            }}
            console.log(valid && xs.length === 1000 && calls <= 9966);
            console.log(calls);
        "#
        );
        let (mut machine, _) = start(&source);
        let mut console = Console::default();
        assert!(matches!(
            machine.run(&mut console, u64::MAX).unwrap(),
            Step::Ended(End::Finished(_))
        ));
        eprintln!(
            "typescript rank={rank} n=1000 comparisons={} charge={}",
            console.lines[1],
            machine.meters().charged
        );
        assert_eq!(console.lines[0], "true");
    }
}
