//! A tool's reply as a cell reads it (FIG-5904).
//!
//! A tool answers with JSON, which reaches the machine with every object a
//! kernel record. A Python cell reads it as `json.loads` gives it: an
//! object is a dict and an array a list, at every depth. The oracle of each
//! law is CPython running the same cell with tools that answer
//! `json.loads` of the same JSON text (`.kiln/FIG-5904/oracle.py` in the
//! lane's evidence).

use std::collections::VecDeque;

use lash_kernel_doc::{Datum, NumberToken};
use lash_kernel_vm::{End, Outcome, Request};

use super::machine;

fn object(fields: &[(&str, Datum)]) -> Datum {
    Datum::Record(
        fields
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect(),
    )
}

fn number(text: &str) -> Datum {
    Datum::Number(NumberToken::new(text).expect("a JSON number"))
}

fn text(text: &str) -> Datum {
    Datum::Text(text.to_string())
}

/// The lines `source` prints when `echo(x)` answers the reply `replies`
/// lists for `x`, each call as soon as it is asked.
fn printed(source: &str, replies: &[(&str, Datum)]) -> Vec<String> {
    let mut pending: VecDeque<Request> = VecDeque::new();
    let mut lines = Vec::new();
    let (last, end) = machine::drive(
        source,
        machine::effects(),
        super::pages::Session::default(),
        |logged, park, text| {
            lines.extend(logged);
            pending.extend(park.requests);
            let request = pending
                .pop_front()
                .unwrap_or_else(|| panic!("a park with nothing pending\n{text}"));
            let Request::Effect(effect) = request else {
                panic!("the cell does not sleep\n{text}");
            };
            let Some(Datum::Text(asked)) = effect.args.first() else {
                panic!("echo is asked for a text\n{text}");
            };
            let (_, reply) = replies
                .iter()
                .find(|(name, _)| name == asked)
                .unwrap_or_else(|| panic!("no reply for `{asked}`\n{text}"));
            vec![(effect.wait, Outcome::Completed(reply.clone()))]
        },
    );
    lines.extend(last);
    assert!(matches!(end, End::Finished(_)), "{end:?}\n{lines:?}");
    lines
}

/// CPython, with `{"a":"v"}` and `{}`: `v 1 v False`.
#[test]
fn a_tool_object_is_a_dict() {
    let source = "\
r = await echo('x')
print(r['a'], len(r), r.get('a'), bool(await echo('y')))
";
    let replies = [("x", object(&[("a", text("v"))])), ("y", object(&[]))];
    assert_eq!(printed(source, &replies), ["v 1 v False"]);
}

/// CPython, with `{"outer":{"inner":1,"z":null},"list":[{"b":"w"},2]}`.
#[test]
fn a_nested_tool_object_is_a_dict_at_every_depth() {
    let source = "\
r = await echo('n')
print(r)
print(r['outer']['inner'], type(r['outer']).__name__, list(r), list(r['outer'].items()), 'outer' in r, r['outer'] == {'inner': 1, 'z': None})
print(r['list'][0]['b'], type(r['list'][0]).__name__, r.get('missing', 'd'), [k for k in r], list(r.values())[1])
r['outer']['added'] = True
print(len(r['outer']), sorted(r['outer'].keys()))
";
    let reply = object(&[
        (
            "outer",
            object(&[("inner", number("1")), ("z", Datum::Null)]),
        ),
        (
            "list",
            Datum::List(vec![object(&[("b", text("w"))]), number("2")]),
        ),
    ]);
    assert_eq!(
        printed(source, &[("n", reply)]),
        [
            "{'outer': {'inner': 1, 'z': None}, 'list': [{'b': 'w'}, 2]}",
            "1 dict ['outer', 'list'] [('inner', 1), ('z', None)] True True",
            "w dict d ['outer', 'list'] [{'b': 'w'}, 2]",
            "3 ['added', 'inner', 'z']",
        ]
    );
}

/// CPython, with `[{"k":1},[{"m":{}}]]`, `{"a":[]}` and `{"g":1}`: a list
/// reply holds dicts, and a reply a task or a gather gives is a dict too.
#[test]
fn a_tool_array_is_a_list_of_dicts_however_it_is_awaited() {
    let source = "\
import asyncio
xs = await echo('l')
print(type(xs).__name__, xs[0]['k'], type(xs[1][0]['m']).__name__, bool(xs[1][0]['m']), len(xs[1][0]))
task = asyncio.create_task(echo('t'))
r = await task
print(type(r).__name__, r['a'], bool(r['a']))
g, h = await asyncio.gather(echo('g'), echo('t'))
print(g['g'], len(h))
";
    let replies = [
        (
            "l",
            Datum::List(vec![
                object(&[("k", number("1"))]),
                Datum::List(vec![object(&[("m", object(&[]))])]),
            ]),
        ),
        ("t", object(&[("a", Datum::List(Vec::new()))])),
        ("g", object(&[("g", number("1"))])),
    ];
    assert_eq!(
        printed(source, &replies),
        ["list 1 dict False 1", "dict [] False", "1 1"]
    );
}
