//! Guest own-property facts and lazy values, projected from the builtin rows.
//!
//! Facts are defined before the helpers; value readers after them. A token's
//! reader closes over a local factory, so constructor/prototype back references
//! do not introduce cycles in library function identities. Lookup closures
//! charge their actual traversal even inside another helper's formula extent.
use super::{Builtin, Row, Table};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
struct Property {
    name: String,
    value: String,
    writable: bool,
    accessor: bool,
    enumerable: bool,
    configurable: bool,
}

fn add(
    properties: &mut Vec<Property>,
    name: &str,
    value: String,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) {
    if !properties.iter().any(|p| p.name == name) {
        properties.push(Property {
            name: name.into(),
            value,
            writable,
            accessor: false,
            enumerable,
            configurable,
        });
    }
}

fn property_class(receiver: super::Receiver) -> Option<&'static str> {
    let tag = receiver.tag();
    super::values::CONSTRUCTORS
        .iter()
        .find(|(kind, _)| *kind == tag)
        .map(|(_, class)| *class)
        .filter(|class| !matches!(*class, "Array" | "String" | "Function"))
}

fn accessor_default(path: &str, name: &str) -> Option<&'static str> {
    (path == "RegExp.prototype").then_some(match name {
        "source" => "\"(?:)\"",
        "flags" => "\"\"",
        _ => "absent",
    })
}

fn getter_token(path: &str, name: &str, helper: &str) -> String {
    let getter_path = format!("{path}.get {name}");
    let special = accessor_default(path, name).map_or_else(String::new, |value|
        format!("if same(kind(callee_this), \"tuple\") {{ if same(callee_this[1], {path:?}) {{ return {value} }} }}"));
    format!(
        "(\"ts.function\", {getter_path:?}, \"get {name}\", 0.0, fn(callee_this, callee_args) {{ {special} let value = invoke {helper}(callee_this) return value }})"
    )
}

/// Static and computed property dispatchers project the same accessor rows
/// as own descriptors. The intrinsic RegExp prototype has fixed defaults.
pub(super) fn member_read(table: &Table, name: &str, uses: &mut Vec<String>) -> (String, usize) {
    let mut source = String::new();
    let mut count = 0;
    for (receiver, helper) in table.properties.get(name).into_iter().flatten() {
        let Some(class) = property_class(*receiver) else {
            continue;
        };
        let path = format!("{class}.prototype");
        let answer = if let Some(value) = accessor_default(&path, name) {
            format!("return {value}")
        } else {
            uses.push((*helper).to_string());
            format!("let value = invoke {helper}(this) return value")
        };
        source.push_str(&format!("if same(path, {path:?}) {{ {answer} }}\n"));
        count += 1;
    }
    (source, count)
}

