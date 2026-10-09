//! Runs a lowered cell on the kernel machine, with scripted tools.
//!
//! The kernel library crate does not hold the functions the helpers call
//! yet (`provisional`), so this module stands natives in for them: enough
//! of each to run the laws of this crate, and no more. When the library
//! lands its registry replaces [`registry`].
//!
//! A run is recorded the way `witness/async/record.mjs` records Node: one
//! epoch per stretch between deliveries, with the outcomes delivered into
//! it, the tool calls and sleeps it requested and the lines it printed.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::{Arc, OnceLock};

use lash_kernel_dialect::Environment;
use lash_kernel_doc::{
    Datum, EffectName, Element, ErrorDatum, ErrorValue, Float, FunctionCatalog, FunctionRegistry,
    Handle, Integer, Name, NativeCall, NativeError, NativeFunction, NativeHeap, Object, Param,
    Signature, Timestamp, Type, Value,
};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, RunError,
    Start, Step, Target, WaitId,
};
use serde::Deserialize;

use crate::{define_helpers, provisional};

type Native = fn(&mut NativeCall<'_>) -> Result<Value, NativeError>;

struct StandIn(Native);

impl NativeFunction for StandIn {
    fn call(&self, mut call: NativeCall<'_>) -> Result<Value, NativeError> {
        (self.0)(&mut call)
    }
}

fn raise(message: &str) -> NativeError {
    NativeError::Raised(ErrorValue::new("type_error", message))
}

fn number(value: &Value) -> Result<f64, NativeError> {
    match value {
        Value::Float(float) => Ok(float.get()),
        Value::Int(integer) => integer
            .to_string()
            .parse()
            .map_err(|_| raise("an integer the stand-in cannot read")),
        _ => Err(raise("expected a number")),
    }
}

fn text(value: &Value) -> Result<&str, NativeError> {
    match value {
        Value::Text(text) => Ok(text),
        _ => Err(raise("expected a text")),
    }
}

fn float(value: f64) -> Value {
    Value::Float(Float::new(value))
}

fn int(value: usize) -> Value {
    Value::Int(Integer::from(i64::try_from(value).unwrap_or(i64::MAX)))
}

/// Arithmetic that stays an integer on two integers.
fn arithmetic(call: &NativeCall<'_>, apply: fn(f64, f64) -> f64) -> Result<Value, NativeError> {
    let result = apply(number(&call.args[0])?, number(&call.args[1])?);
    match (&call.args[0], &call.args[1]) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the laws' integers are small"
        )]
        (Value::Int(_), Value::Int(_)) => Ok(Value::Int(Integer::from(result as i64))),
        _ => Ok(float(result)),
    }
}

fn arithmetic_one(call: &NativeCall<'_>, apply: fn(f64) -> f64) -> Result<Value, NativeError> {
    let result = apply(number(&call.args[0])?);
    match &call.args[0] {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the laws' integers are small"
        )]
        Value::Int(_) => Ok(Value::Int(Integer::from(result as i64))),
        _ => Ok(float(result)),
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Absent => "absent",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Bytes(_) => "bytes",
        Value::Timestamp(_) => "timestamp",
        Value::Tuple(_) => "tuple",
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::Set(_) => "set",
        Value::Record(_) => "record",
        Value::Closure(_) => "closure",
        Value::Error(_) => "error",
        Value::Task(_) => "task",
        Value::Function(_) => "function",
        Value::Handle(_) => "handle",
        Value::Ref(_) => "ref",
    }
}

