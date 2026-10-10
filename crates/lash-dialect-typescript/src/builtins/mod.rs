//! How a name the source does not bind reaches a library function.
//!
//! Each built-in object has one file here that lists its rows, and one
//! helper source in `helpers/` that defines the functions the rows name.
//! Function, method and constructor rows use the dialect's calling convention,
//! `(this: Any, args: List(Any)) -> Any`. Property and `instanceof` rows
//! take the tested value alone; value rows take no arguments.
//!
//! The lowerer resolves a global path (`Math.max`, `new Map`) to its row's
//! function when it lowers. A member's receiver is not known then, so for
//! each method or property name that has rows the package builder generates
//! one dispatcher, `ts.method.<name>` or `ts.property.<name>`, which tests
//! the receiver's kind in row order and otherwise falls back to the
//! object's own property. Lanes that add rows for one name never edit one
//! file.
//!
//! A built-in read as a value is a token, an immutable tuple the program
//! cannot otherwise make: `("ts.function", path, name, length, closure)` for
//! a function, constructor or prototype method, and `("ts.object", path,
//! tag)` for a namespace or a prototype. A token compares equal to every
//! other read of the same path. A function the program makes is the same
//! token with an empty path and one more member, the record of the own
//! properties it has deleted (`lower/functions.rs`); it compares by its
//! closure. Calling one goes through `ts.callable`, which the package
//! builder generates after every row's helper, because a helper cannot call
//! a function defined after it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

mod array;
mod boolean;
mod console;
mod date;
mod error;
mod function;
mod json;
mod lengths;
mod map;
mod math;
mod number;
mod object;
mod promise;
mod regexp;
mod set;
mod string;
mod uri;
mod url;
mod url_search_params;
mod values;

pub(crate) use values::{member_reader, sources as value_sources};

/// What a member's receiver is, as `ts.receiver` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Receiver {
    List,
    Record,
    Text,
    Number,
    Bool,
    Map,
    Set,
    Error,
    Timestamp,
    Closure,
    /// A record whose `brand` field is this text.
    Brand(&'static str),
}

impl Receiver {
    /// The text `ts.receiver` gives a value of this kind.
    pub(crate) fn tag(self) -> String {
        match self {
            Self::Record => "record".into(),
            Self::List => "list".into(),
            Self::Text => "text".into(),
            Self::Number => "number".into(),
            Self::Bool => "bool".into(),
            Self::Map => "map".into(),
            Self::Set => "set".into(),
            Self::Error => "error".into(),
            Self::Timestamp => "timestamp".into(),
            Self::Closure => "closure".into(),
            Self::Brand(brand) => format!("brand:{brand}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// `receiver.name(args)`.
    Method {
        receiver: Receiver,
        name: &'static str,
        function: &'static str,
    },
    /// A read of `receiver.name`: the function is called with the receiver
    /// alone.
    Property {
        receiver: Receiver,
        name: &'static str,
        function: &'static str,
    },
    /// A call of a global path: `Math.max(...)`, `parseInt(...)`.
    Function {
        path: &'static str,
        function: &'static str,
    },
    /// A read of a global path: `Math.PI`. The function is called with no
    /// arguments.
    Value {
        path: &'static str,
        function: &'static str,
    },
    /// `new Class(args)`.
    Constructor {
        class: &'static str,
        function: &'static str,
    },
    /// `value instanceof Class`: the function is called with the value as
    /// its one argument and gives a bool.
    InstanceOf {
        class: &'static str,
        function: &'static str,
    },
}

/// A built-in object's helper source and its rows.
pub(crate) struct Object {
    pub(crate) source: &'static str,
    pub(crate) rows: Vec<Row>,
}

/// Every built-in object, in the order their helpers are defined: an object
/// may call the helpers of those before it.
pub(crate) fn objects() -> Vec<Object> {
    vec![
        function::object(),
        map::object(),
        set::object(),
        boolean::object(),
        object::object(),
        math::object(),
        number::object(),
        date::object(),
        error::object(),
        url::object(),
        url_search_params::object(),
        json::object(),
        console::object(),
        array::object(),
        promise::object(),
        string::object(),
        regexp::object(),
        uri::object(),
    ]
}

/// What calling a built-in function value does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    /// Invokes the row's helper with the call's receiver and arguments.
    Helper(&'static str),
    /// A class only `new` makes: a call raises `TypeError`.
    RequiresNew,
    /// `Function`, which builds a function from source text.
    SourceText,
    /// `Function.prototype`, which ignores its arguments and gives
    /// `undefined`.
    Nothing,
}

impl Call {
    /// The library function a call of the built-in at `path` invokes, and
    /// the class name it takes in place of the call's receiver and
    /// arguments when it takes one.
    pub(crate) fn callee(self, path: &str) -> (&'static str, Option<&str>) {
        match self {
            Self::Helper(helper) => (helper, None),
            Self::RequiresNew => ("ts.requires_new", Some(path)),
            Self::SourceText => ("ts.function_from_text", None),
            Self::Nothing => ("ts.nothing", None),
        }
    }
}

/// What a built-in path is as a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Function {
        call: Call,
        length: u8,
    },
    /// A namespace (`Math`) or a prototype (`Array.prototype`).
    Object,
}

/// Paths ECMA-262 makes one function object: the first is read as the
/// second, whose name it has.
const ALIASES: &[(&str, &str)] = &[
    ("Number.parseFloat", "parseFloat"),
    ("Number.parseInt", "parseInt"),
    ("Set.prototype.keys", "Set.prototype.values"),
];

/// The rows, indexed the ways the lowerer and the package builder ask.
#[derive(Debug, Default)]
pub(crate) struct Table {
    pub(crate) methods: BTreeMap<&'static str, Vec<(Receiver, &'static str)>>,
    pub(crate) properties: BTreeMap<&'static str, Vec<(Receiver, &'static str)>>,
    pub(crate) functions: BTreeMap<&'static str, &'static str>,
    pub(crate) values: BTreeMap<&'static str, &'static str>,
    pub(crate) constructors: BTreeMap<&'static str, &'static str>,
    pub(crate) instance_tests: BTreeMap<&'static str, &'static str>,
    /// The first segment of every global path and every class.
    globals: BTreeSet<&'static str>,
    /// Every path a built-in value names.
    pub(crate) builtins: BTreeMap<String, Builtin>,
    /// The path whose value a method row's helper is, for each helper.
    pub(crate) method_paths: BTreeMap<&'static str, String>,
    /// Every name a built-in has a member by: each method row's name, the
    /// last segment of every path below a built-in, and the members no row
    /// gives (`constructor`, a prototype's `length`, `message`, `name` and
    /// `size`). A
    /// read of one of these names has a reader of its own,
    /// `ts.member.<name>`, so a program reaches the helpers of the names it
    /// reads and no others.
    pub(crate) member_names: BTreeSet<String>,
}

