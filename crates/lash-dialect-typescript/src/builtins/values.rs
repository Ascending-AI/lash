//! The kernel text of the functions that read built-ins as values.
//!
//! A token carries the closure that calls it (`Table::token`), so whatever
//! makes a token reaches that one function's helper. A member is read by
//! the reader of its name, `ts.member.<name>`, which answers only the
//! built-ins that have a member by that name: a program reaches the helpers
//! of the names it reads, as it does through the method dispatchers. Only a
//! computed name, which may be any name, reaches them all.

use std::collections::{BTreeMap, BTreeSet};

use super::{Receiver, Table};

/// The error classes, whose prototypes carry `name` and `message`.
const ERROR_CLASSES: &[&str] = &[
    "AggregateError",
    "Error",
    "EvalError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "TypeError",
    "URIError",
];

/// The class whose instances a value of each `ts.receiver` kind are, for
/// `value.constructor`.
const CONSTRUCTORS: &[(&str, &str)] = &[
    ("list", "Array"),
    ("brand:regex.match", "Array"),
    ("text", "String"),
    ("number", "Number"),
    ("bool", "Boolean"),
    ("map", "Map"),
    ("set", "Set"),
    ("timestamp", "Date"),
    ("brand:date.invalid", "Date"),
    ("brand:regex.ecma", "RegExp"),
    ("brand:url.whatwg", "URL"),
    ("brand:url.search_params", "URLSearchParams"),
    ("closure", "Function"),
];

/// The prototypes a built-in inherits from, nearest first, after its own
/// members: a function's are `Function.prototype`'s, an error prototype's
/// `Error.prototype`'s, and every built-in's but `Object.prototype` itself
/// are `Object.prototype`'s.
const ANCESTORS: &[&str] = &["Function.prototype", "Error.prototype", "Object.prototype"];

/// The generated sources that come before the member readers, in the order
/// they must be defined.
pub(crate) fn sources(table: &Table) -> Vec<String> {
    vec![
        READ.to_string(),
        CALL_MEMBER.to_string(),
        in_operator(table),
    ]
}

fn header(uses: impl IntoIterator<Item = String>) -> String {
    let uses: BTreeSet<String> = uses.into_iter().collect();
    uses.into_iter()
        .map(|name| format!("use {name}\n"))
        .collect()
}

/// The token of `path`, which every caller knows is a built-in value.
fn token(table: &Table, path: &str) -> String {
    table
        .token(path)
        .unwrap_or_else(|| panic!("`{path}` is a built-in value"))
}

/// The library function a token's or a value row's kernel text calls.
fn called(table: &Table, path: &str) -> Option<String> {
    if let Some(function) = table.values.get(path) {
        return Some((*function).to_string());
    }
    let path = super::canonical(path);
    match table.builtins.get(path) {
        Some(super::Builtin::Function { call, .. }) => Some(call.callee(path).0.to_string()),
        _ => None,
    }
}