fn model(table: &Table) -> BTreeMap<String, Vec<Property>> {
    let mut owners = BTreeMap::new();
    for path in table.builtin_paths() {
        let mut properties = Vec::new();
        if let Some(Builtin::Function { length, .. }) = table.builtin(path) {
            add(
                &mut properties,
                "length",
                format!("{length}.0"),
                false,
                false,
                true,
            );
            add(
                &mut properties,
                "name",
                format!("{:?}", super::function_name(path)),
                false,
                false,
                true,
            );
        }
        if table.classes.contains(path.as_ref()) {
            add(
                &mut properties,
                "prototype",
                format!("@{path}.prototype"),
                false,
                false,
                false,
            );
        }
        if let Some(class) = path.strip_suffix(".prototype") {
            if matches!(class, "Array" | "String") {
                add(
                    &mut properties,
                    "length",
                    "0.0".into(),
                    class == "Array",
                    false,
                    false,
                );
            }
            add(
                &mut properties,
                "constructor",
                format!("@{class}"),
                true,
                false,
                true,
            );
            if super::values::ERROR_CLASSES.contains(&class) {
                add(
                    &mut properties,
                    "name",
                    format!("{class:?}"),
                    true,
                    false,
                    true,
                );
                add(&mut properties, "message", "\"\"".into(), true, false, true);
            }
        }
        owners.insert(path.to_string(), properties);
    }
    // Row order is definition order. The same list drives names, attributes
    // and value readers; aliases retain their canonical function identity.
    for row in super::objects().into_iter().flat_map(|object| object.rows) {
        let (path, value, writable) = match row {
            Row::Function { path, .. } | Row::Method { path, .. } => {
                (path, format!("@{path}"), true)
            }
            Row::Value { path, function } => (path, format!("!{function}"), false),
            _ => continue,
        };
        if let Some((parent, name)) = path.rsplit_once('.') {
            add(
                owners.get_mut(parent).unwrap_or_else(|| {
                    unreachable!("each dotted builtin row contributes its parent token")
                }),
                name,
                value,
                writable,
                false,
                writable,
            );
        }
    }
    for (name, rows) in &table.properties {
        for (receiver, helper) in rows {
            let Some(class) = property_class(*receiver) else {
                continue;
            };
            let properties = owners
                .get_mut(&format!("{class}.prototype"))
                .unwrap_or_else(|| unreachable!("each receiver class contributes its prototype"));
            add(properties, name, format!("?{helper}"), false, false, true);
            properties
                .last_mut()
                .unwrap_or_else(|| unreachable!("the getter was added above"))
                .accessor = true;
        }
    }
    let mut getters = BTreeMap::new();
    for (path, properties) in &owners {
        for property in properties {
            if property.accessor {
                let name = format!("get {}", property.name);
                let mut attributes = Vec::new();
                add(&mut attributes, "length", "0.0".into(), false, false, true);
                add(
                    &mut attributes,
                    "name",
                    format!("{name:?}"),
                    false,
                    false,
                    true,
                );
                getters.insert(format!("{path}.{name}"), attributes);
            }
        }
    }
    owners.extend(getters);
    let mut global = Vec::new();
    for name in &table.globals {
        if table.builtin(name).is_some() {
            add(&mut global, name, format!("@{name}"), true, false, true);
        }
    }
    for (name, value) in [("undefined", "absent"), ("NaN", "nan"), ("Infinity", "inf")] {
        add(&mut global, name, value.into(), false, false, false);
    }
    add(
        &mut global,
        "globalThis",
        "@globalThis".into(),
        true,
        false,
        true,
    );
    owners.insert("globalThis".into(), global);
    owners
}

