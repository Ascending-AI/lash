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
//! each property name with rows the package builder generates a reader
//! `ts.property.<name>` that selects by receiver kind. Method calls read
//! their function value before applying it with the receiver. Lanes that add rows for one name never edit one
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

use std::borrow::Cow;
use std::collections::BTreeSet;
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
mod sorted;
mod string;
mod uri;
mod url;
mod url_search_params;
mod values;

use sorted::{SortedMap, SortedSet};
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
    /// An internal record with the kernel tuple marker and this type tag.
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
    /// A prototype's method (`Array.prototype.map`): one function object,
    /// which a read of `path` gives, and which a value of each of
    /// `receivers` answers for the member named by the path's last segment,
    /// so `receiver.name(args)` calls it.
    Method {
        path: &'static str,
        receivers: &'static [Receiver],
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
        console::object(),
        json::object(),
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

/// A built-in function a direct call reaches without its argument list:
/// its row's helper reads a fixed number of arguments, `undefined` for
/// each one missing, and answers what one helper answers over them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Positional {
    /// The helper the row's helper answers with.
    pub(crate) helper: &'static str,
    /// How many arguments it reads.
    pub(crate) arity: usize,
    /// The helper a first argument proven a float takes instead.
    pub(crate) float: Option<&'static str>,
    /// What a call with no arguments gives, where that is not the helper's
    /// answer for `undefined`.
    pub(crate) empty: Option<Empty>,
}

/// What a positional built-in called with no arguments gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Empty {
    /// `String()`.
    Text,
    /// `Number()`.
    Zero,
}

/// Every positional built-in, by path. Each row's helper is the padded
/// argument list passed to `helper` (`helpers/*.kernel`):
/// `ts.boolean.convert`, `ts.math.pow`, `ts.number.convert` and
/// `ts.string.create`.
const POSITIONAL: &[(&str, Positional)] = &[
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
            helper: "ts.to_number",
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
            float: Some("ts.number.to_string"),
            empty: Some(Empty::Text),
        },
    ),
];