/// `ts.read(target, key)`: a member read of a name no built-in has. A
/// built-in function's token answers its `name` and `length`, and has no
/// `prototype`; no built-in's token has an element; anything else of one is
/// not a member the dialect has. A function the program made, whose token's
/// path is empty, is read as `ts.get` reads it.
const READ: &str = "use same\nuse kind\nuse bool.not\nuse num.le\nuse text.concat\nuse error.new\nuse ts.receiver\nuse ts.get\nuse ts.list_index\nuse ts.to_property_key\n\
function ts.read(target: Any, key: Any) -> Any
kernel 1
errors \"type_error\", \"TS_METHOD_UNSUPPORTED\", \"TS_REFLECTION_UNSUPPORTED\"
charge sum(12, deep(key))
body {
  # A plain object's field, read in place. Reading the `brand` field raises
  # `type_error` for a target that is no record or error, and reading the
  # index for one that is no record or a key that is no text: each takes
  # the generic read below.
  try {
    if same(target.brand, absent) { return target[key] }
  } catch generic {}
  let receiver = invoke ts.receiver(target)
  if same(receiver, \"record\") {
    let name = invoke ts.to_property_key(key)
    return target[name]
  }
  let token = same(receiver, \"builtin\")
  if same(receiver, \"closure\") { if same(kind(target), \"tuple\") { set token = bool.not(same(target[1], \"\")) } }
  if token {
    let name = invoke ts.to_property_key(key)
    if same(receiver, \"closure\") {
      if same(name, \"name\") { return target[2] }
      if same(name, \"length\") { return target[3] }
      if same(name, \"prototype\") { return absent }
      for poisoned in [\"caller\", \"arguments\"] {
        if same(name, poisoned) { throw error.new(\"TS_REFLECTION_UNSUPPORTED\", \"Reflection on a built-in function is outside the TypeScript dialect\", null) }
      }
    }
    # No built-in has an element: `Object[0]` is undefined.
    let index = invoke ts.list_index(name, 4294967295.0)
    if num.le(0, index) { return absent }
    throw error.new(\"TS_METHOD_UNSUPPORTED\", text.concat(\"`\", text.concat(target[1], text.concat(\".\", text.concat(name, \"` is not a member the TypeScript dialect has\")))), null)
  }
  let found = invoke ts.get(target, key)
  return found
}
";

/// `ts.member.<name>(value)`: `value.<name>`. A record's own field comes
/// first. A token answers its own member by that name, then the one it
/// inherits; a value of a built-in kind answers the function its method row
/// names, as a token, so `[].map === Array.prototype.map` and a borrowed
/// method runs its own helper whatever receiver it is later called with.
pub(crate) fn member_reader(table: &Table, name: &str) -> String {
    let mut uses: Vec<String> = [
        "same",
        "kind",
        "bool.not",
        "record.contains",
        "ts.receiver",
        "ts.read",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    let answer = |path: &str, uses: &mut Vec<String>| {
        uses.extend(called(table, path));
        match table.values.get(path) {
            Some(function) => format!("let value = invoke {function}()\n      return value"),
            None => format!("return {}", token(table, path)),
        }
    };
    let mut branches = 0usize;
    let mut tokens = String::new();
    for parent in parents(table, name) {
        branches += 1;
        let child = answer(&format!("{parent}.{name}"), &mut uses);
        tokens.push_str(&format!(
            "    if same(path, \"{parent}\") {{\n      {child}\n    }}\n"
        ));
    }
    tokens.push_str(&special(table, name, &mut uses));
    for ancestor in ANCESTORS {
        let path = format!("{ancestor}.{name}");
        if !table.builtins.contains_key(&path) {
            continue;
        }
        branches += 1;
        let child = answer(&path, &mut uses);
        let heirs = match *ancestor {
            "Function.prototype" => "same(receiver, \"closure\")".to_string(),
            "Error.prototype" => {
                let heirs: Vec<String> = ERROR_CLASSES
                    .iter()
                    .filter(|class| **class != "Error")
                    .map(|class| format!("\"{class}.prototype\""))
                    .collect();
                uses.push("list.contains".to_string());
                format!("list.contains([{}], path)", heirs.join(", "))
            }
            _ => {
                uses.push("bool.not".to_string());
                "bool.not(same(path, \"Object.prototype\"))".to_string()
            }
        };
        tokens.push_str(&format!("    if {heirs} {{\n      {child}\n    }}\n"));
    }
    if table
        .builtins
        .contains_key(&format!("Function.prototype.{name}"))
        && !table
            .builtins
            .contains_key(&format!("Object.prototype.{name}"))
    {
        // A namespace or a prototype that is no function does not inherit
        // Function.prototype: `JSON.bind` is undefined.
        tokens.push_str("    if same(receiver, \"builtin\") { return absent }\n");
    }
    let mut kinds = String::new();
    for (receiver, helper) in table.methods.get(name).into_iter().flatten() {
        branches += 1;
        let child = answer(&table.method_paths[helper], &mut uses);
        kinds.push_str(&format!(
            "  if same(receiver, \"{}\") {{\n    {child}\n  }}\n",
            receiver.tag()
        ));
        if *receiver == Receiver::List {
            kinds.push_str(&format!(
                "  if same(receiver, \"brand:regex.match\") {{\n    {child}\n  }}\n"
            ));
        }
    }
    if name == "constructor" {
        kinds.push_str(&constructor(table, &mut uses));
    }
    let inherited = format!("Object.prototype.{name}");
    if table.builtins.contains_key(&inherited) {
        // Every object inherits Object.prototype's methods.
        let child = answer(&inherited, &mut uses);
        kinds.push_str(&format!(
            "  if same(receiver, \"record\") {{\n    {child}\n  }}\n"
        ));
    }
    format!(
        "{}function ts.member.{name}(this: Any) -> Any\nkernel 1\n\
         errors \"type_error\", \"TS_METHOD_UNSUPPORTED\", \"TS_REFLECTION_UNSUPPORTED\"\n\
         charge {}\nbody {{\n\
         \x20 if same(kind(this), \"record\") {{\n\
         \x20   let own = this[\"{name}\"]\n\
         \x20   if same(own, absent) {{}} else {{\n\
         \x20     if same(this.brand, absent) {{ return own }}\n\
         \x20   }}\n\
         \x20 }}\n\
         \x20 let receiver = invoke ts.receiver(this)\n\
         \x20 if same(receiver, \"record\") {{\n\
         \x20   if record.contains(this, \"{name}\") {{ return this[\"{name}\"] }}\n\
         \x20 }}\n\
         \x20 let token = same(receiver, \"builtin\")\n\
         \x20 if same(receiver, \"closure\") {{ if same(kind(this), \"tuple\") {{ set token = bool.not(same(this[1], \"\")) }} }}\n\
         \x20 if token {{\n\
         \x20   let path = this[1]\n{tokens}\
         \x20   let outcome = invoke ts.read(this, \"{name}\")\n\
         \x20   return outcome\n\
         \x20 }}\n{kinds}\
         \x20 let outcome = invoke ts.read(this, \"{name}\")\n\
         \x20 return outcome\n}}\n",
        header(uses),
        8 + branches
    )
}

/// The built-ins with a member called `name` of their own.
fn parents<'t>(table: &'t Table, name: &str) -> BTreeSet<&'t str> {
    table
        .builtins
        .keys()
        .map(String::as_str)
        .chain(table.values.keys().copied())
        .filter_map(|path| path.rsplit_once('.'))
        .filter(|(_, member)| *member == name)
        .map(|(parent, _)| parent)
        .collect()
}

/// The members of a built-in that no row gives: an error prototype's `name`
/// and `message`, the `length` of the prototypes that are themselves an
/// array and a string, a Map's and a Set's `size` getter, and a built-in's
/// `constructor`.
fn special(table: &Table, name: &str, uses: &mut Vec<String>) -> String {
    let mut text = String::new();
    match name {
        "name" | "message" => {
            for class in ERROR_CLASSES {
                let value = if name == "name" { *class } else { "" };
                text.push_str(&format!(
                    "    if same(path, \"{class}.prototype\") {{ return \"{value}\" }}\n"
                ));
            }
        }
        "length" => {
            for listed in ["Array.prototype", "String.prototype"] {
                text.push_str(&format!(
                    "    if same(path, \"{listed}\") {{ return 0.0 }}\n"
                ));
            }
        }
        "size" => {
            uses.push("error.new".to_string());
            for keyed in ["Map.prototype", "Set.prototype"] {
                text.push_str(&format!(
                    "    if same(path, \"{keyed}\") {{ throw error.new(\"type_error\", \"Method get {keyed}.size called on incompatible receiver\", null) }}\n"
                ));
            }
        }
        "constructor" => {
            for path in table.builtins.keys() {
                if let Some(class) = path.strip_suffix(".prototype") {
                    uses.extend(called(table, class));
                    text.push_str(&format!(
                        "    if same(path, \"{path}\") {{ return {} }}\n",
                        token(table, class)
                    ));
                }
            }
            uses.extend(called(table, "Function"));
            uses.extend(called(table, "Object"));
            text.push_str(&format!(
                "    if same(receiver, \"closure\") {{ return {} }}\n    return {}\n",
                token(table, "Function"),
                token(table, "Object")
            ));
        }
        _ => {}
    }
    text
}

/// `value.constructor` for a value of a built-in kind, an error, and a
/// record without a `constructor` of its own.
fn constructor(table: &Table, uses: &mut Vec<String>) -> String {
    let mut text = String::new();
    for (receiver, class) in CONSTRUCTORS {
        uses.extend(called(table, class));
        text.push_str(&format!(
            "  if same(receiver, \"{receiver}\") {{ return {} }}\n",
            token(table, class)
        ));
    }
    uses.push("ts.exotic.error_name".to_string());
    text.push_str(
        "  if same(receiver, \"error\") {\n    let error_class = invoke ts.exotic.error_name(this)\n",
    );
    for class in ERROR_CLASSES {
        uses.extend(called(table, class));
        text.push_str(&format!(
            "    if same(error_class, \"{class}\") {{ return {} }}\n",
            token(table, class)
        ));
    }
    uses.extend(called(table, "Object"));
    text.push_str(&format!(
        "    return {}\n  }}\n  if same(receiver, \"record\") {{ return {} }}\n",
        token(table, "Error"),
        token(table, "Object")
    ));
    text
}

/// `key in target`: an own property, or one the target's class or
/// `Object.prototype` gives it. A built-in's or a function's properties are
/// reflection, which `ts.has` refuses.
fn in_operator(table: &Table) -> String {
    let mut inherited: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    for (name, rows) in table.methods.iter().chain(&table.properties) {
        for (receiver, _) in rows {
            inherited.entry(receiver.tag()).or_default().insert(name);
        }
    }
    let error = inherited.entry("error".to_string()).or_default();
    error.extend(["name", "message"]);
    let regex = inherited.entry("brand:regex.ecma".to_string()).or_default();
    regex.extend([
        "source",
        "flags",
        "global",
        "ignoreCase",
        "multiline",
        "dotAll",
        "unicode",
        "sticky",
    ]);
    let mut branches = String::new();
    for (receiver, names) in &inherited {
        let names: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
        branches.push_str(&format!(
            "  if same(receiver, \"{receiver}\") {{\n    for member in [{}] {{ if same(name, member) {{ return true }} }}\n  }}\n",
            names.join(", ")
        ));
    }
    let mut common: Vec<String> = table
        .builtins
        .keys()
        .filter_map(|path| path.strip_prefix("Object.prototype."))
        .filter(|name| !name.contains('.'))
        .map(|name| format!("\"{name}\""))
        .collect();
    common.push("\"constructor\"".to_string());
    format!(
        "use same\nuse ts.has\nuse ts.receiver\nuse ts.to_property_key\n\
         function ts.in(target: Any, key: Any) -> Bool\nkernel 1\n\
         errors \"type_error\", \"TS_REFLECTION_UNSUPPORTED\"\ncharge sum(12, deep(key))\nbody {{\n\
         \x20 let own = invoke ts.has(target, key)\n\
         \x20 if own {{ return true }}\n\
         \x20 let name = invoke ts.to_property_key(key)\n\
         \x20 let receiver = invoke ts.receiver(target)\n{branches}\
         \x20 for member in [{}] {{ if same(name, member) {{ return true }} }}\n\
         \x20 return false\n}}\n",
        common.join(", ")
    )
}

/// `target.key(args)` where neither a row nor a built-in has the name: the
/// member, called with the object as `this`.
const CALL_MEMBER: &str = "use ts.read\nuse ts.call_value\n\
function ts.call_member(target: Any, key: Any, args: List(Any)) -> Any
kernel 1
errors \"type_error\"
charge sum(16, deep(key))
body {
  let member = invoke ts.read(target, key)
  let outcome = invoke ts.call_value(member, target, key, args)
  return outcome
}
";
