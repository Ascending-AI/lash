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
