//! Runs a lowered cell on the kernel machine, with scripted tools.
//!
//! A run is recorded the way `witness/async/record.mjs` records Node: one
//! epoch per stretch between deliveries, with the outcomes delivered into
//! it, the tool calls and sleeps it requested and the lines it printed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::Environment;
use lash_kernel_doc::{
    Datum, EffectName, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Param, Signature,
    Timestamp, Type,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, RunError,
    Start, Step, Target, WaitId,
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
        for definition in define_helpers(&mut library).unwrap_or_else(|error| panic!("{error}")) {
            registry
                .register(definition, None)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        Arc::new(registry)
    })
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

/// Lowers `source` as a first cell and runs it to its end without a park.
pub(crate) fn end(source: &str) -> Ended {
    end_with_bindings(source, Bindings::default())
}

/// Runs a cell with kernel session data supplied by the law.
pub(crate) fn end_with_bindings(source: &str, bindings: Bindings) -> Ended {
    let (mut machine, text) = start_with_bindings(source, bindings);
    match machine.run(&mut Console::default(), u64::MAX) {
        Ok(Step::Ended(End::Finished(finished))) => Ended::Finished(finished.result),
        Ok(Step::Ended(End::Error(RunError::Uncaught(Datum::Error(error))))) => {
            Ended::Raised(error.kind)
        }
        other => panic!("{other:?}\n{text}"),
    }
}

/// Lowers `source` as a first cell and starts a machine on it. Gives the
/// machine and the document's kernel text.
fn start(source: &str) -> (KernelMachine, String) {
    start_with_bindings(source, Bindings::default())
}

fn start_with_bindings(source: &str, values: Bindings) -> (KernelMachine, String) {
    let library = super::library();
    let effects = effects();
    let bindings = values.variables.keys().cloned().collect::<BTreeSet<_>>();
    let environment = Environment {
        library,
        effects: &effects,
        bindings: &bindings,
    };
    let lowered = crate::lower(source, &environment).unwrap_or_else(|error| panic!("{error}"));
    let text = lash_kernel_doc::print_document(&lowered.document);
    if let Err(invalid) = lash_kernel_doc::validate_document(&lowered.document, library) {
        panic!("{invalid}\n{text}");
    }
    let program = Program {
        document: Arc::new(lowered.document),
        registry: Arc::clone(registry()),
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: values,
    };
    let machine =
        KernelMachine::start(program, BOUNDS, start).unwrap_or_else(|error| panic!("{error}"));
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