/// The rows, indexed the ways the lowerer and the package builder ask.
///
/// Lowering builds this table in every process that lowers TypeScript, so
/// building it indexes the rows and nothing more: what a built-in path is
/// as a value is answered from the rows when asked
/// ([`builtin`](Self::builtin)), and the indexes only the package builder
/// walks are derived on its first use.
#[derive(Debug, Default)]
pub(crate) struct Table {
    /// Each method name's receiver kinds, with the path of the function
    /// each kind's member by that name is.
    pub(crate) methods: SortedMap<Vec<(Receiver, &'static str)>>,
    /// Each property name's receiver kinds, with the function each kind's
    /// read calls.
    pub(crate) properties: SortedMap<Vec<(Receiver, &'static str)>>,
    pub(crate) functions: SortedMap<&'static str>,
    pub(crate) positional: SortedMap<Positional>,
    pub(crate) values: SortedMap<&'static str>,
    pub(crate) constructors: SortedMap<&'static str>,
    pub(crate) instance_tests: SortedMap<&'static str>,
    /// The first segment of every global path and every class.
    globals: SortedSet,
    /// Every class: each constructor's and `instanceof` row's, and each
    /// whose prototype a path runs through.
    classes: SortedSet,
    /// Every name a built-in has a member by: each method row's name, the
    /// last segment of every path below a built-in, and the members no row
    /// gives (`constructor`, a prototype's `length`, `message`, `name` and
    /// `size`). A
    /// read of one of these names has a reader of its own,
    /// `ts.member.<name>`, so a program reaches the helpers of the names it
    /// reads and no others.
    pub(crate) member_names: SortedSet,
    /// Every path [`builtin`](Self::builtin) answers, derived on first use.
    paths: OnceLock<BTreeSet<Cow<'static, str>>>,
}

impl Table {
    fn build() -> Self {
        let (mut methods, mut properties) = (Vec::new(), Vec::new());
        let (mut functions, mut values) = (Vec::new(), Vec::new());
        let (mut constructors, mut instance_tests) = (Vec::new(), Vec::new());
        let (mut globals, mut classes) = (Vec::new(), Vec::new());
        for row in objects().into_iter().flat_map(|object| object.rows) {
            match row {
                Row::Method {
                    path,
                    receivers,
                    function,
                } => {
                    let Some((_, name)) = path
                        .rsplit_once('.')
                        .filter(|(parent, _)| parent.ends_with(".prototype"))
                    else {
                        panic!("method `{path}` is no `X.prototype.<name>`");
                    };
                    globals.push(root(path));
                    functions.push((path, function));
                    methods.extend(receivers.iter().map(|receiver| (name, (*receiver, path))));
                }
                Row::Property {
                    receiver,
                    name,
                    function,
                } => properties.push((name, (receiver, function))),
                Row::Function { path, function } => {
                    globals.push(root(path));
                    functions.push((path, function));
                }
                Row::Value { path, function } => {
                    globals.push(root(path));
                    values.push((path, function));
                }
                Row::Constructor { class, function } => {
                    globals.push(class);
                    classes.push(class);
                    constructors.push((class, function));
                }
                Row::InstanceOf { class, function } => {
                    globals.push(class);
                    classes.push(class);
                    instance_tests.push((class, function));
                }
            }
        }
        let functions = SortedMap::new(functions, "function");
        let values = SortedMap::new(values, "value");
        for (path, _) in POSITIONAL {
            assert!(
                functions.contains_key(path),
                "positional built-in `{path}` has no function row"
            );
        }
        let methods = members(methods, "method");
        let mut member_names: Vec<&'static str> = methods.keys().copied().collect();
        let mut prototype = false;
        for path in functions.keys().chain(values.keys()) {
            let mut segments = path.split('.');
            let class = segments.next();
            let mut segments = segments.peekable();
            if let (Some(class), Some(&"prototype")) = (class, segments.peek()) {
                classes.push(class);
            }
            // Each segment below the first is the last of the path itself
            // or of an object the path runs through; `prototype` is named
            // once below.
            for segment in segments {
                if segment == "prototype" {
                    prototype = true;
                } else {
                    member_names.push(segment);
                }
            }
        }
        let classes = SortedSet::new(classes);
        member_names.extend(classes.iter().filter_map(|class| last_segment(class)));
        if prototype || !classes.is_empty() {
            member_names.push("prototype");
        }
        member_names.extend(["constructor", "length", "message", "name", "size"]);
        Self {
            methods,
            properties: members(properties, "property"),
            functions,
            positional: SortedMap::new(POSITIONAL.to_vec(), "positional"),
            values,
            constructors: SortedMap::new(constructors, "constructor"),
            instance_tests: SortedMap::new(instance_tests, "instanceof"),
            globals: SortedSet::new(globals),
            classes,
            member_names: SortedSet::new(member_names),
            paths: OnceLock::new(),
        }
    }

    /// What `path` names as a value, when it is a built-in: a function
    /// row's path, then a class, then a class's prototype, then an object
    /// a function or value row's path runs through.
    pub(crate) fn builtin(&self, path: &str) -> Option<Builtin> {
        if let Some(helper) = self.functions.get(path) {
            return Some(Builtin::Function {
                call: Call::Helper(helper),
                length: length(path),
            });
        }
        if self.classes.contains(path) {
            let call = match path {
                "Function" => Call::SourceText,
                _ => Call::RequiresNew,
            };
            return Some(Builtin::Function {
                call,
                length: length(path),
            });
        }
        if let Some(class) = path.strip_suffix(".prototype")
            && self.classes.contains(class)
        {
            return Some(if class == "Function" {
                Builtin::Function {
                    call: Call::Nothing,
                    length: 0,
                }
            } else {
                Builtin::Object
            });
        }
        let below = format!("{path}.");
        let runs_through = |paths: &SortedMap<&'static str>| {
            paths
                .from(&below)
                .next()
                .is_some_and(|(found, _)| found.starts_with(&below))
        };
        (runs_through(&self.functions) || runs_through(&self.values)).then_some(Builtin::Object)
    }

    /// Every path [`builtin`](Self::builtin) answers, in order: each
    /// function row's path, each class and its prototype, and each object a
    /// function or value row's path runs through.
    pub(crate) fn builtin_paths(&self) -> &BTreeSet<Cow<'static, str>> {
        self.paths.get_or_init(|| {
            let mut paths: BTreeSet<Cow<'static, str>> = BTreeSet::new();
            for class in &self.classes {
                paths.insert(Cow::Borrowed(*class));
                paths.insert(Cow::Owned(format!("{class}.prototype")));
            }
            for path in self.functions.keys().chain(self.values.keys()) {
                for (end, _) in path.match_indices('.') {
                    paths.insert(Cow::Borrowed(&path[..end]));
                }
            }
            paths.extend(self.functions.keys().map(|path| Cow::Borrowed(*path)));
            for path in &paths {
                assert!(
                    self.builtin(path).is_some(),
                    "`{path}` is a built-in path that names no built-in"
                );
            }
            paths
        })
    }