fn equal(left: &Value, right: &Value) -> bool {
    match (number(left), number(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// The stand-in for one kernel library function, or `None` where no law
/// of this crate reaches it.
fn native(name: &str) -> Option<Native> {
    Some(match name {
        "kind" => |call| Ok(Value::text(kind(&call.args[0]))),
        "same" => |call| Ok(Value::Bool(call.args[0] == call.args[1])),
        "eq" => |call| Ok(Value::Bool(equal(&call.args[0], &call.args[1]))),
        "bool.not" => |call| match call.args[0] {
            Value::Bool(value) => Ok(Value::Bool(!value)),
            _ => Err(raise("expected a bool")),
        },
        "error.new" => |call| {
            Ok(Value::Error(Arc::new(ErrorValue {
                kind: text(&call.args[0])?.to_string(),
                message: text(&call.args[1])?.to_string(),
                data: call.args[2].clone(),
            })))
        },
        "num.add" => |call| arithmetic(call, |a, b| a + b),
        "num.sub" => |call| arithmetic(call, |a, b| a - b),
        "num.mul" => |call| arithmetic(call, |a, b| a * b),
        "num.div" => |call| Ok(float(number(&call.args[0])? / number(&call.args[1])?)),
        "num.neg" => |call| arithmetic_one(call, |a| -a),
        "num.floor" => |call| arithmetic_one(call, f64::floor),
        "num.is_finite" => |call| Ok(Value::Bool(number(&call.args[0])?.is_finite())),
        "text.trim" => |call| Ok(Value::text(text(&call.args[0])?.trim())),
        "text.to_num" => |call| {
            text(&call.args[0])?
                .parse()
                .map(float)
                .map_err(|_| raise("not a number"))
        },
        "num.lt" => |call| Ok(Value::Bool(number(&call.args[0])? < number(&call.args[1])?)),
        "num.le" => |call| {
            Ok(Value::Bool(
                number(&call.args[0])? <= number(&call.args[1])?,
            ))
        },
        "num.is_nan" => |call| Ok(Value::Bool(number(&call.args[0])?.is_nan())),
        "num.to_float" => |call| Ok(float(number(&call.args[0])?)),
        "num.to_text" => |call| match &call.args[0] {
            Value::Float(value) => Ok(Value::text(value.to_string())),
            Value::Int(value) => Ok(Value::text(value.to_string())),
            _ => Err(raise("expected a number")),
        },
        "text.concat" => |call| {
            Ok(Value::text(format!(
                "{}{}",
                text(&call.args[0])?,
                text(&call.args[1])?
            )))
        },
        "text.len" => |call| Ok(int(text(&call.args[0])?.chars().count())),
        "text.slice" => |call| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "positions inside a short text"
            )]
            let (start, end) = (
                number(&call.args[1])? as usize,
                number(&call.args[2])? as usize,
            );
            let slice: String = text(&call.args[0])?
                .chars()
                .skip(start)
                .take(end.saturating_sub(start))
                .collect();
            Ok(Value::text(slice))
        },
        "text.ends_with" => |call| {
            Ok(Value::Bool(
                text(&call.args[0])?.ends_with(text(&call.args[1])?),
            ))
        },
        "list.len" => |call| match call.args[0].object() {
            Some(object) => Ok(int(call.heap.len(object))),
            None => Err(raise("expected a collection")),
        },
        "record.get" => |call| match &call.args[0] {
            Value::Record(record) => Ok(call
                .heap
                .record_get(*record, text(&call.args[1])?)
                .unwrap_or(Value::Absent)),
            _ => Err(raise("expected a record")),
        },
        "record.has" => |call| match &call.args[0] {
            Value::Record(record) => Ok(Value::Bool(
                call.heap
                    .record_get(*record, text(&call.args[1])?)
                    .is_some(),
            )),
            _ => Err(raise("expected a record")),
        },
        "record.keys" => |call| match &call.args[0] {
            Value::Record(record) => {
                let mut keys = Vec::new();
                call.heap.visit(*record, &mut |element| {
                    if let Element::Field { name, .. } = element {
                        keys.push(Value::text(name));
                    }
                    ControlFlow::Continue(())
                });
                call.heap.allocate(Object::List(keys)).map(Value::List)
            }
            _ => Err(raise("expected a record")),
        },
        "json.stringify" => |call| {
            let mut out = String::new();
            stringify(&call.args[0], &*call.heap, &mut out)?;
            Ok(Value::text(out))
        },
        _ => return None,
    })
}

/// Compact JSON of texts, numbers, bools, null, lists and records, which
/// is all the laws print.
fn stringify(value: &Value, heap: &dyn NativeHeap, out: &mut String) -> Result<(), NativeError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Text(text) => out.push_str(&serde_json::Value::from(&**text).to_string()),
        Value::Int(_) | Value::Float(_) => out.push_str(&number(value)?.to_string()),
        Value::List(list) | Value::Record(list) => {
            let record = matches!(value, Value::Record(_));
            let mut members = Vec::new();
            heap.visit(*list, &mut |element| {
                match element {
                    Element::Item(item) => members.push((None, item.clone())),
                    Element::Field { name, value } => {
                        members.push((Some(name.to_string()), value.clone()));
                    }
                    Element::Entry { .. } => {}
                }
                ControlFlow::Continue(())
            });
            out.push(if record { '{' } else { '[' });
            for (index, (name, member)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if let Some(name) = name {
                    out.push_str(&serde_json::Value::from(name.as_str()).to_string());
                    out.push(':');
                }
                stringify(member, heap, out)?;
            }
            out.push(if record { '}' } else { ']' });
        }
        _ => return Err(raise("the stand-in writes only plain data as JSON")),
    }
    Ok(())
}

/// The stand-in kernel library with the dialect's helpers, as the registry
/// a machine runs with and the library the front end lowers against.
fn registry() -> &'static Arc<FunctionRegistry> {
    static REGISTRY: OnceLock<Arc<FunctionRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut library = provisional::kernel_library();
        let mut registry = FunctionRegistry::new();
        let kernel: Vec<_> = library
            .iter()
            .map(|(name, function)| (name.to_string(), *function))
            .collect();
        for (name, function) in kernel {
            let definition = library
                .definition(&function)
                .expect("a listed function has a definition")
                .clone();
            let native = native(&name).unwrap_or(|_| Err(raise("not in the stand-in library")));
            registry
                .register(definition, Some(Arc::new(StandIn(native))))
                .unwrap_or_else(|error| panic!("{error}"));
        }
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
        End::Error(RunError::Uncaught(error)) if error.kind == "thrown" => match error.data {
            Datum::Text(text) => format!("error {text}"),
            other => format!("error {other:?}"),
        },
        End::Error(RunError::Uncaught(error)) => {
            format!("error {}: {}", error.kind, error.message)
        }
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

/// Lowers `source` as a first cell and runs it to its end. At each park the
/// next batch of `deliveries` is delivered, in the order it lists; once the
/// script is used up, the oldest pending request.
pub(crate) fn run(source: &str, deliveries: &[Vec<String>]) -> Recorded {
    let library = super::library();
    let effects = effects();
    let bindings = BTreeSet::new();
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
        bindings: Bindings::default(),
    };
    let mut machine =
        KernelMachine::start(program, BOUNDS, start).unwrap_or_else(|error| panic!("{error}"));
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
