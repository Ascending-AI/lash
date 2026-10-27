//! Project the owning built-in rows to immutable lookup indexes.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::catalog::{Empty, Positional, Receiver, Row, objects};

/// Every positional built-in, by path. Each row's helper is the padded
/// argument list passed to `helper` (`helpers/*.kernel`):
/// `ts.boolean.convert`, `ts.math.pow`, `ts.number.convert` and
/// `ts.string.create`.
pub(crate) const POSITIONAL: &[(&str, Positional)] = &[
    (
        "Boolean",
        Positional {
            helper: "ts.to_boolean",
            arity: 1,
            float: None,
            empty: None,
        },
    ),
    (
        "Math.pow",
        Positional {
            helper: "ts.pow",
            arity: 2,
            float: None,
            empty: None,
        },
    ),
    (
        "Number",
        Positional {
            helper: "ts.number.convert_value",
            arity: 1,
            float: None,
            empty: Some(Empty::Zero),
        },
    ),
    (
        "String",
        Positional {
            helper: "ts.to_string",
            arity: 1,
            float: Some("ts.number.format_value"),
            empty: Some(Empty::Text),
        },
    ),
];

type Members = BTreeMap<&'static str, Vec<(Receiver, &'static str)>>;

pub(crate) fn generate() -> String {
    let (mut methods, mut properties) = (Members::new(), Members::new());
    let (mut functions, mut values) = (BTreeMap::new(), BTreeMap::new());
    let (mut constructors, mut instance_tests) = (BTreeMap::new(), BTreeMap::new());
    let (mut globals, mut classes) = (BTreeSet::new(), BTreeSet::new());
    let mut lengths = BTreeMap::new();
    for row in objects().into_iter().flat_map(|object| object.rows) {
        match row {
            Row::Method {
                path,
                length,
                receivers,
                function,
            } => {
                let (_, name) = path
                    .rsplit_once('.')
                    .filter(|(parent, _)| parent.ends_with(".prototype"))
                    .unwrap_or_else(|| panic!("method `{path}` is no `X.prototype.<name>`"));
                assert!(lengths.insert(path, length).is_none());
                globals.insert(root(path));
                insert(&mut functions, path, function);
                for receiver in receivers {
                    member(&mut methods, name, *receiver, path);
                }
            }
            Row::Property {
                receiver,
                name,
                function,
            } => {
                member(&mut properties, name, receiver, function);
            }
            Row::Function {
                path,
                length,
                function,
            } => {
                assert!(lengths.insert(path, length).is_none());
                globals.insert(root(path));
                insert(&mut functions, path, function);
            }
            Row::Value { path, function } => {
                globals.insert(root(path));
                insert(&mut values, path, function);
            }
            Row::Constructor {
                class,
                length,
                function,
            } => {
                if let Some(length) = length {
                    assert!(lengths.insert(class, length).is_none());
                }
                globals.insert(class);
                classes.insert(class);
                insert(&mut constructors, class, function);
            }
            Row::Class { class, length } => {
                globals.insert(class);
                classes.insert(class);
                assert!(lengths.insert(class, length).is_none());
            }
            Row::InstanceOf { class, function } => {
                globals.insert(class);
                classes.insert(class);
                insert(&mut instance_tests, class, function);
            }
        }
    }
    let mut member_names: BTreeSet<&str> =
        methods.keys().chain(properties.keys()).copied().collect();
    let mut prototype = false;
    for path in functions.keys().chain(values.keys()) {
        let mut segments = path.split('.');
        let class = segments.next();
        let mut segments = segments.peekable();
        if let (Some(class), Some(&"prototype")) = (class, segments.peek()) {
            classes.insert(class);
        }
        for segment in segments {
            if segment == "prototype" {
                prototype = true;
            } else {
                member_names.insert(segment);
            }
        }
    }
    member_names.extend(
        classes
            .iter()
            .filter_map(|class| class.rsplit_once('.').map(|(_, name)| name)),
    );
    if prototype || !classes.is_empty() {
        member_names.insert("prototype");
    }
    member_names.extend(globals.iter().copied());
    member_names.extend([
        "globalThis",
        "undefined",
        "NaN",
        "Infinity",
        "constructor",
        "length",
        "message",
        "name",
        "size",
    ]);
    for path in functions.keys().chain(classes.iter()) {
        assert!(
            lengths.contains_key(path),
            "`{path}` has no declared function length"
        );
    }
    let mut out = String::from("Table {\n");
    out.push_str("lengths: SortedMap::borrowed(&[\n");
    for (path, length) in lengths {
        let _ = writeln!(out, "({path:?}, {length}),");
    }
    out.push_str("]),\n");
    for (name, index) in [("methods", methods), ("properties", properties)] {
        out.push_str(&format!("{name}: SortedMap::borrowed(&[\n"));
        for (key, rows) in index {
            out.push_str(&format!("({key:?}, Rows(&["));
            for (receiver, path) in rows {
                out.push_str(&format!("(Receiver::{receiver:?}, {path:?}),"));
            }
            out.push_str("])),\n");
        }
        out.push_str("]),\n");
    }
    for (name, index) in [
        ("functions", functions.clone()),
        ("values", values),
        ("constructors", constructors),
        ("instance_tests", instance_tests),
    ] {
        out.push_str(&format!("{name}: SortedMap::borrowed(&[\n"));
        for (key, value) in index {
            out.push_str(&format!("({key:?}, {value:?}),\n"));
        }
        out.push_str("]),\n");
    }
    out.push_str("positional: SortedMap::borrowed(&[\n");
    let mut positional = POSITIONAL.to_vec();
    positional.sort_unstable_by_key(|(path, _)| *path);
    assert!(
        positional.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "two positional built-ins answer the same path"
    );
    for (path, positional) in positional {
        assert!(
            functions.contains_key(path),
            "positional built-in `{path}` has no function row"
        );
        let empty = match positional.empty {
            None => "None",
            Some(Empty::Text) => "Some(Empty::Text)",
            Some(Empty::Zero) => "Some(Empty::Zero)",
        };
        let _ = writeln!(
            out,
            "({path:?}, Positional {{ helper: {:?}, arity: {}, float: {:?}, empty: {empty} }}),",
            positional.helper, positional.arity, positional.float
        );
    }
    out.push_str("]),\n");
    for (name, index) in [
        ("globals", globals),
        ("classes", classes),
        ("member_names", member_names),
    ] {
        out.push_str(&format!("{name}: SortedSet::borrowed(&[\n"));
        for key in index {
            out.push_str(&format!("{key:?},\n"));
        }
        out.push_str("]),\n");
    }
    out.push_str("paths: OnceLock::new(),\n}\n");
    out
}

fn root(path: &'static str) -> &'static str {
    path.split('.').next().unwrap_or(path)
}

fn insert(
    index: &mut BTreeMap<&'static str, &'static str>,
    key: &'static str,
    value: &'static str,
) {
    assert!(
        index.insert(key, value).is_none(),
        "two built-in rows answer `{key}`"
    );
}

fn member(index: &mut Members, name: &'static str, receiver: Receiver, function: &'static str) {
    let rows = index.entry(name).or_default();
    assert!(
        rows.iter().all(|(other, _)| *other != receiver),
        "two built-in rows answer `{name}` of {receiver:?}"
    );
    rows.push((receiver, function));
}
