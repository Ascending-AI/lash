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

mod catalog;
mod lengths;
mod metadata;
mod sorted;
mod values;

pub(crate) use catalog::{Empty, Object, Positional, objects};
pub use catalog::{Receiver, Row};
pub(crate) use metadata::{
    dispatch as value_dispatch, facts as metadata_facts, readers as metadata_readers,
};
use sorted::{SortedMap, SortedSet};
pub(crate) use values::{locale_element, member_reader, sources as value_sources};

/// The array methods whose traversal accepts a fixed callback's operands.
/// Both call discovery and lowering use this calling-convention recipe.
const ARRAY_CALLBACKS: [(&str, &str, usize); 3] = [
    ("sort", "ts.array.sort_slots", 2),
    ("map", "ts.array.map_slots", 3),
    ("filter", "ts.array.filter_slots", 3),
];

pub(crate) fn array_callback(method: &str) -> Option<(&'static str, usize)> {
    ARRAY_CALLBACKS
        .iter()
        .find(|(name, _, _)| *name == method)
        .map(|(_, helper, arity)| (*helper, *arity))
}

pub(crate) fn array_callback_method(helper: &str) -> Option<&'static str> {
    ARRAY_CALLBACKS
        .iter()
        .find(|(_, name, _)| *name == helper)
        .map(|(method, _, _)| *method)
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
///
/// The build projects these indexes from the owning rows. Lowering borrows
/// them without allocating or sorting; package-only path enumeration is lazy.
#[derive(Debug, Default)]
pub(crate) struct Table {
    /// Each method name's receiver kinds, with the path of the function
    /// each kind's member by that name is.
    pub(crate) methods: SortedMap<Rows>,
    /// Each property name's receiver kinds, with the function each kind's
    /// read calls.
    pub(crate) properties: SortedMap<Rows>,
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
        if path == "globalThis" {
            return Some(Builtin::Object);
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
            paths.insert(Cow::Borrowed("globalThis"));
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
        "Reflect" => "Reflect",
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

/// The JavaScript `length` of the built-in function at `path`.
fn length(path: &str) -> u8 {
    lengths::LENGTHS
        .binary_search_by(|(row, _)| (*row).cmp(path))
        .map(|found| lengths::LENGTHS[found].1)
        .unwrap_or_else(|_| panic!("`{path}` has no length in builtins/lengths.rs"))
}

/// The build derives these sorted indexes from the owning rows once. A
/// cold lowerer borrows them directly; only package-only path enumeration
/// needs a process-local cache.
pub(crate) fn table() -> &'static Table {
    static TABLE: Table = include!(concat!(env!("OUT_DIR"), "/builtin_table.rs"));
    &TABLE
}

/// One member's receiver rows, in their source order.
#[derive(Debug)]
pub(crate) struct Rows(&'static [(Receiver, &'static str)]);

impl std::ops::Deref for Rows {
    type Target = [(Receiver, &'static str)];

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a (Receiver, &'static str);
    type IntoIter = std::slice::Iter<'a, (Receiver, &'static str)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Whether no text that begins with `prefix` is the internal brand field
/// or a name a property or member row answers (`table.properties`,
/// `table.member_names`, the rows `ts.get_computed` searches). A plain
/// record's field by such a name is all that any reader of it gives, and
/// writing one leaves the record plain.
pub(crate) fn ordinary_prefix(prefix: &str) -> bool {
    let table = table();
    !"__lash_brand".starts_with(prefix)
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
        || matches!(
            name,
            "globalThis" | "undefined" | "NaN" | "Infinity" | "Intl"
        )
}