impl Table {
    fn build() -> Self {
        let mut table = Self::default();
        for row in objects().into_iter().flat_map(|object| object.rows) {
            let taken = match row {
                Row::Method {
                    receiver,
                    name,
                    function,
                } => insert_member(&mut table.methods, receiver, name, function),
                Row::Property {
                    receiver,
                    name,
                    function,
                } => insert_member(&mut table.properties, receiver, name, function),
                Row::Function { path, function } => {
                    table.globals.insert(root(path));
                    table.functions.insert(path, function).is_some()
                }
                Row::Value { path, function } => {
                    table.globals.insert(root(path));
                    table.values.insert(path, function).is_some()
                }
                Row::Constructor { class, function } => {
                    table.globals.insert(class);
                    table.constructors.insert(class, function).is_some()
                }
                Row::InstanceOf { class, function } => {
                    table.globals.insert(class);
                    table.instance_tests.insert(class, function).is_some()
                }
            };
            assert!(
                !taken,
                "two built-in rows answer the same question: {row:?}"
            );
        }
        table.index_values();
        table
    }

    /// Indexes every path a built-in value names: each function row's path,
    /// each class, and each object a path runs through.
    fn index_values(&mut self) {
        let lengths: BTreeMap<&str, u8> = lengths::LENGTHS.iter().copied().collect();
        let length = |path: &str| {
            *lengths
                .get(path)
                .unwrap_or_else(|| panic!("`{path}` has no length in builtins/lengths.rs"))
        };
        let mut classes: BTreeSet<&str> = self
            .constructors
            .keys()
            .chain(self.instance_tests.keys())
            .copied()
            .collect();
        let paths: Vec<&str> = self
            .functions
            .keys()
            .chain(self.values.keys())
            .copied()
            .collect();
        for path in &paths {
            let segments: Vec<&str> = path.split('.').collect();
            if segments.get(1) == Some(&"prototype") {
                classes.insert(segments[0]);
            }
        }
        for (path, helper) in &self.functions {
            let builtin = Builtin::Function {
                call: Call::Helper(helper),
                length: length(path),
            };
            self.builtins.insert((*path).to_string(), builtin);
        }
        for class in &classes {
            let call = match *class {
                "Function" => Call::SourceText,
                _ => Call::RequiresNew,
            };
            self.builtins
                .entry((*class).to_string())
                .or_insert(Builtin::Function {
                    call,
                    length: length(class),
                });
            let prototype = format!("{class}.prototype");
            let builtin = if *class == "Function" {
                Builtin::Function {
                    call: Call::Nothing,
                    length: 0,
                }
            } else {
                Builtin::Object
            };
            self.builtins.entry(prototype).or_insert(builtin);
        }
        for path in &paths {
            let segments: Vec<&str> = path.split('.').collect();
            for end in 1..segments.len() {
                self.builtins
                    .entry(segments[..end].join("."))
                    .or_insert(Builtin::Object);
            }
        }
        let mut unnamed = Vec::new();
        for (name, rows) in &self.methods {
            for (receiver, helper) in rows {
                match self.member_path(name, helper) {
                    Some(path) => {
                        self.method_paths.insert(helper, path);
                    }
                    None => unnamed.push(format!("{receiver:?} `{name}` ({helper})")),
                }
            }
        }
        assert!(
            unnamed.is_empty(),
            "these method rows have no `X.prototype.<name>` function row naming their helper: {unnamed:?}"
        );
        self.member_names = self
            .methods
            .keys()
            .map(ToString::to_string)
            .chain(
                self.builtins
                    .keys()
                    .map(String::as_str)
                    .chain(self.values.keys().copied())
                    .filter_map(|path| path.rsplit_once('.'))
                    .map(|(_, name)| name.to_string()),
            )
            .chain(["constructor", "length", "message", "name", "size"].map(ToString::to_string))
            .collect();
    }