/// The fixed attributes are available without reaching any value's helper.
pub(crate) fn facts(table: &Table) -> String {
    let mut body = String::new();
    for (path, properties) in model(table) {
        let entries = properties
            .iter()
            .map(|p| {
                format!(
                    "{:?}: {{writable: {}, enumerable: {}, configurable: {}, accessor: {}}}",
                    p.name, p.writable, p.enumerable, p.configurable, p.accessor
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        body.push_str(&format!(
            "  if same(path, {path:?}) {{ return {{{entries}}} }}\n"
        ));
    }
    format!(
        "use same\nfunction ts.builtin_own(path: Text) -> Any\nkernel 1\ncharge sum(8, size(result))\nbody {{\nlet lookup = fn(path) {{\n{body}  return {{}}\n}}\nlet output = apply lookup(path)\nreturn output\n}}\n"
    )
}

fn reflection_internal(helper: &str) -> Option<String> {
    helper.strip_prefix("ts.reflect.").map(|name| {
        if name.starts_with("getOwnProperty") {
            format!("ts.object.{name}")
        } else {
            format!("ts.reflect_impl.{name}")
        }
    })
}

pub(crate) fn readers(table: &Table) -> Vec<String> {
    let mut uses = BTreeSet::from(["same".to_string(), "ts.get".to_string()]);
    let mut makes = String::new();
    for path in table.builtin_paths() {
        let mut token = table
            .token(path)
            .unwrap_or_else(|| unreachable!("builtin_paths only contains token paths"));
        if let Some(Builtin::Function { call, .. }) = table.builtin(path) {
            let helper = call.callee(path).0;
            if let Some(internal) = reflection_internal(helper) {
                uses.insert(internal.clone());
                token = token
                    .replace(helper, &internal)
                    .replace("callee_this, callee_args", "reader, callee_args");
            } else {
                uses.insert(helper.into());
            }
        }
        makes.push_str(&format!("if same(path, {path:?}) {{ return {token} }}\n"));
    }
    makes.push_str(
        "if same(path, \"globalThis\") { return (\"ts.object\", \"globalThis\", \"Object\") }\n",
    );
    let mut branches = String::new();
    for (path, properties) in model(table) {
        let mut reads = String::new();
        for p in properties {
            let answer = if let Some(child) = p.value.strip_prefix('@') {
                format!(
                    "let value = apply make({:?})\nreturn value",
                    super::canonical(child)
                )
            } else if let Some(helper) = p.value.strip_prefix('!') {
                uses.insert(helper.into());
                format!("let value = invoke {helper}()\nreturn value")
            } else if let Some(helper) = p.value.strip_prefix('?') {
                uses.insert(helper.into());
                let special = accessor_default(&path, &p.name);
                let getter = getter_token(&path, &p.name, helper);
                reads.push_str(&format!("if same(kind(key), \"tuple\") {{ if same(key[1], {:?}) {{ return {getter} }} }}\n", p.name));
                special.map_or_else(
                    || format!("let value = invoke {helper}(target) return value"),
                    |value| format!("return {value}"),
                )
            } else {
                format!("return {}", p.value)
            };
            reads.push_str(&format!("if same(key, {:?}) {{ {answer} }}\n", p.name));
        }
        branches.push_str(&format!("if same(path, {path:?}) {{\n{reads}}}\n"));
    }
    let mut methods = String::new();
    uses.insert("ts.has_own".into());
    let mut getters = String::new();
    for (name, rows) in &table.properties {
        for (receiver, helper) in rows {
            uses.insert((*helper).into());
            if let Some(class) = property_class(*receiver) {
                let getter = getter_token(&format!("{class}.prototype"), name, helper);
                getters.push_str(&format!("if same(receiver, {:?}) {{ if same(key[1], {name:?}) {{ return {getter} }} }}\n", receiver.tag()));
            }
        }
    }
    methods.push_str(&format!(
        "if same(kind(key), \"tuple\") {{\n{getters}return absent\n}}\n"
    ));
    methods.push_str("let own = invoke ts.has_own(target, key)\nif own { let value = invoke ts.get(target, key) return value }\n");
    for (name, rows) in &table.properties {
        for (receiver, helper) in rows {
            methods.push_str(&format!("if same(receiver, {:?}) {{ if same(key, {name:?}) {{ let value = invoke {helper}(target) return value }} }}\n", receiver.tag()));
        }
    }
    let mut constructors = String::new();
    for (receiver, class) in super::values::CONSTRUCTORS {
        constructors.push_str(&format!(
            "if same(receiver, {receiver:?}) {{ let value = apply make({class:?}) return value }}\n"
        ));
    }
    constructors.push_str("if same(receiver, \"builtin\") { let value = apply make(\"Object\") return value }\nif same(receiver, \"record\") { let value = apply make(\"Object\") return value }\n");
    uses.insert("ts.exotic.error_name".into());
    constructors.push_str(
        "if same(receiver, \"error\") { let class = invoke ts.exotic.error_name(target)\n",
    );
    for class in super::values::ERROR_CLASSES {
        constructors.push_str(&format!(
            "if same(class, {class:?}) {{ let value = apply make({class:?}) return value }}\n"
        ));
    }
    constructors.push_str("let value = apply make(\"Error\") return value }\n");
    methods.push_str(&format!(
        "if same(key, \"constructor\") {{\n{constructors}}}\n"
    ));
    for (name, rows) in &table.methods {
        for (receiver, path) in rows {
            methods.push_str(&format!("if same(receiver, {:?}) {{ if same(key, {name:?}) {{ let value = apply make({path:?}) return value }} }}\n", receiver.tag()));
        }
    }
    for path in table.builtin_paths() {
        if let Some(name) = path.strip_prefix("Object.prototype.")
            && !name.contains('.')
        {
            methods.push_str(&format!("if same(receiver, \"null\") {{}} else {{ if same(receiver, \"absent\") {{}} else {{ if same(key, {name:?}) {{ let value = apply make({path:?}) return value }} }} }}\n"));
        }
    }
    // The metadata's values are produced only on demand. `make` refers to
    // the same local reader for reflected functions, avoiding static library
    // identity cycles while keeping all public paths on the owning rows.
    let header = uses
        .into_iter()
        .map(|name| format!("use {name}\n"))
        .collect::<String>();
    let source = format!(
        "{header}use kind\nuse ts.receiver\nfunction ts.metadata.reader(target: Any, key: Any) -> Any\nkernel 1\ncharge sum(16, size(result))\nbody {{\nlet reader = absent\nlet make = fn(path) {{\n{makes}return absent\n}}\nset reader = fn(target, key) {{\nlet receiver = invoke ts.receiver(target)\nlet token = same(receiver, \"builtin\")\nif same(receiver, \"closure\") {{ if same(kind(target), \"tuple\") {{ set token = true }} }}\nif token {{\nlet path = target[1]\n{branches}\n}}\n{methods}let value = invoke ts.get(target, key)\nreturn value\n}}\nlet value = apply reader(target, key)\nreturn value\n}}\n"
    );
    let mut sources = vec![source];
    let reflected: BTreeSet<_> = table
        .functions
        .iter()
        .filter_map(|(_, helper)| helper.strip_prefix("ts.reflect."))
        .collect();
    for name in reflected {
        let internal = reflection_internal(&format!("ts.reflect.{name}"))
            .unwrap_or_else(|| unreachable!("a reflected helper"));
        sources.push(format!("use ts.metadata.reader\nuse {internal}\nfunction ts.reflect.{name}(this: Any, args: List(Any)) -> Any\nkernel 1\ncharge sum(8, deep(args), deep(result))\nbody {{\nlet reader = fn(target, key) {{ let value = invoke ts.metadata.reader(target, key) return value }}\nlet value = invoke {internal}(reader, args)\nreturn value\n}}\n"));
    }
    sources
}

/// Constructor and callable dispatch use exactly the constructor/function rows.
pub(crate) fn dispatch(table: &Table) -> Vec<String> {
    let mut sources = Vec::new();
    for (name, rows, instance) in [
        ("ts.construct", &table.constructors, false),
        ("ts.instanceof", &table.instance_tests, true),
    ] {
        let mut source = String::from("use same\nuse kind\nuse error.new\nuse ts.receiver\n");
        for (_, helper) in rows {
            source.push_str(&format!("use {helper}\n"));
        }
        let (params, target) = if instance {
            ("value: Any, target: Any", "value")
        } else {
            ("target: Any, args: List(Any)", "absent, args")
        };
        source.push_str(&format!("function {name}({params}) -> Any\nkernel 1\ncharge 12\nbody {{\nlet dispatch = fn({}) {{\nlet receiver = invoke ts.receiver(target)\nif same(receiver, \"closure\") {{ if same(kind(target), \"tuple\") {{\nlet path = target[1]\n", if instance { "value, target" } else { "target, args" }));
        for (class, helper) in rows {
            source.push_str(&format!("if same(path, {class:?}) {{ let output = invoke {helper}({target}) return output }}\n"));
        }
        source.push_str("}}\nthrow error.new(\"type_error\", \"A supported builtin constructor is required\", null)\n}\n");
        source.push_str(if instance {
            "let output = apply dispatch(value, target)\nreturn output\n}\n"
        } else {
            "let output = apply dispatch(target, args)\nreturn output\n}\n"
        });
        sources.push(source);
    }
    let mut uses = BTreeSet::new();
    let mut branches = String::new();
    for path in table.builtin_paths() {
        if let Some(Builtin::Function { call, .. }) = table.builtin(path) {
            let (helper, class) = call.callee(path);
            uses.insert(helper.to_string());
            let args = class.map_or_else(|| "this, args".into(), |name| format!("{name:?}"));
            branches.push_str(&format!(
                "if same(path, {path:?}) {{ let value = invoke {helper}({args}) return value }}\n"
            ));
        }
    }
    for (path, properties) in model(table) {
        for property in properties {
            if let Some(helper) = property.value.strip_prefix('?') {
                uses.insert(helper.to_string());
                let getter = format!("{path}.get {}", property.name);
                branches.push_str(&format!("if same(path, {getter:?}) {{ let value = invoke {helper}(this) return value }}\n"));
            }
        }
    }
    let header = uses
        .into_iter()
        .map(|name| format!("use {name}\n"))
        .collect::<String>();
    sources.push(format!("{header}use same\nuse error.new\nfunction ts.builtin_call(path: Text, this: Any, args: List(Any)) -> Any\nkernel 1\ncharge sum(8, deep(args), deep(result))\nbody {{\nlet dispatch = fn(path, this, args) {{\n{branches}throw error.new(\"type_error\", \"A builtin callable is required\", null)\n}}\nlet output = apply dispatch(path, this, args)\nreturn output\n}}\n"));
    sources
}
