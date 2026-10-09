//! The dialect against CPython, on the arc's shared capture.
//!
//! `witness/captured/<case>.py` is a program in the shape
//! `lash-kernel-conformance/witness/capture.py` runs: `async def
//! main(tool, output)`. `<case>.tools.json` scripts its tool calls, by
//! issue order, and the batches they are answered in, and
//! `<case>.python.json` is what the capture script recorded CPython doing:
//! the values given to `output`, the order tool calls were issued and
//! answered in, and what `main` returned or raised.
//!
//! Each law here lowers the program unchanged, with an adapter that gives
//! it `tool` and `output`, runs it on the kernel machine under the same
//! script and requires the same typed record.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, EffectName, ErrorDatum, Name, Param, Signature, Type};
use lash_kernel_vm::{Bindings, End, Outcome, Request};
use serde::Deserialize;
use serde_json::Value as Json;

use super::machine;

/// Gives the program its two arguments and hands the host what it did:
/// `tool` runs the host's tool of that name, `output` keeps each value as
/// it is when given, as the capture script does, and the end of `main` is
/// reported with them in one last call.
const ADAPTER: &str = "
adapter_outputs = []
async def adapter_tool(name, argument):
    return await host_tool(name, argument)
def adapter_copy(value):
    if isinstance(value, list):
        return [adapter_copy(item) for item in value]
    if isinstance(value, tuple):
        return tuple([adapter_copy(item) for item in value])
    return value
def adapter_output(value):
    adapter_outputs.append(adapter_copy(value))
try:
    adapter_end = ('returned', await main(adapter_tool, adapter_output))
except Exception as adapter_error:
    adapter_end = ('raised', type(adapter_error).__name__, str(adapter_error))
await host_end(adapter_outputs, adapter_end)
";

#[derive(Deserialize)]
struct Script {
    tools: Vec<Row>,
    deliveries: Vec<Vec<usize>>,
}

#[derive(Deserialize)]
struct Row {
    name: String,
    args: Vec<Json>,
    value: Option<Json>,
    error: Option<RowError>,
}

#[derive(Deserialize)]
struct RowError {
    kind: String,
    message: String,
}

#[derive(Debug, PartialEq, Deserialize)]
struct Capture {
    end: Ended,
    prints: Vec<Datum>,
    trace: Vec<Event>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Ended {
    Returned(Datum),
    Raised { kind: String, message: String },
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum Event {
    Requested {
        id: usize,
        tool: String,
        args: Vec<Datum>,
    },
    Delivered {
        id: usize,
    },
}

/// A scripted value as the datum a tool answers with.
fn datum(value: &Json) -> Datum {
    match value {
        Json::Null => Datum::Null,
        Json::Bool(value) => Datum::Bool(*value),
        Json::String(text) => Datum::Text(text.clone()),
        Json::Number(number) => match number.as_i64() {
            Some(integer) => Datum::Int(integer.into()),
            None => panic!("a scripted number is an integer: {number}"),
        },
        Json::Array(items) => Datum::List(items.iter().map(datum).collect()),
        Json::Object(_) => panic!("a scripted value is not an object"),
    }
}

fn effects() -> BTreeMap<EffectName, Signature> {
    let param = |name: &str, ty: Type| Param {
        name: Name::new(name),
        ty,
        optional: false,
    };
    let signature = |params: Vec<Param>| Signature {
        params,
        result: Type::Any,
    };
    BTreeMap::from([
        (
            EffectName::new("host_tool").expect("a tool's name"),
            signature(vec![
                param("name", Type::Text),
                param("argument", Type::Any),
            ]),
        ),
        (
            EffectName::new("host_end").expect("a tool's name"),
            signature(vec![param("outputs", Type::Any), param("end", Type::Any)]),
        ),
    ])
}

fn text(datum: &Datum) -> String {
    match datum {
        Datum::Text(text) => text.clone(),
        other => panic!("expected text, found {other:?}"),
    }
}

/// Runs `source` on the kernel machine under `script` and records it as
/// the capture script records CPython.
fn on_the_kernel(source: &str, script: &Script) -> Capture {
    let mut issued: Vec<Request> = Vec::new();
    let mut batches = script.deliveries.iter();
    let mut trace = Vec::new();
    let mut reported = None;
    let cell = format!("{source}{ADAPTER}");
    let (_, end) = machine::drive(&cell, effects(), Bindings::default(), |_, park, text_| {
        assert!(
            park.withdrawn.is_empty(),
            "the capture script delivers every call"
        );
        let mut outcomes = Vec::new();
        for request in park.requests {
            let Request::Effect(effect) = &request else {
                panic!("a captured case does not sleep\n{text_}");
            };
            if effect.effect.as_str() == "host_end" {
                reported = Some((effect.args[0].clone(), effect.args[1].clone()));
                outcomes.push((effect.wait, Outcome::Completed(Datum::Null)));
                continue;
            }
            let id = issued.len();
            let row = script
                .tools
                .get(id)
                .unwrap_or_else(|| panic!("tool call {id} is not scripted\n{text_}"));
            let tool = text(&effect.args[0]);
            let args = effect.args[1..].to_vec();
            assert_eq!(
                (tool.as_str(), &args),
                (row.name.as_str(), &row.args.iter().map(datum).collect())
            );
            trace.push(Event::Requested { id, tool, args });
            issued.push(request);
        }
        if !outcomes.is_empty() {
            return outcomes;
        }
        let batch = batches
            .next()
            .unwrap_or_else(|| panic!("the run waits past its script\n{text_}"));
        for id in batch {
            let Some(Request::Effect(effect)) = issued.get(*id) else {
                panic!("call {id} is delivered before it is issued\n{text_}");
            };
            let row = &script.tools[*id];
            let outcome = match (&row.value, &row.error) {
                (_, Some(error)) => Outcome::Failed(ErrorDatum {
                    kind: error.kind.clone(),
                    message: error.message.clone(),
                    data: Datum::Null,
                }),
                (Some(value), None) => Outcome::Completed(datum(value)),
                (None, None) => Outcome::Completed(Datum::Null),
            };
            trace.push(Event::Delivered { id: *id });
            outcomes.push((effect.wait, outcome));
        }
        outcomes
    });
    assert!(matches!(end, End::Finished(_)), "{end:?}");
    assert!(batches.next().is_none(), "the script has batches left over");
    let (outputs, end) = reported.expect("the adapter reports the end of `main`");
    let Datum::List(prints) = outputs else {
        panic!("the outputs are a list");
    };
    let end = match end {
        Datum::Tuple(parts) => match parts.as_slice() {
            [_, value] => Ended::Returned(value.clone()),
            [_, kind, message] => Ended::Raised {
                kind: text(kind),
                message: text(message),
            },
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    Capture { end, prints, trace }
}

macro_rules! captured {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let source = include_str!(concat!(
                    "../../witness/captured/", stringify!($name), ".py"
                ));
                let script: Script = serde_json::from_str(include_str!(concat!(
                    "../../witness/captured/", stringify!($name), ".tools.json"
                )))
                .expect("a script of the capture tool");
                let cpython: Capture = serde_json::from_str(include_str!(concat!(
                    "../../witness/captured/", stringify!($name), ".python.json"
                )))
                .expect("a capture of the capture tool");
                assert_eq!(on_the_kernel(source, &script), cpython);
            }
        )*
    };
}

captured!(
    truthiness,
    integers,
    aliasing,
    nonlocal,
    tuple_key,
    key_error,
    keyword,
    dict_mutation,
    fstring,
    gather,
    cleanup_wait,
);