    /// The path of the function a method row answers with: the one path of
    /// its helper, or the one that ends in `.prototype.<name>`.
    fn member_path(&self, name: &str, helper: &str) -> Option<String> {
        let paths: Vec<&str> = self
            .functions
            .iter()
            .filter(|(_, function)| **function == helper)
            .map(|(path, _)| *path)
            .collect();
        let suffix = format!(".prototype.{name}");
        match paths.as_slice() {
            [path] => Some((*path).to_string()),
            _ => paths
                .iter()
                .find(|path| path.ends_with(&suffix))
                .map(|path| (*path).to_string()),
        }
    }

    /// The value a path names, as kernel text: a token, or the value row's
    /// helper called with no arguments.
    pub(crate) fn token(&self, path: &str) -> Option<String> {
        let path = canonical(path);
        match self.builtins.get(path)? {
            Builtin::Function { call, length } => {
                let (helper, args) = call.callee(path);
                Some(format!(
                    "(\"ts.function\", \"{path}\", \"{}\", {length}.0, fn(callee_this, callee_args) {{ let called = invoke {helper}({}) return called }})",
                    function_name(path),
                    args.map_or_else(
                        || "callee_this, callee_args".to_string(),
                        |name| format!("\"{name}\"")
                    )
                ))
            }
            Builtin::Object => Some(format!(
                "(\"ts.object\", \"{path}\", \"{}\")",
                object_tag(path)
            )),
        }
    }
}

/// The path of the one function object `path` names.
pub(crate) fn canonical(path: &str) -> &str {
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == path)
        .map_or(path, |(_, canonical)| canonical)
}

/// A built-in function's `name`: the last segment of its path, or the class
/// itself; `Function.prototype` is the empty name.
pub(crate) fn function_name(path: &str) -> &str {
    if path == "Function.prototype" {
        return "";
    }
    path.rsplit('.').next().unwrap_or(path)
}

/// What `Object.prototype.toString` names a built-in object: its
/// `Symbol.toStringTag`, or the class its prototype is an instance of.
pub(crate) fn object_tag(path: &str) -> &'static str {
    match path {
        "Math" => "Math",
        "JSON" => "JSON",
        "console" => "console",
        "Array.prototype" => "Array",
        "String.prototype" => "String",
        "Number.prototype" => "Number",
        "Boolean.prototype" => "Boolean",
        "Map.prototype" => "Map",
        "Set.prototype" => "Set",
        "Promise.prototype" => "Promise",
        "URL.prototype" => "URL",
        "URLSearchParams.prototype" => "URLSearchParams",
        _ => "Object",
    }
}

fn insert_member(
    members: &mut BTreeMap<&'static str, Vec<(Receiver, &'static str)>>,
    receiver: Receiver,
    name: &'static str,
    function: &'static str,
) -> bool {
    let rows = members.entry(name).or_default();
    let taken = rows.iter().any(|(existing, _)| *existing == receiver);
    rows.push((receiver, function));
    taken
}

fn root(path: &'static str) -> &'static str {
    path.split('.').next().unwrap_or(path)
}

pub(crate) fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(Table::build)
}

/// Whether `name` is a global a built-in row answers for: a namespace
/// (`Math`), a function (`parseInt`) or a class (`Map`).
pub(crate) fn is_global(name: &str) -> bool {
    table().globals.contains(name)
        || matches!(name, "globalThis" | "undefined" | "NaN" | "Infinity")
}