    /// The value a path names, as kernel text: a token, or the value row's
    /// helper called with no arguments.
    pub(crate) fn token(&self, path: &str) -> Option<String> {
        let path = canonical(path);
        match self.builtin(path)? {
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

/// Member rows by name, each name's rows in the order they came: one
/// row per receiver kind, with its row's path or function.
fn members(
    mut rows: Vec<(&'static str, (Receiver, &'static str))>,
    what: &str,
) -> SortedMap<Vec<(Receiver, &'static str)>> {
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut grouped: Vec<(&'static str, Vec<(Receiver, &'static str)>)> = Vec::new();
    for (name, (receiver, function)) in rows {
        match grouped.last_mut() {
            Some((last, group)) if *last == name => {
                assert!(
                    group.iter().all(|(existing, _)| *existing != receiver),
                    "two built-in rows answer the same question: {what} `{name}` of {receiver:?}"
                );
                group.push((receiver, function));
            }
            _ => grouped.push((name, vec![(receiver, function)])),
        }
    }
    SortedMap::sorted(grouped)
}

/// The JavaScript `length` of the built-in function at `path`.
fn length(path: &str) -> u8 {
    lengths::LENGTHS
        .binary_search_by(|(row, _)| (*row).cmp(path))
        .map(|found| lengths::LENGTHS[found].1)
        .unwrap_or_else(|_| panic!("`{path}` has no length in builtins/lengths.rs"))
}

/// The last segment of a dotted path, when it has more than one.
fn last_segment(path: &'static str) -> Option<&'static str> {
    path.rsplit_once('.').map(|(_, name)| name)
}

fn root(path: &'static str) -> &'static str {
    path.split('.').next().unwrap_or(path)
}

pub(crate) fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(Table::build)
}

/// Whether no text that begins with `prefix` is the internal brand field
/// or a name a property or member row answers (`table.properties`,
/// `table.member_names`, the rows `ts.get_computed` searches). A plain
/// record's field by such a name is all that any reader of it gives, and
/// writing one leaves the record plain.
pub(crate) fn ordinary_prefix(prefix: &str) -> bool {
    let table = table();
    !"__ts_brand".starts_with(prefix)
        && !table
            .properties
            .from(prefix)
            .next()
            .is_some_and(|(name, _)| name.starts_with(prefix))
        && !table
            .member_names
            .from(prefix)
            .next()
            .is_some_and(|name| name.starts_with(prefix))
}

/// Whether `name` is a global a built-in row answers for: a namespace
/// (`Math`), a function (`parseInt`) or a class (`Map`).
pub(crate) fn is_global(name: &str) -> bool {
    table().globals.contains(name)
        || matches!(name, "globalThis" | "undefined" | "NaN" | "Infinity")
}
